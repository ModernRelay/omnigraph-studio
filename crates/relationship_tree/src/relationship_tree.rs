//! The relationship tree (spec 005): a node, its relationship groups with counts, the nodes at
//! the other end, their groups, and so on, as a path tree over a walk ([`Exploration`]) it shares
//! with a canvas of the same root. It is a tab, a view of one node (spec 020).
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/005-relationship-tree/plan.md`.

mod rows;

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyContext,
    KeyDownEvent, ListSizingBehavior, Render, ScrollStrategy, Subscription,
    UniformListScrollHandle, Window, uniform_list,
};
use gq_language::builder::GroupBy;
use graph_store::{
    Entry, EntryKind, Exploration, ExplorationEvent, GraphSession, Loadable, NodeRef, SessionEvent,
};
use smallvec::SmallVec;
use studio_actions::graph::{OpenType, PeekNode};
use studio_actions::graph::{ShowNodeInTree, ShowNodeOnCanvas};
use studio_actions::tree::{
    ClearFilter, Collapse, CollapseAll, Expand, ExpandAll, OpenPage, OpenSelected, OpenTable,
    PeekSelected, RerootHere, SelectNext, SelectPrevious, ShowOnCanvas, ToggleEmptyGroups,
};
use studio_actions::workspace::Refresh;
use studio_ui::ds::space;
use studio_ui::menu::ContextMenu;
use studio_ui::type_swatch;
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace};
use ui::prelude::*;
use ui::{IndentGuideColors, PopoverMenu, StickyCandidate, Tooltip};

use rows::{RowId, RowTag};

const ROW_HEIGHT: f32 = studio_ui::ds::size::SIDEBAR_ROW;
const INDENT: f32 = studio_ui::ds::space::LG;
const LEFT_PAD: f32 = studio_ui::ds::space::MD;
const DISCLOSURE: f32 = studio_ui::ds::size::DISCLOSURE;

/// What a tree tab saves and comes back from.
const TREE_KIND: &str = "tree";

/// Registers the tree's tab and the action that opens it. The tree's keys are in `assets/keymaps/`.
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, TREE_KIND, |data, workspace, window, cx| {
        let node = data
            .get("type_name")
            .and_then(serde_json::Value::as_str)
            .zip(data.get("id").and_then(serde_json::Value::as_str));
        if let Some((type_name, id)) = node {
            let at_commit = data
                .get("at_commit")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            open_tree(
                workspace,
                NodeRef::new(type_name, id),
                at_commit,
                false,
                window,
                cx,
            );
        }
    });
    cx.observe_new(|workspace: &mut Workspace, window, _cx| {
        if window.is_none() {
            return;
        }
        workspace.register_action(|workspace, action: &ShowNodeInTree, window, cx| {
            // The tab it was opened from stays (spec 020).
            workspace.keep_active_tab(cx);
            open_tree(
                workspace,
                NodeRef::new(action.type_name.clone(), action.id.clone()),
                action.at_commit.clone(),
                action.preview,
                window,
                cx,
            );
        });
    })
    .detach();
}

fn tree_key(graph_id: &str, node: &NodeRef, at_commit: Option<&str>) -> SharedString {
    let key = format!("tree:{graph_id}:{}:{}", node.type_name, node.id);
    match at_commit {
        Some(commit) => format!("{key}@{commit}").into(),
        None => key.into(),
    }
}

fn open_tree(
    workspace: &mut Workspace,
    node: NodeRef,
    at_commit: Option<String>,
    preview: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    let key = tree_key(&session.read(cx).graph_id, &node, at_commit.as_deref());
    workspace.open_item_with::<RelationshipTree>(key, preview, window, cx, move |_, cx| {
        RelationshipTree::new(session, node, at_commit, cx)
    });
}

#[derive(Clone)]
struct StickyRow {
    row: usize,
    depth: usize,
}

impl StickyCandidate for StickyRow {
    fn depth(&self) -> usize {
        self.depth
    }
}

/// A row's element key: a hash of what the row is (its path and kind, and for a loading or
/// error row what it reads, since a group's count and its page can both fail), the same
/// across rebuilds wherever the row moves in the list.
fn row_key(entry: &Entry) -> usize {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rows::row_id(entry).hash(&mut hasher);
    if let EntryKind::Loading { target } | EntryKind::Error { target, .. } = &entry.kind {
        target.hash(&mut hasher);
    }
    hasher.finish() as usize
}

/// Whether a row opens, and whether it is open: a node that has relationships, a group or a
/// value with neighbours.
fn disclosure(entry: &Entry) -> (bool, bool) {
    match &entry.kind {
        EntryKind::Node {
            expandable,
            expanded,
            ..
        } => (*expandable, *expanded),
        EntryKind::Group {
            count, expanded, ..
        } => (*count != Loadable::Loaded(0), *expanded),
        EntryKind::Bucket {
            count, expanded, ..
        } => (*count > 0, *expanded),
        _ => (false, false),
    }
}

pub(crate) struct RelationshipTree {
    session: Entity<GraphSession>,
    /// The walk this tab shows, shared with a canvas of the same root and commit. The tab holds
    /// it: closing every tab on a walk ends it.
    exploration: Entity<Exploration>,
    root: NodeRef,
    /// The commit this tab is held at; none reads the branch head.
    at_commit: Option<String>,
    /// The walk changed while the tab was not drawn: rows are made again when it next is.
    rebuild_owed: bool,
    /// The walk's selection moved while the tab was not drawn.
    selection_owed: bool,
    /// The exploration's, shared with it and the canvas: made once per change.
    entries: Arc<Vec<Entry>>,
    /// Whether some relationship group in `entries` has no relationships: worked out with
    /// them, not on every draw of the actions.
    any_empty: bool,
    /// Indices into `entries` of the rows shown under the filter.
    visible: Vec<usize>,
    selected: Option<RowId>,
    /// The selected row's index at the last rebuild, to keep the cursor in place when the row
    /// it was on is replaced.
    selected_row_index_hint: Option<usize>,
    filter: String,
    /// Leave out relationship groups with no relationships.
    hide_empty: bool,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
    /// The read ahead for the row the pointer rests on, once it has rested, with that row's
    /// path.
    prefetch: Option<(graph_store::ExplorePath, gpui::Task<()>)>,
    /// A rebuild waiting for reads that finish together.
    _loaded: Option<gpui::Task<()>>,
}

impl Focusable for RelationshipTree {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for RelationshipTree {}

impl Item for RelationshipTree {
    fn tab_title(&self, cx: &App) -> SharedString {
        let state = self.exploration.read(cx).state();
        let name = rows::node_text(state, &self.root);
        match &self.at_commit {
            Some(commit) => format!("Tree: {name} @ {}", studio_ui::short_commit_id(commit)).into(),
            None => format!("Tree: {name}").into(),
        }
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTree)
    }

    fn dedupe_key(&self, cx: &App) -> Option<SharedString> {
        Some(tree_key(
            &self.session.read(cx).graph_id,
            &self.root,
            self.at_commit.as_deref(),
        ))
    }

    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        Some(studio_actions::Thing::Node {
            type_name: self.root.type_name.clone(),
            id: self.root.id.clone(),
        })
    }

    fn is_stale(&self, cx: &App) -> bool {
        self.exploration.read(cx).newer_available()
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: TREE_KIND.into(),
            data: serde_json::json!({
                "type_name": self.root.type_name,
                "id": self.root.id,
                "at_commit": self.at_commit,
            }),
        })
    }
}

impl RelationshipTree {
    fn new(
        session: Entity<GraphSession>,
        root: NodeRef,
        at_commit: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let exploration = Exploration::for_walk(&session, root.clone(), at_commit.clone(), cx);
        let subscriptions = vec![
            cx.subscribe(&exploration, |this, _, event, cx| match event {
                ExplorationEvent::Changed => {
                    this._loaded = None;
                    cx.emit(ItemEvent::UpdateTab);
                    this.owe_rebuild(cx);
                }
                // Reads that finish together (a node's counts) are shown in one go.
                ExplorationEvent::Loaded => {
                    if this._loaded.is_none() {
                        this._loaded = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(8))
                                .await;
                            this.update(cx, |this, cx| {
                                this._loaded = None;
                                cx.emit(ItemEvent::UpdateTab);
                                this.owe_rebuild(cx);
                            })
                            .ok();
                        }));
                    }
                }
                ExplorationEvent::SelectionChanged => {
                    this.selection_owed = true;
                    cx.notify();
                }
            }),
            cx.subscribe(&session, |this, _, event: &SessionEvent, cx| {
                if *event == SessionEvent::SchemaChanged {
                    this.owe_rebuild(cx);
                }
            }),
        ];
        let mut tree = Self {
            session,
            exploration,
            root,
            at_commit,
            rebuild_owed: false,
            selection_owed: false,
            entries: Arc::default(),
            any_empty: false,
            visible: Vec::new(),
            selected: None,
            selected_row_index_hint: None,
            filter: String::new(),
            hide_empty: false,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
            prefetch: None,
            _loaded: None,
        };
        tree.rebuild(cx);
        tree.follow_selection(cx);
        tree
    }

    /// Makes the rows again when the tab is next drawn: a hidden tab makes none at each answer
    /// (`docs/drawing.md`).
    fn owe_rebuild(&mut self, cx: &mut Context<Self>) {
        self.rebuild_owed = true;
        cx.notify();
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.rebuild_owed = false;
        self.entries = self.exploration.read(cx).entries();
        self.any_empty = self.entries.iter().any(|entry| {
            matches!(
                entry.kind,
                EntryKind::Group {
                    count: Loadable::Loaded(0),
                    ..
                }
            )
        });
        self.refilter(cx);
    }

    /// Recomputes the visible rows. Call after `entries` changed, or with the previous
    /// selected row when the rows it pointed into are gone.
    fn refilter(&mut self, cx: &mut Context<Self>) {
        let previous_row = self.selected_row_index_hint;
        self.visible = {
            {
                let exploration = self.exploration.read(cx);
                rows::filter(
                    &self.entries,
                    exploration.state(),
                    exploration.schema().as_deref(),
                    &self.filter,
                    self.hide_empty,
                )
            }
        };
        if let Some(selected) = &self.selected
            && rows::find(&self.visible, &self.entries, selected).is_none()
        {
            // A "Show more", loading or error row gives way to the rows it loaded: keep the
            // cursor where it was. Anything else falls back to its closest ancestor.
            let in_place = matches!(selected.tag, RowTag::More | RowTag::Loading | RowTag::Error)
                .then_some(previous_row)
                .flatten()
                .filter(|row| *row < self.visible.len());
            self.selected = in_place
                .or_else(|| rows::nearest(&self.visible, &self.entries, selected))
                .map(|row| rows::row_id(&self.entries[self.visible[row]]));
        }
        self.selected_row_index_hint = self.selected_row();
        cx.notify();
    }

    /// Selects the row the exploration's selection points at (a canvas click, a reveal).
    fn follow_selection(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.exploration.read(cx).selection().cloned() else {
            return;
        };
        let row = self.visible.iter().position(|ix| {
            let entry = &self.entries[*ix];
            entry.path == path
                && matches!(
                    entry.kind,
                    EntryKind::Node { .. } | EntryKind::Group { .. } | EntryKind::Bucket { .. }
                )
        });
        if let Some(row) = row {
            let entry = &self.entries[self.visible[row]];
            if self
                .selected
                .as_ref()
                .is_some_and(|id| rows::is_row(entry, id))
            {
                return;
            }
            self.selected = Some(rows::row_id(entry));
            self.scroll_handle
                .scroll_to_item(row, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    fn selected_row(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        // Where it last was first: the list asks every frame, and rows rarely move.
        if let Some(hint) = self.selected_row_index_hint
            && self
                .visible
                .get(hint)
                .is_some_and(|ix| rows::is_row(&self.entries[*ix], id))
        {
            return Some(hint);
        }
        rows::find(&self.visible, &self.entries, id)
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.selected_row()
            .map(|row| &self.entries[self.visible[row]])
    }

    fn select_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.visible.get(row).map(|ix| &self.entries[*ix]) else {
            return;
        };
        let id = rows::row_id(entry);
        let shared = matches!(
            entry.kind,
            EntryKind::Node { .. } | EntryKind::Group { .. } | EntryKind::Bucket { .. }
        )
        .then(|| entry.path.clone());
        self.selected = Some(id);
        self.selected_row_index_hint = Some(row);
        self.scroll_handle
            .scroll_to_item(row, ScrollStrategy::Nearest);
        if let Some(path) = shared {
            self.exploration
                .update(cx, |exploration, cx| exploration.select(Some(path), cx));
        }
        cx.notify();
    }

    fn with_exploration(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Exploration, &mut Context<Exploration>),
    ) {
        self.exploration.clone().update(cx, f);
    }

    // ---- actions ----

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let next = match self.selected_row() {
            Some(row) => (row + 1).min(self.visible.len().saturating_sub(1)),
            None => 0,
        };
        self.select_row(next, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let previous = self.selected_row().map_or(0, |row| row.saturating_sub(1));
        self.select_row(previous, cx);
    }

    fn expand(&mut self, _: &Expand, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return self.select_row(0, cx);
        };
        let entry = self.entries[self.visible[row]].clone();
        let (expandable, expanded) = disclosure(&entry);
        if expandable && !expanded {
            self.with_exploration(cx, |e, cx| e.expand(&entry.path, cx));
        } else if let Some(child) = rows::first_child(&self.visible, &self.entries, row) {
            self.select_row(child, cx);
        }
    }

    fn collapse(&mut self, _: &Collapse, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let entry = self.entries[self.visible[row]].clone();
        let expanded = matches!(
            entry.kind,
            EntryKind::Node { expanded: true, .. }
                | EntryKind::Group { expanded: true, .. }
                | EntryKind::Bucket { expanded: true, .. }
        );
        if expanded {
            self.with_exploration(cx, |e, cx| e.collapse(&entry.path, cx));
        } else if let Some(parent) = rows::parent_row(&self.visible, &self.entries, row) {
            self.select_row(parent, cx);
        }
    }

    fn expand_all(&mut self, _: &ExpandAll, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        match entry.kind {
            EntryKind::Node { .. } | EntryKind::Group { .. } => {
                self.with_exploration(cx, |e, cx| e.expand_all(&entry.path, cx));
            }
            EntryKind::Bucket { .. } => {
                self.with_exploration(cx, |e, cx| e.expand(&entry.path, cx));
            }
            _ => {}
        }
    }

    fn collapse_all(&mut self, _: &CollapseAll, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let entry = &self.entries[self.visible[row]];
        let path = match entry.kind {
            EntryKind::Node { .. } | EntryKind::Group { .. } | EntryKind::Bucket { .. } => {
                entry.path.clone()
            }
            _ => match rows::parent_row(&self.visible, &self.entries, row) {
                Some(parent) => {
                    let path = self.entries[self.visible[parent]].path.clone();
                    self.select_row(parent, cx);
                    path
                }
                None => return,
            },
        };
        self.with_exploration(cx, |e, cx| e.collapse_subtree(&path, cx));
    }

    fn open_selected(&mut self, _: &OpenSelected, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row() {
            self.activate(row, window, cx);
        }
    }

    /// What ↵ and a double click do on a row.
    fn activate(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.visible.get(row).map(|ix| self.entries[*ix].clone()) else {
            return;
        };
        match entry.kind {
            EntryKind::Node { node, .. } => self.open_node(&node, window, cx),
            EntryKind::Field {
                target: Some(target),
                ..
            } => self.open_node(&target.node, window, cx),
            EntryKind::Group { .. } | EntryKind::Bucket { .. } => {
                self.with_exploration(cx, |e, cx| e.toggle(&entry.path, cx));
            }
            EntryKind::More { key, .. } => {
                // With text typed, a group not read in full is searched on the server.
                let filter = self.filter.trim().to_owned();
                let searching = self.group_search(&key, cx);
                if !filter.is_empty() && searching.as_deref() != Some(filter.as_str()) {
                    self.with_exploration(cx, |e, cx| e.search_group(&key, &filter, cx));
                } else {
                    self.with_exploration(cx, |e, cx| e.load_more(&key, cx));
                }
            }
            EntryKind::Cycle { ancestor, .. } => {
                if let Ok(ancestor_row) = self.visible.binary_search(&ancestor) {
                    self.select_row(ancestor_row, cx);
                }
            }
            EntryKind::Error { target, .. } => {
                self.with_exploration(cx, |e, cx| e.retry(&target, cx));
            }
            EntryKind::Field { .. } | EntryKind::OtherValues { .. } | EntryKind::Loading { .. } => {
            }
        }
    }

    /// A node's page, at the commit this tree is held at.
    fn open_node(&self, node: &NodeRef, window: &mut Window, cx: &mut App) {
        window.dispatch_action(
            Box::new(studio_actions::graph::OpenNodePage {
                type_name: node.type_name.clone(),
                id: node.id.clone(),
                at_commit: self.at_commit.clone(),
            }),
            cx,
        );
    }

    /// Walks from the selected node in a tab of its own; Back returns to this one (spec 020).
    fn reroot_here(&mut self, _: &RerootHere, window: &mut Window, cx: &mut Context<Self>) {
        let Some(EntryKind::Node { node, .. }) = self.selected_entry().map(|entry| &entry.kind)
        else {
            return;
        };
        window.dispatch_action(
            Box::new(ShowNodeInTree {
                type_name: node.type_name.clone(),
                id: node.id.clone(),
                at_commit: self.at_commit.clone(),
                preview: true,
            }),
            cx,
        );
    }

    /// The node the selected row shows: a node, a cycle back to one, or a field's target.
    fn selected_node(&self) -> Option<NodeRef> {
        match &self.selected_entry()?.kind {
            EntryKind::Node { node, .. } | EntryKind::Cycle { node, .. } => Some(node.clone()),
            EntryKind::Field {
                target: Some(target),
                ..
            } => Some(target.node.clone()),
            _ => None,
        }
    }

    fn show_on_canvas(&mut self, _: &ShowOnCanvas, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.selected_node() else {
            return;
        };
        window.dispatch_action(
            Box::new(ShowNodeOnCanvas {
                type_name: node.type_name,
                id: node.id,
                at_commit: self.at_commit.clone(),
                preview: true,
            }),
            cx,
        );
    }

    fn clear_filter(&mut self, _: &ClearFilter, _: &mut Window, cx: &mut Context<Self>) {
        self.filter.clear();
        self.with_exploration(cx, |e, cx| e.clear_searches(cx));
        self.refilter(cx);
    }

    /// The text a group's neighbours are narrowed to on the server, if any.
    fn group_search(&self, key: &graph_store::GroupKey, cx: &App) -> Option<String> {
        self.exploration.read(cx).state().searches.get(key).cloned()
    }

    /// Space: the selected node in the inspector, focus staying in the tree.
    fn peek_selected(&mut self, _: &PeekSelected, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.selected_node() else {
            return;
        };
        window.dispatch_action(
            Box::new(PeekNode {
                type_name: node.type_name,
                id: node.id,
                at_commit: self.at_commit.clone(),
            }),
            cx,
        );
    }

    /// ⌘↵: the selected node's page (spec 014); a relationship group, which has no page,
    /// opens its table as ⌥↵ does.
    fn open_page(&mut self, _: &OpenPage, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected_entry().map(|entry| &entry.kind) {
            Some(EntryKind::Node { node, .. } | EntryKind::Cycle { node, .. }) => {
                window.dispatch_action(
                    Box::new(studio_actions::graph::OpenNodePage {
                        type_name: node.type_name.clone(),
                        id: node.id.clone(),
                        at_commit: self.at_commit.clone(),
                    }),
                    cx,
                );
            }
            _ => self.open_table(&OpenTable, window, cx),
        }
    }

    /// ⌥↵: a node's own type table filtered to it; on a relationship group, the other type's
    /// table filtered to the nodes related to this one.
    fn open_table(&mut self, _: &OpenTable, window: &mut Window, cx: &mut Context<Self>) {
        let action = match self.selected_entry().map(|entry| &entry.kind) {
            Some(EntryKind::Node { node, .. } | EntryKind::Cycle { node, .. }) => OpenType {
                type_name: node.type_name.clone(),
                at_commit: self.at_commit.clone(),
                filter_id: Some(node.id.clone()),
                via: None,
                preview: false,
            },
            Some(EntryKind::Group { key, .. } | EntryKind::Bucket { key, .. }) => {
                // Seen from the other type, the relationship runs the other way.
                let back = gq_language::builder::reversed(&key.group, &key.node.type_name);
                OpenType {
                    type_name: key.group.other_type.clone(),
                    at_commit: self.at_commit.clone(),
                    filter_id: Some(key.node.id.clone()),
                    via: Some(gq_language::builder::relationship_key(&back)),
                    preview: false,
                }
            }
            _ => return,
        };
        window.dispatch_action(Box::new(action), cx);
    }

    fn toggle_empty_groups(
        &mut self,
        _: &ToggleEmptyGroups,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hide_empty = !self.hide_empty;
        self.refilter(cx);
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if modifiers.control || modifiers.platform || modifiers.alt || modifiers.function {
            return;
        }
        if keystroke.key == "backspace" {
            if self.filter.pop().is_some() {
                // An emptied filter is a cleared one: a group narrowed on the server for it
                // reads whole again.
                if self.filter.is_empty() {
                    self.with_exploration(cx, |e, cx| e.clear_searches(cx));
                }
                self.refilter(cx);
                cx.stop_propagation();
            }
            return;
        }
        let Some(text) = keystroke.key_char.as_deref() else {
            return;
        };
        if text.chars().all(|c| !c.is_control()) && !text.is_empty() {
            if self.filter.is_empty() && text == " " {
                return;
            }
            self.filter.push_str(text);
            self.refilter(cx);
            cx.stop_propagation();
        }
    }

    // ---- rendering ----

    fn render_row(
        &self,
        row: usize,
        selected: bool,
        focused: bool,
        sticky: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected_bg = studio_ui::ds::surface::selected_chrome(cx);
        let hover_bg = studio_ui::ds::surface::hover(cx);
        let focus_border = studio_ui::ds::surface::focus(cx);
        let panel_bg = studio_ui::ds::surface::chrome(cx);
        let entry = &self.entries[self.visible[row]];
        let state = self.exploration.read(cx).state();

        let (disclosure, expanded) = disclosure(entry);
        let toggle_path = entry.path.clone();
        let chevron = div()
            .id((
                if sticky {
                    "sticky-disclosure"
                } else {
                    "disclosure"
                },
                row,
            ))
            .w(px(DISCLOSURE))
            .flex_none()
            .when(disclosure, |d| {
                d.child(studio_ui::ds::icon::meta(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                }))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    let path = toggle_path.clone();
                    this.with_exploration(cx, |e, cx| e.toggle(&path, cx));
                }))
            });

        let content: AnyElement = match &entry.kind {
            EntryKind::Node { node, via, .. } => {
                let root = entry.path.is_root();
                let label = state.label(node).map(str::to_owned);
                // The key shows on the root and the selected row; elsewhere the tooltip has it.
                let key = (label.is_some() && (root || selected)).then(|| node.id.clone());
                // One short edge value, right-aligned like a count; the tooltip has them all.
                let hint = via.as_ref().and_then(|via| rows::edge_hint(&via.edge));
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap(px(space::SM))
                    // The root carries the type's mark; below it a dot, since each
                    // relationship already names the type it leads to.
                    .child(if root {
                        type_swatch(&node.type_name, cx)
                    } else {
                        studio_ui::ds::type_dot(&node.type_name)
                    })
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            studio_ui::ds::text::body(label.unwrap_or_else(|| node.id.clone()))
                                .when(root, |label| {
                                    label.weight(studio_ui::ds::text::weight::HEADING)
                                })
                                .truncate(),
                        ),
                    )
                    .children(key.map(|key| {
                        div()
                            .min_w(px(studio_ui::ds::size::SHORT_LABEL))
                            .flex_shrink(1.)
                            .child(studio_ui::ds::text::meta(key).truncate_start())
                    }))
                    .child(div().flex_1())
                    .children(hint.map(|hint| studio_ui::ds::text::meta(hint).single_line()))
                    .into_any_element()
            }
            EntryKind::Cycle { node, .. } => h_flex()
                .min_w_0()
                .flex_1()
                .gap(px(space::SM))
                .child(studio_ui::ds::text::muted("↺"))
                .child(type_swatch(&node.type_name, cx))
                .child(
                    div()
                        .min_w_0()
                        .flex_shrink(1.)
                        .child(studio_ui::ds::text::muted(rows::node_text(state, node)).truncate()),
                )
                .child(
                    studio_ui::ds::text::meta("already above")
                        .color(Color::Disabled)
                        .single_line(),
                )
                .into_any_element(),
            EntryKind::Field {
                key,
                target,
                loading,
            } => {
                let value = match target {
                    Some(target) => rows::node_text(state, &target.node),
                    None if *loading => "…".to_owned(),
                    None => "—".to_owned(),
                };
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap(px(space::SM))
                    .child(
                        studio_ui::ds::text::meta(format!(
                            "{} →",
                            gq_language::traversal_name(&key.group.edge)
                        ))
                        .single_line(),
                    )
                    // The target's dot, as on a neighbour row; none when there is no target.
                    .when(target.is_some(), |row| {
                        row.child(studio_ui::ds::type_dot(&key.group.other_type))
                    })
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            studio_ui::ds::text::body(value)
                                .when(target.is_none(), |label| label.color(Color::Muted))
                                .truncate(),
                        ),
                    )
                    .into_any_element()
            }
            EntryKind::Group { key, count, .. } => {
                let empty = *count == Loadable::Loaded(0);
                let text_color = if empty {
                    Color::Disabled
                } else {
                    Color::Default
                };
                let muted = if empty { Color::Disabled } else { Color::Muted };
                let arrow = key.group.direction.arrow();
                let count_label = match count {
                    Loadable::Loaded(n) => n.to_string(),
                    Loadable::Failed(_) => "!".to_owned(),
                    Loadable::NotLoaded | Loadable::Loading => "…".to_owned(),
                };
                let grouped_by = self
                    .exploration
                    .read(cx)
                    .grouping(&key.group)
                    .map(GroupBy::label);
                let group_menu = (!empty)
                    .then(|| self.group_by_menu(row, entry, &key.group, sticky, selected, cx))
                    .flatten();
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap(px(space::SM))
                    .child(studio_ui::ds::text::body(arrow).color(muted))
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            studio_ui::ds::text::body(key.group.edge.clone())
                                .color(text_color)
                                .truncate(),
                        ),
                    )
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            studio_ui::ds::text::meta(key.group.other_type.clone())
                                .color(muted)
                                .truncate(),
                        ),
                    )
                    .children(grouped_by.map(|property| {
                        div().min_w_0().flex_shrink(1.).child(
                            studio_ui::ds::text::meta(format!("by {property}"))
                                .color(Color::Accent)
                                .truncate(),
                        )
                    }))
                    .child(div().flex_1())
                    .children(group_menu)
                    .child(
                        studio_ui::ds::text::meta(count_label)
                            .color(if matches!(count, Loadable::Failed(_)) {
                                Color::Error
                            } else {
                                muted
                            })
                            .single_line(),
                    )
                    .into_any_element()
            }
            EntryKind::Bucket { key, count, .. } => {
                let missing = key
                    .within
                    .as_ref()
                    .is_none_or(|within| within.value.is_null());
                let schema = self.exploration.read(cx).schema();
                let text = rows::bucket_row_text(key, state, schema.as_deref());
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap(px(space::SM))
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            studio_ui::ds::text::body(text)
                                .color(if missing {
                                    Color::Muted
                                } else {
                                    Color::Default
                                })
                                .when(missing, |label| label.italic())
                                .truncate(),
                        ),
                    )
                    .child(div().flex_1())
                    .child(studio_ui::ds::text::meta(count.to_string()).single_line())
                    .into_any_element()
            }
            EntryKind::OtherValues { shown, rest } => {
                studio_ui::ds::text::meta(rows::other_values_label(*shown, *rest))
                    .single_line()
                    .into_any_element()
            }
            EntryKind::More { shown, total, key } => {
                let filter = self.filter.trim();
                let searching = self.group_search(key, cx);
                let text = if !filter.is_empty() && searching.as_deref() != Some(filter) {
                    match total {
                        Some(total) => format!("Find “{filter}” among {total}"),
                        None => format!("Find “{filter}” in this group"),
                    }
                } else if let Some(search) = searching {
                    format!("Show more matching “{search}”")
                } else {
                    rows::more_label(*shown, *total, self.exploration.read(cx).page_size(cx))
                };
                studio_ui::ds::text::meta(text)
                    .color(Color::Accent)
                    .single_line()
                    .into_any_element()
            }
            EntryKind::Loading { .. } => studio_ui::ds::text::meta("Reading…")
                .single_line()
                .into_any_element(),
            EntryKind::Error { target, message } => {
                let target = target.clone();
                let tooltip: SharedString = message.clone().into();
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap(px(space::XS))
                    .child(studio_ui::ds::icon::meta(IconName::Warning).color(Color::Error))
                    .child(
                        div()
                            .id(("error", row))
                            .min_w_0()
                            .flex_shrink(1.)
                            .tooltip(Tooltip::text(tooltip))
                            .child(studio_ui::ds::text::error_note(message.clone()).truncate()),
                    )
                    .child(
                        studio_ui::ds::row_button(("retry", row), "Retry")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let target = target.clone();
                                this.with_exploration(cx, |e, cx| e.retry(&target, cx));
                            })),
                    )
                    .into_any_element()
            }
        };

        // A closed node or relationship the pointer rests on reads ahead what it opens to.
        let prefetch_path = match &entry.kind {
            EntryKind::Node {
                expanded: false, ..
            }
            | EntryKind::Group {
                expanded: false, ..
            }
            | EntryKind::Bucket {
                expanded: false, ..
            } => Some(entry.path.clone()),
            _ => None,
        };
        // A node's hover card: what it is, and the properties of the edge that led here.
        let card = match &entry.kind {
            EntryKind::Node { node, via, .. } => {
                let relationship =
                    entry
                        .parent
                        .and_then(|parent| match &self.entries.get(parent)?.kind {
                            // As the group row reads it: `→ WorksAt`.
                            EntryKind::Group { key, .. } | EntryKind::Bucket { key, .. } => {
                                let arrow = key.group.direction.arrow();
                                Some(format!("{arrow} {}", key.group.edge))
                            }
                            _ => None,
                        });
                Some(HoverCard {
                    type_name: node.type_name.clone(),
                    id: node.id.clone(),
                    label: state.label(node).map(str::to_owned),
                    relationship,
                    edge: via.as_ref().map(|via| via.edge.clone()).unwrap_or_default(),
                })
            }
            _ => None,
        };

        // A click goes to the listeners of the frame drawn last, and the rows may have changed
        // since: it acts on the row it was drawn for, found again by what that row is.
        let identity = rows::row_id(entry);
        // Keyed by what the row is, not where it is: GPUI keeps an element's hover by its id, so
        // a row the pointer rests on while rows above it come or go stays the row its read
        // ahead is for, and leaving it stops that read, not another row's.
        h_flex()
            .id((
                if sticky { "sticky-row" } else { "tree-row" },
                row_key(entry),
            ))
            .when(!sticky, |d| {
                d.debug_selector(move || format!("tree-row-{row}"))
            })
            .group("tree-row")
            .w_full()
            .h(px(ROW_HEIGHT))
            .pl(px(LEFT_PAD + entry.depth as f32 * INDENT))
            .pr(px(space::MD))
            .gap(px(space::XS))
            .items_center()
            .bg(panel_bg)
            .border_l(px(studio_ui::ds::size::ACCENT_LINE))
            .border_color(gpui::transparent_black())
            .when(selected, |d| d.bg(selected_bg))
            .when(selected && focused, |d| d.border_color(focus_border))
            .hover(|style| style.bg(hover_bg))
            .when_some(prefetch_path, |row, path| {
                row.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    this.prefetch(&path, *hovered, cx)
                }))
            })
            .when_some(card, |d, card| {
                d.tooltip(Tooltip::element(move |_, cx| card.render(cx)))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                let Some(row) = rows::find(&this.visible, &this.entries, &identity) else {
                    return;
                };
                this.select_row(row, cx);
                window.focus(&this.focus_handle, cx);
                let kind = this.visible.get(row).map(|ix| &this.entries[*ix].kind);
                let single_activates = matches!(
                    kind,
                    Some(
                        EntryKind::Group { .. }
                            | EntryKind::Bucket { .. }
                            | EntryKind::More { .. }
                            | EntryKind::Cycle { .. }
                    )
                );
                // A row a click opens takes a double click's second click as nothing more, so
                // it does not close again; a node row opens on the second.
                let clicks = event.click_count();
                if (single_activates && clicks == 1) || (!single_activates && clicks == 2) {
                    this.activate(row, window, cx);
                }
            }))
            .child(chevron)
            .child(content)
            .into_any_element()
    }

    /// The button on a relationship group that groups its neighbours by a property of the
    /// other type or of the edge, shown while the row is hovered or selected.
    fn group_by_menu(
        &self,
        row: usize,
        entry: &Entry,
        group: &gq_language::RelationshipGroup,
        sticky: bool,
        selected: bool,
        cx: &App,
    ) -> Option<AnyElement> {
        let exploration = self.exploration.clone();
        let schema = exploration.read(cx).schema()?;
        if !GroupBy::has_choices(&schema, group) {
            return None;
        }
        let current = exploration.read(cx).grouping(group).cloned();
        // The trigger shows while the row is hovered or selected, or while grouping is on.
        let always = selected || current.is_some();
        let group = group.clone();
        let path = entry.path.clone();
        let id = (
            if sticky {
                "sticky-group-by"
            } else {
                "group-by"
            },
            row,
        );
        let tooltip = match &current {
            Some(by) => format!("Grouped by {}", by.label()),
            None => "Group by a property or a relationship".to_owned(),
        };
        let menu = PopoverMenu::new(SharedString::from(format!("{}-{row}", id.0)))
            .trigger(
                studio_ui::ds::row_icon_button(id, IconName::ListTree)
                    .icon_color(Color::Muted)
                    .toggle_state(current.is_some())
                    .tooltip(Tooltip::text(tooltip)),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                // Built when the menu opens, not for every row drawn.
                let sections = GroupBy::sections(&schema, &group);
                let (exploration, group, path, current) = (
                    exploration.clone(),
                    group.clone(),
                    path.clone(),
                    current.clone(),
                );
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    // Choosing a property opens the group on its values.
                    let set = |by: Option<GroupBy>| {
                        let (exploration, group, path) =
                            (exploration.clone(), group.clone(), path.clone());
                        move |_: &mut Window, cx: &mut App| {
                            exploration.update(cx, |e, cx| {
                                let open = by.is_some();
                                e.set_grouping(&group, by.clone(), cx);
                                if open {
                                    e.expand(&path, cx);
                                }
                            });
                        }
                    };
                    menu = menu.toggleable_entry(
                        "No grouping",
                        current.is_none(),
                        IconPosition::Start,
                        None,
                        set(None),
                    );
                    for (heading, section) in &sections {
                        menu = menu.separator().header(heading.clone());
                        for by in section {
                            menu = menu.toggleable_entry(
                                by.label(),
                                current.as_ref() == Some(by),
                                IconPosition::Start,
                                None,
                                set(Some(by.clone())),
                            );
                        }
                    }
                    menu
                }))
            });
        Some(
            div()
                .flex_none()
                .when(!always, |d| d.visible_on_hover("tree-row"))
                .child(menu)
                .into_any_element(),
        )
    }

    /// The tab's toolbar: hide relationships with none, and close everything below the root.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let hide_empty = self.hide_empty;
        studio_ui::ds::toolbar(cx).child(div().flex_1()).child(
            studio_ui::ds::toolbar_actions()
                // The toggle shows only when it changes something: an empty group, or
                // hiding on.
                .when(hide_empty || self.any_empty, |row| {
                    row.child(
                        studio_ui::ds::row_icon_button(
                            "toggle-empty-groups",
                            if hide_empty {
                                IconName::EyeOff
                            } else {
                                IconName::Eye
                            },
                        )
                        .icon_color(Color::Muted)
                        .toggle_state(hide_empty)
                        .tooltip(move |_, cx| {
                            Tooltip::for_action(
                                if hide_empty {
                                    "Show relationships with none"
                                } else {
                                    "Hide relationships with none"
                                },
                                &ToggleEmptyGroups,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_empty_groups(&ToggleEmptyGroups, window, cx)
                        })),
                    )
                })
                .child(
                    studio_ui::ds::row_icon_button("collapse-to-root", IconName::ListCollapse)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Collapse all"))
                        .on_click(cx.listener(|this, _, _, cx| this.collapse_to_root(cx))),
                ),
        )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.exploration.read(cx);
        let newer = e.newer_available();
        let filter = self.filter.clone();
        if !newer && filter.is_empty() {
            return None;
        }
        let field = studio_ui::ds::surface::sunken(cx);
        Some(
            v_flex()
                .flex_none()
                .px(px(space::MD))
                .pb(px(space::XS))
                .gap(px(space::XS))
                .when(newer, |d| {
                    d.child(
                        studio_ui::ds::row_button("newer", "Newer data · Refresh")
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .tooltip({
                                let focus = self.focus_handle.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        "The branch moved since this tree was read. Refresh reads it again.",
                                        &Refresh,
                                        &focus,
                                        cx,
                                    )
                                }
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.with_exploration(cx, |e, cx| e.refresh(cx));
                            })),
                    )
                })
                // What is typed filters the rows; it reads as a field so it is not taken for
                // a row.
                .when(!filter.is_empty(), |d| {
                    d.child(
                        h_flex()
                            .h(px(ROW_HEIGHT))
                            .px(px(space::MD))
                            .gap(px(space::SM))
                            .rounded(px(studio_ui::ds::radii(cx).sm))
                            .bg(field)
                            .child(
                                studio_ui::ds::icon::meta(IconName::MagnifyingGlass),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .child(studio_ui::ds::text::body(filter).truncate()),
                            )
                            .child(
                                studio_ui::ds::text::meta("esc")
                                    .color(Color::Disabled),
                            ),
                    )
                })
                .into_any_element(),
        )
    }

    /// Reads ahead what opening `path` will show once the pointer has rested on its row;
    /// leaving the row first reads nothing, and leaving it after stops what was read ahead for
    /// it and is still out ([`Exploration::end_prefetch`]). A row the pointer left cancels only
    /// its own reads: moving down, the row entered hears the move before the row left does.
    fn prefetch(&mut self, path: &graph_store::ExplorePath, hovered: bool, cx: &mut Context<Self>) {
        if !hovered {
            if self
                .prefetch
                .as_ref()
                .is_some_and(|(pending, _)| pending == path)
            {
                self.prefetch = None;
            }
            self.with_exploration(cx, |e, cx| e.end_prefetch(path, cx));
            return;
        }
        let exploration = self.exploration.clone();
        let read = path.clone();
        let task = cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            exploration.update(cx, |e, cx| e.prefetch(&read, cx));
        });
        self.prefetch = Some((path.clone(), task));
    }

    /// Closes everything below the root.
    fn collapse_to_root(&mut self, cx: &mut Context<Self>) {
        let root = graph_store::ExplorePath::root();
        self.with_exploration(cx, |e, cx| {
            e.collapse_subtree(&root, cx);
            e.expand(&root, cx);
        });
    }

    /// The rows, in what the panel leaves below its header: the tree sizes itself from its
    /// dock, so the row the keyboard selects is scrolled into what shows.
    fn render_list(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.focus_handle.contains_focused(window, cx);
        let indent_colors = IndentGuideColors::panel(cx);
        let sticky_colors = IndentGuideColors::panel(cx);
        uniform_list(
            "relationship-tree",
            self.visible.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let selected = this.selected_row();
                range
                    .map(|row| this.render_row(row, selected == Some(row), focused, false, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .with_decoration(
            ui::indent_guides(px(INDENT), indent_colors)
                .with_left_offset(px(LEFT_PAD + 2. + DISCLOSURE / 2.))
                .with_compute_indents_fn(cx.entity(), |this, range, _window, _cx| {
                    range
                        .map(|row| this.entries[this.visible[row]].depth)
                        .collect::<SmallVec<[usize; 64]>>()
                }),
        )
        .with_decoration(
            ui::sticky_items(
                cx.entity(),
                |this, range, _window, _cx| {
                    range
                        .map(|row| StickyRow {
                            row,
                            depth: this.entries[this.visible[row]].depth,
                        })
                        .collect()
                },
                move |this, anchor: StickyRow, window, cx| {
                    let focused = this.focus_handle.contains_focused(window, cx);
                    let selected = this.selected_row();
                    let entry = this.visible[anchor.row];
                    rows::ancestors(&this.entries, entry)
                        .into_iter()
                        // The rows drawn are in tree order.
                        .filter_map(|ix| this.visible.binary_search(&ix).ok())
                        .map(|row| this.render_row(row, selected == Some(row), focused, true, cx))
                        .collect()
                },
            )
            .with_decoration(
                ui::indent_guides(px(INDENT), sticky_colors)
                    .with_left_offset(px(LEFT_PAD + 2. + DISCLOSURE / 2.)),
            ),
        )
        .with_sizing_behavior(ListSizingBehavior::Auto)
        .track_scroll(&self.scroll_handle)
        .debug_selector(|| "relationship-tree-list".into())
        .flex_1()
        .min_h_0()
        .pb(px(space::MD))
        .into_any_element()
    }
}

impl Render for RelationshipTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // What the session's events left to read is read once the tree is drawn.
        if self.exploration.read(cx).owes_reads() {
            let exploration = self.exploration.clone();
            cx.defer(move |cx| exploration.update(cx, |e, cx| e.read_owed(cx)));
        }
        // What the walk's changes left to show is shown once the tab is drawn.
        if self.rebuild_owed {
            self.rebuild(cx);
        }
        if std::mem::take(&mut self.selection_owed) {
            self.follow_selection(cx);
        }
        let mut context = KeyContext::new_with_defaults();
        context.add("Tree");
        if !self.filter.is_empty() {
            context.add("filtering");
        }
        let root = v_flex()
            .id("relationship-tree-root")
            .key_context(context)
            .track_focus(&self.focus_handle)
            // The dock's host gives the tree its size (it scrolls itself).
            .size_full()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::expand))
            .on_action(cx.listener(Self::collapse))
            .on_action(cx.listener(Self::expand_all))
            .on_action(cx.listener(Self::collapse_all))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::reroot_here))
            .on_action(cx.listener(Self::show_on_canvas))
            .on_action(cx.listener(Self::clear_filter))
            .on_action(cx.listener(Self::peek_selected))
            .on_action(cx.listener(Self::open_table))
            .on_action(cx.listener(Self::open_page))
            .on_action(cx.listener(Self::toggle_empty_groups))
            // ⌘R reads the tree again at the branch head, as its Newer data button does, and
            // goes on to the workspace, which reads the graph's schema, branches and counts.
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.with_exploration(cx, |exploration, cx| exploration.refresh(cx));
                cx.propagate();
            }))
            .on_key_down(cx.listener(Self::key_down));

        let toolbar = self.render_toolbar(cx);
        if self.entries.is_empty() {
            return root
                .child(toolbar)
                .child(studio_ui::ds::sidebar_note("Reading…"))
                .into_any_element();
        }

        // The header (Refresh, the filter) as tall as it is; the rows take the rest of the tab.
        let header = self.render_header(cx);
        let list = self.render_list(window, cx);
        root.child(toolbar)
            .children(header)
            .child(list)
            .into_any_element()
    }
}

/// What a node row's hover card shows (spec 005): the node's label, type and key, then every
/// property of the edge that led to it, one per line.
struct HoverCard {
    type_name: String,
    id: String,
    label: Option<String>,
    /// The relationship as its group row reads (`→ WorksAt`), when the node was reached
    /// through one.
    relationship: Option<String>,
    edge: Vec<(String, serde_json::Value)>,
}

impl HoverCard {
    fn render(&self, cx: &App) -> AnyElement {
        let name_width = self
            .edge
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(0)
            .min(16) as f32
            * 6.5
            + 8.;
        v_flex()
            .gap(px(space::XS))
            .max_w(px(studio_ui::ds::size::FIELD_WIDE))
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .child(studio_ui::ds::type_dot(&self.type_name))
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            studio_ui::ds::text::body(
                                self.label.clone().unwrap_or_else(|| self.id.clone()),
                            )
                            .weight(studio_ui::ds::text::weight::HEADING)
                            .truncate(),
                        ),
                    )
                    .child(studio_ui::ds::text::meta(self.type_name.clone()).single_line()),
            )
            .when(self.label.is_some(), |card| {
                card.child(
                    studio_ui::ds::text::meta(self.id.clone())
                        .buffer_font(cx)
                        .truncate(),
                )
            })
            .when(!self.edge.is_empty(), |card| {
                card.child(
                    v_flex()
                        .mt(px(space::XS))
                        .pt(px(space::XS))
                        .gap(px(space::XXS))
                        .border_t_1()
                        .border_color(studio_ui::ds::surface::rule(cx))
                        .children(self.relationship.clone().map(studio_ui::ds::text::meta))
                        .children(self.edge.iter().map(|(name, value)| {
                            let value = rows::edge_value(value);
                            h_flex()
                                .gap(px(space::MD))
                                .child(
                                    div()
                                        .w(px(name_width))
                                        .flex_none()
                                        .child(studio_ui::ds::text::meta(name.clone()).truncate()),
                                )
                                .child(
                                    div().min_w_0().flex_1().child(
                                        studio_ui::ds::text::meta(value)
                                            .color(Color::Default)
                                            .truncate(),
                                    ),
                                )
                        })),
                )
            })
            .into_any_element()
    }
}

/// How to give the tree a node, with the keys the keymap binds: explore a table row, or open a
/// node from the command palette.
#[cfg(test)]
mod tests {
    use graph_store::test_server::followed_session;

    use super::*;

    /// A row's element key follows what the row is, wherever it sits in the list, so its hover
    /// (and so its read ahead) stays with it when rows above it come or go; a group's count
    /// error and its page error, at one path, are told apart.
    #[test]
    fn a_rows_key_follows_the_row_not_its_place() {
        use graph_store::{ExplorePath, GroupKey, LoadTarget, NodeRef};
        let group = gq_language::RelationshipGroup {
            edge: "Knows".into(),
            direction: gq_language::Direction::Outgoing,
            other_type: "Person".into(),
        };
        let key = GroupKey::new(NodeRef::new("Person", "ada"), group.clone());
        let path = ExplorePath::root().child_group(&group);
        let at = |depth, kind| Entry {
            path: path.clone(),
            depth,
            parent: Some(depth),
            kind,
        };
        let error = |target| EntryKind::Error {
            target,
            message: "refused".into(),
        };
        let count = error(LoadTarget::Count(key.clone()));
        assert_eq!(
            row_key(&at(1, count.clone())),
            row_key(&at(4, count.clone()))
        );
        assert_ne!(
            row_key(&at(1, count)),
            row_key(&at(1, error(LoadTarget::Page(key))))
        );
    }

    /// What the dock's host gives a panel that scrolls itself: its size, here shorter than the
    /// window, as a bottom dock across the window or a tall breadcrumb leaves it.
    const DOCK_HEIGHT: f32 = 320.;

    struct Dock {
        tree: Entity<RelationshipTree>,
    }

    impl Render for Dock {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                div()
                    .debug_selector(|| "dock".into())
                    .w(px(260.))
                    .h(px(DOCK_HEIGHT))
                    .child(self.tree.clone()),
            )
        }
    }

    /// A tab not drawn makes no rows at each answer; what it owes it makes when drawn. Two tabs on
    /// one root and commit share a walk; another commit is another walk (spec 020).
    #[gpui::test]
    async fn a_hidden_tree_makes_no_rows_and_a_walk_is_shared_by_root_and_commit(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(studio_theme::init_test);
        let (session, _served) = followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\n",
            |_| serde_json::json!([{ "id": "p1", "label": "Ada", "n": 0 }]),
            cx,
        );
        cx.run_until_parked();
        let ada = NodeRef::new("Person", "p1");
        let tree = cx.new(|cx| RelationshipTree::new(session.clone(), ada.clone(), None, cx));
        let before = tree.read_with(cx, |tree, _| tree.entries.clone());
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(10));
        cx.run_until_parked();
        tree.read_with(cx, |tree, cx| {
            assert!(tree.rebuild_owed, "the answers are owed to the next draw");
            assert!(Arc::ptr_eq(&tree.entries, &before), "and made no rows");
            assert!(tree.exploration.read(cx).root().is_some());
        });
        let same = cx.update(|cx| Exploration::for_walk(&session, ada.clone(), None, cx));
        assert_eq!(same, tree.read_with(cx, |tree, _| tree.exploration.clone()));
        let held = cx.update(|cx| Exploration::for_walk(&session, ada, Some("c0".into()), cx));
        assert_ne!(held, same);
    }

    /// ⌘R where the tree has the keyboard reads it again at the branch head, as its "Newer
    /// data · Refresh" does, and takes the offer down. A new token reads it again too, once the
    /// tree is drawn, with nothing else asking.
    #[gpui::test]
    async fn cmd_r_reads_the_tree_again(cx: &mut gpui::TestAppContext) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        cx.update(studio_theme::init_test);
        let reads = Arc::new(AtomicUsize::new(0));
        let roots = Arc::new(AtomicUsize::new(0));
        let (counted, rooted) = (reads.clone(), roots.clone());
        let (session, served) = followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\n",
            move |asked| {
                counted.fetch_add(1, Ordering::SeqCst);
                if asked["name"] == "studio_node" {
                    rooted.fetch_add(1, Ordering::SeqCst);
                }
                serde_json::json!([{ "id": "p1", "label": "Ada", "n": 0 }])
            },
            cx,
        );
        cx.run_until_parked();
        let shown = session.clone();
        let (dock, cx) = cx.add_window_view(move |_, cx| Dock {
            tree: cx.new(|cx| {
                RelationshipTree::new(shown.clone(), NodeRef::new("Person", "p1"), None, cx)
            }),
        });
        let exploration = dock.read_with(cx, |dock, cx| dock.tree.read(cx).exploration.clone());
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..3 {
                cx.run_until_parked();
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(10));
            }
            cx.run_until_parked();
        };
        settle(cx);
        // Another writer changes the root: the tree offers the newer data.
        served.lock().unwrap().commit(
            "c2",
            vec![graph_store::test_server::node_change("Person", "p1")],
        );
        session.update(cx, |session, cx| session.check_heads(cx));
        settle(cx);
        assert!(exploration.read_with(cx, |exploration, _| exploration.newer_available()));
        let before = reads.load(Ordering::SeqCst);
        let tree = dock.read_with(cx, |dock, _| dock.tree.clone());
        cx.update(|window, cx| window.focus(&tree.focus_handle(cx), cx));
        cx.dispatch_action(Refresh);
        settle(cx);
        assert!(reads.load(Ordering::SeqCst) > before, "read again");
        assert!(!exploration.read_with(cx, |exploration, _| exploration.newer_available()));

        let before = roots.load(Ordering::SeqCst);
        let connection = session.read_with(cx, |session, _| session.connection.clone());
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("another".into()), cx)
        });
        settle(cx);
        assert!(
            roots.load(Ordering::SeqCst) > before,
            "the drawn tree reads its root again under the new token"
        );
        assert!(!exploration.read_with(cx, |exploration, _| exploration.owes_reads()));
    }

    /// The rows take what the tree's dock leaves below its header, not the window's height, so
    /// the row the keyboard selects scrolls into what shows.
    #[gpui::test]
    async fn the_rows_fit_the_dock_and_the_selected_row_shows(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let edges: String = (0..12)
            .map(|i| format!("edge Rel{i}: Person -> Person\n"))
            .collect();
        let schema = format!("node Person {{\n  slug: String @key\n  name: String\n}}\n{edges}");
        let (session, _served) = followed_session(
            &schema,
            |_| serde_json::json!([{ "id": "p1", "label": "Ada", "n": 0 }]),
            cx,
        );
        cx.run_until_parked();
        let shown = session.clone();
        let (dock, cx) = cx.add_window_view(move |_, cx| Dock {
            tree: cx.new(|cx| {
                RelationshipTree::new(shown.clone(), NodeRef::new("Person", "p1"), None, cx)
            }),
        });
        // The tree draws reads that land together once they all have.
        for _ in 0..3 {
            cx.run_until_parked();
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(10));
        }
        cx.run_until_parked();

        let dock_bounds = cx.debug_bounds("dock").expect("the dock");
        let list = cx.debug_bounds("relationship-tree-list").expect("the rows");
        assert!(
            list.top() >= dock_bounds.top() && list.bottom() <= dock_bounds.bottom(),
            "the rows {list:?} within the dock {dock_bounds:?}"
        );
        let tree = dock.read_with(cx, |dock, _| dock.tree.clone());
        let last = tree.read_with(cx, |tree, _| tree.visible.len() - 1);
        assert!(
            (last + 1) as f32 * ROW_HEIGHT > DOCK_HEIGHT,
            "more rows than the dock holds"
        );
        tree.update(cx, |tree, cx| tree.select_row(last, cx));
        cx.run_until_parked();
        let selector: &'static str = format!("tree-row-{last}").leak();
        let row = cx
            .debug_bounds(selector)
            .expect("the selected row is drawn");
        assert!(
            row.bottom() <= dock_bounds.bottom(),
            "the selected row {row:?} shows in the dock {dock_bounds:?}"
        );
    }
}
