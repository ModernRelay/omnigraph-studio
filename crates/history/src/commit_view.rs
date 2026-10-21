//! One commit's changes against its first parent (spec 009), with the same entity cards as the
//! review. Shown in History's detail split, or as its own tab through `OpenCommit`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use change_cards::{CardLines, CardRows, Row};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListState, Render, SharedString, Subscription, Task, Window, list,
};
use gq_language::SchemaModel;
use graph_client::ReadError;
use graph_client::api::{ChangeCauseOutput, ChangeDiffRefusalReason, CommitKind, CommitOutput};
use graph_store::{
    ChangeFold, EntityGroups, FoldedChanges, GraphSession, GroupRun, GroupSection, NodeNames,
    SessionEvent, TypeCounts,
};
use studio_actions::graph::{CreateBranchAt, OpenCommit, OpenQueryAt, OpenType};
use studio_actions::history::{LoadMoreChanges, NewBranchHere};
use studio_ui::menu::ContextMenu;
use studio_ui::{now_micros, relative_time, short_commit_id, utc_date_time};
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace};
use ui::prelude::*;
use ui::{KeyBinding, PopoverMenu, Tooltip};

/// Changes read before asking the person to load more (constitution IX).
const CAP: usize = 5_000;
/// Changes asked for per page: the whole of [`CAP`], since each page restarts the server's
/// comparison after the last change returned; the server's byte budget still bounds a page.
const PAGE: u32 = CAP as u32;

/// A commit's counts, once its changes are read; History shows them in its list.
#[derive(Debug, Clone)]
pub struct CommitCounts {
    pub commit_id: String,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    /// More changes are still to read: the counts are of those read so far.
    pub partial: bool,
}

enum State {
    Loading,
    Ready,
    Failed(SharedString),
    /// The graph's first commit: it has no parent to compare with, so its changes are not
    /// asked for (the server refuses them, `parentless_commit`).
    First,
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
    /// The wait before a stepped-to commit's first read; dropped with the view.
    _pause: Option<Task<()>>,
    _regroup_task: Option<Task<()>>,
    _schema_subscription: Subscription,
}

impl EventEmitter<ItemEvent> for CommitView {}
impl EventEmitter<CommitCounts> for CommitView {}

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

    /// A commit stepped to, which reads its changes only once the stepping has paused for
    /// `pause`: a held arrow key passes commits whose reads the server would otherwise start
    /// and the view drop. The header shows at once.
    pub fn after_pause(
        session: Entity<GraphSession>,
        commit_id: String,
        commit: Option<CommitOutput>,
        pause: Duration,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::unread(session, commit_id, commit, cx);
        view._pause = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(pause).await;
            this.update(cx, |this, cx| {
                this.read_commit(cx);
                this.load_more(cx);
            })
            .ok();
        }));
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
            _names_task: None,
            counts: Vec::new(),
            rows: Vec::new(),
            next_page_token: None,
            more_error: None,
            list_state: ListState::new(0, ListAlignment::Top, px(600.)),
            focus_handle: cx.focus_handle(),
            _task: None,
            _pause: None,
            _regroup_task: None,
            _schema_subscription: schema_subscription,
        }
    }

    pub fn commit_id(&self) -> &str {
        &self.commit_id
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
        self.state = State::First;
        true
    }

    /// The commit a new branch can start at: this one, when it is published.
    fn branch_here(&self) -> Option<&str> {
        branch_start(self.commit.as_ref()?)
    }

    /// Whether the list's last row offers to read more changes ("Load more changes", or "Try
    /// again" after a read of them failed), with no read under way.
    fn offers_more(&self) -> bool {
        self.next_page_token.is_some() && !matches!(self.state, State::Loading)
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
                        let block = graph_client::api::ChangeBlockOutput {
                            cause: page.cause.clone(),
                            changes: page.changes,
                        };
                        cause = Some(page.cause);
                        fold = cx
                            .background_executor()
                            .spawn(async move {
                                fold.add_block(block);
                                fold
                            })
                            .await;
                        token = page.next_page_token;
                        if token.is_none() || fold.raw_changes() - start >= CAP {
                            break Ok(());
                        }
                    }
                    // The graph's first commit: it has no parent to compare with.
                    Err(error) if is_parentless(&error) => break Err(None),
                    Err(error) => break Err(Some(describe(&error))),
                }
            };
            let read_any = fold.raw_changes() > start;
            // Finished, counted and laid out off the main thread; the fold is kept for the
            // next pages.
            let schema = session.read_with(cx, |session, _| session.schema_model());
            let made_with = schema.clone();
            let (fold, folded, groups, runs, counts, rows) = cx
                .background_executor()
                .spawn(async move {
                    let folded = fold.clone().finish();
                    let counts = folded.counts();
                    let groups = EntityGroups::new(&folded, schema.as_deref());
                    let (rows, runs) = change_cards::rows(&folded, &groups, |_| true);
                    (fold, Arc::new(folded), groups, runs, counts, rows)
                })
                .await;
            this.update(cx, |this, cx| {
                this.fold = fold;
                let result = match result {
                    Err(None) => {
                        this.next_page_token = None;
                        this.more_error = None;
                        this.state = State::First;
                        cx.notify();
                        return;
                    }
                    // A failure after changes were shown keeps them, and those read since.
                    Err(Some(message)) if read_any || !this.rows.is_empty() => {
                        this.more_error = Some(message.into());
                        Ok(())
                    }
                    Err(Some(message)) => Err(message),
                    Ok(()) => Ok(()),
                };
                match result {
                    Ok(()) => {
                        this.cause = cause.or(this.cause.take());
                        this.counts = counts;
                        this.rows = rows;
                        this.runs = runs;
                        this.groups = groups;
                        this.folded = folded;
                        this.schema = made_with;
                        this.card_rows = CardRows::default();
                        this.read_names(cx);
                        this.next_page_token = token;
                        // Rows read further down keep the place the person scrolled to.
                        this.reset_list();
                        this.state = State::Ready;
                        // A schema that arrived while these were grouped groups them again.
                        this.regroup(cx);
                        let (added, changed, removed) =
                            this.counts.iter().fold((0, 0, 0), |(a, c, r), t| {
                                (a + t.added, c + t.changed, r + t.removed)
                            });
                        cx.emit(CommitCounts {
                            commit_id: this.commit_id.clone(),
                            added,
                            changed,
                            removed,
                            partial: this.next_page_token.is_some(),
                        });
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
            let names = names.await;
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
        let same = match (&self.schema, &schema) {
            (Some(made), Some(current)) => Arc::ptr_eq(made, current) || made.same_as(current),
            (None, None) => true,
            _ => false,
        };
        if same {
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
                this.schema = made_with;
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
        let changes = match &self.state {
            State::Ready => {
                let total = self.folded.entity_count() + self.folded.relationship_count();
                let mut text = format!(
                    "{} change{}",
                    studio_ui::group_digits(total),
                    if total == 1 { "" } else { "s" }
                );
                if self.next_page_token.is_some() {
                    text.push_str(" so far");
                }
                Some(text)
            }
            State::Loading => Some("Reading changes…".into()),
            State::Failed(_) | State::First => None,
        };
        // A parent is a link to its own commit.
        let parent_link = |id: String, icon: IconName, name: &'static str, tooltip: String| {
            let short = short_commit_id(&id);
            div()
                .id(name)
                .cursor_pointer()
                .tooltip(Tooltip::text(tooltip))
                .on_click(move |_, window, cx| {
                    window.dispatch_action(
                        Box::new(OpenCommit {
                            commit_id: id.clone(),
                        }),
                        cx,
                    )
                })
                .child(studio_ui::ds::meta_code(icon, short, cx))
                .into_any_element()
        };
        let mut facts: Vec<AnyElement> = vec![
            studio_ui::ds::meta(IconName::Person, actor.unwrap_or_else(|| "—".into()))
                .into_any_element(),
        ];
        if let Some(at) = at {
            facts.push(
                div()
                    .id("commit-when")
                    .tooltip(Tooltip::text(utc_date_time(at)))
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
        if let Some(changes) = changes {
            facts.push(studio_ui::ds::text::meta(changes).into_any_element());
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
                    .tooltip(Tooltip::text(format!("{full_id}\nClick to copy")))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(full_id.clone()))
                    })
                    .child(
                        studio_ui::ds::text::heading(short_commit_id(&self.commit_id))
                            .buffer_font(cx),
                    ),
            )
            .child(studio_ui::ds::meta_row(facts).flex_wrap())
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
                        studio_ui::ds::toolbar_button("open-table-at-button", "Table")
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::Table))
                            .tooltip(Tooltip::text(
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
                                    }),
                                );
                            }
                            menu
                        }))
                    }),
            )
            .child(
                studio_ui::ds::toolbar_button("query-at", "Query")
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(IconName::FileCode))
                    .tooltip(Tooltip::text("Open a query that reads this commit"))
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
                    studio_ui::ds::toolbar_button("branch-here", "Branch")
                        .style(ButtonStyle::Subtle)
                        .start_icon(Icon::new(IconName::GitBranch))
                        .tooltip(move |_, cx| {
                            Tooltip::with_meta_in(
                                "Start a new branch at this commit",
                                Some(&NewBranchHere),
                                cli.clone(),
                                &focus,
                                cx,
                            )
                        })
                        .on_click(move |_, window, cx| new_branch_at(&commit, window, cx)),
                )
            })
    }

    fn render_row(
        &mut self,
        row: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let more = self.next_page_token.is_some();
        match self.rows.get(row).copied() {
            Some(Row::Header(ix)) => div()
                .px(px(studio_ui::ds::space::EDGE))
                .child(change_cards::run_header(
                    &self.runs[ix],
                    |section| match section {
                        GroupSection::Existing => "Changes to existing entities".to_owned(),
                        GroupSection::Added => "Added".to_owned(),
                    },
                    cx,
                ))
                .into_any_element(),
            Some(Row::Entity(ix)) => {
                // The schema the groups were made with, which the cards' rows agree with.
                let schema = self.schema.as_deref();
                let card = change_cards::entity_card(
                    &self.groups,
                    &self.folded,
                    ix,
                    schema,
                    &self.names,
                    &self.card_rows,
                    ("commit-change", ix),
                );
                let group = &self.groups.groups[ix];
                let card_key = (group.type_name.clone(), group.id.clone());
                let total = self.groups.line_count(ix);
                let shown = self.card_lines.shown(&card_key, total, None);
                let left_out = total - shown;
                // Only the lines the card shows are made.
                let card_lines = self
                    .groups
                    .lines(&self.folded, ix, schema, &self.names, shown);
                let counts = (card_lines.added, card_lines.changed, card_lines.removed);
                let lines = card_lines
                    .entries
                    .into_iter()
                    .map(|entry| {
                        // A mirror's other end is the card it is shown on in full.
                        change_cards::relationship_line(
                            entry,
                            &self.folded,
                            schema,
                            &self.card_rows,
                            "shown there",
                        )
                    })
                    .collect();
                let mut card = card.relationships(counts, lines);
                if left_out > 0 {
                    card = card.relationships_footer(
                        studio_ui::ds::button(
                            ("more-relationships", ix),
                            change_cards::more_lines_label(left_out),
                        )
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.card_lines.show_more(card_key.clone(), shown);
                            cx.notify();
                        })),
                    );
                }
                div()
                    .px(px(studio_ui::ds::space::EDGE))
                    .py(px(studio_ui::ds::space::XS))
                    .child(card)
                    .into_any_element()
            }
            None if more => h_flex()
                .px(px(studio_ui::ds::space::XL))
                .py(px(studio_ui::ds::space::MD))
                .gap(px(studio_ui::ds::space::MD))
                .child(
                    studio_ui::ds::button(
                        "load-more",
                        if matches!(self.state, State::Loading) {
                            "Reading changes…"
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
fn branch_fact(
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

/// What the graph's first commit is called where its changes would show.
pub(crate) const FIRST_COMMIT: &str = "The graph's first commit";

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
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.open_item::<CommitView>(commit_key(&id), window, cx, |_, cx| {
        CommitView::new(session, id, listed, cx)
    });
}

/// Opens a listed commit's own tab with the commit as listed ([`open_in`]). A list drawn
/// outside a workspace's window has no workspace to open it in, and dispatches [`OpenCommit`],
/// which reads the commit by its id.
pub(crate) fn open_listed(
    session: &Entity<GraphSession>,
    commit: &CommitOutput,
    window: &mut Window,
    cx: &mut App,
) {
    let id = commit.graph_commit_id.clone();
    let Some(workspace) = studio_workspace::workspace_for(window) else {
        window.dispatch_action(Box::new(OpenCommit { commit_id: id }), cx);
        return;
    };
    let (session, listed) = (session.clone(), commit.clone());
    workspace.update(cx, |workspace, cx| {
        open_in(workspace, session, id, Some(listed), window, cx)
    });
}

/// The CLI command that starts a branch at `commit_id`.
pub(crate) fn branch_here_cli(commit_id: &str) -> String {
    format!("omnigraph branch create <name> --from-commit {commit_id}")
}

/// Whether the server refused a commit's changes because the commit has no parent: the
/// graph's first commit.
fn is_parentless(error: &ReadError) -> bool {
    matches!(
        error,
        ReadError::Refused(summary)
            if summary.change_diff_refusal.as_ref().map(|refusal| refusal.reason)
                == Some(ChangeDiffRefusalReason::ParentlessCommit)
    )
}

/// Why a commit's changes could not be read, with the server's reason as it came. A comparison
/// the server refused says what stopped it by the refusal's reason: a schema change between the
/// commit and its parent, naming the type at it; any other reason claims no schema change.
fn describe(error: &ReadError) -> String {
    match error {
        ReadError::Refused(summary) if summary.status == 409 => {
            let message = &summary.message;
            match summary.change_diff_refusal.as_ref() {
                Some(refusal) if refusal.reason == ChangeDiffRefusalReason::SchemaBoundary => {
                    match &refusal.type_name {
                        Some(type_name) => format!(
                            "This commit cannot be compared with its parent across a change to \
                             {type_name}'s schema: {message}"
                        ),
                        None => format!(
                            "This commit cannot be compared with its parent across a schema \
                             change: {message}"
                        ),
                    }
                }
                _ => format!("This commit cannot be compared with its parent: {message}"),
            }
        }
        ReadError::Refused(summary) if summary.status == 410 => {
            "The history behind this commit was cleaned up, so its changes cannot be read.".into()
        }
        other => other.to_string(),
    }
}

/// The list's rows: a header for each run of one type, then its cards.
impl Render for CommitView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        let more = usize::from(self.next_page_token.is_some());
        let body = match &self.state {
            State::Failed(message) => {
                studio_ui::ds::error_state("The changes could not be read", message.clone())
                    .into_any_element()
            }
            State::Ready if self.rows.is_empty() => {
                studio_ui::ds::empty_state(IconName::Check, "No entity changes").into_any_element()
            }
            State::First => studio_ui::ds::empty_state(IconName::GitCommit, FIRST_COMMIT)
                .child(studio_ui::ds::text::muted(
                    "It has no parent, so it has no changes to compare.",
                ))
                .into_any_element(),
            State::Loading if self.rows.is_empty() => {
                studio_ui::ds::loading_state("Reading changes…").into_any_element()
            }
            _ => {
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
            .child(header)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::TestAppContext;
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::json;

    use super::*;

    /// A comparison the server refused says why by the refusal's typed reason: the graph's first
    /// commit is told apart, a schema change names its type, and another 409 is told as the
    /// server put it, claiming no schema change.
    #[test]
    fn a_refused_comparison_says_why() {
        use ChangeDiffRefusalReason::{ParentlessCommit, SchemaBoundary};
        use graph_client::api::ChangeDiffRefusalOutput;
        let refused = |refusal: Option<(ChangeDiffRefusalReason, Option<&str>)>| {
            ReadError::Refused(graph_client::ApiErrorSummary {
                status: 409,
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
            })
        };
        assert!(is_parentless(&refused(Some((ParentlessCommit, None)))));
        assert!(!is_parentless(&refused(Some((SchemaBoundary, None)))));
        assert!(!is_parentless(&refused(None)));
        let typed = describe(&refused(Some((SchemaBoundary, Some("Person")))));
        assert!(typed.contains("a change to Person's schema"), "{typed}");
        let untyped = describe(&refused(Some((SchemaBoundary, None))));
        assert!(untyped.contains("across a schema change"), "{untyped}");
        let other = describe(&refused(None));
        assert!(!other.contains("schema"), "{other}");
        // Each keeps the server's reason as it came.
        for said in [typed, untyped, other] {
            assert!(said.ends_with(": as the server put it"), "{said}");
        }
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
        let partial = Arc::new(std::sync::Mutex::new(Vec::new()));
        let heard = partial.clone();
        cx.update(|cx| {
            cx.subscribe(&view, move |_, counts: &CommitCounts, _| {
                heard.lock().unwrap().push(counts.partial)
            })
            .detach()
        });
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
        assert_eq!(*partial.lock().unwrap(), [true, false]);
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
            assert!(!Arc::ptr_eq(&current, &grouped), "read again");
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
                    // As OmniGraph refuses the graph's first commit's changes.
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
                assert!(matches!(view.state, State::First));
                assert_eq!(view.branch_here(), Some("c0"), "a branch can start there");
            });
        }
    }

    /// A session on a server that answers a commit by its id only after `held` of the test
    /// clock, every commit's changes at once (the graph's first commit, `c0`, refused for having
    /// no parent, as OmniGraph refuses it), and notes every path asked for.
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
            assert!(matches!(view.state, State::First));
        });
        cx.executor().advance_clock(held);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.commit_pending);
            assert_eq!(view.branch_here(), Some("c9"));
            assert!(matches!(view.state, State::Ready));
        });
        first.read_with(cx, |view, _| assert!(matches!(view.state, State::First)));
        let asked = asked.lock().unwrap();
        let times = |end: &str| asked.iter().filter(|path| path.ends_with(end)).count();
        assert_eq!(times("/commits/c0/changes"), 1);
        assert_eq!(times("/commits/c9/changes"), 1);
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
        cx.update(|window, cx| open_listed(&session, &listed, window, cx));
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

    /// A commit stepped past reads nothing; the one the steps stop on reads once they pause.
    #[gpui::test]
    async fn a_stepped_commit_reads_only_once_the_steps_pause(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = reads.clone();
        let http = FakeHttpClient::create(move |request| {
            let changes = request.uri().path().ends_with("/changes");
            if changes {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            async move {
                let cause = json!({ "graph_commit_id": "c9", "parent_commit_id": "c8", "authored_branch": "main", "authored_at": 0 });
                let body = json!({ "cause": cause, "changes": [] });
                Ok(Response::builder()
                    .status(if changes { 200 } else { 404 })
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
        let pause = Duration::from_millis(120);
        let passed =
            cx.new(|cx| CommitView::after_pause(session.clone(), "c8".into(), None, pause, cx));
        cx.run_until_parked();
        drop(passed);
        let stopped = cx.new(|cx| CommitView::after_pause(session, "c9".into(), None, pause, cx));
        cx.run_until_parked();
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "nothing reads before the pause"
        );
        cx.executor().advance_clock(pause);
        cx.run_until_parked();
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "only the commit stopped on reads"
        );
        stopped.read_with(cx, |view, _| assert!(matches!(view.state, State::Ready)));
    }
}
