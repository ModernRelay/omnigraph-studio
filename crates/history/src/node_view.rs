//! A node's history (spec 046): the node's fourth view, beside its page, tree and canvas. It is
//! a timeline of the node's changes, newest first: who made each, on which branch or through which merge, when and in which commit, its
//! properties before and after and its relationship lines at either end. A run of one actor's
//! small edits folds into one entry with its net change.
//!
//! Omnigraph has no read of one entity's commits (E62) and keeps no operation or message with a
//! commit (E60): the first window covers the branch's newest
//! [`NODE_HISTORY_FIRST_COMMITS`](graph_store::node_history::NODE_HISTORY_FIRST_COMMITS) commits, read as point reads of the node at each
//! ([`node_history::read_window`](graph_store::node_history::read_window)), with an honest exit to the [`NODE_HISTORY_COMMITS`]
//! before them. It reads only once its tab is shown.
//!
//! Forked in part from the closed #146 (spec 040, `feat/ux-p5-node-history`).

use std::collections::HashSet;
use std::ops::Range;

use gpui::{
    AnyElement, AnyView, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ListAlignment, ListState, Render, SharedString, StyleRefinement, Subscription, Task,
    WeakEntity, Window, list,
};
use gq_language::RelationshipGroup;
use gq_language::SchemaModel;
use graph_store::ledger::ReadView;
use graph_store::node_history::{
    Coverage, Creation, NODE_HISTORY_COMMITS, NODE_HISTORY_EDGES, NodeHistoryEntry,
    NodeHistoryPage, NodeHistoryWindow, NodeScope, NodeState, OlderWindow, Reach, SearchFound,
    WindowCommit, net_of, runs,
};
use graph_store::scope::ReadScope;
use graph_store::{FoldedChange, GraphSession, SessionEvent, entity_name, relationship_words};
use studio_actions::changes::ToggleChange;
use studio_actions::graph::{OpenCommit, OpenNodePage, ShowNodeInTree, ShowNodeOnCanvas};
use studio_actions::history::{FindWhenCreated, ReadOlderCommits};
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

/// What a search for the node's creation starts from: where the listing goes on, and the oldest
/// commit read, with the node there.
type SearchFrom = (OlderWindow, (String, NodeState));

/// How the search for the node's creation stands.
#[derive(Clone)]
enum Search {
    Idle,
    Running,
    Done,
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
    /// The node at the newest commit of the first window: its name when no entry has it.
    newest: Option<NodeState>,
    /// The oldest commit read, with the node there: where a search for its creation starts.
    oldest: Option<(String, NodeState)>,
    /// What the windows read followed, how far back they reach, and what is known of the
    /// node's creation: every sentence about what was read reads it.
    coverage: Coverage,
    /// What the search for the node's creation found, and how it stands.
    found: Option<SearchFound>,
    search: Search,
    _search_task: Option<Task<()>>,
    older: Older,
    /// Requests sent for the windows shown.
    requests: usize,
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
        matches!(self.state, State::Reading)
            || matches!(self.older, Older::Reading(_))
            || matches!(self.search, Search::Running)
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
            // Any move of the branch's head past the one read, however it was seen: a head
            // placed by a read after a write raced another writer's says so only this way.
            SessionEvent::HeadMoved { branch, head }
                if *branch == this.branch
                    && this.head.as_ref().is_some_and(|read| read != head) =>
            {
                if !this.newer {
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
            SessionEvent::CredentialChanged => {
                if this.label.take().is_some() {
                    cx.emit(ItemEvent::UpdateTab);
                }
                this.reload(cx)
            }
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
            newest: None,
            oldest: None,
            coverage: Coverage::default(),
            found: None,
            search: Search::Idle,
            _search_task: None,
            older: Older::None,
            requests: 0,
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
        self.newest = None;
        self.oldest = None;
        self.coverage = Coverage::default();
        self.found = None;
        self.search = Search::Idle;
        self._search_task = None;
        self.older = Older::None;
        self.requests = 0;
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
            graph_store::node_history::read_window(session, &scope, &branch, node, window, cx)
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
            newest,
            oldest,
            entries,
            commits,
            older,
            coverage,
            requests,
        } = page;
        if self.head.is_none() {
            self.head = Some(head);
            self.newest = newest;
            self.coverage = coverage;
            cx.emit(ItemEvent::Selected);
        } else {
            self.coverage.below(coverage);
        }
        self.oldest = oldest;
        if self.label.is_none() {
            let schema = self.session.read(cx).schema_model();
            let label = entries
                .iter()
                .find_map(|entry| entry.node.as_ref())
                .and_then(|node| entity_name(node, schema.as_deref()))
                .or_else(|| {
                    let newest = self.newest.as_ref().filter(|n| n.exists)?;
                    let label = schema.as_ref()?.node(&self.type_name)?.label_property()?;
                    newest.properties.get(label)?.as_str().map(str::to_owned)
                });
            if label.is_some() {
                self.label = label;
                cx.emit(ItemEvent::UpdateTab);
            }
        }
        self.entries.extend(entries);
        self.commits.extend(commits);
        self.requests += requests;
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
        let mut rows = Vec::new();
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
        self.keep_place();
    }

    /// Fits the list to its rows, keeping the row at the top of the view where it is: older
    /// commits land below the reader, never under them.
    fn keep_place(&mut self) {
        if self.list_state.item_count() != self.rows.len() {
            let top = self.list_state.logical_scroll_top();
            self.list_state.reset(self.rows.len());
            self.list_state.scroll_to(top);
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
        let window = Box::new(window);
        // No entry read yet, and every group followed: the older window's newest entry is the
        // node's last change, which the page shows.
        let nothing_newer = self.entries.is_empty() && self.coverage.last_change_known();
        self.read(
            NodeHistoryWindow::Older {
                head,
                window,
                nothing_newer,
            },
            cx,
        );
        cx.notify();
    }

    fn offers_older(&self) -> bool {
        matches!(self.older, Older::Waits(_) | Older::Failed(..))
    }

    /// Where a search for the node's creation would start, while the windows read did not reach
    /// it: the node is there at the oldest commit read, and older commits remain.
    fn search_from(&self) -> Option<SearchFrom> {
        let (Older::Waits(window) | Older::Failed(window, _)) = &self.older else {
            return None;
        };
        let oldest = self.oldest.clone().filter(|(_, node)| node.exists)?;
        let uncreated = self.coverage.creation != Creation::InEntries;
        uncreated.then(|| (window.clone(), oldest))
    }

    /// Whether the end row offers the search: it has somewhere to start and has not run.
    fn offers_search(&self) -> bool {
        !matches!(self.search, Search::Running | Search::Done) && self.search_from().is_some()
    }

    /// Searches the commits before those read for when the node was created (spec 046): a
    /// listing of them, then a binary search of point reads. Only for its creation, never for
    /// the list of changes.
    fn find_when_created(&mut self, cx: &mut Context<Self>) {
        if matches!(self.search, Search::Running) {
            return;
        }
        let Some((window, base)) = self.search_from() else {
            return;
        };
        let Some(node) = self.scope_of(cx) else {
            return;
        };
        let (branch, scope) = (self.branch.clone(), self.scope.clone());
        let read = self.session.update(cx, |session, cx| {
            graph_store::node_history::search_creation(
                session,
                &scope,
                &branch,
                node,
                window.page_token,
                base,
                cx,
            )
        });
        self.search = Search::Running;
        self._search_task = Some(cx.spawn(async move |this, cx| {
            let answer = read.await;
            this.update(cx, |this, cx| {
                match answer {
                    Ok(found) => {
                        this.search = Search::Done;
                        this.coverage.searched(&found);
                        this.found = Some(found);
                    }
                    Err(error) => this.search = Search::Failed(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
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
            Row::End => None,
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
        self.keep_place();
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

    /// The search for the node's creation, in the end row: its button while it is offered, its
    /// progress, what it found, its failure, or what stopped it.
    fn render_search(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let note = match (&self.search, self.found.as_ref()) {
            (Search::Failed(error), _) => Some(format!("The search could not finish: {error}")),
            (Search::Done, Some(found)) => {
                let requests = studio_ui::plural(found.requests, "request", "requests");
                Some(match &self.coverage.creation {
                    Creation::Found(commit) => format!(
                        "Created by {} in {}, {}: found by a search of earlier commits with \
                         {requests}.",
                        commit.actor.clone().unwrap_or_else(|| "no actor".into()),
                        short_commit_id(&commit.id),
                        relative_time(commit.at, now_micros())
                    ),
                    Creation::Before(commit) => format!(
                        "Created before {}, {}, the oldest commit searched ({requests}).",
                        short_commit_id(&commit.id),
                        relative_time(commit.at, now_micros())
                    ),
                    Creation::NotSearched | Creation::InEntries => {
                        format!("Searched with {requests}.")
                    }
                })
            }
            _ => None,
        };
        let running = matches!(self.search, Search::Running);
        let offered = self.offers_search();
        if note.is_none() && !offered && !running {
            return None;
        }
        let focus = self.focus_handle.clone();
        Some(
            h_flex()
                .debug_selector(|| "node-history-search".into())
                .gap(px(space::MD))
                .when(offered || running, |row| {
                    row.child(
                        studio_ui::ds::button(
                            "find-when-created",
                            if running {
                                "Searching earlier commits…"
                            } else {
                                "Find when it was created"
                            },
                        )
                        .style(ButtonStyle::Subtle)
                        .disabled(running)
                        .key_binding(KeyBinding::for_action_in(&FindWhenCreated, &focus, cx))
                        .tip(studio_ui::ds::tooltip(
                            "Search the commits before those read, with a few point reads of the \
                             node, for the one that created it",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.find_when_created(cx))),
                    )
                })
                .children(note.map(studio_ui::ds::text::meta))
                .into_any_element(),
        )
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
                relationship_line((ix, line), edge, &self.type_name, &self.id, schema)
                    .into_any_element()
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
                .child(studio_ui::ds::text::meta(self.removal_words()))
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

    /// The groups too large to follow, in words, when there are any: "Too large to follow, … :
    /// WorksAt → Company."
    fn too_large_words(&self) -> Option<String> {
        if self.coverage.follows_every_group() {
            return None;
        }
        Some(format!(
            "Too large to follow, with {NODE_HISTORY_EDGES} relationships or more at a commit, \
             so its changes there are not shown: {}.",
            group_words(&self.coverage.too_large)
        ))
    }

    /// The end of a sentence that says what no commit changed: "." when every group was
    /// followed, else which relationships were not.
    fn followed_words(&self) -> String {
        if self.coverage.follows_every_group() {
            " or its relationships.".to_owned()
        } else {
            format!(
                " or the relationships Studio could follow. Not followed, too large: {}.",
                group_words(&self.coverage.too_large)
            )
        }
    }

    /// The empty state's title and sentence: "since the graph began" only when the windows
    /// reach it and every group was followed.
    fn empty_words(&self) -> (String, String) {
        let name = self.label.as_deref().unwrap_or(&self.id);
        let commits = studio_ui::plural(self.commits.len(), "commit", "commits");
        let followed = self.followed_words();
        if self.coverage.reaches_the_start() {
            return (
                "No changes".to_owned(),
                format!(
                    "No commit on {} changed {name} or its relationships since the graph began.",
                    self.branch
                ),
            );
        }
        let title = if self.offers_older() {
            format!("No changes in the last {commits}")
        } else {
            "No changes found".to_owned()
        };
        (
            title,
            format!(
                "No commit in the newest {commits} on {} changed {name}{followed}",
                self.branch
            ),
        )
    }

    /// What a removal says of relationships it lists none of: the node had none when it was
    /// removed, or a group was too large to follow there.
    fn removal_words(&self) -> String {
        if self.coverage.follows_every_group() {
            "It had no relationships when it was removed.".to_owned()
        } else {
            format!(
                "Its relationships went with it. Not listed, too large to follow: {}.",
                group_words(&self.coverage.too_large)
            )
        }
    }

    /// Why the read of older commits failed, in words, when it did.
    fn older_failure(&self) -> Option<String> {
        match &self.older {
            Older::Failed(_, error) => Some(format!("Older commits could not be read: {error}")),
            _ => None,
        }
    }

    /// The control for the commits before those read, wherever it shows: "Read the 200 commits
    /// before", "Reading older commits…" while they are read, or "Try again" after a failure.
    /// None once there are no older commits.
    fn older_button(&self, id: &'static str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let label = match &self.older {
            Older::None => return None,
            Older::Waits(_) => format!("Read the {NODE_HISTORY_COMMITS} commits before"),
            Older::Reading(_) => "Reading older commits…".to_owned(),
            Older::Failed(..) => "Try again".to_owned(),
        };
        let reading = matches!(self.older, Older::Reading(_));
        Some(
            studio_ui::ds::button(id, label)
                .style(ButtonStyle::Subtle)
                .disabled(reading)
                .key_binding(KeyBinding::for_action_in(
                    &ReadOlderCommits,
                    &self.focus_handle,
                    cx,
                ))
                .on_click(cx.listener(|this, _, _, cx| this.read_older(cx)))
                .into_any_element(),
        )
    }

    /// The list's last row: how far back the history was read, what it could not follow, and the
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
            Older::None if self.coverage.reach == Reach::GraphStart => (
                format!("{back}, the graph's first commit. {read} read."),
                None,
            ),
            Older::None => (format!("{back}."), None),
            Older::Waits(_) | Older::Failed(..) | Older::Reading(_) => {
                let said = match self.older_failure() {
                    Some(failure) => format!("{back}. {failure}"),
                    None => format!("{back}. {read} read."),
                };
                (said, self.older_button("read-older", cx))
            }
        };
        let reads = format!(
            "{} read, {} at once: the node at each commit and its edges through each \
             relationship",
            studio_ui::plural(self.requests, "request", "requests"),
            graph_store::node_history::NODE_HISTORY_READS_AT_ONCE
        );
        let reach = match &self.coverage.reach {
            Reach::Boundary(boundary) => Some(format!(
                "History stops at a schema change{} in {}. Omnigraph does not compare across \
                 one.",
                boundary
                    .type_name
                    .as_ref()
                    .map(|t| format!(" to {t}"))
                    .unwrap_or_default(),
                short_commit_id(&boundary.commit)
            )),
            Reach::Stopped(why) => Some(format!(
                "The read stopped short: {}. The changes shown are those read before it.",
                studio_ui::clause(why)
            )),
            Reach::Window | Reach::GraphStart => None,
        };
        v_flex()
            .px(px(space::EDGE))
            .py(px(space::MD))
            .gap(px(space::SM))
            .children(self.too_large_words().map(studio_ui::ds::text::muted))
            .children(reach.map(studio_ui::ds::text::muted))
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .child(
                        div()
                            .id("history-read")
                            .tip(studio_ui::ds::tooltip(reads))
                            .child(studio_ui::ds::text::meta(said)),
                    )
                    .children(button),
            )
            .children(self.render_search(cx))
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

/// Relationship groups as their headers name them: "WorksAt → Company, Knows ← Person".
fn group_words(groups: &[RelationshipGroup]) -> String {
    groups
        .iter()
        .map(|g| match g.direction {
            gq_language::Direction::Outgoing => format!("{} → {}", g.edge, g.other_type),
            gq_language::Direction::Incoming => format!("{} ← {}", g.edge, g.other_type),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `14:02` for Unix microseconds, in UTC.
fn clock(micros: i64) -> String {
    utc_date_time(micros)[11..16].to_owned()
}

/// A relationship of the node a commit changed, read from the node: "+ knows → grace", at
/// whichever end the node is.
fn relationship_line(
    id: (usize, usize),
    edge: &FoldedChange,
    node_type: &str,
    node_id: &str,
    schema: Option<&SchemaModel>,
) -> RelationshipLine {
    let (outgoing, other) = node_end(edge, node_type, node_id, schema);
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

/// Whether the node is a relationship's source, and the id at its other end. The schema says
/// which end is the node's type; only where both ends are of its type are the ids compared, so a
/// node of another type with the same key is never taken for it.
fn node_end(
    edge: &FoldedChange,
    node_type: &str,
    node_id: &str,
    schema: Option<&SchemaModel>,
) -> (bool, String) {
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
    // Which end the node is: the schema says, unless both ends are of its type (another node
    // may have the same key in another type).
    let edge_type = schema.and_then(|schema| schema.edge(&edge.type_name));
    let outgoing = match edge_type {
        Some(e) if e.from == node_type && e.to != node_type => true,
        Some(e) if e.to == node_type && e.from != node_type => false,
        _ => from == node_id,
    };
    let other = if outgoing { to } else { from };
    (outgoing, other)
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
            State::Ready if self.entries.is_empty() => match self.coverage.reach.clone() {
                Reach::Boundary(boundary) => {
                    let detail = format!(
                        "History stops at commit {}: Omnigraph does not compare across a schema \
                         change, and no commit since changed {}{}",
                        short_commit_id(&boundary.commit),
                        self.label.as_deref().unwrap_or(&self.id),
                        self.followed_words()
                    );
                    change_cards::schema_change_state(boundary.type_name.as_deref())
                        .child(studio_ui::ds::state_detail(detail))
                        .into_any_element()
                }
                // What was read before the read stopped, with its reason, in the end row.
                Reach::Stopped(_) => AnyView::from(self.list_view.clone())
                    .cached(StyleRefinement::default().size_full())
                    .into_any_element(),
                Reach::Window | Reach::GraphStart => {
                    let (title, detail) = self.empty_words();
                    studio_ui::ds::empty_state(IconName::Check, title)
                        .debug_selector(|| "node-history-empty".into())
                        .child(studio_ui::ds::state_detail(detail))
                        // The older read's progress and failure show here as in the end row.
                        .children(self.older_failure().map(|failure| {
                            div()
                                .debug_selector(|| "node-history-older-failed".into())
                                .child(studio_ui::ds::state_detail(failure))
                        }))
                        .children(self.older_button("read-older-empty", cx))
                        // The search for its creation is offered here too: the node is there,
                        // unchanged, and created before the window.
                        .children(self.render_search(cx))
                        .into_any_element()
                }
            },
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
            .when(self.offers_search(), |view| {
                view.on_action(
                    cx.listener(|this, _: &FindWhenCreated, _, cx| this.find_when_created(cx)),
                )
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
        title: String?\n    email: String?\n}\n\
        node Company {\n    slug: String @key\n}\n\
        edge Knows: Person -> Person\n\
        edge WorksAt: Person -> Company\n";

    /// Commits `c0` (the graph's first) to `c249` on main, newest first.
    const COMMITS: usize = 250;

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
        /// Commits at or before `c200` predate a schema change to Person: a read there is
        /// refused (E52).
        boundary: bool,
        /// `c220` is a load of 50,000 new people.
        load: bool,
        /// History at or before `c200` was reclaimed: a read there answers 410.
        gone: bool,
        /// A point read of the node before `c150` fails with a 500, as a search's might.
        old_reads_fail: bool,
        /// A point read of the node at `c230` fails with a 500.
        fails_at_c230: bool,
    }

    /// Who made commit `n`.
    fn actor(n: usize) -> &'static str {
        match n {
            5 => "act-old",
            235 => "act-ops",
            _ => "act-dev",
        }
    }

    /// A person at commit `n`: her properties, or `None` before she exists or after she is
    /// removed. Ada: created at c5, renamed by the merge at c225, email at c230, title at c239
    /// and c240. Zed: created at c100, removed at c245. Bob: retitled at c241. Hub: there from
    /// the graph's first commit, never changed. Big: there from the first commit, removed at
    /// c245.
    fn person(slug: &str, n: usize) -> Option<Value> {
        match slug {
            "ada" if n >= 5 => Some(json!({
                "slug": "ada",
                "name": if n >= 225 { "Ada Lovelace" } else { "Ada" },
                "title": if n >= 240 { "C" } else if n >= 239 { "B" } else { "A" },
                "email": if n >= 230 { "ada@x" } else { "a@x" },
            })),
            "zed" if (100..245).contains(&n) => Some(json!({ "slug": "zed", "name": "Zed" })),
            "bob" => Some(json!({ "slug": "bob", "title": if n >= 241 { "B" } else { "A" } })),
            "hub" => Some(json!({ "slug": "hub" })),
            // There from the first commit, retitled at c150 only: before the first window.
            "early" => Some(json!({ "slug": "early", "title": if n >= 150 { "B" } else { "A" } })),
            // There from the first commit; works at 1,000 companies only before c210.
            "was-hub" => Some(json!({ "slug": "was-hub" })),
            "big" if n < 245 => Some(json!({ "slug": "big" })),
            _ => None,
        }
    }

    /// The edges of a person at commit `n` through the relationship the query names, as
    /// `(edge id, other end)`: Ada works at Acme from c230 and Grace knows her from c235; Zed
    /// knows Bob from c101 until he is removed at c245. Hub and Big work at 1,000 companies at
    /// every commit they are there: a page of 1,000, too large to follow.
    fn edges(slug: &str, query: &str, n: usize) -> Vec<(String, String)> {
        let incoming_knows = query.contains("$m $e:knows $n");
        let outgoing_works = query.contains("$n $e:worksAt $m");
        let outgoing_knows = query.contains("$n $e:knows $m");
        let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            list.iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect()
        };
        if (matches!(slug, "hub" | "big") || (slug == "was-hub" && n < 210)) && outgoing_works {
            return (0..NODE_HISTORY_EDGES)
                .map(|e| (format!("w{e:04}"), format!("co-{e:04}")))
                .collect();
        }
        match slug {
            "ada" if outgoing_works && n >= 230 => pairs(&[("w1", "acme")]),
            // Linked to Eve at c100 and unlinked at c120: empty at both ends of the window.
            "ada" if outgoing_knows && (100..120).contains(&n) => pairs(&[("k9", "eve")]),
            "ada" if incoming_knows && n >= 235 => pairs(&[("k1", "grace")]),
            "zed" if outgoing_knows && (101..245).contains(&n) => pairs(&[("k2", "bob")]),
            _ => Vec::new(),
        }
    }

    /// A server whose main holds [`COMMITS`] commits. It pages the listing as Omnigraph does,
    /// answers each point read at its snapshot, names `b1`'s branch, and notes every request.
    /// `/commits` fails with 500 while `failing` holds.
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
                KeyBinding::new("cmd-alt-f", FindWhenCreated, Some("NodeHistory")),
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
                "merged_parent_commit_id": merged, "actor_id": actor(n),
            })
        };
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let param = |name: &str| {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                    .map(str::to_owned)
            };
            let limit: usize = param("limit").and_then(|l| l.parse().ok()).unwrap_or(100);
            let offset: usize = param("page_token")
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
            let fails = fails.load(Ordering::SeqCst);
            let noted = noted.clone();
            async move {
                let mut body = Vec::new();
                use futures::AsyncReadExt as _;
                request.into_body().read_to_end(&mut body).await.ok();
                let sent: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let name = sent["name"].as_str().unwrap_or_default().to_owned();
                noted.lock().unwrap().push(if name.is_empty() {
                    format!("{path}?{query}")
                } else {
                    format!("{path} {name} {}", sent["snapshot"].as_str().unwrap_or("-"))
                });
                let (status, answer) = if path.ends_with("/schema") {
                    (200, json!({ "schema_source": SCHEMA }))
                } else if path.ends_with("/commits") && fails {
                    (
                        500,
                        json!({ "error": "the server broke", "code": "internal" }),
                    )
                } else if path.ends_with("/commits") {
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
                            "created_at": at(224), "kind": "published",
                            "graph_branch": "feature" }),
                    )
                } else if path.ends_with("/query") {
                    let snapshot = sent["snapshot"].as_str().unwrap_or("c249");
                    let n: usize = snapshot.trim_start_matches('c').parse().unwrap();
                    let slug = sent["params"]["id"].as_str().unwrap_or_default();
                    let text = sent["query"].as_str().unwrap_or_default();
                    if served.boundary && n <= 200 {
                        (
                            400,
                            json!({ "error": "property 'email' of node type 'Person' does not \
                                exist at this snapshot", "code": "bad_request" }),
                        )
                    } else if served.gone && n <= 200 {
                        (
                            410,
                            json!({ "error": "history before this commit was reclaimed",
                                "code": "gone" }),
                        )
                    } else if (served.fails_at_c230 && n == 230 && name == "node_history_node")
                        || (served.old_reads_fail && n < 150)
                    {
                        (
                            500,
                            json!({ "error": "the server broke", "code": "internal" }),
                        )
                    } else if name == "node_history_node" {
                        let mut row = person(slug, n);
                        if let Some(Value::Object(node)) = &mut row {
                            node.insert("@id".into(), json!(slug));
                        }
                        let rows: Vec<Value> = row.map(|n| json!({ "n": n })).into_iter().collect();
                        (200, read(&name, snapshot, rows))
                    } else {
                        let rows: Vec<Value> = edges(slug, text, n)
                            .into_iter()
                            .map(|(edge, id)| json!({ "edge": edge, "id": id }))
                            .collect();
                        (200, read(&name, snapshot, rows))
                    }
                } else {
                    (200, json!({}))
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(answer.to_string()))
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
        let _ = served.load;
        (session, asked, failing)
    }

    /// A query's answer at `snapshot`.
    fn read(name: &str, snapshot: &str, rows: Vec<Value>) -> Value {
        json!({ "query_name": name, "target": { "branch": null, "snapshot": snapshot },
            "row_count": rows.len(), "rows": rows, "graph_commit_id": snapshot })
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

    fn by_view(session: &Entity<GraphSession>, what: &str, cx: &mut VisualTestContext) -> u64 {
        session.read_with(cx, |session, _| {
            session
                .ledger()
                .tallies()
                .filter(|((name, asker, answered), _)| {
                    name.as_ref() == what
                        && *asker == Asker::View(ReadView::NodeHistory)
                        && answered.sent()
                })
                .map(|(_, tally)| tally.count)
                .sum()
        })
    }

    /// A node's history lists the branch's newest 50 commits and reads the node at each and at
    /// the oldest one's parent, and its edges through every relationship group at each; then
    /// the merged branch. Each read is named in the ledger. It shows the commits that changed
    /// the node or a relationship at either of its ends, newest first; a run of one actor's edits
    /// within ten minutes folds into one, with its net change. The 200 commits before are read
    /// on asking.
    #[gpui::test]
    async fn a_nodes_history_reads_the_node_at_each_commit(cx: &mut TestAppContext) {
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
        assert_eq!(shown, rows, "c241 and c245 changed others; c5 is older");
        // 1 listing; the node at c249..c200 and c199; its 3 relationship groups at each, as she
        // exists at each; 1 merge.
        assert_eq!(by_view(&session, "node_history_commits", cx), 1);
        assert_eq!(by_view(&session, "node_history_node", cx), 51);
        assert_eq!(by_view(&session, "node_history_edges", cx), 3 * 51);
        assert_eq!(by_view(&session, "node_history_merge", cx), 1);
        {
            let asked = asked.lock().unwrap();
            assert!(
                asked[0].contains("/commits?branch=main&limit=50"),
                "{asked:?}"
            );
            assert!(
                asked.iter().all(|a| !a.contains("/changes")),
                "the change feed is not read"
            );
            assert!(asked.iter().any(|a| a.ends_with("node_history_node c199")));
        }
        view.read_with(cx, |view, cx| {
            assert_eq!(view.requests, 1 + 51 + 153 + 1);
            assert_eq!(view.commits.len(), 50);
            assert_eq!(view.label.as_deref(), Some("Ada Lovelace"));
            assert!(view.offers_older());
            let (node, _) = net_of(&view.entries[view.runs[0].clone()]);
            let node = node.unwrap();
            assert_eq!(node.before.unwrap().properties["title"], json!("A"));
            assert_eq!(node.after.unwrap().properties["title"], json!("C"));
            let merge = view.entries.iter().find(|e| e.commit == "c225").unwrap();
            assert_eq!(merge.merged_branch.as_deref(), Some("feature"));
            let last = view.session.read(cx).last_change("Person", "ada").cloned();
            assert_eq!(
                last.map(|l| (l.commit, l.head)),
                Some(("c240".into(), "c249".into()))
            );
        });
        // The 200 commits before: Eve's link, made at c100 and unmade at c120 with Ada's
        // outgoing Knows empty at both ends of the window, is seen.
        view.update(cx, |view, cx| view.read_older(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let shown = view.shown();
            assert_eq!(shown[shown.len() - 3].0, commits(&[120]));
            assert_eq!(shown[shown.len() - 2].0, commits(&[100]));
            assert_eq!(shown[shown.len() - 1].0, commits(&[5]), "her creation");
            assert_eq!(view.commits.len(), COMMITS);
        });
        let asked = asked.lock().unwrap();
        assert!(
            asked
                .iter()
                .any(|a| a.contains("/commits?branch=main&limit=200&page_token=50")),
            "{asked:?}"
        );
    }

    /// A relationship between two types whose nodes share a key reads from the schema's ends:
    /// Company `acme`'s WorksAt from Person `acme` is incoming.
    #[test]
    fn a_shared_key_in_another_type_does_not_turn_a_line_around() {
        let schema = gq_language::SchemaModel::parse(SCHEMA).unwrap();
        let edge = FoldedChange {
            kind: graph_store::EntityKind::Edge,
            type_id: "WorksAt".into(),
            type_name: "WorksAt".into(),
            id: "w9".into(),
            before: None,
            after: Some(graph_client::api::ChangeImageOutput {
                properties: Default::default(),
                endpoints: Some(json!({ "from": "acme", "to": "acme" })),
            }),
            commits: vec!["c1".into()],
        };
        assert_eq!(
            node_end(&edge, "Company", "acme", Some(&schema)),
            (false, "acme".to_owned())
        );
        assert_eq!(
            node_end(&edge, "Person", "acme", Some(&schema)),
            (true, "acme".to_owned())
        );
    }

    /// Older commits land below the reader: the row at the top of the view stays there.
    #[gpui::test]
    async fn older_commits_land_below_the_reader(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "ada", cx);
        // A window short enough that the list scrolls.
        cx.simulate_resize(gpui::size(px(900.), px(240.)));
        cx.run_until_parked();
        view.update(cx, |view, _| {
            view.list_state.scroll_to(gpui::ListOffset {
                item_ix: 2,
                offset_in_item: px(0.),
            })
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.list_state.logical_scroll_top().item_ix, 2, "scrolled");
        });
        view.update(cx, |view, cx| view.read_older(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.commits.len(), COMMITS);
            assert_eq!(view.list_state.logical_scroll_top().item_ix, 2);
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
                let (from, to) = entry.edges[0].endpoints().unwrap();
                (from == "ada", if from == "ada" { to } else { from })
            };
            assert_eq!(ends("c235"), (false, "grace".to_owned()), "incoming");
            assert_eq!(ends("c230"), (true, "acme".to_owned()), "outgoing");
        });
    }

    /// A window that does not reach the node's creation offers Find when it was created, which
    /// lists the commits before and binary-searches them with point reads, finding it in a few;
    /// reading the 200 commits before then shows the creation as an entry, read from the node,
    /// and the search is offered no more.
    #[gpui::test]
    async fn the_end_row_finds_when_the_node_was_created(cx: &mut TestAppContext) {
        let (session, asked, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "ada", cx);
        view.read_with(cx, |view, _| {
            assert!(view.offers_search(), "c5 is before the window");
        });
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        asked.lock().unwrap().clear();
        cx.simulate_keystrokes("cmd-alt-f");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let found = view.found.as_ref().expect("searched");
            assert!(
                matches!(&view.coverage.creation, Creation::Found(c) if c.id == "c5"),
                "{:?}",
                view.coverage.creation
            );
            assert!(found.requests <= 1 + 8, "a listing and log2(200) reads");
            assert!(!view.offers_search(), "it ran");
        });
        {
            let asked = asked.lock().unwrap();
            let reads = asked
                .iter()
                .filter(|a| a.contains("node_history_node"))
                .count();
            assert!((1..=8).contains(&reads), "{asked:?}");
        }
        assert!(by_view(&session, "node_history_search_commits", cx) >= 1);
        cx.simulate_keystrokes("cmd-alt-n");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.commits.len(),
                COMMITS,
                "it reached the graph's first commit"
            );
            assert!(!view.offers_older());
            assert!(view.entries.last().unwrap().created());
            assert!(view.search_from().is_none(), "its creation is an entry");
        });
    }

    /// A window that holds the node's creation offers no search.
    #[gpui::test]
    async fn a_window_with_the_creation_offers_no_search(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "zed", cx);
        view.read_with(cx, |view, _| assert!(view.offers_search(), "c100 is older"));
        view.update(cx, |view, cx| view.read_older(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.entries.iter().any(|e| e.created()));
            assert!(!view.offers_search());
        });
    }

    /// A node removed at the head still opens its history: its removal, with the relationship
    /// that went with it, and its creation are entries.
    #[gpui::test]
    async fn a_removed_node_shows_its_removal_and_creation(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "zed", cx);
        view.update(cx, |view, cx| view.read_older(cx));
        cx.run_until_parked();
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
            assert!(view.newest.as_ref().is_some_and(|n| !n.exists));
            assert_eq!(view.runs.len(), 3, "a creation and a removal never fold");
        });
    }

    /// A schema change between two commits stops the history there (E52): the commits since it
    /// are shown and no older window is offered.
    #[gpui::test]
    async fn a_schema_change_stops_the_history_and_says_so(cx: &mut TestAppContext) {
        let (session, _, _) = serve(
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
                view.coverage.reach,
                Reach::Boundary(graph_store::node_history::SchemaBoundary {
                    commit: "c201".into(),
                    type_name: Some("Person".into())
                })
            );
            assert_eq!(view.commits.len(), 48, "c202 to c249");
            assert_eq!(view.shown().len(), 4);
            assert!(!view.offers_older(), "the walk stops at the boundary");
        });
    }

    /// A window that stops short (history reclaimed before c201) keeps what it read and says why,
    /// offers no older window, and never says the node had no changes since the graph began.
    #[gpui::test]
    async fn a_window_that_stops_short_says_so_and_stops(cx: &mut TestAppContext) {
        let (session, _, _) = serve(
            Served {
                gone: true,
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, "nobody", cx);
        view.read_with(cx, |view, _| {
            assert!(matches!(view.state, State::Ready));
            assert!(view.entries.is_empty());
            let Reach::Stopped(stopped) = &view.coverage.reach else {
                panic!("the reason: {:?}", view.coverage.reach)
            };
            assert!(stopped.contains("reclaimed"), "{stopped}");
            assert!(!view.offers_older(), "no older window past a stop");
        });
        assert!(
            cx.debug_bounds("node-history-empty").is_none(),
            "not \"no commit changed it since the graph began\""
        );
        let (view, cx) = open(&session, "ada", cx);
        view.read_with(cx, |view, _| {
            assert!(matches!(view.coverage.reach, Reach::Stopped(_)));
            assert!(!view.offers_older());
            assert_eq!(view.shown().len(), 4, "what was read stays");
        });
    }

    /// A failed read of older commits leaves Find when it was created offered: it needs only
    /// where they go on, which the failed read keeps.
    #[gpui::test]
    async fn the_search_stays_offered_after_a_failed_older_read(cx: &mut TestAppContext) {
        let (session, _, failing) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "ada", cx);
        failing.store(true, Ordering::SeqCst);
        view.update(cx, |view, cx| view.read_older(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.older, Older::Failed(..)));
            assert!(view.offers_search(), "the search is still offered");
        });
        failing.store(false, Ordering::SeqCst);
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.simulate_keystrokes("cmd-alt-f");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.found.is_some(), "searched");
            assert!(matches!(&view.coverage.creation, Creation::Found(c) if c.id == "c5"));
        });
    }

    /// A node with a relationship group too large to follow never reads that no commit changed
    /// its relationships, nor "since the graph began": the empty state names the group, and no
    /// last change is claimed (review of #162).
    #[gpui::test]
    async fn a_group_too_large_to_follow_is_named_and_never_claims_the_start(
        cx: &mut TestAppContext,
    ) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "hub", cx);
        view.read_with(cx, |view, _| {
            assert!(view.entries.is_empty());
            assert!(!view.coverage.follows_every_group());
            assert!(!view.coverage.reaches_the_start());
            let (title, detail) = view.empty_words();
            assert_eq!(title, "No changes in the last 50 commits");
            assert!(!detail.contains("since the graph began"), "{detail}");
            assert!(!detail.contains("or its relationships"), "{detail}");
            assert!(
                detail.contains("relationships Studio could follow"),
                "{detail}"
            );
            assert!(detail.contains("WorksAt → Company"), "{detail}");
        });
        assert!(cx.debug_bounds("node-history-empty").is_some());
        let last = session.read_with(cx, |session, _| {
            session.last_change("Person", "hub").cloned()
        });
        assert!(last.is_none(), "no last change is claimed");
    }

    /// A node that is there, unchanged in the window and created before it, offers Find when it
    /// was created in its empty state, and shows what the search found there (Bugbot, #162).
    #[gpui::test]
    async fn an_unchanged_node_offers_the_search_in_its_empty_state(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "hub", cx);
        assert!(cx.debug_bounds("node-history-empty").is_some());
        assert!(cx.debug_bounds("node-history-search").is_some(), "offered");
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.simulate_keystrokes("cmd-alt-f");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(&view.coverage.creation, Creation::Found(c) if c.id == "c0"));
        });
        assert!(
            cx.debug_bounds("node-history-search").is_some(),
            "its note shows"
        );
    }

    /// A node unchanged in the first window gets its last change from an older window's newest
    /// entry, which the page then shows (Bugbot, #162).
    #[gpui::test]
    async fn an_older_windows_newest_entry_is_the_last_change(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "early", cx);
        let last = |cx: &mut VisualTestContext| {
            session.read_with(cx, |session, _| {
                session
                    .last_change("Person", "early")
                    .map(|l| (l.commit.clone(), l.head.clone()))
            })
        };
        assert_eq!(last(cx), None, "nothing in the first window");
        view.update(cx, |view, cx| view.read_older(cx));
        cx.run_until_parked();
        assert_eq!(last(cx), Some(("c150".into(), "c249".into())));
    }

    /// With nothing changed in the window, the empty state shows the older read's progress and
    /// its failure, with Try again, as the end row does (Bugbot, #162).
    #[gpui::test]
    async fn the_empty_state_shows_a_failed_older_read(cx: &mut TestAppContext) {
        let (session, _, failing) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "early", cx);
        assert!(cx.debug_bounds("node-history-empty").is_some());
        failing.store(true, Ordering::SeqCst);
        view.update(cx, |view, cx| view.read_older(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.older, Older::Failed(..)))
        });
        assert!(cx.debug_bounds("node-history-empty").is_some());
        assert!(
            cx.debug_bounds("node-history-older-failed").is_some(),
            "the failure is shown"
        );
    }

    /// A creation search whose point read fails can be run again: it failed, it did not finish
    /// (Bugbot, #162).
    #[gpui::test]
    async fn a_failed_creation_search_is_offered_again(cx: &mut TestAppContext) {
        let (session, _, _) = serve(
            Served {
                old_reads_fail: true,
                ..Default::default()
            },
            cx,
        );
        let (view, cx) = open(&session, "early", cx);
        view.read_with(cx, |view, _| assert!(view.offers_search()));
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.simulate_keystrokes("cmd-alt-f");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.search, Search::Failed(_)), "it failed");
            assert!(view.offers_search(), "and is offered again");
            assert_eq!(view.coverage.creation, Creation::NotSearched);
        });
    }

    /// A group full only at commits a failed read cut off says nothing of the history shown: the
    /// window stops at c231 and follows every group there (Bugbot, #162).
    #[gpui::test]
    async fn a_group_full_only_past_a_failed_read_is_not_named(cx: &mut TestAppContext) {
        let (session, _, _) = serve(
            Served {
                fails_at_c230: true,
                ..Default::default()
            },
            cx,
        );
        let (view, _cx) = open(&session, "was-hub", cx);
        view.read_with(_cx, |view, _| {
            assert!(matches!(view.coverage.reach, Reach::Stopped(_)));
            assert!(
                view.coverage.follows_every_group(),
                "{:?}",
                view.coverage.too_large
            );
        });
    }

    /// Any move of the branch's head past the one read offers Refresh, not only a commit the
    /// session reports as such (Bugbot, #162).
    #[gpui::test]
    async fn a_head_moved_past_the_one_read_offers_refresh(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "ada", cx);
        let moved = |head: &str, cx: &mut VisualTestContext| {
            let head = head.to_owned();
            session.update(cx, |_, cx| {
                cx.emit(SessionEvent::HeadMoved {
                    branch: "main".into(),
                    head,
                })
            });
            cx.run_until_parked();
        };
        moved("c249", cx);
        assert!(!view.read_with(cx, |view, _| view.newer), "the head read");
        moved("c250", cx);
        assert!(
            view.read_with(cx, |view, _| view.newer),
            "Refresh is offered"
        );
    }

    /// A removal whose relationships were too large to follow says so, not that none were read
    /// (review of #162; Bugbot).
    #[gpui::test]
    async fn a_removal_says_when_its_relationships_were_too_large(cx: &mut TestAppContext) {
        let (session, _, _) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "big", cx);
        view.read_with(cx, |view, _| {
            assert!(view.entries[0].removed());
            assert!(view.entries[0].edges.is_empty());
            let said = view.removal_words();
            assert!(said.contains("too large to follow"), "{said}");
            assert!(said.contains("WorksAt → Company"), "{said}");
        });
        let (view, cx) = open(&session, "zed", cx);
        view.read_with(cx, |view, _| {
            assert!(view.coverage.follows_every_group());
        });
    }

    /// A new credential forgets the name the old one read: the tab is named by the id until the
    /// history is read again under the new one.
    #[gpui::test]
    async fn a_new_credential_forgets_the_old_name(cx: &mut TestAppContext) {
        let (session, _, failing) = serve(Served::default(), cx);
        let (view, cx) = open(&session, "ada", cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.label.as_deref(), Some("Ada Lovelace"))
        });
        failing.store(true, Ordering::SeqCst);
        session.update(cx, |_, cx| cx.emit(SessionEvent::CredentialChanged));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(matches!(view.state, State::Failed(_)));
            assert_eq!(view.label, None, "the old credential's name is gone");
            assert_eq!(Item::tab_title(view, cx).as_ref(), "History · ada");
        });
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
        let (view, cx) = open(&session, "ada", cx);
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
        view.update(cx, |view, cx| view.reload(cx));
        view.read_with(cx, |view, cx| {
            assert!(matches!(view.state, State::Reading), "reading again");
            assert!(Item::settling(view, cx));
        });
        cx.run_until_parked();
        assert_eq!(listings(&asked), 2);
        view.read_with(cx, |view, _| assert!(matches!(view.state, State::Ready)));
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.dispatch_action(Refresh);
        cx.run_until_parked();
        assert_eq!(listings(&asked), 3, "⌘R reads it again");
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
        assert_eq!(
            selected(cx).as_deref(),
            Some("c240"),
            "the newest stays selected"
        );
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
