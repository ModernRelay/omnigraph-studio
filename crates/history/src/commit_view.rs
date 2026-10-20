//! One commit's changes against its first parent (spec 009), with the same entity cards as the
//! review. Shown in History's detail split, or as its own tab through `OpenCommit`.

use std::time::Duration;

use change_cards::{CardLines, CardRows};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListState, Render, SharedString, Task, Window, list,
};
use graph_client::ReadError;
use graph_client::api::{ChangeCauseOutput, CommitOutput};
use graph_store::{
    ChangeFold, EntityGroups, FoldedChanges, GraphSession, GroupRun, GroupSection, NodeNames,
    TypeCounts,
};
use studio_actions::graph::{OpenCommit, OpenQueryAt, OpenType};
use studio_ui::menu::ContextMenu;
use studio_ui::{now_micros, relative_time, short_commit_id, utc_date_time};
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;
use ui::{PopoverMenu, Tooltip};

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

#[derive(Debug, Clone, Copy)]
enum Row {
    /// The header of a run of one type's cards, by its index in `runs`.
    Header(usize),
    /// An entity's card, by its group.
    Entity(usize),
}

enum State {
    Loading,
    Ready,
    Failed(SharedString),
}

pub struct CommitView {
    session: Entity<GraphSession>,
    commit_id: String,
    commit: Option<CommitOutput>,
    cause: Option<ChangeCauseOutput>,
    state: State,
    fold: ChangeFold,
    folded: FoldedChanges,
    /// The changes by entity, each node with its relationship changes, as the review shows
    /// them.
    groups: EntityGroups,
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
            this.update(cx, |this, cx| this.load_more(cx)).ok();
        }));
        view
    }

    fn unread(
        session: Entity<GraphSession>,
        commit_id: String,
        commit: Option<CommitOutput>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            session,
            commit_id,
            commit,
            cause: None,
            state: State::Loading,
            fold: ChangeFold::new(),
            folded: FoldedChanges::default(),
            groups: EntityGroups::default(),
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
        }
    }

    pub fn commit_id(&self) -> &str {
        &self.commit_id
    }

    /// Reads the next pages of changes, up to [`CAP`] more.
    fn load_more(&mut self, cx: &mut Context<Self>) {
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
                    Err(error) => break Err(describe(&error)),
                }
            };
            let read_any = fold.raw_changes() > start;
            // Finished, counted and laid out off the main thread; the fold is kept for the
            // next pages.
            let schema = session.read_with(cx, |session, _| session.schema_model());
            let (fold, folded, groups, runs, counts, rows) = cx
                .background_executor()
                .spawn(async move {
                    let folded = fold.clone().finish();
                    let counts = folded.counts();
                    let groups = EntityGroups::new(&folded, schema.as_deref());
                    let (rows, runs) = build_rows(&folded, &groups);
                    (fold, folded, groups, runs, counts, rows)
                })
                .await;
            this.update(cx, |this, cx| {
                this.fold = fold;
                // A failure after changes were shown keeps them, and those read since.
                let result = match result {
                    Err(message) if read_any || !this.rows.is_empty() => {
                        this.more_error = Some(message.into());
                        Ok(())
                    }
                    result => result,
                };
                match result {
                    Ok(()) => {
                        this.cause = cause.or(this.cause.take());
                        this.counts = counts;
                        this.rows = rows;
                        this.runs = runs;
                        let ends: Vec<_> = groups
                            .unnamed_ends()
                            .into_iter()
                            .filter(|end| !this.names.contains_key(end))
                            .collect();
                        this.groups = groups;
                        this.folded = folded;
                        this.card_rows = CardRows::default();
                        if !ends.is_empty() {
                            let commit = Some(this.commit_id.clone());
                            let names = this
                                .session
                                .update(cx, |session, cx| session.node_names(ends, commit, cx));
                            this._names_task = Some(cx.spawn(async move |this, cx| {
                                let names = names.await;
                                this.update(cx, |this, cx| {
                                    this.names.extend(names);
                                    cx.notify();
                                })
                                .ok();
                            }));
                        }
                        this.next_page_token = token;
                        // Rows read further down keep the place the person scrolled to.
                        let top = this.list_state.logical_scroll_top();
                        let more = usize::from(this.next_page_token.is_some());
                        this.list_state.reset(this.rows.len() + more);
                        this.list_state.scroll_to(top);
                        this.state = State::Ready;
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
        // A listed commit without an authored branch was written on main.
        let branch = self
            .cause
            .as_ref()
            .map(|c| c.authored_branch.clone())
            .or_else(|| {
                self.commit
                    .as_ref()
                    .map(|c| c.graph_branch.clone().unwrap_or_else(|| "main".into()))
            });
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
            State::Failed(_) => None,
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
        if let Some(branch) = branch {
            facts.push(studio_ui::ds::meta(IconName::GitBranch, branch).into_any_element());
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
    /// 009).
    fn render_open_at(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let commit = self.commit_id.clone();
        // The types changed in this commit first, then the rest of the schema.
        let mut types: Vec<String> = self.counts.iter().map(|c| c.type_name.clone()).collect();
        if let Some(schema) = self.session.read(cx).schema_model() {
            for name in schema
                .nodes
                .iter()
                .map(|n| &n.name)
                .chain(schema.edges.iter().map(|e| &e.name))
            {
                if !types.contains(name) {
                    types.push(name.clone());
                }
            }
        }
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
                        let types = types.clone();
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
                let schema = self.session.read(cx).schema_model();
                let schema = schema.as_deref();
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

fn describe(error: &ReadError) -> String {
    match error {
        ReadError::Refused(summary) if summary.status == 409 => format!(
            "This commit cannot be compared with a parent (the first commit, or a schema change): {}",
            summary.message
        ),
        ReadError::Refused(summary) if summary.status == 410 => {
            "The history behind this commit was cleaned up, so its changes cannot be read.".into()
        }
        other => other.to_string(),
    }
}

/// The list's rows: a header for each run of one type, then its cards.
fn build_rows(folded: &FoldedChanges, groups: &EntityGroups) -> (Vec<Row>, Vec<GroupRun>) {
    let runs = groups.runs(folded, |_| true);
    let mut rows = Vec::new();
    for (ix, run) in runs.iter().enumerate() {
        rows.push(Row::Header(ix));
        rows.extend(run.groups.iter().map(|&group| Row::Entity(group)));
    }
    (rows, runs)
}

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
                    .header(graph_client::CONTRACT_HEADER, "0.13")
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
                    .header(graph_client::CONTRACT_HEADER, "0.13")
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
