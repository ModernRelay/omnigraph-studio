//! A node's history (spec 040): the commits of the window's branch that changed the node or
//! one of its relationships, newest first, each with who made it, what it was in Studio's own
//! words where Studio made it, and its before and after as a review's card shows them. A view
//! of one node in a tab of its own, beside its page, tree and canvas.
//!
//! Omnigraph has no read of one entity's commits (E62) and keeps no operation or message with a
//! commit (E60): a window covers the branch's newest [`NODE_HISTORY_COMMITS`] commits, read from
//! the change feed filtered to the node's types ([`GraphSession::node_history`]), with an
//! honest exit to the commits before them.

use gpui::{
    AnyElement, AnyView, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ListAlignment, ListState, Render, SharedString, StyleRefinement, Subscription, Task,
    WeakEntity, Window, list,
};
use gq_language::SchemaModel;
use graph_store::node_history::{
    NODE_HISTORY_COMMITS, NodeHistoryEntry, NodeHistoryPage, NodeHistoryWindow, NodeScope,
    OlderWindow,
};
use graph_store::{FoldedChange, GraphSession, SessionEvent, entity_name, relationship_words};
use studio_actions::graph::{OpenCommit, OpenNodePage, ShowNodeInTree, ShowNodeOnCanvas};
use studio_actions::history::ReadOlderCommits;
use studio_actions::workspace::Refresh;
use studio_actions::{ViewKind, ViewLink};
use studio_ui::ds::Tipped as _;
use studio_ui::ds::{radii, space};
use studio_ui::{RelationshipLine, now_micros, relative_time, short_commit_id, utc_date_time};
use studio_workspace::{Inspected, InspectedNode, Item, ItemEvent, SavedItem};
use ui::KeyBinding;
use ui::prelude::*;

/// What the view has read.
enum State {
    /// Nothing read yet, or read again from the start.
    Reading,
    Ready,
    /// The first window could not be read.
    Failed(SharedString),
}

/// How the read of older commits stands.
#[derive(Clone)]
enum Older {
    /// The window read reached the graph's first commit.
    None,
    /// Commits before those read wait.
    Waits(OlderWindow),
    Reading(OlderWindow),
    Failed(OlderWindow, SharedString),
}

pub struct NodeHistoryView {
    session: Entity<GraphSession>,
    type_name: String,
    id: String,
    /// The branch read: the window's, followed when it changes.
    branch: String,
    /// The node's name, from its newest change read, for the tab.
    label: Option<String>,
    state: State,
    /// The head the first window was read at: every window answers for it (constitution II).
    head: Option<String>,
    entries: Vec<NodeHistoryEntry>,
    /// First-parent commits the windows read cover.
    commits: usize,
    older: Older,
    /// Why a window was not read to its end, when one was not.
    stopped: Option<SharedString>,
    /// The branch moved past the head read: Refresh reads it again.
    newer: bool,
    list_state: ListState,
    focus_handle: FocusHandle,
    /// The history is read when the tab is next drawn: a tab restored behind another reads
    /// nothing until it is shown.
    read_owed: bool,
    _task: Option<Task<()>>,
    _session_events: Subscription,
    /// The entries, a view of their own drawn cached (`docs/drawing.md`).
    list_view: Entity<NodeHistoryRows>,
}

impl EventEmitter<ItemEvent> for NodeHistoryView {}

impl Focusable for NodeHistoryView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// One node's history tab, by its type and id.
pub fn node_history_key(type_name: &str, id: &str) -> String {
    format!("node-history:{type_name}:{id}")
}

impl Item for NodeHistoryView {
    fn tab_title(&self, _cx: &App) -> SharedString {
        format!("History · {}", self.label.as_deref().unwrap_or(&self.id)).into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::HistoryRerun)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(node_history_key(&self.type_name, &self.id).into())
    }

    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        Some(studio_actions::Thing::Node {
            type_name: self.type_name.clone(),
            id: self.id.clone(),
        })
    }

    /// A node's views: its page, tree and canvas, and this.
    fn views(&self, _cx: &App) -> Vec<ViewLink> {
        vec![
            ViewLink::to(ViewKind::Page),
            ViewLink::to(ViewKind::Tree),
            ViewLink::to(ViewKind::Canvas),
            ViewLink::current(ViewKind::History),
        ]
    }

    fn view_action(&self, kind: ViewKind, _cx: &App) -> Option<Box<dyn gpui::Action>> {
        let (type_name, id) = (self.type_name.clone(), self.id.clone());
        match kind {
            ViewKind::Page => Some(Box::new(OpenNodePage {
                type_name,
                id,
                at_commit: None,
                preview: true,
            })),
            ViewKind::Tree => Some(Box::new(ShowNodeInTree {
                type_name,
                id,
                at_commit: None,
                preview: true,
            })),
            ViewKind::Canvas => Some(Box::new(ShowNodeOnCanvas {
                type_name,
                id,
                at_commit: None,
                preview: true,
            })),
            _ => None,
        }
    }

    /// The inspector shows the node, at the head the history was read at.
    fn inspected(&mut self, _cx: &mut Context<Self>) -> Option<Inspected> {
        Some(Inspected::Node(InspectedNode {
            type_name: self.type_name.clone(),
            id: self.id.clone(),
            at_commit: None,
            snapshot: self.head.clone(),
        }))
    }

    fn selected_ids(&self, _cx: &App) -> Option<studio_workspace::SelectedIds> {
        Some(studio_workspace::SelectedIds {
            type_name: Some(self.type_name.clone()),
            ids: vec![self.id.clone()],
            root: None,
        })
    }

    fn is_stale(&self, _cx: &App) -> bool {
        self.newer
    }

    fn caches_its_parts(&self) -> bool {
        true
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "node_history".into(),
            data: serde_json::json!({
                "type_name": self.type_name,
                "id": self.id,
                "label": self.label,
            }),
        })
    }
}

impl NodeHistoryView {
    /// A node's history, read once the tab is shown.
    pub fn new(
        session: Entity<GraphSession>,
        type_name: String,
        id: String,
        label: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch = session.read(cx).branch.clone();
        let subscription = cx.subscribe(&session, |this, session, event, cx| match event {
            SessionEvent::Committed { branch } if *branch == this.branch => {
                if !this.newer && this.head.is_some() {
                    this.newer = true;
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }
            }
            SessionEvent::BranchChanged => {
                this.branch = session.read(cx).branch.clone();
                this.reload(cx);
            }
            SessionEvent::SchemaChanged => this.reload(cx),
            _ => {}
        });
        let list_view = NodeHistoryRows::new(cx);
        Self {
            session,
            type_name,
            id,
            branch,
            label,
            state: State::Reading,
            head: None,
            entries: Vec::new(),
            commits: 0,
            older: Older::None,
            stopped: None,
            newer: false,
            list_state: ListState::new(0, ListAlignment::Top, px(1000.)),
            focus_handle: cx.focus_handle(),
            read_owed: true,
            _task: None,
            _session_events: subscription,
            list_view,
        }
    }

    /// The history read again from the branch head.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.state = State::Reading;
        self.entries.clear();
        self.commits = 0;
        self.older = Older::None;
        self.stopped = None;
        self.head = None;
        if self.newer {
            self.newer = false;
            cx.emit(ItemEvent::UpdateTab);
        }
        self.read(NodeHistoryWindow::Newest, cx);
        cx.notify();
    }

    fn scope(&self, cx: &App) -> Option<NodeScope> {
        let schema = self.session.read(cx).schema_model()?;
        Some(NodeScope::new(&schema, &self.type_name, &self.id))
    }

    /// Reads `window`, and takes it below what was read.
    fn read(&mut self, window: NodeHistoryWindow, cx: &mut Context<Self>) {
        let Some(scope) = self.scope(cx) else {
            // The schema is not read yet: its arrival reads the history (SchemaChanged).
            return;
        };
        let older = matches!(window, NodeHistoryWindow::Older { .. });
        let branch = self.branch.clone();
        let read = self.session.update(cx, |session, cx| {
            session.node_history(&branch, scope, window, cx)
        });
        self._task = Some(cx.spawn(async move |this, cx| {
            let answer = read.await;
            this.update(cx, |this, cx| {
                match answer {
                    Ok(page) => this.take(page, cx),
                    Err(error) => {
                        let error: SharedString = error.to_string().into();
                        match (&this.older, older) {
                            (Older::Reading(window), true) => {
                                this.older = Older::Failed(window.clone(), error)
                            }
                            _ => this.state = State::Failed(error),
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn take(&mut self, page: NodeHistoryPage, cx: &mut Context<Self>) {
        let NodeHistoryPage {
            head,
            entries,
            commits,
            older,
            stopped,
        } = page;
        if self.head.is_none() {
            self.head = Some(head);
            cx.emit(ItemEvent::Selected);
        }
        if self.label.is_none() {
            let schema = self.session.read(cx).schema_model();
            let label = entries
                .iter()
                .find_map(|entry| entry.node.as_ref())
                .and_then(|node| entity_name(node, schema.as_deref()));
            if label.is_some() {
                self.label = label;
                cx.emit(ItemEvent::UpdateTab);
            }
        }
        self.entries.extend(entries);
        self.commits += commits;
        self.stopped = stopped.map(Into::into);
        self.older = match older {
            Some(window) => Older::Waits(window),
            None => Older::None,
        };
        self.state = State::Ready;
    }

    /// Reads the commits before those read, as the list's last row offers.
    fn read_older(&mut self, cx: &mut Context<Self>) {
        let (Older::Waits(window) | Older::Failed(window, _)) = self.older.clone() else {
            return;
        };
        let Some(head) = self.head.clone() else {
            return;
        };
        self.older = Older::Reading(window.clone());
        self.read(NodeHistoryWindow::Older { head, window }, cx);
        cx.notify();
    }

    fn offers_older(&self) -> bool {
        matches!(self.older, Older::Waits(_) | Older::Failed(..))
    }

    /// The node's history, as a test reads it: each entry's commit, actor and whether it
    /// changed the node itself, and how many relationships.
    #[cfg(test)]
    pub(crate) fn entries(&self) -> Vec<(String, Option<String>, bool, usize)> {
        self.entries
            .iter()
            .map(|e| {
                (
                    e.commit.clone(),
                    e.actor.clone(),
                    e.node.is_some(),
                    e.edges.len(),
                )
            })
            .collect()
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.entries.len() + 1;
        if self.list_state.item_count() != count {
            self.list_state.reset(count);
        }
        list(
            self.list_state.clone(),
            cx.processor(|this, ix, window, cx| this.render_row(ix, window, cx)),
        )
        .flex_1()
        .min_h_0()
        .py(px(space::MD))
        .into_any_element()
    }

    fn render_row(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.entries.get(ix) {
            Some(entry) => {
                let words = self
                    .session
                    .read(cx)
                    .own_words(&entry.commit)
                    .map(str::to_owned);
                let schema = self.session.read(cx).schema_model();
                entry_card(ix, entry, words, &self.id, schema.as_deref(), cx)
            }
            None => self.render_end(cx),
        }
    }

    /// The list's last row: how far back the history was read, and the commits before them.
    fn render_end(&self, cx: &mut Context<Self>) -> AnyElement {
        let read = format!(
            "{} read",
            studio_ui::plural(self.commits, "commit", "commits")
        );
        let (said, button): (String, Option<AnyElement>) = match &self.older {
            Older::None => (format!("{read}, back to the graph's first"), None),
            Older::Waits(_) | Older::Failed(..) | Older::Reading(_) => {
                let reading = matches!(self.older, Older::Reading(_));
                let failed = match &self.older {
                    Older::Failed(_, error) => Some(error.clone()),
                    _ => None,
                };
                let label = if reading {
                    "Reading older commits…".to_owned()
                } else if failed.is_some() {
                    "Try again".to_owned()
                } else {
                    format!("Read the {NODE_HISTORY_COMMITS} commits before")
                };
                let button = studio_ui::ds::button("read-older", label)
                    .style(ButtonStyle::Subtle)
                    .disabled(reading)
                    .key_binding(KeyBinding::for_action_in(
                        &ReadOlderCommits,
                        &self.focus_handle,
                        cx,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.read_older(cx)))
                    .into_any_element();
                let said = match failed {
                    Some(error) => format!("{read}. Older commits could not be read: {error}"),
                    None => format!("{read}. Older changes are in the commits before them."),
                };
                (said, Some(button))
            }
        };
        v_flex()
            .px(px(space::XL))
            .py(px(space::MD))
            .gap(px(space::SM))
            .children(self.stopped.clone().map(|why| {
                studio_ui::ds::text::muted(format!(
                    "The read stopped short: {}. The changes shown are those read before it.",
                    studio_ui::clause(why.as_ref())
                ))
            }))
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .child(studio_ui::ds::text::meta(said))
                    .children(button),
            )
            .into_any_element()
    }
}

/// One commit's card: who made it, what it was, when, its commit; then what it did to the node
/// and its relationships, as a review's card says it.
fn entry_card(
    ix: usize,
    entry: &NodeHistoryEntry,
    words: Option<String>,
    node_id: &str,
    schema: Option<&SchemaModel>,
    cx: &App,
) -> AnyElement {
    let kind = match &entry.node {
        Some(node) => change_cards::kind_of_change(node),
        None => studio_ui::ChangeKind::Changed,
    };
    let what = words.or_else(|| {
        entry
            .merged_parent
            .as_ref()
            .map(|merged| format!("Merge of commit {}", short_commit_id(merged)))
    });
    let commit = entry.commit.clone();
    let open_commit = move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
        window.dispatch_action(
            Box::new(OpenCommit {
                commit_id: commit.clone(),
                preview: event.click_count() < 2,
            }),
            cx,
        )
    };
    let head = h_flex()
        .min_h(px(studio_ui::ds::size::HEADER))
        .px(px(space::LG))
        .gap(px(space::MD))
        .items_center()
        .child(change_cards::kind_mark(("history-kind", ix), kind))
        .child(
            studio_ui::ds::text::body(entry.actor.clone().unwrap_or_else(|| "no actor".into()))
                .single_line(),
        )
        .children(what.map(|what| {
            div()
                .min_w_0()
                .child(studio_ui::ds::text::muted(what).single_line().truncate())
        }))
        .child(div().flex_1())
        .child(
            div()
                .id(("history-when", ix))
                .tip(studio_ui::ds::tooltip(utc_date_time(entry.at)))
                .child(studio_ui::ds::text::meta(relative_time(
                    entry.at,
                    now_micros(),
                ))),
        )
        .child(
            div()
                .id(("history-commit", ix))
                .cursor_pointer()
                .tip(studio_ui::ds::tooltip("Open the commit"))
                .on_click(open_commit)
                .child(studio_ui::ds::text::code(
                    short_commit_id(&entry.commit),
                    cx,
                )),
        );
    let rows: Vec<AnyElement> = entry
        .node
        .as_ref()
        .map(|node| change_cards::rows_of(node, schema))
        .into_iter()
        .flatten()
        .map(|row| {
            studio_ui::ds::dotted(div().border_t_1(), cx)
                .px(px(space::LG))
                .py(px(space::SM))
                .child(studio_ui::property_row(&row, kind, None, cx))
                .into_any_element()
        })
        .collect();
    let lines: Vec<AnyElement> = entry
        .edges
        .iter()
        .enumerate()
        .map(|(line, edge)| relationship_line((ix, line), edge, node_id, schema).into_any_element())
        .collect();
    let relationships = (!lines.is_empty()).then(|| {
        studio_ui::ds::dotted(v_flex().border_t_1(), cx)
            .px(px(space::LG))
            .py(px(space::SM))
            .gap(px(space::WITHIN))
            .children(lines)
    });
    div()
        .px(px(space::XL))
        .pb(px(space::LG))
        .child(
            v_flex()
                .w_full()
                .border_1()
                .border_color(studio_ui::ds::surface::outline(cx))
                .rounded(px(radii(cx).md))
                .child(head)
                .children(rows)
                .children(relationships),
        )
        .into_any_element()
}

/// A relationship of the node a commit changed, read from the node: "+ blocked by → lk-share".
fn relationship_line(
    id: (usize, usize),
    edge: &FoldedChange,
    node_id: &str,
    schema: Option<&SchemaModel>,
) -> RelationshipLine {
    let (from, to) = edge.endpoints().unwrap_or_default();
    let outgoing = from == node_id;
    let other = if outgoing { to } else { from };
    let words = match schema.and_then(|schema| schema.edge(&edge.type_name)) {
        Some(edge_type) => relationship_words(&edge_type.name, &edge_type.from, &edge_type.to),
        None => edge.type_name.clone(),
    };
    let id = SharedString::from(format!("history-line-{}-{}", id.0, id.1));
    RelationshipLine::new(
        id,
        Some(change_cards::kind_of_change(edge)),
        words,
        outgoing,
        other,
    )
    .rows(change_cards::rows_of(edge, schema).into())
}

impl Render for NodeHistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.read_owed) {
            cx.defer_in(window, |this, _, cx| this.reload(cx));
        }
        let summary = match self.state {
            State::Ready => format!(
                "{} on {} in the newest {}",
                studio_ui::plural(self.entries.len(), "change", "changes"),
                self.branch,
                studio_ui::plural(self.commits, "commit", "commits"),
            ),
            State::Reading => "Reading…".to_owned(),
            State::Failed(_) => String::new(),
        };
        let header = studio_ui::ds::toolbar(cx)
            .children(studio_workspace::view_strip(Item::views(self, cx), cx))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .child(studio_ui::ds::text::muted(summary).single_line().truncate()),
            )
            .when(self.newer, |header| {
                header.child(
                    studio_ui::ds::button("node-history-newer", "Newer commits · Refresh")
                        .style(ButtonStyle::Subtle)
                        .tip(studio_ui::ds::tooltip(format!(
                            "{} changed after this history was read",
                            self.branch
                        )))
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                )
            })
            .child(div().flex_1())
            .child(
                studio_ui::ds::icon_button("refresh-node-history", IconName::RotateCw)
                    .icon_color(Color::Muted)
                    .tip_fn(|_, cx| {
                        studio_ui::ds::tooltip_for_action_view(
                            "Read the history again",
                            &Refresh,
                            cx,
                        )
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            );
        let body = match &self.state {
            State::Failed(error) => {
                studio_ui::ds::error_state("The node's history could not be read", error.clone())
                    .child(
                        studio_ui::ds::button("read-node-history-again", "Try again")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .into_any_element()
            }
            State::Reading => change_cards::reading_state(None).into_any_element(),
            State::Ready if self.entries.is_empty() && !self.offers_older() => {
                change_cards::no_changes_state(format!(
                    "No commit on {} changed {} or its relationships since the graph began.",
                    self.branch, self.id
                ))
                .into_any_element()
            }
            State::Ready => AnyView::from(self.list_view.clone())
                .cached(StyleRefinement::default().size_full())
                .into_any_element(),
        };
        v_flex()
            .key_context("NodeHistory")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(studio_ui::ds::surface::document(cx))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.reload(cx)))
            .when(self.offers_older(), |view| {
                view.on_action(cx.listener(|this, _: &ReadOlderCommits, _, cx| this.read_older(cx)))
            })
            .child(header)
            .child(body)
    }
}

/// The entries as a view of their own, drawn cached and again when the history view notifies
/// (it observes it), as History's rows are (`docs/drawing.md`).
pub(crate) struct NodeHistoryRows {
    history: WeakEntity<NodeHistoryView>,
    _changed: Subscription,
    /// How often the list was drawn, for the test that guards it.
    #[cfg(test)]
    pub(crate) renders: usize,
}

impl NodeHistoryRows {
    /// Made with the view, not on its first draw.
    fn new(cx: &mut Context<NodeHistoryView>) -> Entity<Self> {
        let history = cx.entity();
        cx.new(|cx| NodeHistoryRows {
            history: history.downgrade(),
            _changed: cx.observe(&history, |_, _, cx| cx.notify()),
            #[cfg(test)]
            renders: 0,
        })
    }
}

impl Render for NodeHistoryRows {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let list = self
            .history
            .update(cx, |history, cx| history.render_list(cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element());
        v_flex().size_full().child(list)
    }
}

/// The view's list, for a test.
#[cfg(test)]
impl NodeHistoryView {
    pub(crate) fn rows_view(&self) -> &Entity<NodeHistoryRows> {
        &self.list_view
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::{TestAppContext, VisualTestContext};
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::{Value, json};

    use super::*;

    const SCHEMA: &str = "node Issue {\n    slug: String @key\n    title: String?\n    \
        status: String?\n}\n\
        node Comment {\n    slug: String @key\n}\n\
        edge BlockedBy: Issue -> Issue\n\
        edge CommentOn: Comment -> Issue\n";

    /// Commits `c0` (the graph's first) to `c249` on main, newest first.
    const COMMITS: usize = 250;

    fn id(n: usize) -> String {
        format!("c{n}")
    }

    /// A server whose main holds [`COMMITS`] commits: `c5` (by act-writer) closed lk-acl,
    /// `c220` (by act-admin) set its title and linked a comment to it, and `c230` changed
    /// another issue. It pages the listing as Omnigraph does and answers the change feed from
    /// `after:<commit>` to the head, noting every request.
    fn serve(cx: &mut TestAppContext) -> (Entity<GraphSession>, Arc<Mutex<Vec<String>>>) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
        });
        let asked = Arc::new(Mutex::new(Vec::new()));
        let noted = asked.clone();
        let commit = |n: usize| {
            json!({
                "graph_commit_id": id(n), "graph_manifest_version": n, "kind": "published",
                "created_at": n as i64 * 1_000_000,
                "parent_commit_id": n.checked_sub(1).map(id), "actor_id": "act-writer",
            })
        };
        let block = |n: usize| -> Value {
            let issue = |slug: &str, before: Value, after: Value| {
                json!({ "kind": "node", "type": { "id": "t1", "name": "Issue" }, "id": slug,
                    "op": "update", "before": { "properties": before },
                    "after": { "properties": after } })
            };
            let (actor, changes) = match n {
                5 => (
                    "act-writer",
                    json!([issue(
                        "lk-acl",
                        json!({ "slug": "lk-acl", "status": "open" }),
                        json!({ "slug": "lk-acl", "status": "closed" })
                    )]),
                ),
                220 => (
                    "act-admin",
                    json!([
                        issue(
                            "lk-acl",
                            json!({ "slug": "lk-acl", "title": "ACLs" }),
                            json!({ "slug": "lk-acl", "title": "Per-notebook ACLs" })
                        ),
                        { "kind": "edge", "type": { "id": "t3", "name": "CommentOn" },
                          "id": "e1", "op": "insert",
                          "after": { "properties": {},
                                     "endpoints": { "from": "c-1", "to": "lk-acl" } } }
                    ]),
                ),
                230 => (
                    "act-admin",
                    json!([issue(
                        "lk-share",
                        json!({ "slug": "lk-share", "status": "open" }),
                        json!({ "slug": "lk-share", "status": "closed" })
                    )]),
                ),
                _ => ("act-writer", json!([])),
            };
            json!({ "cause": { "graph_commit_id": id(n), "authored_branch": "main",
                "authored_at": n as i64 * 1_000_000, "actor_id": actor,
                "parent_commit_id": n.checked_sub(1).map(id) }, "changes": changes })
        };
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            noted.lock().unwrap().push(format!("{path}?{query}"));
            let param = |name: &str| {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                    .map(|value| value.replace("%3A", ":"))
            };
            let body = if path.ends_with("/schema") {
                json!({ "schema_source": SCHEMA })
            } else if path.ends_with("/commits") {
                let limit: usize = param("limit").and_then(|l| l.parse().ok()).unwrap_or(100);
                let offset: usize = param("page_token")
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0);
                let newest = COMMITS - 1 - offset;
                let page: Vec<Value> = (0..=newest).rev().take(limit).map(commit).collect();
                let next = (offset + limit < COMMITS).then(|| (offset + limit).to_string());
                json!({ "commits": page, "head_commit_id": id(COMMITS - 1),
                    "next_page_token": next })
            } else if path.ends_with("/changes") {
                let from = match param("start").as_deref() {
                    Some("beginning") => 0,
                    Some(start) => {
                        start
                            .trim_start_matches("after:c")
                            .parse::<usize>()
                            .unwrap()
                            + 1
                    }
                    None => 0,
                };
                let blocks: Vec<Value> = (from..COMMITS).map(block).collect();
                json!({ "blocks": blocks, "cursor": "end", "caught_up": true })
            } else {
                json!({})
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
        session.update(cx, |session, cx| session.reload_schema(cx));
        cx.run_until_parked();
        (session, asked)
    }

    fn open<'a>(
        session: &Entity<GraphSession>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<NodeHistoryView>, &'a mut VisualTestContext) {
        let session = session.clone();
        let (view, cx) = cx.add_window_view(move |_, cx| {
            NodeHistoryView::new(session, "Issue".into(), "lk-acl".into(), None, cx)
        });
        cx.run_until_parked();
        (view, cx)
    }

    /// A node's history reads the branch's newest 200 commits in two requests (the listing, then
    /// the feed from just before its oldest, filtered to the node's types), keeps the commits
    /// that changed the node or a relationship ending at it, newest first, and reads the commits
    /// before them only when asked, stopping where the first window began.
    #[gpui::test]
    async fn a_nodes_history_reads_a_window_of_commits_and_the_one_before(cx: &mut TestAppContext) {
        let (session, asked) = serve(cx);
        asked.lock().unwrap().clear();
        let (view, cx) = open(&session, cx);
        let read = |cx: &mut VisualTestContext| view.read_with(cx, |view, _| view.entries());
        assert_eq!(
            read(cx),
            [("c220".to_owned(), Some("act-admin".to_owned()), true, 1)],
            "c230 changed another issue; c5 is older than the window"
        );
        {
            let asked = asked.lock().unwrap();
            assert_eq!(asked.len(), 2, "{asked:?}");
            assert!(
                asked[0].contains("/commits?branch=main&limit=200"),
                "{asked:?}"
            );
            assert!(
                asked[1].contains("start=after%3Ac49") || asked[1].contains("start=after:c49"),
                "the feed starts just before the window's oldest commit: {asked:?}"
            );
            assert!(
                asked[1].contains("type=Issue")
                    && asked[1].contains("type=BlockedBy")
                    && asked[1].contains("type=CommentOn"),
                "{asked:?}"
            );
        }
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits, 200);
            assert_eq!(view.label.as_deref(), Some("Per-notebook ACLs"));
            assert!(view.offers_older());
        });
        asked.lock().unwrap().clear();
        view.update(cx, |view, cx| view.read_older(cx));
        cx.run_until_parked();
        assert_eq!(
            read(cx),
            [
                ("c220".to_owned(), Some("act-admin".to_owned()), true, 1),
                ("c5".to_owned(), Some("act-writer".to_owned()), true, 0),
            ]
        );
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.commits, COMMITS,
                "the older window stops where the first began"
            );
            assert!(!view.offers_older(), "it reached the graph's first commit");
        });
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(asked[0].contains("page_token=200"), "{asked:?}");
        assert!(asked[1].contains("start=beginning"), "{asked:?}");
    }

    /// The entries are a cached view of their own: a commit on another branch draws nothing, a
    /// commit on the branch read draws the list once, for the header's Refresh, and the older
    /// commits landing draw it once more.
    #[gpui::test]
    async fn the_history_draws_its_list_only_for_what_it_shows(cx: &mut TestAppContext) {
        let (session, _) = serve(cx);
        let (view, cx) = open(&session, cx);
        let renders = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, cx| view.rows_view().read(cx).renders)
        };
        assert!(renders(cx) > 0, "the list is drawn");
        let before = renders(cx);
        session.update(cx, |_, cx| {
            cx.emit(SessionEvent::Committed {
                branch: "agent/x".into(),
            })
        });
        cx.run_until_parked();
        assert_eq!(renders(cx), before, "another branch's commit draws nothing");
        session.update(cx, |_, cx| {
            cx.emit(SessionEvent::Committed {
                branch: "main".into(),
            })
        });
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.newer),
            "Refresh is offered"
        );
        assert_eq!(renders(cx), before + 1);
        let before = renders(cx);
        view.update(cx, |view, cx| view.read_older(cx));
        cx.run_until_parked();
        assert_eq!(renders(cx), before + 2, "one for reading, one for landing");
    }
}
