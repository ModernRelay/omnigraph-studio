//! A node's history (spec 046): the node's fourth view, beside its page, tree and canvas. It
//! opens on "Now", who last set each property within what was read, then the changes, newest
//! first: who made each, on which branch or through which merge, when and in which commit, its
//! properties before and after and its relationship lines at either end. A run of one actor's
//! small edits folds into one entry with its net change.
//!
//! Omnigraph has no read of one entity's commits (E62) and keeps no operation or message with a
//! commit (E60): a window covers the branch's newest [`NODE_HISTORY_COMMITS`] commits, read from
//! the change feed filtered to the node's types ([`GraphSession::node_history_in`]), with an
//! honest exit to the commits before them. It reads only once its tab is shown.
//!
//! Forked in part from the closed #146 (spec 040, `feat/ux-p5-node-history`).

use std::collections::HashSet;
use std::ops::Range;

use gpui::{
    AnyElement, AnyView, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ListAlignment, ListState, Render, SharedString, StyleRefinement, Subscription, Task,
    WeakEntity, Window, list,
};
use gq_language::{ScalarKind, SchemaModel};
use graph_store::ledger::ReadView;
use graph_store::node_history::{
    Blame, BlameSet, InsertsSkipped, NODE_HISTORY_COMMITS, NODE_HISTORY_INSERT_CAP,
    NodeHistoryEntry, NodeHistoryPage, NodeHistoryWindow, NodeScope, OlderWindow, SchemaBoundary,
    WindowCommit, blame, net_of, newest_image, runs,
};
use graph_store::scope::ReadScope;
use graph_store::{FoldedChange, GraphSession, SessionEvent, entity_name, relationship_words};
use serde_json::Value;
use studio_actions::changes::ToggleChange;
use studio_actions::graph::{OpenCommit, OpenNodePage, ShowNodeInTree, ShowNodeOnCanvas};
use studio_actions::history::ReadOlderCommits;
use studio_actions::workspace::Refresh;
use studio_actions::{ViewKind, ViewLink};
use studio_ui::ds::Tipped as _;
use studio_ui::ds::{radii, size, space};
use studio_ui::{
    ChangeKind, RelationshipLine, now_micros, relative_time, short_commit_id, utc_date_time,
};
use studio_workspace::{Inspected, InspectedNode, Item, ItemEvent, SavedItem};
use ui::KeyBinding;
use ui::prelude::*;

/// What the view has read.
enum State {
    /// Nothing read yet, or read again from the start.
    Reading,
    Ready,
    /// The first window could not be read: the reason.
    Failed(SharedString),
}

/// How the read of older commits stands.
#[derive(Clone)]
enum Older {
    /// The windows read reached the graph's first commit, a schema change or cleaned-up
    /// history.
    None,
    /// Commits before those read wait.
    Waits(OlderWindow),
    Reading(OlderWindow),
    Failed(OlderWindow, SharedString),
}

/// One row of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// Who set each property ("Now").
    Now,
    /// A folded run of entries, by its index in the runs.
    Run(usize),
    /// One entry; `run` is the run it belongs to, `first` when it heads an unfolded run of more
    /// than one.
    Entry {
        entry: usize,
        run: usize,
        first: bool,
    },
    /// How far the history was read, and the commits before it.
    End,
}

impl Row {
    fn selectable(self) -> bool {
        matches!(self, Row::Run(_) | Row::Entry { .. })
    }
}

pub struct NodeHistoryView {
    session: Entity<GraphSession>,
    type_name: String,
    id: String,
    /// The branch read: the window's, followed when it changes.
    branch: String,
    /// The node's name, from its newest image read, for the tab.
    label: Option<String>,
    state: State,
    /// The head the first window was read at: every window answers for it (constitution II).
    head: Option<String>,
    /// The scope the reads are asked through, so the ledger names this view (spec 030).
    scope: ReadScope,
    /// Every window's entries, newest first.
    entries: Vec<NodeHistoryEntry>,
    /// Every window's commits, newest first.
    commits: Vec<WindowCommit>,
    /// Each window whose insert pass stopped at its cap.
    skipped: Vec<InsertsSkipped>,
    boundary: Option<SchemaBoundary>,
    cleaned_up: bool,
    older: Older,
    /// Why a window was not read to its end, when one was not.
    stopped: Option<SharedString>,
    /// Requests sent and change-feed bytes read for the windows shown.
    requests: usize,
    feed_bytes: usize,
    /// The branch moved past the head read: Refresh reads it again.
    newer: bool,
    /// The runs of `entries`, and those unfolded, by their newest commit.
    runs: Vec<Range<usize>>,
    unfolded: HashSet<String>,
    rows: Vec<Row>,
    /// The row selected, by its index in `rows`.
    selected: Option<usize>,
    list_state: ListState,
    focus_handle: FocusHandle,
    /// The history is read when the tab is next drawn: a tab restored behind another reads
    /// nothing until it is shown.
    read_owed: bool,
    _task: Option<Task<()>>,
    _session_events: Subscription,
    /// The rows, a view of their own drawn cached (`docs/drawing.md`).
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

    fn settling(&self, _cx: &App) -> bool {
        matches!(self.state, State::Reading) || matches!(self.older, Older::Reading(_))
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
        let scope = session.update(cx, |session, _| {
            session.read_scope(&branch, ReadView::NodeHistory)
        });
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
            // A new token may read less: what the old one read goes, and the history is read
            // again with the new one, as a node's page does.
            SessionEvent::CredentialChanged => this.reload(cx),
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
            scope,
            entries: Vec::new(),
            commits: Vec::new(),
            skipped: Vec::new(),
            boundary: None,
            cleaned_up: false,
            older: Older::None,
            stopped: None,
            requests: 0,
            feed_bytes: 0,
            newer: false,
            runs: Vec::new(),
            unfolded: HashSet::new(),
            rows: Vec::new(),
            selected: None,
            list_state: ListState::new(0, ListAlignment::Top, px(1000.)),
            focus_handle: cx.focus_handle(),
            read_owed: true,
            _task: None,
            _session_events: subscription,
            list_view,
        }
    }

    /// The history read again from the branch head. Only the person asks for it: Refresh, Try
    /// again, a branch switch, a new token or schema; nothing is asked again by itself.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        // A schema that could not be read is read again first; its arrival reads the history.
        let schema_failed = matches!(
            self.session.read(cx).schema,
            graph_store::Loadable::Failed(_)
        );
        if schema_failed && matches!(self.state, State::Failed(_)) {
            self.session
                .update(cx, |session, cx| session.reload_schema(cx));
        }
        self.state = State::Reading;
        self.entries.clear();
        self.commits.clear();
        self.skipped.clear();
        self.boundary = None;
        self.cleaned_up = false;
        self.older = Older::None;
        self.stopped = None;
        self.requests = 0;
        self.feed_bytes = 0;
        self.head = None;
        self.selected = None;
        self.rebuild_rows(cx);
        if self.newer {
            self.newer = false;
            cx.emit(ItemEvent::UpdateTab);
        }
        self.read(NodeHistoryWindow::Newest, cx);
        cx.notify();
    }

    fn scope_of(&self, cx: &App) -> Option<NodeScope> {
        let schema = self.session.read(cx).schema_model()?;
        Some(NodeScope::new(&schema, &self.type_name, &self.id))
    }

    /// Reads `window`, and takes it below what was read.
    fn read(&mut self, window: NodeHistoryWindow, cx: &mut Context<Self>) {
        let Some(node) = self.scope_of(cx) else {
            // The schema is not read yet: its arrival reads the history (SchemaChanged). One that
            // could not be read says so, and Try again reads it again.
            if let graph_store::Loadable::Failed(error) = &self.session.read(cx).schema {
                self.state =
                    State::Failed(format!("The graph's schema could not be read: {error}").into());
            }
            return;
        };
        let older = matches!(window, NodeHistoryWindow::Older { .. });
        let (branch, scope) = (self.branch.clone(), self.scope.clone());
        let read = self.session.update(cx, |session, cx| {
            session.node_history_in(&scope, &branch, node, window, cx)
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
            inserts_skipped,
            boundary,
            cleaned_up,
            requests,
            feed_bytes,
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
        self.commits.extend(commits);
        self.skipped.extend(inserts_skipped);
        if boundary.is_some() {
            self.boundary = boundary;
        }
        self.cleaned_up |= cleaned_up;
        self.requests += requests;
        self.feed_bytes += feed_bytes;
        self.stopped = stopped.map(Into::into);
        self.older = match older {
            Some(window) => Older::Waits(window),
            None => Older::None,
        };
        self.state = State::Ready;
        self.rebuild_rows(cx);
        if self.selected.is_none() {
            self.selected = self.rows.iter().position(|row| row.selectable());
        }
    }

    /// Lays the rows out again from the entries and what is unfolded.
    fn rebuild_rows(&mut self, cx: &App) {
        // Studio's own writes this session fold apart from another writer's.
        let session = self.session.read(cx);
        let studio: Vec<bool> = self
            .entries
            .iter()
            .map(|e| session.own_words(&e.commit).is_some())
            .collect();
        self.runs = runs(&self.entries, &studio);
        let mut rows = vec![Row::Now];
        for (ix, run) in self.runs.iter().enumerate() {
            let folded = run.len() > 1 && !self.unfolded.contains(&self.entries[run.start].commit);
            if folded {
                rows.push(Row::Run(ix));
            } else {
                rows.extend(run.clone().map(|entry| Row::Entry {
                    entry,
                    run: ix,
                    first: run.len() > 1 && entry == run.start,
                }));
            }
        }
        rows.push(Row::End);
        self.rows = rows;
        if self.list_state.item_count() != self.rows.len() {
            self.list_state.reset(self.rows.len());
        }
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

    fn select(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.selected == Some(row) {
            return;
        }
        self.selected = Some(row);
        self.list_state.scroll_to_reveal_item(row);
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let from = self.selected.map_or(0, |s| s + 1);
        if let Some(next) = (from..self.rows.len()).find(|&ix| self.rows[ix].selectable()) {
            self.select(next, cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.selected else {
            return;
        };
        if let Some(previous) = (0..from).rev().find(|&ix| self.rows[ix].selectable()) {
            self.select(previous, cx);
        }
    }

    /// The commit a row opens: an entry's, or a folded run's newest.
    fn commit_of(&self, row: Row) -> Option<&str> {
        match row {
            Row::Run(run) => Some(self.entries[self.runs[run].start].commit.as_str()),
            Row::Entry { entry, .. } => Some(self.entries[entry].commit.as_str()),
            Row::Now | Row::End => None,
        }
    }

    /// ↵ opens the selected entry's commit in History.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self
            .selected
            .and_then(|ix| self.rows.get(ix).copied())
            .and_then(|row| self.commit_of(row))
        else {
            return;
        };
        window.dispatch_action(
            Box::new(OpenCommit {
                commit_id: commit.to_owned(),
                preview: true,
            }),
            cx,
        );
    }

    /// Space folds or unfolds the selected entry's run.
    fn toggle(&mut self, _: &ToggleChange, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected.and_then(|ix| self.rows.get(ix).copied()) else {
            return;
        };
        let run = match row {
            Row::Run(run) => run,
            Row::Entry { run, .. } if self.runs[run].len() > 1 => run,
            _ => return,
        };
        self.toggle_run(run, cx);
    }

    fn toggle_run(&mut self, run: usize, cx: &mut Context<Self>) {
        let key = self.entries[self.runs[run].start].commit.clone();
        if !self.unfolded.remove(&key) {
            self.unfolded.insert(key);
        }
        self.rebuild_rows(cx);
        // The selection stays on the run: its row when folded, its first entry when not.
        self.selected = self.rows.iter().position(|row| match *row {
            Row::Run(r) => r == run,
            Row::Entry { run: r, first, .. } => r == run && (first || self.runs[run].len() == 1),
            _ => false,
        });
        if let Some(selected) = self.selected {
            self.list_state.scroll_to_reveal_item(selected);
        }
        cx.notify();
    }

    /// The oldest commit any property was read back to: past it a value was set "before".
    fn blame_bound(&self) -> Option<WindowCommit> {
        // A value no commit read set was set at or before the oldest commit read, or, where the
        // inserts of newer commits were not read, at or before the newest of those.
        let skipped = self.skipped.iter().map(|s| &s.newest).max_by_key(|c| c.at);
        skipped.or(self.commits.last()).cloned()
    }

    fn blame_rows(&self, schema: Option<&SchemaModel>) -> Vec<Blame> {
        let properties: Vec<String> = schema
            .and_then(|schema| schema.node(&self.type_name))
            .map(|node| node.properties.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default();
        blame(&self.entries, &properties, self.blame_bound().as_ref())
    }

    /// The node's history, as a test reads it: each row's commits, actor, whether it changed
    /// the node itself, and how many relationships.
    #[cfg(test)]
    pub(crate) fn shown(&self) -> Vec<(Vec<String>, Option<String>, bool, usize)> {
        self.rows
            .iter()
            .filter_map(|row| {
                let range = match *row {
                    Row::Run(run) => self.runs[run].clone(),
                    Row::Entry { entry, .. } => entry..entry + 1,
                    _ => return None,
                };
                let run = &self.entries[range];
                let (node, edges) = net_of(run);
                Some((
                    run.iter().map(|e| e.commit.clone()).collect(),
                    run[0].actor.clone(),
                    node.is_some(),
                    edges.len(),
                ))
            })
            .collect()
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.list_state.item_count() != self.rows.len() {
            self.list_state.reset(self.rows.len());
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
        let Some(row) = self.rows.get(ix).copied() else {
            return gpui::Empty.into_any_element();
        };
        let selected = self.selected == Some(ix);
        let schema = self.session.read(cx).schema_model();
        let body = match row {
            Row::Now => self.render_now(schema.as_deref()),
            Row::End => self.render_end(cx),
            Row::Run(run) => {
                let range = self.runs[run].clone();
                let (node, edges) = net_of(&self.entries[range.clone()]);
                let card = Card {
                    ix,
                    entries: &self.entries[range],
                    node: node.as_ref(),
                    edges: &edges,
                    fold: Some((run, true)),
                    selected,
                };
                self.render_card(card, schema.as_deref(), cx)
            }
            Row::Entry { entry, run, first } => {
                let e = &self.entries[entry];
                let card = Card {
                    ix,
                    entries: std::slice::from_ref(e),
                    node: e.node.as_ref(),
                    edges: &e.edges,
                    fold: first.then_some((run, false)),
                    selected,
                };
                self.render_card(card, schema.as_deref(), cx)
            }
        };
        // One column at the reading width, centred, as a node's page reads (spec 014).
        h_flex()
            .w_full()
            .justify_center()
            .px(px(space::PAGE))
            .child(div().w_full().max_w(px(size::READING_WIDTH)).child(body))
            .into_any_element()
    }

    /// "Now": each property, its value, and who set it, when and in which commit, within what
    /// was read; "before" a time where the read did not reach.
    fn render_now(&self, schema: Option<&SchemaModel>) -> AnyElement {
        let title = change_cards::section_title("Now");
        let image = newest_image(&self.entries);
        if let Some((_, true)) = image {
            let removal = self
                .entries
                .iter()
                .find(|e| e.node.is_some())
                .expect("the image's entry");
            let who = removal.actor.clone().unwrap_or_else(|| "no actor".into());
            return v_flex()
                .child(title)
                .child(
                    div()
                        .px(px(space::EDGE))
                        .child(studio_ui::ds::text::body(format!(
                            "Removed by {who} in {}, {}. Its relationships went with it.",
                            short_commit_id(&removal.commit),
                            relative_time(removal.at, now_micros()),
                        ))),
                )
                .child(div().h(px(space::LG)))
                .child(change_cards::section_title("Changes"))
                .into_any_element();
        }
        let blame = self.blame_rows(schema);
        let blobs: Vec<&str> = schema
            .and_then(|schema| schema.node(&self.type_name))
            .map(|node| {
                node.properties
                    .iter()
                    .filter(|p| p.kind == ScalarKind::Blob)
                    .map(|p| p.name.as_str())
                    .collect()
            })
            .unwrap_or_default();
        let rows: Vec<AnyElement> = blame
            .iter()
            .enumerate()
            .map(|(n, row)| {
                let value: SharedString = match &row.value {
                    None => "—".into(),
                    Some(Value::Null) => "—".into(),
                    Some(value) if blobs.contains(&row.property.as_str()) => {
                        change_cards::value_words(value, true).into()
                    }
                    Some(value) => studio_ui::format_value(value).into(),
                };
                let (who, tip): (SharedString, SharedString) = match &row.set {
                    BlameSet::In { entry, created } => {
                        let e = &self.entries[*entry];
                        let actor = e.actor.clone().unwrap_or_else(|| "no actor".into());
                        let how = match (created, &e.merged_branch, &e.merged_parent) {
                            (true, ..) => " · created".to_owned(),
                            (false, Some(branch), _) => format!(" · via merge of {branch}"),
                            (false, None, Some(_)) => " · via merge".to_owned(),
                            _ => String::new(),
                        };
                        (
                            format!(
                                "{actor}{how} · {} · {}",
                                relative_time(e.at, now_micros()),
                                short_commit_id(&e.commit)
                            )
                            .into(),
                            format!("Set in commit {} · {}", e.commit, utc_date_time(e.at)).into(),
                        )
                    }
                    BlameSet::Before(commit) => (
                        format!("before {}", short_time(commit.at)).into(),
                        format!(
                            "No commit read set it: it was set at or before commit {}. Read \
                             older commits to find it.",
                            commit.id
                        )
                        .into(),
                    ),
                };
                let muted = row.value.is_none();
                studio_ui::ds::document_row(row.property.clone(), size::DOCUMENT_LABEL)
                    .child(
                        h_flex()
                            .h(px(size::PROPERTY_ROW))
                            .items_center()
                            .flex_1()
                            .min_w_0()
                            .child(if muted {
                                studio_ui::ds::text::muted(value).truncate()
                            } else {
                                studio_ui::ds::text::body(value).truncate()
                            }),
                    )
                    .child(
                        h_flex()
                            .id(("now-set", n))
                            .h(px(size::PROPERTY_ROW))
                            .items_center()
                            .flex_none()
                            .tip(studio_ui::ds::tooltip(tip))
                            .child(studio_ui::ds::text::meta(who).single_line()),
                    )
                    .into_any_element()
            })
            .collect();
        let unread = image.is_none() && !blame.is_empty();
        v_flex()
            .child(title)
            .child(
                v_flex()
                    .px(px(space::EDGE))
                    .gap(px(space::WITHIN))
                    .children(rows)
                    .when(unread, |rows| {
                        rows.child(studio_ui::ds::text::meta(
                            "No commit read changed the node itself, so its values are on its \
                             page.",
                        ))
                    }),
            )
            .child(div().h(px(space::LG)))
            .child(change_cards::section_title("Changes"))
            .when(self.entries.is_empty(), |now| {
                now.child(
                    div()
                        .px(px(space::EDGE))
                        .child(studio_ui::ds::text::muted(format!(
                            "No changes in the last {}.",
                            studio_ui::plural(self.commits.len(), "commit", "commits")
                        ))),
                )
            })
            .into_any_element()
    }

    /// One entry's card, or a folded run's: who made it, how and where, when, its commit; then
    /// what it did to the node and its relationships, as a review's card says it.
    fn render_card(
        &self,
        card: Card<'_>,
        schema: Option<&SchemaModel>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Card {
            ix,
            entries,
            node,
            edges,
            fold,
            selected,
        } = card;
        let newest = &entries[0];
        let oldest = &entries[entries.len() - 1];
        // The node's own change, or for an entry that changed only relationships, "·".
        let kind = match node {
            Some(node) => change_cards::kind_of_change(node),
            None => ChangeKind::Unchanged,
        };
        let session = self.session.read(cx);
        let words = (entries.len() == 1)
            .then(|| session.own_words(&newest.commit).map(str::to_owned))
            .flatten();
        let studio = session.own_words(&newest.commit).is_some();
        let mut facts: Vec<String> = Vec::new();
        if studio {
            facts.push("Studio".into());
        } else if newest.by_agent() {
            facts.push("agent".into());
        }
        if newest.created() {
            facts.push("created".into());
        } else if newest.removed() {
            facts.push("removed".into());
        }
        match (&newest.merged_branch, &newest.merged_parent) {
            (Some(branch), _) => facts.push(format!("via merge of {branch}")),
            (None, Some(merged)) => {
                facts.push(format!("via merge of commit {}", short_commit_id(merged)))
            }
            (None, None) => facts.push(format!("on {}", newest.branch)),
        }
        let when = if entries.len() > 1 {
            format!(
                "{} · {}–{}, {}",
                studio_ui::plural(entries.len(), "change", "changes"),
                clock(oldest.at),
                clock(newest.at),
                relative_time(newest.at, now_micros())
            )
        } else {
            relative_time(newest.at, now_micros())
        };
        let when_tip = if entries.len() > 1 {
            format!(
                "{} to {}",
                utc_date_time(oldest.at),
                utc_date_time(newest.at)
            )
        } else {
            utc_date_time(newest.at)
        };
        let commit = newest.commit.clone();
        let open_commit = move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
            window.dispatch_action(
                Box::new(OpenCommit {
                    commit_id: commit.clone(),
                    preview: event.click_count() < 2,
                }),
                cx,
            )
        };
        let focus = self.focus_handle.clone();
        let fold_button = fold.map(|(run, folded)| {
            let label = if folded {
                format!("{} commits", entries.len())
            } else {
                "Fold".to_owned()
            };
            let tip = if folded {
                "Show each commit of this run"
            } else {
                "Fold this run into one entry"
            };
            let focus = focus.clone();
            studio_ui::ds::row_button(("history-fold", ix), label)
                .style(ButtonStyle::Subtle)
                .end_icon(Icon::new(if folded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronUp
                }))
                .tip_fn(move |_, cx| {
                    studio_ui::ds::tooltip_for_action_in_view(tip, &ToggleChange, &focus, cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_run(run, cx)))
                .into_any_element()
        });
        let row_ix = ix;
        let head = h_flex()
            .id(("history-entry", ix))
            .min_h(px(size::HEADER))
            .px(px(space::LG))
            .gap(px(space::MD))
            .items_center()
            .rounded_t(px(radii(cx).md))
            .when(selected, |head| {
                head.bg(studio_ui::ds::surface::selected_document(cx))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select(row_ix, cx);
                window.focus(&this.focus_handle, cx);
            }))
            .child(change_cards::kind_mark(("history-kind", ix), kind))
            .child(
                studio_ui::ds::text::body(
                    newest.actor.clone().unwrap_or_else(|| "no actor".into()),
                )
                .single_line(),
            )
            .child(
                div().min_w_0().child(
                    studio_ui::ds::text::muted(facts.join(" · "))
                        .single_line()
                        .truncate(),
                ),
            )
            .children(words.map(|words| {
                div()
                    .min_w_0()
                    .child(studio_ui::ds::text::muted(words).single_line().truncate())
            }))
            .child(div().flex_1())
            .children(fold_button)
            .child(
                div()
                    .id(("history-when", ix))
                    .flex_none()
                    .tip(studio_ui::ds::tooltip(when_tip))
                    .child(studio_ui::ds::text::meta(when).single_line()),
            )
            .child(
                div()
                    .id(("history-commit", ix))
                    .flex_none()
                    .cursor_pointer()
                    .tip_fn({
                        let focus = focus.clone();
                        move |_, cx| {
                            studio_ui::ds::tooltip_for_action_in_view(
                                "Open the commit",
                                &menu::Confirm,
                                &focus,
                                cx,
                            )
                        }
                    })
                    .on_click(open_commit)
                    .child(studio_ui::ds::text::code(
                        short_commit_id(&newest.commit),
                        cx,
                    )),
            );
        let rows: Vec<AnyElement> = node
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
        let lines: Vec<AnyElement> = edges
            .iter()
            .enumerate()
            .map(|(line, edge)| {
                relationship_line((ix, line), edge, &self.id, schema).into_any_element()
            })
            .collect();
        let relationships = (!lines.is_empty()).then(|| {
            studio_ui::ds::dotted(v_flex().border_t_1(), cx)
                .px(px(space::LG))
                .py(px(space::SM))
                .gap(px(space::WITHIN))
                .children(lines)
        });
        let went_with = (newest.removed() && edges.is_empty()).then(|| {
            studio_ui::ds::dotted(div().border_t_1(), cx)
                .px(px(space::LG))
                .py(px(space::SM))
                .child(studio_ui::ds::text::meta(
                    "Its relationships went with it; none were read in this window.",
                ))
        });
        div()
            .pb(px(space::LG))
            .child(
                v_flex()
                    .w_full()
                    .border_1()
                    .border_color(studio_ui::ds::surface::outline(cx))
                    .rounded(px(radii(cx).md))
                    .child(head)
                    .children(rows)
                    .children(relationships)
                    .children(went_with),
            )
            .into_any_element()
    }

    /// The list's last row: how far back the history was read, what a window skipped, and the
    /// commits before them.
    fn render_end(&self, cx: &mut Context<Self>) -> AnyElement {
        let back = match self.commits.last() {
            Some(oldest) => format!(
                "Read back to {}, {}",
                short_commit_id(&oldest.id),
                relative_time(oldest.at, now_micros())
            ),
            None => "Nothing read".to_owned(),
        };
        let read = studio_ui::plural(self.commits.len(), "commit", "commits");
        let (said, button): (String, Option<AnyElement>) = match &self.older {
            Older::None if self.boundary.is_some() || self.cleaned_up => (format!("{back}."), None),
            Older::None => (
                format!("{back}, the graph's first commit. {read} read."),
                None,
            ),
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
                    Some(error) => format!("{back}. Older commits could not be read: {error}"),
                    None => format!("{back}. {read} read."),
                };
                (said, Some(button))
            }
        };
        let bytes = format!(
            "{} · {} of changes",
            studio_ui::plural(self.requests, "request", "requests"),
            studio_ui::format_bytes(self.feed_bytes as u64)
        );
        v_flex()
            .px(px(space::EDGE))
            .py(px(space::MD))
            .gap(px(space::SM))
            .children(self.skipped.iter().map(|skipped| {
                studio_ui::ds::text::muted(format!(
                    "New nodes and relationships in {} were not read: they hold more than {} of \
                     new rows, such as a load. A relationship added or the node's creation there \
                     is not shown.",
                    if skipped.commits == 1 {
                        "one commit".to_owned()
                    } else {
                        format!("the newest {} commits of a window", skipped.commits)
                    },
                    studio_ui::format_bytes(NODE_HISTORY_INSERT_CAP as u64)
                ))
            }))
            .children(self.boundary.as_ref().map(|boundary| {
                studio_ui::ds::text::muted(format!(
                    "History stops at a schema change{} in {}. Omnigraph does not compare \
                     across one.",
                    boundary
                        .type_name
                        .as_ref()
                        .map(|t| format!(" to {t}"))
                        .unwrap_or_default(),
                    short_commit_id(&boundary.commit)
                ))
            }))
            .when(self.cleaned_up, |end| {
                end.child(studio_ui::ds::text::muted(
                    "The history before these commits was cleaned up.",
                ))
            })
            .children(self.stopped.clone().map(|why| {
                studio_ui::ds::text::muted(format!(
                    "The read stopped short: {}. The changes shown are those read before it.",
                    studio_ui::clause(why.as_ref())
                ))
            }))
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .child(
                        div()
                            .id("history-read")
                            .tip(studio_ui::ds::tooltip(bytes))
                            .child(studio_ui::ds::text::meta(said)),
                    )
                    .children(button),
            )
            .into_any_element()
    }
}

/// What one card draws.
struct Card<'a> {
    /// The row's index in the list.
    ix: usize,
    /// The entries it stands for, newest first: one, or a run's.
    entries: &'a [NodeHistoryEntry],
    node: Option<&'a FoldedChange>,
    edges: &'a [FoldedChange],
    /// The run it folds or heads, and whether it is folded.
    fold: Option<(usize, bool)>,
    selected: bool,
}

/// `14:02` for Unix microseconds, in UTC.
fn clock(micros: i64) -> String {
    utc_date_time(micros)[11..16].to_owned()
}

/// `2026-11-17 14:02 UTC` for Unix microseconds.
fn short_time(micros: i64) -> String {
    let full = utc_date_time(micros);
    format!("{} UTC", &full[..16])
}

/// A relationship of the node a commit changed, read from the node: "+ knows → grace", at
/// whichever end the node is.
fn relationship_line(
    id: (usize, usize),
    edge: &FoldedChange,
    node_id: &str,
    schema: Option<&SchemaModel>,
) -> RelationshipLine {
    // The ends the node held: a relationship moved away from it is read from the image that
    // still names it.
    let ends = |image: Option<&graph_client::api::ChangeImageOutput>| {
        let ends = image?.endpoints.as_ref()?;
        Some((
            ends.get("from")?.as_str()?.to_owned(),
            ends.get("to")?.as_str()?.to_owned(),
        ))
    };
    let (from, to) = [edge.after.as_ref(), edge.before.as_ref()]
        .into_iter()
        .filter_map(ends)
        .find(|(from, to)| from == node_id || to == node_id)
        .or_else(|| edge.endpoints())
        .unwrap_or_default();
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
                studio_ui::plural(self.commits.len(), "commit", "commits"),
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
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .tip_fn({
                            let branch = self.branch.clone();
                            move |_, cx| {
                                studio_ui::ds::tooltip_for_action_view(
                                    format!("{branch} changed after this history was read"),
                                    &Refresh,
                                    cx,
                                )
                            }
                        })
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
                            .tip_fn(|_, cx| {
                                studio_ui::ds::tooltip_for_action_view(
                                    "Read the history again",
                                    &Refresh,
                                    cx,
                                )
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .into_any_element()
            }
            State::Reading => {
                studio_ui::ds::loading_state("Reading the history…").into_any_element()
            }
            State::Ready if self.entries.is_empty() && self.boundary.is_some() => {
                let boundary = self.boundary.as_ref().expect("a boundary");
                change_cards::schema_change_state(boundary.type_name.as_deref())
                    .child(studio_ui::ds::state_detail(format!(
                        "History stops at commit {}: Omnigraph does not compare across a schema \
                         change, and no commit since changed {}.",
                        short_commit_id(&boundary.commit),
                        self.label.as_deref().unwrap_or(&self.id)
                    )))
                    .into_any_element()
            }
            State::Ready if self.entries.is_empty() => {
                let name = self.label.as_deref().unwrap_or(&self.id);
                let detail = if self.offers_older() {
                    format!(
                        "No commit in the newest {} on {} changed {name} or its relationships.",
                        studio_ui::plural(self.commits.len(), "commit", "commits"),
                        self.branch
                    )
                } else {
                    format!(
                        "No commit on {} changed {name} or its relationships since the graph \
                         began.",
                        self.branch
                    )
                };
                let title = if self.offers_older() {
                    format!(
                        "No changes in the last {}",
                        studio_ui::plural(self.commits.len(), "commit", "commits")
                    )
                } else {
                    "No changes".to_owned()
                };
                studio_ui::ds::empty_state(IconName::Check, title)
                    .debug_selector(|| "node-history-empty".into())
                    .child(studio_ui::ds::state_detail(detail))
                    .children(self.skipped.first().map(|_| {
                        studio_ui::ds::state_detail(
                            "Some commits held a load of new rows, which were not read.",
                        )
                    }))
                    .when(self.offers_older(), |state| {
                        state.child(
                            studio_ui::ds::button(
                                "read-older-empty",
                                format!("Read the {NODE_HISTORY_COMMITS} commits before"),
                            )
                            .style(ButtonStyle::Subtle)
                            .key_binding(KeyBinding::for_action_in(
                                &ReadOlderCommits,
                                &self.focus_handle,
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.read_older(cx))),
                        )
                    })
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
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::toggle))
            .when(self.offers_older(), |view| {
                view.on_action(cx.listener(|this, _: &ReadOlderCommits, _, cx| this.read_older(cx)))
            })
            .child(header)
            .child(body)
    }
}

/// The rows as a view of their own, drawn cached and again when the history view notifies (it
/// observes it), as History's rows are (`docs/drawing.md`).
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use gpui::{KeyBinding, TestAppContext, VisualTestContext};
    use graph_store::ledger::Asker;
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::{Value, json};

    use super::*;

    const SCHEMA: &str = "node Person {\n    slug: String @key\n    name: String?\n    \
        title: String?\n    email: String?\n    bio: String?\n}\n\
        node Company {\n    slug: String @key\n}\n\
        edge Knows: Person -> Person\n\
        edge WorksAt: Person -> Company\n";

    /// Commits `c0` (the graph's first) to `c249` on main, newest first.
    const COMMITS: usize = 250;

    /// Commits a feed page covers, as Omnigraph ends a page on a whole commit.
    const PAGE_COMMITS: usize = 20;

    fn id(n: usize) -> String {
        format!("c{n}")
    }

    /// Commit `n` was made `n` minutes after the epoch.
    fn at(n: usize) -> i64 {
        n as i64 * 60 * 1_000_000
    }

    /// What the fake server holds beyond its history.
    #[derive(Default, Clone, Copy)]
    struct Served {
        /// `c220` is a load of new people past the insert pass's cap.
        load: bool,
        /// `c200` changed Person's schema: the feed refuses to read across it (409).
        boundary: bool,
    }

    fn person(slug: &str, op: &str, before: Option<Value>, after: Option<Value>) -> Value {
        let image = |p: Option<Value>| p.map(|p| json!({ "properties": p }));
        json!({ "kind": "node", "type": { "id": "t1", "name": "Person" }, "id": slug, "op": op,
            "before": image(before), "after": image(after) })
    }

    fn edge(name: &str, op: &str, from: &str, to: &str) -> Value {
        let image = json!({ "properties": {}, "endpoints": { "from": from, "to": to } });
        let (before, after) = if op == "delete" {
            (Some(image), None)
        } else {
            (None, Some(image))
        };
        json!({ "kind": "edge", "type": { "id": format!("t-{name}"), "name": name },
            "id": format!("{name}:{from}:{to}"), "op": op, "before": before, "after": after })
    }

    /// Ada as she was at each commit's side.
    fn ada(name: &str, title: &str, email: &str) -> Value {
        json!({ "slug": "ada", "name": name, "title": title, "email": email, "bio": null })
    }

    /// The changes of commit `n`, and who made it: the history of Ada, of Zed (created at c100,
    /// linked to Bob at c101, removed with that relationship at c245), and of Bob's title.
    fn changes_of(n: usize, served: Served) -> (&'static str, Value) {
        match n {
            5 => (
                "act-old",
                json!([person("ada", "insert", None, Some(ada("Ada", "A", "a@x")))]),
            ),
            100 => (
                "act-dev",
                json!([person(
                    "zed",
                    "insert",
                    None,
                    Some(json!({ "slug": "zed", "name": "Zed" }))
                )]),
            ),
            101 => ("act-dev", json!([edge("Knows", "insert", "zed", "bob")])),
            220 if served.load => (
                "act-load",
                json!([person(
                    "bulk",
                    "insert",
                    None,
                    Some(json!({ "slug": "bulk", "bio": "x".repeat(NODE_HISTORY_INSERT_CAP) }))
                )]),
            ),
            225 => (
                "act-dev",
                json!([person(
                    "ada",
                    "update",
                    Some(ada("Ada", "A", "a@x")),
                    Some(ada("Ada Lovelace", "A", "a@x"))
                )]),
            ),
            230 => (
                "act-dev",
                json!([
                    person(
                        "ada",
                        "update",
                        Some(ada("Ada Lovelace", "A", "a@x")),
                        Some(ada("Ada Lovelace", "A", "ada@x"))
                    ),
                    edge("WorksAt", "insert", "ada", "acme")
                ]),
            ),
            235 => ("act-ops", json!([edge("Knows", "insert", "grace", "ada")])),
            239 => (
                "act-dev",
                json!([person(
                    "ada",
                    "update",
                    Some(ada("Ada Lovelace", "A", "ada@x")),
                    Some(ada("Ada Lovelace", "B", "ada@x"))
                )]),
            ),
            240 => (
                "act-dev",
                json!([person(
                    "ada",
                    "update",
                    Some(ada("Ada Lovelace", "B", "ada@x")),
                    Some(ada("Ada Lovelace", "C", "ada@x"))
                )]),
            ),
            241 => (
                "act-dev",
                json!([person(
                    "bob",
                    "update",
                    Some(json!({ "slug": "bob", "title": "A" })),
                    Some(json!({ "slug": "bob", "title": "B" }))
                )]),
            ),
            245 => (
                "act-dev",
                json!([
                    person(
                        "zed",
                        "delete",
                        Some(json!({ "slug": "zed", "name": "Zed" })),
                        None
                    ),
                    edge("Knows", "delete", "zed", "bob")
                ]),
            ),
            _ => ("act-dev", json!([])),
        }
    }

    /// A server whose main holds [`COMMITS`] commits ([`changes_of`]). It pages the listing as
    /// Omnigraph does, answers the change feed from `start` (or a page token) to the head in
    /// pages of [`PAGE_COMMITS`] commits filtered by `op`, names `b1`'s branch, and notes every
    /// request. `/commits` fails with 500 while `failing` holds.
    fn serve(
        served: Served,
        cx: &mut TestAppContext,
    ) -> (
        Entity<GraphSession>,
        Arc<Mutex<Vec<String>>>,
        Arc<AtomicBool>,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.bind_keys([
                KeyBinding::new("down", menu::SelectNext, Some("NodeHistory")),
                KeyBinding::new("j", menu::SelectNext, Some("NodeHistory")),
                KeyBinding::new("up", menu::SelectPrevious, Some("NodeHistory")),
                KeyBinding::new("k", menu::SelectPrevious, Some("NodeHistory")),
                KeyBinding::new("enter", menu::Confirm, Some("NodeHistory")),
                KeyBinding::new("space", ToggleChange, Some("NodeHistory")),
                KeyBinding::new("cmd-alt-n", ReadOlderCommits, Some("NodeHistory")),
            ]);
        });
        let asked = Arc::new(Mutex::new(Vec::new()));
        let failing = Arc::new(AtomicBool::new(false));
        let (noted, fails) = (asked.clone(), failing.clone());
        let commit = |n: usize| {
            let merged = (n == 225).then(|| "b1".to_owned());
            json!({
                "graph_commit_id": id(n), "graph_manifest_version": n, "kind": "published",
                "created_at": at(n), "parent_commit_id": n.checked_sub(1).map(id),
                "merged_parent_commit_id": merged, "actor_id": "act-dev",
            })
        };
        let block = move |n: usize, ops: &[String]| -> Value {
            let (actor, changes) = changes_of(n, served);
            let changes: Vec<Value> = changes
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| ops.is_empty() || ops.iter().any(|op| c["op"] == json!(op)))
                .cloned()
                .collect();
            let merged = (n == 225).then(|| "b1".to_owned());
            json!({ "cause": { "graph_commit_id": id(n), "authored_branch": "main",
                "authored_at": at(n), "actor_id": actor, "merged_parent_commit_id": merged,
                "parent_commit_id": n.checked_sub(1).map(id) }, "changes": changes })
        };
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            noted.lock().unwrap().push(format!("{path}?{query}"));
            let params = |name: &str| -> Vec<String> {
                query
                    .split('&')
                    .filter_map(|pair| pair.strip_prefix(&format!("{name}=")))
                    .map(|value| value.replace("%3A", ":"))
                    .collect()
            };
            let param = |name: &str| params(name).into_iter().next();
            let (status, body) = if path.ends_with("/schema") {
                (200, json!({ "schema_source": SCHEMA }))
            } else if path.ends_with("/commits") && fails.load(Ordering::SeqCst) {
                (
                    500,
                    json!({ "error": "the server broke", "code": "internal" }),
                )
            } else if path.ends_with("/commits") {
                let limit: usize = param("limit").and_then(|l| l.parse().ok()).unwrap_or(100);
                let offset: usize = param("page_token")
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0);
                let newest = COMMITS - 1 - offset;
                let page: Vec<Value> = (0..=newest).rev().take(limit).map(commit).collect();
                let next = (offset + limit < COMMITS).then(|| (offset + limit).to_string());
                (
                    200,
                    json!({ "commits": page, "head_commit_id": id(COMMITS - 1),
                        "next_page_token": next }),
                )
            } else if path.ends_with("/commits/b1") {
                (
                    200,
                    json!({ "graph_commit_id": "b1", "graph_manifest_version": 1,
                        "created_at": at(224), "kind": "published", "graph_branch": "feature" }),
                )
            } else if path.ends_with("/changes") {
                let from = match (param("page_token"), param("start").as_deref()) {
                    (Some(token), _) => token.parse::<usize>().unwrap(),
                    (None, Some("beginning")) | (None, None) => 0,
                    (None, Some(start)) => {
                        start
                            .trim_start_matches("after:c")
                            .parse::<usize>()
                            .unwrap()
                            + 1
                    }
                };
                let ops = params("op");
                let to = (from + PAGE_COMMITS).min(COMMITS);
                if served.boundary && from <= 200 {
                    (
                        409,
                        json!({ "error": "schema boundary", "code": "conflict",
                            "change_diff_refusal": { "reason": "schema_boundary",
                            "graph_commit_id": "c200", "type_name": "Person" } }),
                    )
                } else {
                    let blocks: Vec<Value> = (from..to).map(|n| block(n, &ops)).collect();
                    let more = to < COMMITS;
                    (
                        200,
                        json!({ "blocks": blocks, "cursor": "end", "caught_up": !more,
                            "next_page_token": more.then(|| to.to_string()) }),
                    )
                }
            } else {
                (200, json!({}))
            };
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
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        session.update(cx, |session, cx| session.reload_schema(cx));
        cx.run_until_parked();
        (session, asked, failing)
    }

    fn open<'a>(
        session: &Entity<GraphSession>,
        node: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<NodeHistoryView>, &'a mut VisualTestContext) {
        let (session, node) = (session.clone(), node.to_owned());
        let (view, cx) = cx.add_window_view(move |_, cx| {
            NodeHistoryView::new(session, "Person".into(), node, None, cx)
        });
        cx.run_until_parked();
        (view, cx)
    }

    fn commits(ids: &[usize]) -> Vec<String> {
        ids.iter().map(|&n| id(n)).collect()
    }

    /// A node's history reads the branch's newest 200 commits: the listing, then the feed from
    /// just before its oldest commit filtered to the node's types, updates and deletes first,
    /// then inserts, then the merged branch's name, each read named in the ledger. It shows the
    /// commits that changed the node or a relationship at either of its ends, newest first; a
    /// run of one actor's edits within ten minutes folds into one, with its net change.
    #[gpui::test]
    async fn a_nodes_history_reads_a_window_and_shows_its_entries(cx: &mut TestAppContext) {
        let (session, asked, _) = serve(Served::default(), cx);
        asked.lock().unwrap().clear();
        let (view, cx) = open(&session, "ada", cx);
        let shown = view.read_with(cx, |view, _| view.shown());
        let rows: Vec<(Vec<String>, Option<String>, bool, usize)> = vec![
            (commits(&[240, 239]), Some("act-dev".into()), true, 0),
            (commits(&[235]), Some("act-ops".into()), false, 1),
            (commits(&[230]), Some("act-dev".into()), true, 1),
            (commits(&[225]), Some("act-dev".into()), true, 0),
        ];
        assert_eq!(
            shown, rows,
            "c241 and c245 changed others; c5 and c101 are older"
        );
        {
            let asked = asked.lock().unwrap();
            assert!(
                asked[0].contains("/commits?branch=main&limit=200"),
                "{asked:?}"
            );
            let feed: Vec<&String> = asked.iter().filter(|a| a.contains("/changes")).collect();
            // 200 commits in pages of 20, twice: the edits, then the inserts.
            assert_eq!(feed.len(), 20, "{feed:?}");
            assert!(feed[0].contains("start=after%3Ac49"), "{feed:?}");
            for part in ["type=Person", "type=Knows", "type=WorksAt"] {
                assert!(feed[0].contains(part), "{feed:?}");
            }
            assert!(feed[0].contains("op=update") && feed[0].contains("op=delete"));
            assert!(!feed[0].contains("op=insert"), "edits first");
            assert!(feed[10].contains("op=insert") && !feed[10].contains("op=update"));
            assert!(
                asked.iter().any(|a| a.ends_with("/commits/b1?")),
                "{asked:?}"
            );
            assert_eq!(asked.len(), 22, "{asked:?}");
        }
        // Every read is named in the ledger, as asked by this view.
        session.read_with(cx, |session, _| {
            let ledger = session.ledger();
            let by_view = |what: &str| {
                ledger
                    .tallies()
                    .filter(|((name, asker, answered), _)| {
                        name.as_ref() == what
                            && *asker == Asker::View(ReadView::NodeHistory)
                            && answered.sent()
                    })
                    .map(|(_, tally)| tally.count)
                    .sum::<u64>()
            };
            assert_eq!(by_view("node_history_commits"), 1);
            assert_eq!(by_view("node_history_changes"), 10);
            assert_eq!(by_view("node_history_inserts"), 10);
            assert_eq!(by_view("node_history_merge"), 1);
        });
        view.read_with(cx, |view, cx| {
            assert_eq!(view.commits.len(), 200);
            assert_eq!(view.label.as_deref(), Some("Ada Lovelace"));
            assert!(view.offers_older());
            // The fold's net change: the title from A to C over two commits.
            let (node, _) = net_of(&view.entries[view.runs[0].clone()]);
            let node = node.unwrap();
            assert_eq!(node.before.unwrap().properties["title"], json!("A"));
            assert_eq!(node.after.unwrap().properties["title"], json!("C"));
            // The merge names its branch.
            let merge = view.entries.iter().find(|e| e.commit == "c225").unwrap();
            assert_eq!(merge.merged_branch.as_deref(), Some("feature"));
            // The page may now say who changed Ada last, at the head read.
            let last = view.session.read(cx).last_change("Person", "ada").cloned();
            assert_eq!(
                last.map(|l| (l.commit, l.head)),
                Some(("c240".into(), "c249".into()))
            );
        });
    }

    /// Relationship lines name the other end, whichever end the node is: Ada is the target of
    /// Grace's Knows and the source of her WorksAt.
    #[gpui::test]
    async fn relationship_lines_read_from_either_end(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "ada", cx);
        view.read_with(cx, |view, _| {
            let ends = |commit: &str| {
                let entry = view.entries.iter().find(|e| e.commit == commit).unwrap();
                let edge = &entry.edges[0];
                let (from, to) = edge.endpoints().unwrap();
                (from == "ada", if from == "ada" { to } else { from })
            };
            assert_eq!(ends("c235"), (false, "grace".to_owned()), "incoming");
            assert_eq!(ends("c230"), (true, "acme".to_owned()), "outgoing");
        });
    }

    /// "Now" names each property's newest setter within the window, the merge's branch for the
    /// name, and "before" the oldest commit read for the key, which no commit read set; reading
    /// the 200 commits before finds its creation, and stops where the first window began.
    #[gpui::test]
    async fn now_says_who_set_each_value_and_before_past_the_window(cx: &mut TestAppContext) {
        let (session, asked, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "ada", cx);
        let now = |view: &NodeHistoryView, cx: &App| -> Vec<(String, String)> {
            let schema = view.session.read(cx).schema_model();
            view.blame_rows(schema.as_deref())
                .into_iter()
                .map(|b| {
                    let set = match b.set {
                        BlameSet::In { entry, created } => {
                            format!(
                                "{}{}",
                                view.entries[entry].commit,
                                if created { "+" } else { "" }
                            )
                        }
                        BlameSet::Before(commit) => format!("before {}", commit.id),
                    };
                    (b.property, set)
                })
                .collect()
        };
        let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            list.iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect()
        };
        view.read_with(cx, |view, cx| {
            assert_eq!(
                now(view, cx),
                pairs(&[
                    ("slug", "before c50"),
                    ("name", "c225"),
                    ("title", "c240"),
                    ("email", "c230"),
                    ("bio", "before c50"),
                ])
            );
        });
        asked.lock().unwrap().clear();
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.simulate_keystrokes("cmd-alt-n");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.commits.len(),
                COMMITS,
                "it stopped where the first began"
            );
            assert!(!view.offers_older(), "it reached the graph's first commit");
            assert_eq!(
                now(view, cx),
                pairs(&[
                    ("slug", "c5+"),
                    ("name", "c225"),
                    ("title", "c240"),
                    ("email", "c230"),
                    ("bio", "c5+"),
                ])
            );
            assert!(view.entries.last().unwrap().created());
        });
        let asked = asked.lock().unwrap();
        assert!(asked[0].contains("page_token=200"), "{asked:?}");
        assert!(
            asked
                .iter()
                .any(|a| a.contains("/changes") && a.contains("start=beginning")),
            "{asked:?}"
        );
    }

    /// A node removed at the head still opens its history: its removal, with the relationship
    /// that went with it, and its creation are entries, and "Now" says who removed it.
    #[gpui::test]
    async fn a_removed_node_shows_its_removal_and_creation(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "zed", cx);
        view.read_with(cx, |view, _| {
            let kinds: Vec<(&str, bool, bool, usize)> = view
                .entries
                .iter()
                .map(|e| (e.commit.as_str(), e.created(), e.removed(), e.edges.len()))
                .collect();
            assert_eq!(
                kinds,
                [
                    ("c245", false, true, 1),
                    ("c101", false, false, 1),
                    ("c100", true, false, 0)
                ]
            );
            assert!(matches!(newest_image(&view.entries), Some((_, true))));
            // A creation and a removal never fold.
            assert_eq!(view.runs.len(), 3);
        });
    }

    /// A load past the insert pass's cap stops that pass and the view says what it skipped:
    /// the newer commits' inserts are not read, the edits still are, and a value no edit set
    /// reads "before" the newest commit whose inserts were skipped.
    #[gpui::test]
    async fn the_insert_cap_stops_the_pass_and_says_what_it_skipped(cx: &mut TestAppContext) {
        let (session, asked, _) = serve(
            Served {
                load: true,
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, "ada", cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.skipped.len(), 1);
            // Pages of 20 from c50: c210..c229 holds the load, so c209 is the newest read.
            assert_eq!(view.skipped[0].commits, 40, "c210 to c249");
            assert_eq!(view.skipped[0].newest.id, "c249");
            let shown: Vec<Vec<String>> = view.shown().into_iter().map(|row| row.0).collect();
            // c235's added relationship is in the skipped inserts, so the edits of c240, c239
            // and c230, by one actor within ten minutes of each other, now run together.
            assert_eq!(
                shown,
                [commits(&[240, 239, 230]), commits(&[225])],
                "the edits are read"
            );
            assert!(view.entries.iter().all(|e| e.edges.is_empty()));
            assert!(matches!(
                view.blame_bound(),
                Some(WindowCommit { ref id, .. }) if id == "c249"
            ));
        });
        let inserts = asked
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.contains("/changes") && a.contains("op=insert"))
            .count();
        assert_eq!(inserts, 9, "eight pages read, the ninth past the cap");
    }

    /// A schema change in the window stops the history there: the feed is read again from just
    /// after it, the commits since are shown, and no older window is offered (E52).
    #[gpui::test]
    async fn a_schema_change_stops_the_history_and_says_so(cx: &mut TestAppContext) {
        let (session, asked, _) = serve(
            Served {
                boundary: true,
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, "ada", cx);
        view.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Ready));
            assert_eq!(
                view.boundary,
                Some(SchemaBoundary {
                    commit: "c200".into(),
                    type_name: Some("Person".into())
                })
            );
            assert_eq!(view.commits.len(), 49, "c201 to c249");
            assert_eq!(view.shown().len(), 4, "the changes since the boundary");
            assert!(!view.offers_older(), "the walk stops at the boundary");
        });
        let asked = asked.lock().unwrap();
        assert!(
            asked.iter().any(|a| a.contains("start=after%3Ac200")),
            "read again from just after it: {asked:?}"
        );
    }

    /// No change in the window: the empty state says so and offers the commits before.
    #[gpui::test]
    async fn an_unchanged_node_says_so_and_offers_the_commits_before(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "nobody", cx);
        view.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Ready));
            assert!(view.entries.is_empty());
            assert!(view.offers_older());
        });
        assert!(cx.debug_bounds("node-history-empty").is_some());
    }

    /// Reading, then a failure that offers Try again and asks nothing again by itself; Try
    /// again (⌘R) reads it.
    #[gpui::test]
    async fn reading_then_a_failure_waits_for_try_again(cx: &mut TestAppContext) {
        let (session, asked, failing) = serve(Served::default(), cx);
        failing.store(true, Ordering::SeqCst);
        asked.lock().unwrap().clear();
        let session_ = session.clone();
        let (view, cx) = cx.add_window_view(move |_, cx| {
            NodeHistoryView::new(session_, "Person".into(), "ada".into(), None, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| match &view.state {
            State::Failed(why) => assert!(why.contains("the server broke"), "{why}"),
            _ => panic!("failed"),
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(60));
        cx.run_until_parked();
        let listings = |asked: &Arc<Mutex<Vec<String>>>| {
            asked
                .lock()
                .unwrap()
                .iter()
                .filter(|a| a.contains("/commits?"))
                .count()
        };
        assert_eq!(listings(&asked), 1, "nothing is asked again by itself");
        failing.store(false, Ordering::SeqCst);
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        // Try again reads it, showing the reading state meanwhile.
        view.update(cx, |view, cx| view.reload(cx));
        view.read_with(cx, |view, cx| {
            assert!(matches!(view.state, State::Reading), "reading again");
            assert!(Item::settling(view, cx));
        });
        cx.run_until_parked();
        assert_eq!(listings(&asked), 2);
        // ⌘R does too.
        cx.dispatch_action(Refresh);
        cx.run_until_parked();
        assert_eq!(listings(&asked), 3);
        view.read_with(cx, |view, _| assert!(matches!(view.state, State::Ready)));
    }

    /// The keyboard: ↓ and J move between entries, ↑ and K back; Space unfolds a run into its
    /// commits and folds it again; ↵ opens the selected commit.
    #[gpui::test]
    async fn the_keyboard_moves_folds_and_opens(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let opened = Arc::new(Mutex::new(Vec::new()));
        let heard = opened.clone();
        cx.update(|cx| {
            cx.on_action(move |action: &OpenCommit, _| {
                heard.lock().unwrap().push(action.commit_id.clone())
            });
        });
        let (view, cx) = open(&session, "ada", cx);
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        let selected = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| {
                view.selected
                    .and_then(|ix| view.commit_of(view.rows[ix]))
                    .map(str::to_owned)
            })
        };
        assert_eq!(selected(cx).as_deref(), Some("c240"), "the newest, folded");
        cx.simulate_keystrokes("down");
        assert_eq!(selected(cx).as_deref(), Some("c235"));
        cx.simulate_keystrokes("j");
        assert_eq!(selected(cx).as_deref(), Some("c230"));
        cx.simulate_keystrokes("k up");
        assert_eq!(selected(cx).as_deref(), Some("c240"));
        cx.simulate_keystrokes("up");
        assert_eq!(selected(cx).as_deref(), Some("c240"), "Now is not an entry");

        // Space unfolds the run into its two commits, and folds it again.
        cx.simulate_keystrokes("space");
        view.read_with(cx, |view, _| {
            assert_eq!(view.shown()[0].0, commits(&[240]));
            assert_eq!(view.shown()[1].0, commits(&[239]));
        });
        cx.simulate_keystrokes("down");
        assert_eq!(selected(cx).as_deref(), Some("c239"));
        cx.simulate_keystrokes("space");
        view.read_with(cx, |view, _| {
            assert_eq!(view.shown()[0].0, commits(&[240, 239]))
        });
        assert_eq!(selected(cx).as_deref(), Some("c240"), "on the folded run");

        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        assert_eq!(*opened.lock().unwrap(), ["c235"]);
    }

    /// The rows are a cached view of their own: another branch's commit draws nothing, a key
    /// draws them once, and older commits landing draw them once for reading and once more.
    #[gpui::test]
    async fn the_rows_draw_once_a_key_and_only_for_what_they_show(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "ada", cx);
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
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
        // The first key after the pointer draws the whole window once (`docs/drawing.md`).
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        for key in ["down", "j", "k", "up"] {
            let before = renders(cx);
            cx.simulate_keystrokes(key);
            cx.run_until_parked();
            assert_eq!(renders(cx), before + 1, "{key} draws the rows once");
        }
        let before = renders(cx);
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
