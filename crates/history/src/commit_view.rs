//! One commit's changes against its first parent (spec 009), with the same entity cards as the
//! review. Its own tab, opened from History or through `OpenCommit`.

use std::collections::HashSet;
use std::sync::Arc;

use change_cards::{CardLines, CardRows, CommitCounts, Row};
use gpui::{
    AnyElement, AnyView, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListState, Render, SharedString, StyleRefinement, Subscription, Task, WeakEntity, Window, list,
};
use gq_language::SchemaModel;
use graph_client::api::{ChangeCauseOutput, CommitKind, CommitOutput};
use graph_store::{
    ChangeFold, EntityGroups, FoldedChanges, GraphSession, GroupRun, GroupSection, NodeNames,
    SessionEvent, TypeCounts,
};
use studio_actions::changes::{OpenChanged, ToggleChange};
use studio_actions::graph::{CreateBranchAt, OpenCommit, OpenNodePage, OpenQueryAt, OpenType};
use studio_actions::history::{LoadMoreChanges, NewBranchHere};
use studio_ui::ds::Tipped;
use studio_ui::menu::ContextMenu;
use studio_ui::{now_micros, relative_time, short_commit_id, utc_date_time};
use studio_workspace::{Inspected, InspectedNode, Item, ItemEvent, SavedItem, Workspace};
use ui::prelude::*;
use ui::{KeyBinding, PopoverMenu};

/// Changes read before asking the person to load more (constitution IX).
const CAP: usize = 5_000;
/// Changes asked for per page: the whole of [`CAP`], since each page restarts the server's
/// comparison after the last change returned; the server's byte budget still bounds a page.
const PAGE: u32 = CAP as u32;

enum State {
    Loading,
    Ready,
    Failed(SharedString),
    /// Changes the server will never list, said as History's rows say them
    /// ([`crate::counts::lasting`]): the graph's first commit, whose changes are not asked for;
    /// a schema change; history cleaned up; another comparison refused. Asking again gets the
    /// same answer, so nothing offers it.
    Lasting(CommitCounts),
}

/// Why a read of a commit's changes stopped short of them.
enum Stop {
    Lasting(CommitCounts),
    Failed(String),
}

pub struct CommitView {
    session: Entity<GraphSession>,
    commit_id: String,
    /// The commit as listed; read by its id when the view opens without it (its own tab).
    commit: Option<CommitOutput>,
    /// The commit is being read: until it is in, the header names no branch, since a commit
    /// held off every branch names the branch it was written on.
    commit_pending: bool,
    _commit_task: Option<Task<()>>,
    cause: Option<ChangeCauseOutput>,
    state: State,
    fold: ChangeFold,
    /// Shared with the work done off the main thread: grouping again under another schema.
    folded: Arc<FoldedChanges>,
    /// The changes by entity, each node with its relationship changes, as the review shows
    /// them.
    groups: EntityGroups,
    /// The schema the groups and the cards' rows were made with: one with another text, loaded
    /// since or applied, or naming nodes otherwise, groups the changes again.
    schema: Option<Arc<SchemaModel>>,
    /// The cards' property rows, made as they are first drawn; new with each fold.
    card_rows: CardRows,
    runs: Vec<GroupRun>,
    /// Names of the nodes known here only by id, read at the commit.
    names: NodeNames,
    /// How many relationship lines each card shows.
    card_lines: CardLines,
    /// The run each row is in, by row: a folded run's entity rows draw nothing.
    row_runs: Vec<usize>,
    /// The runs folded to their row, as the review folds them: by section, kind and type.
    folded_runs: HashSet<(GroupSection, bool, String)>,
    /// The entities unfolded to their changes, by type and id; each row starts folded, as the
    /// review's do.
    open: HashSet<(String, String)>,
    _names_task: Option<Task<()>>,
    counts: Vec<TypeCounts>,
    rows: Vec<Row>,
    next_page_token: Option<String>,
    /// Why the last read of more changes stopped: what was read before it stays, and the next
    /// press goes on from the page that failed.
    more_error: Option<SharedString>,
    list_state: ListState,
    focus_handle: FocusHandle,
    _task: Option<Task<()>>,
    /// The commit and its changes are read when the tab is next drawn ([`Self::when_shown`]):
    /// a tab restored with the workspace behind another reads nothing until it is shown.
    read_owed: bool,
    _regroup_task: Option<Task<()>>,
    _schema_subscription: Subscription,
    /// The entity selected, by its group: J and K move it, Space unfolds it, ⌘↵ opens its page,
    /// and the inspector shows its node (spec 033, as a review's selection does).
    selected: Option<usize>,
    /// The selected entity by type and id, which [`Self::selected`] is found again from when
    /// the groups are made again: a later page or a schema groups and orders them anew.
    selected_key: Option<(String, String)>,
    /// The first change was selected once, as the commit opened.
    selected_first: bool,
    /// The node the workspace was last told is selected ([`ItemEvent::Selected`]).
    inspected: Option<InspectedNode>,
    /// The large runs were folded once, when the changes were first shown ([`change_cards::FOLD_PAST`]).
    folded_large: bool,
    /// The changes' list, a view of its own drawn cached (`docs/drawing.md`): the header's reads
    /// and the tab's own redraws leave it as it was.
    list_view: Entity<CommitRows>,
}

impl EventEmitter<ItemEvent> for CommitView {}

impl Focusable for CommitView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for CommitView {
    fn tab_title(&self, _cx: &App) -> SharedString {
        format!("Commit {}", short_commit_id(&self.commit_id)).into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::GitCommit)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(commit_key(&self.commit_id).into())
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "commit".into(),
            data: serde_json::json!({ "commit_id": self.commit_id }),
        })
    }

    fn inspected(&mut self, _cx: &mut Context<Self>) -> Option<Inspected> {
        self.selected_node().map(Inspected::Node)
    }

    fn caches_its_parts(&self) -> bool {
        true
    }
}

pub fn commit_key(commit_id: &str) -> String {
    format!("commit:{commit_id}")
}

impl CommitView {
    pub fn new(
        session: Entity<GraphSession>,
        commit_id: String,
        commit: Option<CommitOutput>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::unread(session, commit_id, commit, cx);
        view.read_commit(cx);
        view.load_more(cx);
        view
    }

    /// A commit tab restored with the workspace, which reads the commit and its changes once it
    /// is first drawn ([`Self::read_owed`]).
    pub fn when_shown(
        session: Entity<GraphSession>,
        commit_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::unread(session, commit_id, None, cx);
        view.read_owed = true;
        view
    }

    fn unread(
        session: Entity<GraphSession>,
        commit_id: String,
        commit: Option<CommitOutput>,
        cx: &mut Context<Self>,
    ) -> Self {
        let schema_subscription = cx.subscribe(&session, |this, _, event: &SessionEvent, cx| {
            if let SessionEvent::SchemaChanged = event {
                this.regroup(cx);
            }
        });
        Self {
            session,
            commit_id,
            commit,
            commit_pending: false,
            _commit_task: None,
            cause: None,
            state: State::Loading,
            fold: ChangeFold::new(),
            folded: Arc::default(),
            groups: EntityGroups::default(),
            schema: None,
            card_rows: CardRows::default(),
            runs: Vec::new(),
            names: NodeNames::default(),
            card_lines: CardLines::default(),
            row_runs: Vec::new(),
            folded_runs: HashSet::new(),
            open: HashSet::new(),
            _names_task: None,
            counts: Vec::new(),
            rows: Vec::new(),
            next_page_token: None,
            more_error: None,
            list_state: ListState::new(0, ListAlignment::Top, px(600.)),
            focus_handle: cx.focus_handle(),
            _task: None,
            read_owed: false,
            _regroup_task: None,
            _schema_subscription: schema_subscription,
            selected: None,
            selected_key: None,
            selected_first: false,
            inspected: None,
            folded_large: false,
            list_view: CommitRows::new(cx),
        }
    }

    /// Reads the commit itself when the view came without it (a tab restored, a toast's or a
    /// parent link's), beside its changes, which do not wait for it: its kind says whether a
    /// branch can start at it and where it is. One found to be the graph's first stops a read
    /// of its changes still out, which the server would refuse. One that cannot be read leaves
    /// the header to the changes' cause, and offers no branch.
    fn read_commit(&mut self, cx: &mut Context<Self>) {
        if self.commit.is_some() || self._commit_task.is_some() {
            return;
        }
        let id = self.commit_id.clone();
        let read = self
            .session
            .update(cx, |session, cx| session.commit(&id, cx));
        self.commit_pending = true;
        self._commit_task = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| {
                this.commit_pending = false;
                if let Ok(commit) = read {
                    this.commit = Some(commit);
                }
                // The first commit has no changes to read.
                this.settle_if_first();
                cx.notify();
            })
            .ok();
        }));
    }

    /// Whether the commit is the graph's first, as far as it is known: one with no parent,
    /// which the server cannot compare with anything.
    fn is_first(&self) -> bool {
        self.commit.as_ref().is_some_and(is_first_commit)
    }

    /// Shows the graph's first commit as such, and drops a read of its changes still out, which
    /// the server would refuse. Whether it is the first.
    fn settle_if_first(&mut self) -> bool {
        if !self.is_first() {
            return false;
        }
        self._task = None;
        self.more_error = None;
        self.next_page_token = None;
        self.state = State::Lasting(CommitCounts::First);
        true
    }

    /// The commit a new branch can start at: this one, when it is published.
    fn branch_here(&self) -> Option<&str> {
        branch_start(self.commit.as_ref()?)
    }

    /// Whether the view offers to read more changes ("Load more changes", or "Try again" after
    /// a read of them failed, the first included), with no read under way.
    fn offers_more(&self) -> bool {
        match self.state {
            State::Failed(_) => true,
            State::Loading => false,
            _ => self.next_page_token.is_some(),
        }
    }

    /// Reads the next pages of changes, up to [`CAP`] more. The graph's first commit, known by
    /// the commit as listed, has none to read; one not known yet is asked for its changes at
    /// once, and the server's refusal of a commit without a parent (`parentless_commit`) shows
    /// it as the first, so no read waits for another.
    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.settle_if_first() {
            return;
        }
        // One read at a time: the one under way holds the fold.
        if matches!(self.state, State::Loading) && self._task.is_some() {
            return;
        }
        self.state = State::Loading;
        self.more_error = None;
        let session = self.session.clone();
        let commit_id = self.commit_id.clone();
        // Kept while it is read: the list's read-more line stays, so the list is not laid out
        // again from its top under the person's place.
        let mut token = self.next_page_token.clone();
        // Moved, not copied, on the main thread: it comes back with the pages read.
        let mut fold = std::mem::take(&mut self.fold);
        let start = fold.raw_changes();
        self._task = Some(cx.spawn(async move |this, cx| {
            let mut cause = None;
            let result = loop {
                let read = session.update(cx, |session, cx| {
                    session.commit_changes(&commit_id, token.clone(), PAGE, cx)
                });
                match read.await {
                    Ok(page) => {
                        let (block_cause, changes) = (page.cause.clone(), page.changes);
                        cause = Some(page.cause);
                        fold = cx
                            .background_executor()
                            .spawn(async move {
                                // Shared with the session's memo: copied here, off the main
                                // thread.
                                fold.add_block(graph_client::api::ChangeBlockOutput {
                                    cause: block_cause,
                                    changes: Arc::unwrap_or_clone(changes),
                                });
                                fold
                            })
                            .await;
                        token = page.next_page_token;
                        if token.is_none() || fold.raw_changes() - start >= CAP {
                            break Ok(());
                        }
                    }
                    // One reading of a refusal, shared with History's rows.
                    Err(error) => match crate::counts::lasting(&error) {
                        Some(said) => break Err(Stop::Lasting(said)),
                        None => break Err(Stop::Failed(error.to_string())),
                    },
                }
            };
            let read_any = fold.raw_changes() > start;
            // Finished, counted and laid out off the main thread. The fold is kept for the next
            // pages while there are more, which needs a copy of it to finish; with the last page
            // in it is finished itself, so the commit's changes are held once.
            let more = token.is_some();
            let schema = session.read_with(cx, |session, _| session.schema_model());
            let made_with = schema.clone();
            let (fold, folded, groups, runs, counts, rows) = cx
                .background_executor()
                .spawn(async move {
                    let (fold, folded) = if more {
                        let folded = fold.clone().finish();
                        (fold, folded)
                    } else {
                        // Nothing is read after the last page.
                        (ChangeFold::new(), fold.finish())
                    };
                    let counts = folded.counts();
                    let groups = EntityGroups::new(&folded, schema.as_deref());
                    let (rows, runs) = change_cards::rows(&folded, &groups, |_| true);
                    (fold, Arc::new(folded), groups, runs, counts, rows)
                })
                .await;
            this.update(cx, |this, cx| {
                this.fold = fold;
                let result = match result {
                    // A page after changes were shown keeps them, says why the rest cannot be
                    // read, and offers no more, since asking again gets the same answer.
                    Err(Stop::Lasting(said)) if read_any || !this.rows.is_empty() => {
                        this.more_error = Some(said.words().into());
                        token = None;
                        Ok(())
                    }
                    Err(Stop::Lasting(said)) => {
                        this.next_page_token = None;
                        this.more_error = None;
                        this.state = State::Lasting(said);
                        cx.notify();
                        return;
                    }
                    // A failure after changes were shown keeps them, and those read since.
                    Err(Stop::Failed(message)) if read_any || !this.rows.is_empty() => {
                        this.more_error = Some(message.into());
                        Ok(())
                    }
                    Err(Stop::Failed(message)) => Err(message),
                    Ok(()) => Ok(()),
                };
                match result {
                    Ok(()) => {
                        this.cause = cause.or(this.cause.take());
                        this.counts = counts;
                        this.rows = rows;
                        this.runs = runs;
                        this.row_runs = row_runs(&this.rows);
                        this.groups = groups;
                        this.folded = folded;
                        this.schema = made_with;
                        this.card_rows = CardRows::default();
                        this.read_names(cx);
                        this.next_page_token = token;
                        this.fold_large_once();
                        // The groups are made anew: the selection is found again by its entity.
                        this.reselect(cx);
                        // The first change is selected and unfolded as the commit opens, as the
                        // review's is, so its values and its node show from the start.
                        if !std::mem::replace(&mut this.selected_first, true) {
                            let first = this.visible_groups().first().copied();
                            this.select_group(first, cx);
                            if let Some((type_name, id)) = this.selected_key.clone() {
                                this.open.insert((type_name, id));
                            }
                        }
                        // Rows read further down keep the place the person scrolled to.
                        this.reset_list();
                        this.state = State::Ready;
                        // A schema that arrived while these were grouped groups them again.
                        this.regroup(cx);
                    }
                    Err(message) => this.state = State::Failed(message.into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Reads the names of the nodes known here only by id, at the commit, those not known yet.
    fn read_names(&mut self, cx: &mut Context<Self>) {
        let ends: Vec<_> = self
            .groups
            .unnamed_ends()
            .into_iter()
            .filter(|end| !self.names.contains_key(end))
            .collect();
        if ends.is_empty() {
            return;
        }
        let commit = Some(self.commit_id.clone());
        let names = self
            .session
            .update(cx, |session, cx| session.node_names(ends, commit, cx));
        self._names_task = Some(cx.spawn(async move |this, cx| {
            let (names, _) = names.await;
            this.update(cx, |this, cx| {
                this.names.extend(names);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Lays the list out for its rows again, keeping the place the person scrolled to.
    fn reset_list(&mut self) {
        let top = self.list_state.logical_scroll_top();
        let more = usize::from(self.next_page_token.is_some());
        self.list_state.reset(self.rows.len() + more);
        self.list_state.scroll_to(top);
    }

    /// Groups the changes read again when they were grouped under another schema than the
    /// session's, off the main thread: one loaded after they were read, applied, or naming
    /// nodes otherwise. A schema read again with the same text and node names groups them
    /// alike, and leaves them. The changes stay as read; a read under way groups its own.
    fn regroup(&mut self, cx: &mut Context<Self>) {
        let schema = self.session.read(cx).schema_model();
        if SchemaModel::same_held(self.schema.as_ref(), schema.as_ref()) {
            // A regroup under way is for a schema since gone.
            self._regroup_task = None;
            return;
        }
        if matches!(self.state, State::Loading) {
            return;
        }
        let folded = self.folded.clone();
        self._regroup_task = Some(cx.spawn(async move |this, cx| {
            let (grouping, made_with) = (folded.clone(), schema.clone());
            let (groups, rows, runs) = cx
                .background_executor()
                .spawn(async move {
                    let groups = EntityGroups::new(&grouping, schema.as_deref());
                    let (rows, runs) = change_cards::rows(&grouping, &groups, |_| true);
                    (groups, rows, runs)
                })
                .await;
            this.update(cx, |this, cx| {
                // Grouped for changes read since: those were grouped as they came.
                if !Arc::ptr_eq(&this.folded, &folded) {
                    return;
                }
                this.groups = groups;
                this.rows = rows;
                this.runs = runs;
                this.row_runs = row_runs(&this.rows);
                this.schema = made_with;
                // Grouped again, the groups are others: the selection is found again by its
                // entity.
                this.reselect(cx);
                this.card_rows = CardRows::default();
                this.read_names(cx);
                this.reset_list();
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let actor = self
            .commit
            .as_ref()
            .and_then(|c| c.actor_id.clone())
            .or_else(|| self.cause.as_ref().and_then(|c| c.actor_id.clone()));
        let at = self
            .commit
            .as_ref()
            .map(|c| c.created_at)
            .or_else(|| self.cause.as_ref().map(|c| c.authored_at));
        let branch = branch_fact(
            self.commit.as_ref(),
            self.cause.as_ref(),
            self.commit_pending,
        );
        let parent = self
            .commit
            .as_ref()
            .and_then(|c| c.parent_commit_id.clone())
            .or_else(|| self.cause.as_ref().and_then(|c| c.parent_commit_id.clone()));
        let merged = self
            .commit
            .as_ref()
            .and_then(|c| c.merged_parent_commit_id.clone())
            .or_else(|| {
                self.cause
                    .as_ref()
                    .and_then(|c| c.merged_parent_commit_id.clone())
            });
        // What the commit changed, by type, once read; "so far" while more are to read.
        let summary = matches!(self.state, State::Ready | State::Loading)
            .then(|| {
                self.counts
                    .iter()
                    .any(|c| c.added + c.changed + c.removed > 0)
            })
            .filter(|any| *any)
            .map(|_| {
                h_flex()
                    .gap(px(studio_ui::ds::space::MD))
                    .items_center()
                    .flex_wrap()
                    .child(change_cards::summary_line(&self.counts, cx))
                    .when(self.next_page_token.is_some(), |line| {
                        line.child(studio_ui::ds::text::meta("so far"))
                    })
            });
        // A parent is a link to its own commit.
        let parent_link = |id: String, icon: IconName, name: &'static str, tooltip: String| {
            let short = short_commit_id(&id);
            div()
                .id(name)
                .cursor_pointer()
                .tip(studio_ui::ds::tooltip(tooltip))
                .on_click(move |_, window, cx| {
                    window.dispatch_action(
                        Box::new(OpenCommit {
                            commit_id: id.clone(),
                            preview: false,
                        }),
                        cx,
                    )
                })
                .child(studio_ui::ds::meta_code(icon, short, cx))
                .into_any_element()
        };
        // An actor is a principal's id, which can be longer than the pane: it truncates, its
        // whole id in a tooltip.
        let actor = actor.unwrap_or_else(|| "—".into());
        let mut facts: Vec<AnyElement> = vec![
            div()
                .id("commit-actor")
                .min_w_0()
                .max_w_full()
                .tip(studio_ui::ds::tooltip(actor.clone()))
                .child(
                    h_flex()
                        .min_w_0()
                        .gap(px(studio_ui::ds::space::XS))
                        .child(studio_ui::ds::icon::meta(IconName::Person))
                        .child(
                            div()
                                .min_w_0()
                                .child(studio_ui::ds::text::meta(actor).single_line().truncate()),
                        ),
                )
                .into_any_element(),
        ];
        if let Some(at) = at {
            facts.push(
                div()
                    .id("commit-when")
                    .tip(studio_ui::ds::tooltip(utc_date_time(at)))
                    .child(studio_ui::ds::meta(
                        IconName::Clock,
                        relative_time(at, now_micros()),
                    ))
                    .into_any_element(),
            );
        }
        if let Some((icon, branch)) = branch {
            facts.push(studio_ui::ds::meta(icon, branch).into_any_element());
        }
        if let Some(parent) = parent {
            facts.push(parent_link(
                parent,
                IconName::GitCommit,
                "commit-parent",
                "Open the parent commit".into(),
            ));
        }
        if let Some(merged) = merged {
            facts.push(parent_link(
                merged,
                IconName::PullRequest,
                "commit-merged-parent",
                "Open the merged branch's commit".into(),
            ));
        }
        let full_id = self.commit_id.clone();
        v_flex()
            .flex_none()
            .px(px(studio_ui::ds::space::EDGE))
            .pt(px(studio_ui::ds::space::LG))
            .pb(px(studio_ui::ds::space::LG))
            .gap(px(studio_ui::ds::space::SM))
            .border_b_1()
            .border_color(studio_ui::ds::surface::rule(cx))
            .child(
                h_flex()
                    .gap(px(studio_ui::ds::space::INLINE))
                    .child(studio_ui::ds::icon::meta(IconName::GitCommit))
                    .child(studio_ui::ds::text::meta("Commit"))
                    .child(div().flex_1())
                    .child(self.render_open_at(cx)),
            )
            .child(
                div()
                    .id("commit-title")
                    .cursor_pointer()
                    .tip(studio_ui::ds::tooltip(format!("{full_id}\nClick to copy")))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(full_id.clone()))
                    })
                    .child(
                        studio_ui::ds::text::heading(short_commit_id(&self.commit_id))
                            .buffer_font(cx),
                    ),
            )
            .child(studio_ui::ds::meta_row(facts).flex_wrap())
            .children(summary)
            .into_any_element()
    }

    /// Opens the graph as it was at this commit, read-only: a type's table, or a query (spec
    /// 009); and starts a branch at it, when it is published (spec 008).
    fn render_open_at(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let branch_at = self.branch_here().map(str::to_owned);
        let focus = self.focus_handle.clone();
        let commit = self.commit_id.clone();
        let changed: Vec<String> = self.counts.iter().map(|c| c.type_name.clone()).collect();
        let session = self.session.clone();
        let table_commit = commit.clone();
        h_flex()
            .gap(px(studio_ui::ds::space::XS))
            .child(
                PopoverMenu::new("open-table-at")
                    .trigger(
                        studio_ui::ds::button("open-table-at-button", "Table")
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::Table))
                            .tip(studio_ui::ds::tooltip(
                                "Open a type's table as it was at this commit",
                            )),
                    )
                    .menu(move |window, cx| {
                        // The types changed in this commit first, then the rest of the schema,
                        // listed as the menu opens.
                        let mut types = changed.clone();
                        let mut listed: HashSet<String> = types.iter().cloned().collect();
                        if let Some(schema) = session.read(cx).schema_model() {
                            for name in schema
                                .nodes
                                .iter()
                                .map(|n| &n.name)
                                .chain(schema.edges.iter().map(|e| &e.name))
                            {
                                if listed.insert(name.clone()) {
                                    types.push(name.clone());
                                }
                            }
                        }
                        let commit = table_commit.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for name in types {
                                menu = menu.action(
                                    name.clone(),
                                    Box::new(OpenType {
                                        type_name: name,
                                        at_commit: Some(commit.clone()),
                                        filter_id: None,
                                        via: None,
                                        preview: false,
                                        filters: None,
                                        separate: false,
                                    }),
                                );
                            }
                            menu
                        }))
                    }),
            )
            .child(
                studio_ui::ds::button("query-at", "Query")
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(IconName::FileCode))
                    .tip(studio_ui::ds::tooltip(
                        "Open a query that reads this commit",
                    ))
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(OpenQueryAt {
                                commit_id: commit.clone(),
                            }),
                            cx,
                        )
                    }),
            )
            .when_some(branch_at, |row, commit| {
                let cli = branch_here_cli(&commit);
                row.child(
                    studio_ui::ds::button("branch-here", "Branch")
                        .style(ButtonStyle::Subtle)
                        .start_icon(Icon::new(IconName::GitBranch))
                        .tip_fn(move |_, cx| {
                            studio_ui::ds::tooltip_for_action_in(
                                "Start a new branch at this commit",
                                &NewBranchHere,
                                &focus,
                            )
                            .meta(cli.clone())
                            .into_view(cx)
                        })
                        .on_click(move |_, window, cx| new_branch_at(&commit, window, cx)),
                )
            })
    }

    /// What changed in entity `ix`, in full, as its row unfolds: as the review's
    /// ([`change_cards::entity_body`]), its relationship lines a few at a time.
    fn entity_body(&self, ix: usize, kind: studio_ui::ChangeKind, cx: &mut Context<Self>) -> Div {
        let schema = self.schema.as_deref();
        let group = &self.groups.groups[ix];
        let rows = group
            .change
            .map(|own| change_cards::change_rows(&self.folded, own, schema, &self.card_rows));
        let card_key = (group.type_name.clone(), group.id.clone());
        let total = self.groups.line_count(ix);
        let shown = self.card_lines.shown(&card_key, total, None);
        let left_out = total - shown;
        // Only the lines the row shows are made.
        let card_lines = self
            .groups
            .lines(&self.folded, ix, schema, &self.names, shown);
        let counts = (card_lines.added, card_lines.changed, card_lines.removed);
        let lines = card_lines
            .entries
            .into_iter()
            .map(|entry| {
                // A mirror's other end is the entity it is shown on in full.
                change_cards::relationship_line(
                    entry,
                    &self.folded,
                    schema,
                    &self.card_rows,
                    "shown there",
                )
            })
            .collect();
        let more = (left_out > 0).then(|| {
            change_cards::more_lines_button(
                ix,
                left_out,
                cx.listener(move |this, _, _, cx| {
                    this.card_lines.show_more(card_key.clone(), shown);
                    cx.notify();
                }),
            )
            .into_any_element()
        });
        change_cards::entity_body(rows, kind, None, lines, counts, more, cx)
            .px(px(studio_ui::ds::space::LG))
            .pb(px(studio_ui::ds::space::LG))
    }

    /// Folds the runs too long to read row by row the first time changes are shown, when the
    /// commit holds others ([`change_cards::starts_folded`]); later pages leave the person's
    /// folds as they are.
    fn fold_large_once(&mut self) {
        if self.folded_large || self.runs.is_empty() {
            return;
        }
        self.folded_large = true;
        let keys: Vec<_> = change_cards::starts_folded(&self.runs)
            .map(|run| run_key(run, &self.groups))
            .collect();
        self.folded_runs.extend(keys);
    }

    /// The entities' groups in list order, leaving out those of folded runs.
    fn visible_groups(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(row, kind)| match kind {
                Row::Entity(ix) => {
                    let run = self.row_runs.get(row).copied().unwrap_or_default();
                    let folded = self
                        .runs
                        .get(run)
                        .is_some_and(|run| self.folded_runs.contains(&run_key(run, &self.groups)));
                    (!folded).then_some(*ix)
                }
                Row::Header(_) => None,
            })
            .collect()
    }

    /// J and K: the next or previous entity shown, scrolled into view.
    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let visible = self.visible_groups();
        let Some(&first) = visible.first() else {
            return;
        };
        let next = match self
            .selected
            .and_then(|s| visible.iter().position(|&ix| ix == s))
        {
            Some(at) if forward => visible[(at + 1).min(visible.len() - 1)],
            Some(at) => visible[at.saturating_sub(1)],
            None => first,
        };
        self.select_group(Some(next), cx);
        if let Some(row) = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Entity(ix) if *ix == next))
        {
            self.list_state.scroll_to_reveal_item(row);
        }
        self.report_selection(cx);
        cx.notify();
    }

    /// Space: the selected entity unfolds, or folds back.
    fn toggle_selected(&mut self, cx: &mut Context<Self>) {
        let Some(group) = self.selected.and_then(|ix| self.groups.groups.get(ix)) else {
            return;
        };
        let key = (group.type_name.clone(), group.id.clone());
        if !self.open.remove(&key) {
            self.open.insert(key);
        }
        cx.notify();
    }

    /// Where entity `ix`'s page opens: its node at this commit, or, for one the commit removed,
    /// at its parent, where it last was. None for a relationship, whose ends have the pages.
    fn page_of(&self, ix: usize) -> Option<(String, String, String)> {
        let group = self.groups.groups.get(ix)?;
        if group.is_edge {
            return None;
        }
        let removed = group.change.is_some_and(|own| {
            change_cards::kind_of_change(&self.folded.changes[own])
                == studio_ui::ChangeKind::Removed
        });
        let commit = if removed {
            self.commit
                .as_ref()
                .and_then(|c| c.parent_commit_id.clone())
                .or_else(|| self.cause.as_ref().and_then(|c| c.parent_commit_id.clone()))?
        } else {
            self.commit_id.clone()
        };
        Some((group.type_name.clone(), group.id.clone(), commit))
    }

    /// The node of the selected entity, as the inspector shows it, held where its page opens.
    fn selected_node(&self) -> Option<InspectedNode> {
        let (type_name, id, commit) = self.page_of(self.selected?)?;
        Some(InspectedNode {
            type_name,
            id,
            at_commit: Some(commit),
            snapshot: None,
        })
    }

    /// Selects group `ix`, by its index and its entity, and tells the inspector.
    fn select_group(&mut self, ix: Option<usize>, cx: &mut Context<Self>) {
        self.selected = ix;
        self.selected_key = ix
            .and_then(|ix| self.groups.groups.get(ix))
            .map(|group| (group.type_name.clone(), group.id.clone()));
        self.report_selection(cx);
    }

    /// Finds the selected entity among groups made anew, which a later page or another schema
    /// orders otherwise; one no longer among them leaves nothing selected. The inspector is
    /// told either way.
    fn reselect(&mut self, cx: &mut Context<Self>) {
        let found = self.selected_key.as_ref().and_then(|(type_name, id)| {
            self.groups
                .groups
                .iter()
                .position(|group| group.type_name == *type_name && group.id == *id)
        });
        self.select_group(found, cx);
    }

    /// Tells the workspace when the selected entity's node is another one, or none.
    fn report_selection(&mut self, cx: &mut Context<Self>) {
        let node = self.selected_node();
        if node != self.inspected {
            self.inspected = node;
            cx.emit(ItemEvent::Selected);
        }
    }

    /// Opens entity `ix`'s page where the change left it ([`Self::page_of`]), read-only.
    fn open_page(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((type_name, id, commit)) = self.page_of(ix) else {
            return;
        };
        self.select_group(Some(ix), cx);
        window.dispatch_action(
            Box::new(OpenNodePage {
                type_name,
                id,
                at_commit: Some(commit),
                preview: false,
            }),
            cx,
        );
    }

    /// The list of changes, as [`CommitRows`] draws it.
    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let more = usize::from(self.next_page_token.is_some());
        if self.list_state.item_count() != self.rows.len() + more {
            self.list_state.reset(self.rows.len() + more);
        }
        list(
            self.list_state.clone(),
            cx.processor(|this, row, window, cx| this.render_row(row, window, cx)),
        )
        .flex_1()
        .min_h_0()
        .py(px(studio_ui::ds::space::MD))
        .into_any_element()
    }

    fn render_row(
        &mut self,
        row: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let more = self.next_page_token.is_some();
        match self.rows.get(row).copied() {
            Some(Row::Header(ix)) => {
                let run = &self.runs[ix];
                let key = run_key(run, &self.groups);
                let folded = self.folded_runs.contains(&key);
                change_cards::run_row(
                    ("commit-run", ix),
                    run,
                    change_cards::RunWords {
                        section: run
                            .section
                            .map(|section| change_cards::section_words(section).to_owned()),
                        detail: change_cards::run_detail(run),
                        when: None,
                    },
                    folded,
                    cx.listener(move |this, _, _, cx| {
                        if !this.folded_runs.remove(&key) {
                            this.folded_runs.insert(key.clone());
                        }
                        cx.notify();
                    }),
                    cx,
                )
            }
            Some(Row::Entity(ix)) => {
                let run = self.row_runs.get(row).copied().unwrap_or_default();
                if self
                    .runs
                    .get(run)
                    .is_some_and(|run| self.folded_runs.contains(&run_key(run, &self.groups)))
                {
                    return div().into_any_element();
                }
                // The schema the groups were made with, which the rows agree with.
                let schema = self.schema.as_deref();
                let first = row == 0 || matches!(self.rows[row - 1], Row::Header(_));
                let last = self
                    .rows
                    .get(row + 1)
                    .is_none_or(|next| matches!(next, Row::Header(_)));
                let group = &self.groups.groups[ix];
                let card_key = (group.type_name.clone(), group.id.clone());
                let open = self.open.contains(&card_key);
                let (kind, name, id) =
                    change_cards::identity(&self.groups, &self.folded, ix, schema, &self.names);
                let summary = change_cards::entity_summary(&self.folded, group);
                let body = open.then(|| self.entity_body(ix, kind, cx));
                let open_button = self.page_of(ix).map(|_| {
                    change_cards::open_button(
                        ("open-change", ix),
                        &self.focus_handle,
                        cx.listener(move |this, _, window, cx| this.open_page(ix, window, cx)),
                    )
                });
                change_cards::entity_row(
                    change_cards::EntityRow {
                        id: ("commit-change", ix).into(),
                        kind,
                        name,
                        entity_id: id,
                        summary,
                        status: None,
                        first,
                        last,
                        open,
                        selected: self.selected == Some(ix),
                        dimmed: false,
                        open_button,
                    },
                    body,
                    // A click selects the entity and unfolds it, or folds it again (spec 020).
                    cx.listener(move |this, _, _, cx| {
                        this.select_group(Some(ix), cx);
                        if !this.open.remove(&card_key) {
                            this.open.insert(card_key.clone());
                        }
                        cx.notify();
                    }),
                    cx,
                )
            }
            None if more => h_flex()
                .px(px(studio_ui::ds::space::XL))
                .py(px(studio_ui::ds::space::MD))
                .gap(px(studio_ui::ds::space::MD))
                .child(
                    studio_ui::ds::button(
                        "load-more",
                        if matches!(self.state, State::Loading) {
                            "Reading the changes…"
                        } else if self.more_error.is_some() {
                            "Try again"
                        } else {
                            "Load more changes"
                        },
                    )
                    .style(ButtonStyle::Subtle)
                    .disabled(matches!(self.state, State::Loading))
                    .key_binding(KeyBinding::for_action_in(
                        &LoadMoreChanges,
                        &self.focus_handle,
                        cx,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                )
                .children(self.more_error.clone().map(|error| {
                    studio_ui::ds::text::muted(format!("More changes could not be read: {error}"))
                }))
                .into_any_element(),
            None => div().into_any_element(),
        }
    }
}

/// Where the commit is, for its header: the branch that published it, or, for a commit held
/// off every branch, what it is; it names the branch it was written on, which it is not on. A
/// commit still being read (`pending`) says nothing yet, since its kind decides; one that could
/// not be read is placed by its changes' cause, which names its authored branch.
pub(crate) fn branch_fact(
    commit: Option<&CommitOutput>,
    cause: Option<&ChangeCauseOutput>,
    pending: bool,
) -> Option<(IconName, String)> {
    let Some(commit) = commit else {
        return cause
            .filter(|_| !pending)
            .map(|cause| (IconName::GitBranch, cause.authored_branch.clone()));
    };
    let on = || commit.graph_branch.clone().unwrap_or_else(|| "main".into());
    Some(match commit.kind {
        CommitKind::Published => (IconName::GitBranch, on()),
        CommitKind::Unpublished => (
            IconName::GitCommit,
            format!("Unpublished, on no branch until published to {}", on()),
        ),
        CommitKind::Unattached => (
            IconName::GitCommit,
            "Unattached snapshot, on no branch".to_owned(),
        ),
        // A kind a later server sends: where it is, Studio cannot say.
        CommitKind::Other => return None,
    })
}

/// Whether `commit` is the graph's first: it has no parent, so it has no changes the server can
/// tell (it refuses them with a 409, `parentless_commit`), and none are asked for.
pub(crate) fn is_first_commit(commit: &CommitOutput) -> bool {
    commit.parent_commit_id.is_none() && commit.merged_parent_commit_id.is_none()
}

/// The commit a branch can start at: a published one. A proposal or an unattached snapshot is
/// in no branch's history, and the server refuses a branch there.
pub(crate) fn branch_start(commit: &CommitOutput) -> Option<&str> {
    (commit.kind == CommitKind::Published).then_some(commit.graph_commit_id.as_str())
}

/// Opens the new-branch dialog, started at `commit_id`.
pub(crate) fn new_branch_at(commit_id: &str, window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        Box::new(CreateBranchAt {
            commit_id: commit_id.to_owned(),
        }),
        cx,
    );
}

/// Opens commit `id`'s own tab in `workspace`, or brings it forward. `listed` is the commit as
/// History listed it, which the tab keeps, so it reads only the commit's changes; a tab without
/// it reads the commit by its id beside them.
pub(crate) fn open_in(
    workspace: &mut Workspace,
    session: Entity<GraphSession>,
    id: String,
    listed: Option<CommitOutput>,
    preview: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.open_item_with::<CommitView>(commit_key(&id), preview, window, cx, |_, cx| {
        CommitView::new(session, id, listed, cx)
    });
}

/// Opens a listed commit's own tab with the commit as listed ([`open_in`]). A list drawn
/// outside a workspace's window has no workspace to open it in, and dispatches [`OpenCommit`],
/// which reads the commit by its id.
pub(crate) fn open_listed(
    session: &Entity<GraphSession>,
    commit: &CommitOutput,
    preview: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let id = commit.graph_commit_id.clone();
    let Some(workspace) = studio_workspace::workspace_for(window) else {
        window.dispatch_action(
            Box::new(OpenCommit {
                commit_id: id,
                preview,
            }),
            cx,
        );
        return;
    };
    let (session, listed) = (session.clone(), commit.clone());
    // Deferred: History calls this from its own click and ↵ handlers, while it is being
    // updated, and finding the tab reads every tab's key, History's among them.
    window.defer(cx, move |window, cx| {
        workspace.update(cx, |workspace, cx| {
            // The click came from History, the active tab: a preview it opens must not take
            // its place (spec 020, the source tab is never replaced).
            if preview {
                workspace.keep_active_tab(cx);
            }
            open_in(workspace, session, id, Some(listed), preview, window, cx)
        });
    });
}

/// The CLI command that starts a branch at `commit_id`.
pub(crate) fn branch_here_cli(commit_id: &str) -> String {
    format!("omnigraph branch create <name> --from-commit {commit_id}")
}

/// The list's rows: a header for each run of one type, then its cards.
impl Render for CommitView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.read_owed) {
            cx.defer_in(window, |this, _, cx| {
                this.read_commit(cx);
                this.load_more(cx);
            });
        }
        let header = self.render_header(cx);
        let body = match &self.state {
            // Read again from the start: nothing of the commit's changes was read.
            State::Failed(message) => {
                studio_ui::ds::error_state("The changes could not be read", message.clone())
                    .child(
                        studio_ui::ds::button("read-changes-again", "Try again")
                            .style(ButtonStyle::Subtle)
                            .key_binding(KeyBinding::for_action_in(
                                &LoadMoreChanges,
                                &self.focus_handle,
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                    )
                    .into_any_element()
            }
            State::Ready if self.rows.is_empty() => change_cards::no_changes_state(
                "This commit changed no node or relationship against its parent.",
            )
            .into_any_element(),
            State::Lasting(said) => change_cards::lasting_state(said)
                .map(IntoElement::into_any_element)
                .unwrap_or_else(|| gpui::Empty.into_any_element()),
            State::Loading if self.rows.is_empty() => {
                change_cards::reading_state(None).into_any_element()
            }
            _ => AnyView::from(self.list_view.clone())
                .cached(StyleRefinement::default().size_full())
                .into_any_element(),
        };
        v_flex()
            .key_context("CommitView")
            .track_focus(&self.focus_handle)
            .size_full()
            .when(self.branch_here().is_some(), |view| {
                view.on_action(cx.listener(|this, _: &NewBranchHere, window, cx| {
                    if let Some(commit) = this.branch_here() {
                        new_branch_at(commit, window, cx)
                    }
                }))
            })
            // As the list's last row offers: more changes to read, or a read of them to try
            // again, while none is under way.
            .when(self.offers_more(), |view| {
                view.on_action(cx.listener(|this, _: &LoadMoreChanges, _, cx| this.load_more(cx)))
            })
            // As the review moves among its changes (spec 033).
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.step(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleChange, _, cx| this.toggle_selected(cx)))
            .when(
                self.selected.and_then(|ix| self.page_of(ix)).is_some(),
                |view| {
                    view.on_action(cx.listener(|this, _: &OpenChanged, window, cx| {
                        if let Some(ix) = this.selected {
                            this.open_page(ix, window, cx);
                        }
                    }))
                },
            )
            .child(header)
            .child(body)
    }
}

/// A commit's list of changes as a view of its own, drawn cached and again when the commit view
/// notifies (it observes it), as History's rows are (`docs/drawing.md`).
pub(crate) struct CommitRows {
    commit: WeakEntity<CommitView>,
    _changed: Subscription,
    /// How often the list was drawn, for the tests that guard it.
    #[cfg(test)]
    pub(crate) renders: usize,
}

impl CommitRows {
    /// Made with the view, not on its first draw.
    fn new(cx: &mut Context<CommitView>) -> Entity<Self> {
        let commit = cx.entity();
        cx.new(|cx| CommitRows {
            commit: commit.downgrade(),
            _changed: cx.observe(&commit, |_, _, cx| cx.notify()),
            #[cfg(test)]
            renders: 0,
        })
    }
}

impl Render for CommitRows {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let list = self
            .commit
            .update(cx, |commit, cx| commit.render_list(cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element());
        v_flex().size_full().child(list)
    }
}

/// The run each of `rows` is in: an entity's is the header's above it.
fn row_runs(rows: &[Row]) -> Vec<usize> {
    let mut run = 0;
    rows.iter()
        .map(|row| {
            if let Row::Header(ix) = row {
                run = *ix;
            }
            run
        })
        .collect()
}

/// What a run is folded by: its section, whether it is of relationships, and its type.
fn run_key(run: &GroupRun, groups: &EntityGroups) -> (GroupSection, bool, String) {
    let section = run
        .groups
        .first()
        .map_or(GroupSection::Added, |&g| groups.groups[g].section);
    (section, run.is_edge, run.type_name.clone())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use graph_client::ReadError;
    use graph_client::api::ChangeDiffRefusalReason;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::TestAppContext;
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::json;

    use super::*;

    /// A session whose server answers every read of commit c9's changes with `answer`.
    fn answering(
        answer: impl Fn() -> (u16, serde_json::Value) + Send + Sync + 'static,
        cx: &mut TestAppContext,
    ) -> Entity<GraphSession> {
        let answer = Arc::new(answer);
        let http = FakeHttpClient::create(move |_| {
            let (status, body) = answer();
            async move {
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx))
    }

    /// J and K move among a commit's changes as the review's do, each key drawing the list once,
    /// and the inspector follows the node selected, held at the commit (spec 033).
    #[gpui::test]
    async fn moving_through_a_commits_changes_draws_the_list_once_a_key(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let session = answering(
            || {
                let change = |id: &str| json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": id, "op": "insert", "before": null, "after": { "properties": { "slug": id } } });
                let cause = json!({ "graph_commit_id": "c9", "parent_commit_id": "c8", "authored_branch": "main", "authored_at": 0 });
                (
                    200,
                    json!({ "cause": cause, "changes": [change("p1"), change("p2"), change("p3")] }),
                )
            },
            cx,
        );
        let (view, cx) =
            cx.add_window_view(|_, cx| CommitView::new(session, "c9".into(), None, cx));
        cx.run_until_parked();
        let renders = |cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| view.list_view.read(cx).renders)
        };
        assert!(renders(cx) > 0, "the list is drawn");
        let selected = |cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, _| {
                view.selected_node().map(|node| (node.id, node.at_commit))
            })
        };
        assert_eq!(
            selected(cx),
            Some(("p1".to_owned(), Some("c9".to_owned()))),
            "the first change is selected as the commit opens"
        );
        for (key, id) in [(true, "p2"), (true, "p3"), (true, "p3"), (false, "p2")] {
            let before = renders(cx);
            view.update(cx, |view, cx| view.step(key, cx));
            cx.run_until_parked();
            assert_eq!(renders(cx), before + 1, "one draw a key");
            assert_eq!(selected(cx), Some((id.to_owned(), Some("c9".to_owned()))));
        }
        // The first was unfolded as the commit opened; Space unfolds the selected change, and
        // folds it back.
        view.read_with(cx, |view, _| {
            assert!(view.open.contains(&("Person".to_owned(), "p1".to_owned())));
            assert!(!view.open.contains(&("Person".to_owned(), "p2".to_owned())));
        });
        view.update(cx, |view, cx| view.toggle_selected(cx));
        view.read_with(cx, |view, _| {
            assert!(view.open.contains(&("Person".to_owned(), "p2".to_owned())))
        });
    }

    /// The selection stays on its node when a later page sorts a change before it: the groups
    /// are made anew and ordered otherwise (review of #130; it failed at fa76ad1, where the
    /// selection was an index and moved to p00000).
    #[gpui::test]
    async fn the_selection_stays_on_its_node_when_a_later_page_sorts_first(
        cx: &mut TestAppContext,
    ) {
        cx.update(studio_theme::init_test);
        let http = FakeHttpClient::create(move |request| {
            let uri = request.uri().to_string();
            let cause = json!({ "graph_commit_id": "c9", "parent_commit_id": "c8", "authored_branch": "main", "authored_at": 0 });
            let body = if !uri.contains("/changes") {
                json!({})
            } else if !uri.contains("t2") {
                let changes: Vec<_> = (0..CAP)
                    .map(|i| json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": format!("p{i:05}"), "op": "insert", "before": null, "after": { "properties": { "slug": format!("p{i:05}") } } }))
                    .collect();
                json!({ "cause": cause, "changes": changes, "next_page_token": "t2" })
            } else {
                let company = json!({ "kind": "node", "type": { "id": "Company", "name": "Company" }, "id": "c1", "op": "update", "before": { "properties": { "slug": "c1", "city": "Tokyo" } }, "after": { "properties": { "slug": "c1", "city": "Osaka" } } });
                json!({ "cause": cause, "changes": [company] })
            };
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        let view = cx.new(|cx| CommitView::new(session, "c9".into(), None, cx));
        cx.run_until_parked();
        let node = |view: &CommitView| view.selected_node().map(|node| node.id);
        view.update(cx, |view, cx| {
            assert_eq!(node(view).as_deref(), Some("p00000"));
            view.step(true, cx);
            assert_eq!(node(view).as_deref(), Some("p00001"));
            view.load_more(cx);
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            assert_eq!(view.groups.groups.len(), CAP + 1, "both pages read");
            assert_eq!(node(view).as_deref(), Some("p00001"));
            // An entity no longer among the groups leaves nothing selected.
            view.groups.groups.retain(|group| group.id != "p00001");
            view.reselect(cx);
            assert_eq!(node(view), None);
        });
    }

    /// A commit whose history was cleaned up says so, as History's rows do, and offers nothing
    /// to try again, since asking again gets the same 410 (review of #130).
    #[gpui::test]
    async fn a_cleaned_up_commit_says_so_and_offers_no_retry(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let session = answering(
            || {
                (
                    410,
                    json!({ "error": "history cleaned up", "code": "gone" }),
                )
            },
            cx,
        );
        let view = cx.new(|cx| CommitView::new(session, "c9".into(), None, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(
                matches!(view.state, State::Lasting(CommitCounts::CleanedUp)),
                "cleaned up, as History's row says"
            );
            assert!(!view.offers_more(), "no Try again, no LoadMoreChanges");
        });
    }

    /// A commit across a schema change says so, as what it is rather than a failure, and is
    /// not offered again, since asking again gets the same answer (spec 033).
    #[gpui::test]
    async fn a_schema_change_is_said_as_one_and_not_offered_again(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let session = answering(
            || {
                (
                    409,
                    json!({ "error": "entity changes cross an unprovable schema boundary at type 'Person'", "code": "conflict", "change_diff_refusal": { "reason": "schema_boundary", "graph_commit_id": "c9", "type_name": "Person" } }),
                )
            },
            cx,
        );
        let view = cx.new(|cx| CommitView::new(session, "c9".into(), None, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(
                matches!(&view.state, State::Lasting(CommitCounts::SchemaChange(Some(t))) if t == "Person"),
                "a schema change, at Person"
            );
            assert!(!view.offers_more(), "nothing to try again");
        });
    }

    /// A comparison the server refused says why by the refusal's typed reason: the graph's first
    /// commit is told apart, a schema change names its type, and another 409 is told as the
    /// server put it, claiming no schema change.
    #[test]
    fn a_refused_comparison_says_why() {
        use ChangeDiffRefusalReason::{ParentlessCommit, SchemaBoundary};
        use graph_client::api::ChangeDiffRefusalOutput;
        let refused = |status: u16, refusal: Option<(ChangeDiffRefusalReason, Option<&str>)>| {
            ReadError::Refused(graph_client::ApiErrorSummary {
                status,
                message: "as the server put it".into(),
                code: None,
                diagnostic: None,
                change_diff_refusal: refusal.map(|(reason, type_name)| {
                    Box::new(ChangeDiffRefusalOutput {
                        reason,
                        graph_commit_id: "c1".into(),
                        type_name: type_name.map(str::to_owned),
                    })
                }),
                challenge: None,
                unauthorized: None,
            })
        };
        let lasting = crate::counts::lasting;
        assert_eq!(
            lasting(&refused(409, Some((ParentlessCommit, None)))),
            Some(CommitCounts::First)
        );
        assert_eq!(
            lasting(&refused(409, Some((SchemaBoundary, Some("Person"))))),
            Some(CommitCounts::SchemaChange(Some("Person".into())))
        );
        assert_eq!(
            lasting(&refused(409, Some((SchemaBoundary, None)))),
            Some(CommitCounts::SchemaChange(None))
        );
        // Another refused comparison, or one whose reason this build does not know, claims no
        // schema change and keeps the server's reason as it came.
        let other = lasting(&refused(409, None)).expect("lasting");
        assert_eq!(
            other,
            CommitCounts::NotComparable("as the server put it".into())
        );
        assert!(other.words().ends_with(": as the server put it"));
        assert!(!other.words().contains("schema"));
        assert_eq!(lasting(&refused(410, None)), Some(CommitCounts::CleanedUp));
        // A failure that may pass is not lasting: it is offered again.
        assert_eq!(lasting(&refused(503, None)), None);
    }

    /// A failed read of more changes keeps those read before it, and the next press goes on
    /// from the page that failed; the counts say they are partial until the last page is in.
    #[gpui::test]
    async fn a_failed_page_keeps_what_was_read_and_goes_on_from_it(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let failures = Arc::new(AtomicUsize::new(0));
        let failed = failures.clone();
        let http = FakeHttpClient::create(move |request| {
            let query = request.uri().query().unwrap_or_default().to_owned();
            let failed = failed.clone();
            async move {
                let change = |id: &str| json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": id, "op": "insert", "before": null, "after": { "properties": { "slug": id } } });
                let cause = json!({ "graph_commit_id": "c9", "parent_commit_id": "c8", "authored_branch": "main", "authored_at": 0 });
                let (status, body) = if !query.contains("page_token") {
                    (
                        200,
                        json!({ "cause": cause, "changes": [change("p1")], "next_page_token": "t2" }),
                    )
                } else if failed.fetch_add(1, Ordering::SeqCst) == 0 {
                    (500, json!({ "error": "busy" }))
                } else {
                    (200, json!({ "cause": cause, "changes": [change("p2")] }))
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        let view = cx.new(|cx| CommitView::new(session, "c9".into(), None, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Ready));
            assert_eq!(view.folded.changes.len(), 1, "the first page stays");
            assert_eq!(view.next_page_token.as_deref(), Some("t2"));
            assert!(view.more_error.is_some());
        });
        view.update(cx, |view, cx| view.load_more(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.folded.changes.len(), 2);
            assert_eq!(view.next_page_token, None);
            assert!(view.more_error.is_none());
        });
    }

    /// A first read of the changes that failed is offered again, by its button and the Load
    /// more key, and read from the start. Before, the tab stayed failed until it was closed.
    #[gpui::test]
    async fn a_failed_first_read_is_offered_again(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let failures = Arc::new(AtomicUsize::new(0));
        let failed = failures.clone();
        let http = FakeHttpClient::create(move |request| {
            let changes = request.uri().path().ends_with("/changes");
            let failed = failed.clone();
            async move {
                let change = json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": "p1", "op": "insert", "before": null, "after": { "properties": { "slug": "p1" } } });
                let cause = json!({ "graph_commit_id": "c9", "parent_commit_id": "c8", "authored_branch": "main", "authored_at": 0 });
                let (status, body) = if changes && failed.fetch_add(1, Ordering::SeqCst) == 0 {
                    (503, json!({ "error": "busy" }))
                } else {
                    (200, json!({ "cause": cause, "changes": [change] }))
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        let view = cx.new(|cx| CommitView::new(session, "c9".into(), None, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Failed(_)));
            assert!(view.offers_more(), "its key is offered");
        });
        view.update(cx, |view, cx| view.load_more(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Ready));
            assert_eq!(view.folded.changes.len(), 1);
        });
    }

    /// A schema that arrives after the changes were grouped groups them again from what was
    /// read, without reading the commit again: a relationship stands alone until a schema says
    /// which nodes it joins, then shows on its node's card.
    #[gpui::test]
    async fn a_schema_that_arrives_after_the_read_groups_the_changes_again(
        cx: &mut TestAppContext,
    ) {
        cx.update(studio_theme::init_test);
        let schema_served = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let (served, counted) = (schema_served.clone(), reads.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (served, counted) = (served.clone(), counted.clone());
            async move {
                let (status, body) = if path.ends_with("/schema") && served.load(Ordering::SeqCst) {
                    (
                        200,
                        json!({ "schema_source": "node Person {\n  slug: String @key\n}\nnode Company {\n  slug: String @key\n}\nedge WorksAt: Person -> Company\n" }),
                    )
                } else if path.ends_with("/changes") {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let cause = json!({ "graph_commit_id": "c9", "parent_commit_id": "c8", "authored_branch": "main", "authored_at": 0 });
                    let person = json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": "p1", "op": "insert", "before": null, "after": { "properties": { "slug": "p1" } } });
                    let works = json!({ "kind": "edge", "type": { "id": "WorksAt", "name": "WorksAt" }, "id": "e1", "op": "insert", "before": null, "after": { "properties": {}, "endpoints": { "from": "p1", "to": "c1" } } });
                    (200, json!({ "cause": cause, "changes": [person, works] }))
                } else {
                    (404, json!({ "error": "not here" }))
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        let view = cx.new(|cx| CommitView::new(session.clone(), "c9".into(), None, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.schema.is_none());
            assert!(
                view.groups.groups.iter().any(|group| group.is_edge),
                "a relationship stands alone without a schema"
            );
        });
        schema_served.store(true, Ordering::SeqCst);
        session.update(cx, |session, cx| session.reload_schema(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let current = view
                .session
                .read(cx)
                .schema_model()
                .expect("the schema loaded");
            assert!(
                view.schema
                    .as_ref()
                    .is_some_and(|made| Arc::ptr_eq(made, &current))
            );
            assert!(
                view.groups.groups.iter().all(|group| !group.is_edge),
                "the relationship shows on its node's card"
            );
        });
        // The same text read again groups nothing again, so the list keeps its place.
        let grouped = view.read_with(cx, |view, _| view.schema.clone().unwrap());
        session.update(cx, |session, cx| session.reload_schema(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let current = view.session.read(cx).schema_model().unwrap();
            assert!(
                Arc::ptr_eq(&current, &grouped),
                "the schema held, not compiled again"
            );
            assert!(
                view.schema
                    .as_ref()
                    .is_some_and(|made| Arc::ptr_eq(made, &grouped)),
                "not grouped again"
            );
            assert!(view._regroup_task.is_none());
        });
        assert_eq!(reads.load(Ordering::SeqCst), 1, "the commit was read again");
    }

    /// A commit opened in its own tab reads itself for its kind, and offers a branch at it
    /// only when it is published: its Branch button and its key start one there, and a
    /// proposal offers neither. A commit shown beside History comes listed and is not read.
    #[gpui::test]
    async fn a_branch_starts_here_only_at_a_published_commit(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = asked.clone();
        let commit = |id: &str, kind: &str| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": kind, "created_at": 0, "parent_commit_id": "c0" });
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            seen.lock().unwrap().push(path.clone());
            async move {
                let cause = |id: &str| json!({ "graph_commit_id": id, "parent_commit_id": "c0", "authored_branch": "main", "authored_at": 0 });
                let (status, body) = match path.rsplit_once("/commits/") {
                    // As Omnigraph refuses the graph's first commit's changes.
                    Some((_, "c0/changes")) => (
                        409,
                        json!({ "error": "commit 'c0' has no first parent; bootstrap from an exact baseline", "code": "conflict",
                                "change_diff_refusal": { "reason": "parentless_commit", "graph_commit_id": "c0" } }),
                    ),
                    Some((_, rest)) if rest.ends_with("/changes") => {
                        let id = rest.trim_end_matches("/changes");
                        (200, json!({ "cause": cause(id), "changes": [] }))
                    }
                    Some((_, "c9")) => (200, commit("c9", "published")),
                    Some((_, "c8")) => (200, commit("c8", "unpublished")),
                    Some((_, "c6")) => (200, commit("c6", "unattached")),
                    // The graph's first commit: no parent.
                    Some((_, "c0")) => (
                        200,
                        json!({ "graph_commit_id": "c0", "graph_manifest_version": 1, "kind": "published", "created_at": 0 }),
                    ),
                    _ => (404, json!({ "error": "not here" })),
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        let started = Arc::new(std::sync::Mutex::new(Vec::new()));
        let heard = started.clone();
        cx.update(|cx| {
            cx.on_action(move |action: &CreateBranchAt, _| {
                heard.lock().unwrap().push(action.commit_id.clone())
            });
        });
        // A held commit names the branch it was written on, which it is not on: the header says
        // what it is instead.
        let held = |text: &str| Some((IconName::GitCommit, text.to_owned()));
        for (id, offered, place) in [
            ("c9", true, Some((IconName::GitBranch, "main".to_owned()))),
            (
                "c8",
                false,
                held("Unpublished, on no branch until published to main"),
            ),
            ("c6", false, held("Unattached snapshot, on no branch")),
        ] {
            let (view, cx) =
                cx.add_window_view(|_, cx| CommitView::new(session.clone(), id.into(), None, cx));
            cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert_eq!(view.branch_here(), offered.then_some(id), "{id}");
                assert_eq!(
                    branch_fact(
                        view.commit.as_ref(),
                        view.cause.as_ref(),
                        view.commit_pending
                    ),
                    place,
                    "{id}"
                );
            });
            let available = cx.update(|window, cx| window.is_action_available(&NewBranchHere, cx));
            assert_eq!(available, offered, "{id}");
            cx.dispatch_action(NewBranchHere);
            cx.run_until_parked();
        }
        assert_eq!(*started.lock().unwrap(), ["c9"]);
        // A commit still being read names no branch, though its changes' cause names the one
        // it was written on: a held commit is not on it. One that could not be read is placed
        // by the cause.
        let cause: ChangeCauseOutput = serde_json::from_value(
            json!({ "graph_commit_id": "c8", "parent_commit_id": "c0", "authored_branch": "main", "authored_at": 0 }),
        )
        .unwrap();
        assert_eq!(branch_fact(None, Some(&cause), true), None);
        assert_eq!(
            branch_fact(None, Some(&cause), false),
            Some((IconName::GitBranch, "main".to_owned()))
        );

        let listed: CommitOutput = serde_json::from_value(commit("c7", "published")).unwrap();
        let beside = cx.new(|cx| CommitView::new(session.clone(), "c7".into(), Some(listed), cx));
        cx.run_until_parked();
        beside.read_with(cx, |view, _| assert_eq!(view.branch_here(), Some("c7")));
        assert!(
            !asked
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.ends_with("/commits/c7")),
            "a listed commit was read again"
        );

        // The graph's first commit, listed or in its own tab, says what it is. Listed, it has
        // no parent, so its changes are never asked for; in its own tab they are asked for
        // beside its own read, once, and the server's refusal shows it as the first.
        let first: CommitOutput = serde_json::from_value(
            json!({ "graph_commit_id": "c0", "graph_manifest_version": 1, "kind": "published", "created_at": 0 }),
        )
        .unwrap();
        let first_changes = || {
            asked
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.ends_with("/commits/c0/changes"))
                .count()
        };
        let listed = cx.new(|cx| CommitView::new(session.clone(), "c0".into(), Some(first), cx));
        cx.run_until_parked();
        assert_eq!(
            first_changes(),
            0,
            "the listed first commit's changes were asked for"
        );
        let own = cx.new(|cx| CommitView::new(session.clone(), "c0".into(), None, cx));
        cx.run_until_parked();
        assert_eq!(first_changes(), 1);
        for view in [listed, own] {
            view.read_with(cx, |view, _| {
                assert!(matches!(view.state, State::Lasting(CommitCounts::First)));
                assert_eq!(view.branch_here(), Some("c0"), "a branch can start there");
            });
        }
    }

    /// A session on a server that answers a commit by its id only after `held` of the test
    /// clock, every commit's changes at once (the graph's first commit, `c0`, refused for having
    /// no parent, as Omnigraph refuses it), and notes every path asked for.
    fn held_commit_session(
        held: Duration,
        cx: &mut TestAppContext,
    ) -> (Entity<GraphSession>, Arc<std::sync::Mutex<Vec<String>>>) {
        cx.update(studio_theme::init_test);
        let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = asked.clone();
        let executor = cx.executor();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            seen.lock().unwrap().push(path.clone());
            let executor = executor.clone();
            async move {
                let cause = |id: &str| json!({ "graph_commit_id": id, "parent_commit_id": "c8", "authored_branch": "main", "authored_at": 0 });
                let insert = json!({ "kind": "node", "type": { "id": "Person", "name": "Person" }, "id": "p1", "op": "insert", "before": null, "after": { "properties": { "slug": "p1" } } });
                let (status, body) = match path.rsplit_once("/commits/") {
                    Some((_, "c0/changes")) => (
                        409,
                        json!({ "error": "commit 'c0' has no first parent; bootstrap from an exact baseline", "code": "conflict",
                                "change_diff_refusal": { "reason": "parentless_commit", "graph_commit_id": "c0" } }),
                    ),
                    Some((_, rest)) if rest.ends_with("/changes") => {
                        let id = rest.trim_end_matches("/changes");
                        (200, json!({ "cause": cause(id), "changes": [insert] }))
                    }
                    Some((_, id)) => {
                        executor.timer(held).await;
                        let parent = (id != "c0").then_some("c8");
                        (
                            200,
                            json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": "published", "created_at": 0, "parent_commit_id": parent }),
                        )
                    }
                    None => (404, json!({ "error": "not here" })),
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        (session, asked)
    }

    /// A commit tab restored with the workspace reads nothing until it is drawn: not the commit,
    /// nor its changes. Drawn, it reads both. Before, every restored commit tab read them at
    /// start, shown or not.
    #[gpui::test]
    async fn a_restored_commit_tab_reads_when_it_is_shown(cx: &mut TestAppContext) {
        let (session, asked) = held_commit_session(Duration::ZERO, cx);
        cx.run_until_parked();
        asked.lock().unwrap().clear();
        let hidden = cx.new(|cx| CommitView::when_shown(session.clone(), "c9".into(), cx));
        cx.run_until_parked();
        assert!(
            !asked
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.contains("/commits/c9")),
            "nothing read while it is not drawn"
        );
        hidden.read_with(cx, |view, _| assert!(view.read_owed));
        let (shown, cx) =
            cx.add_window_view(|_, cx| CommitView::when_shown(session.clone(), "c9".into(), cx));
        cx.run_until_parked();
        shown.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Ready));
            assert_eq!(view.folded.changes.len(), 1);
        });
    }

    /// A commit named by its id alone (a restored tab, a toast's "View commit", a parent link)
    /// asks for its changes at once, beside its own read, rather than one after the other. The
    /// graph's first commit, refused its changes for having no parent, shows as the first while
    /// its own read is still out, and is asked for them once.
    #[gpui::test]
    async fn a_commit_named_by_its_id_reads_its_changes_at_once(cx: &mut TestAppContext) {
        let held = Duration::from_secs(5);
        let (session, asked) = held_commit_session(held, cx);
        let view = cx.new(|cx| CommitView::new(session.clone(), "c9".into(), None, cx));
        let first = cx.new(|cx| CommitView::new(session.clone(), "c0".into(), None, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.commit_pending, "its own read is still out");
            assert!(
                matches!(view.state, State::Ready),
                "its changes did not wait"
            );
            assert_eq!(view.folded.changes.len(), 1);
        });
        first.read_with(cx, |view, _| {
            assert!(view.commit_pending);
            assert!(matches!(view.state, State::Lasting(CommitCounts::First)));
        });
        cx.executor().advance_clock(held);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.commit_pending);
            assert_eq!(view.branch_here(), Some("c9"));
            assert!(matches!(view.state, State::Ready));
        });
        first.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Lasting(CommitCounts::First)))
        });
        let asked = asked.lock().unwrap();
        let times = |end: &str| asked.iter().filter(|path| path.ends_with(end)).count();
        assert_eq!(times("/commits/c0/changes"), 1);
        assert_eq!(times("/commits/c9/changes"), 1);
    }

    /// A commit previewed from a History tab that is itself the preview keeps History: the
    /// preview slot goes to the commit, and History stays as a tab of its own (spec 020).
    #[gpui::test]
    async fn a_commit_previewed_from_a_preview_history_keeps_it(cx: &mut TestAppContext) {
        let (session, _asked) = held_commit_session(Duration::from_secs(5), cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let history = workspace.update_in(cx, |workspace, window, cx| {
            let session = session.clone();
            workspace.open_item_with::<crate::view::HistoryView>(
                crate::view::history_key("main"),
                true,
                window,
                cx,
                |window, cx| crate::view::HistoryView::new(session, "main".into(), window, cx),
            )
        });
        cx.run_until_parked();
        let history = history.expect("History opened");
        let listed: CommitOutput = serde_json::from_value(
            json!({ "graph_commit_id": "c7", "graph_manifest_version": 1, "kind": "published", "created_at": 0, "parent_commit_id": "c6" }),
        )
        .unwrap();
        cx.update(|window, cx| open_listed(&session, &listed, true, window, cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            let ids: Vec<_> = workspace
                .items()
                .iter()
                .map(|item| item.item_id())
                .collect();
            assert_eq!(ids.len(), 2, "History stayed beside the commit");
            assert_eq!(ids[0], history.entity_id());
            assert!(
                workspace
                    .active_item()
                    .and_then(|item| item.to_any().downcast::<CommitView>().ok())
                    .is_some(),
                "the commit's tab is active"
            );
        });
    }

    /// A commit opened from History's own click or ↵, while History is being updated, opens
    /// its tab: finding the tab reads History's key, which panicked inside that update.
    #[gpui::test]
    async fn a_commit_opened_from_inside_history_opens_its_tab(cx: &mut TestAppContext) {
        let (session, _asked) = held_commit_session(Duration::from_secs(5), cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let history = workspace
            .update_in(cx, |workspace, window, cx| {
                let session = session.clone();
                workspace.open_item_with::<crate::view::HistoryView>(
                    crate::view::history_key("main"),
                    false,
                    window,
                    cx,
                    |window, cx| crate::view::HistoryView::new(session, "main".into(), window, cx),
                )
            })
            .expect("History opened");
        cx.run_until_parked();
        let listed: CommitOutput = serde_json::from_value(
            json!({ "graph_commit_id": "c7", "graph_manifest_version": 1, "kind": "published", "created_at": 0, "parent_commit_id": "c6" }),
        )
        .unwrap();
        history.update_in(cx, |_, window, cx| {
            open_listed(&session, &listed, true, window, cx)
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace
                    .active_item()
                    .and_then(|item| item.to_any().downcast::<CommitView>().ok())
                    .is_some(),
                "the commit's tab is active"
            );
        });
    }

    /// A commit History lists opens in its own tab with the commit as listed: the tab reads its
    /// changes, and not the commit again.
    #[gpui::test]
    async fn a_listed_commit_opens_its_tab_without_reading_itself(cx: &mut TestAppContext) {
        let (session, asked) = held_commit_session(Duration::from_secs(5), cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let listed: CommitOutput = serde_json::from_value(
            json!({ "graph_commit_id": "c7", "graph_manifest_version": 1, "kind": "published", "created_at": 0, "parent_commit_id": "c6" }),
        )
        .unwrap();
        cx.update(|window, cx| open_listed(&session, &listed, false, window, cx));
        cx.run_until_parked();
        let tab = workspace
            .read_with(cx, |workspace, _| {
                workspace
                    .active_item()
                    .and_then(|item| item.to_any().downcast::<CommitView>().ok())
            })
            .expect("the commit's tab");
        tab.read_with(cx, |view, _| {
            assert!(!view.commit_pending, "the commit was not read again");
            assert_eq!(view.branch_here(), Some("c7"));
            assert!(matches!(view.state, State::Ready));
        });
        let asked = asked.lock().unwrap();
        assert!(!asked.iter().any(|path| path.ends_with("/commits/c7")));
        assert_eq!(
            asked
                .iter()
                .filter(|path| path.ends_with("/commits/c7/changes"))
                .count(),
            1
        );
    }
}
