//! The relationship tree (spec 005): a node, its relationship groups with counts, the nodes at
//! the other end, their groups, and so on, as a path tree over the session's shared
//! [`Exploration`]. It is the left dock's "Explore" panel.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/005-relationship-tree/plan.md`.
//!
//! Rooting: the tree registers a workspace handler for `OpenNode` that shows the Explore panel,
//! reveals the node (selects it where the tree already shows it, else roots the tree there and
//! focuses the tree) and then
//! calls `cx.propagate()`, so a later handler, such as the inspector's, still runs. GPUI stops
//! at the first bubble handler that does not propagate, so a feature crate that handles
//! `OpenNode` before this one and does not propagate should call
//! `Exploration::for_session(session, cx).update(cx, |e, cx| e.reveal(node, cx))` itself: the
//! exploration lives in `graph_store`, which every feature crate may use.

mod rows;

use std::ops::Range;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
    KeyContext, KeyDownEvent, ListSizingBehavior, Render, ScrollStrategy, Subscription,
    UniformListScrollHandle, Window, uniform_list,
};
use gq_language::builder::{GroupBy, GroupOn};
use graph_store::{
    Entry, EntryKind, Exploration, ExplorationEvent, GraphSession, Loadable, NodeRef, SessionEvent,
};
use smallvec::SmallVec;
use studio_actions::graph::{OpenNode, ShowNodeOnCanvas};
use studio_actions::graph::{OpenType, PeekNode};
use studio_actions::tree::{
    ClearFilter, Collapse, CollapseAll, Expand, ExpandAll, OpenSelected, OpenTable, PeekSelected,
    RerootHere, SelectNext, SelectPrevious, ShowOnCanvas, ToggleEmptyGroups,
};
use studio_ui::type_swatch;
use studio_workspace::{PanelPosition, Workspace, WorkspaceEvent};
use ui::prelude::*;
use ui::{ContextMenu, IndentGuideColors, PopoverMenu, StickyCandidate, Tooltip};

pub use rows::{RowId, RowTag};

/// The tree's panel id in the left dock.
const EXPLORE_PANEL: &str = "explore";
const ROW_HEIGHT: f32 = studio_ui::ds::size::SIDEBAR_ROW;
const INDENT: f32 = 12.;
const LEFT_PAD: f32 = 8.;
const DISCLOSURE: f32 = 12.;

/// Registers the tree's keys and adds the "Explore" panel to every workspace.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let handle = cx.entity();
        let session = workspace.session().cloned();
        let tree = cx.new(|cx| RelationshipTree::new(&handle, session, window, cx));
        workspace.add_panel(
            PanelPosition::Left,
            EXPLORE_PANEL,
            "Explore",
            IconName::ListTree,
            tree.clone().into(),
            cx,
        );
        let actions = cx.new(|cx| ExploreActions::new(&tree, cx));
        workspace.set_panel_actions(EXPLORE_PANEL, actions.into(), cx);
        workspace.register_action(move |workspace, action: &OpenNode, window, cx| {
            if let Some(session) = workspace.session().cloned() {
                workspace.activate_panel(EXPLORE_PANEL, cx);
                let node = NodeRef::new(action.type_name.clone(), action.id.clone());
                reveal(&tree, &session, node, window, cx);
            }
            cx.propagate();
        });
    })
    .detach();
}

fn reveal(
    tree: &Entity<RelationshipTree>,
    session: &Entity<GraphSession>,
    node: NodeRef,
    window: &mut Window,
    cx: &mut App,
) {
    let exploration = Exploration::for_session(session, cx);
    let visible = exploration.update(cx, |exploration, cx| {
        let visible = exploration
            .schema(cx)
            .and_then(|schema| exploration.state().path_of(&schema, &node))
            .is_some();
        exploration.reveal(node, cx);
        visible
    });
    if !visible {
        window.focus(&tree.focus_handle(cx), cx);
    }
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

pub struct RelationshipTree {
    exploration: Option<Entity<Exploration>>,
    entries: Vec<Entry>,
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
    exploration_subscriptions: Vec<Subscription>,
    _workspace_subscription: Subscription,
    /// The read ahead for the row the pointer rests on, once it has rested.
    _prefetch: Option<gpui::Task<()>>,
    /// A rebuild waiting for reads that finish together.
    _loaded: Option<gpui::Task<()>>,
}

impl Focusable for RelationshipTree {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl RelationshipTree {
    fn new(
        workspace: &Entity<Workspace>,
        session: Option<Entity<GraphSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_subscription = cx.subscribe_in(
            workspace,
            window,
            |this, workspace, event: &WorkspaceEvent, window, cx| {
                let WorkspaceEvent::SessionChanged = event;
                let session = workspace.read(cx).session().cloned();
                // With nothing focused, GPUI dispatches actions from the window's root node,
                // above the workspace's handlers; give the keyboard a home once a graph opens.
                if session.is_some() && window.focused(cx).is_none() {
                    window.focus(&this.focus_handle, cx);
                }
                this.set_session(session, cx);
            },
        );
        let mut tree = Self {
            exploration: None,
            entries: Vec::new(),
            visible: Vec::new(),
            selected: None,
            selected_row_index_hint: None,
            filter: String::new(),
            hide_empty: false,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            exploration_subscriptions: Vec::new(),
            _workspace_subscription: workspace_subscription,
            _prefetch: None,
            _loaded: None,
        };
        if session.is_some() && window.focused(cx).is_none() {
            window.focus(&tree.focus_handle, cx);
        }
        tree.set_session(session, cx);
        tree
    }

    /// The exploration this tree shows.
    pub fn exploration(&self) -> Option<&Entity<Exploration>> {
        self.exploration.as_ref()
    }

    fn set_session(&mut self, session: Option<Entity<GraphSession>>, cx: &mut Context<Self>) {
        let exploration = session
            .as_ref()
            .map(|session| Exploration::for_session(session, cx));
        if exploration == self.exploration {
            return;
        }
        self.exploration_subscriptions.clear();
        if let (Some(exploration), Some(session)) = (&exploration, &session) {
            self.exploration_subscriptions
                .push(cx.subscribe(exploration, |this, _, event, cx| match event {
                    ExplorationEvent::Changed => {
                        this._loaded = None;
                        this.rebuild(cx);
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
                                    this.rebuild(cx);
                                })
                                .ok();
                            }));
                        }
                    }
                    ExplorationEvent::SelectionChanged => this.follow_selection(cx),
                }));
            self.exploration_subscriptions.push(cx.subscribe(
                session,
                |this, _, event: &SessionEvent, cx| {
                    if *event == SessionEvent::SchemaChanged {
                        this.rebuild(cx);
                    }
                },
            ));
        }
        self.exploration = exploration;
        self.selected = None;
        self.filter.clear();
        self.rebuild(cx);
        self.follow_selection(cx);
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.entries = match &self.exploration {
            Some(exploration) => exploration.read(cx).entries(cx),
            None => Vec::new(),
        };
        self.refilter(cx);
    }

    /// Recomputes the visible rows. Call after `entries` changed, or with the previous
    /// selected row when the rows it pointed into are gone.
    fn refilter(&mut self, cx: &mut Context<Self>) {
        let previous_row = self.selected_row_index_hint;
        self.visible = match &self.exploration {
            Some(exploration) => rows::filter(
                &self.entries,
                exploration.read(cx).state(),
                &self.filter,
                self.hide_empty,
            ),
            None => Vec::new(),
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
        let Some(path) = self
            .exploration
            .as_ref()
            .and_then(|exploration| exploration.read(cx).selection().cloned())
        else {
            return;
        };
        if self.selected.as_ref().is_some_and(|id| id.path == path) {
            return;
        }
        let row = self.visible.iter().position(|ix| {
            let entry = &self.entries[*ix];
            entry.path == path
                && matches!(
                    entry.kind,
                    EntryKind::Node { .. } | EntryKind::Group { .. } | EntryKind::Bucket { .. }
                )
        });
        if let Some(row) = row {
            self.selected = Some(rows::row_id(&self.entries[self.visible[row]]));
            self.scroll_handle
                .scroll_to_item(row, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    fn selected_row(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
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
        if let (Some(path), Some(exploration)) = (shared, self.exploration.clone()) {
            exploration.update(cx, |exploration, cx| exploration.select(Some(path), cx));
        }
        cx.notify();
    }

    fn with_exploration(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Exploration, &mut Context<Exploration>),
    ) {
        if let Some(exploration) = self.exploration.clone() {
            exploration.update(cx, f);
        }
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
        let (expandable, expanded) = match &entry.kind {
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
        };
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
        let entry = self.entries[self.visible[row]].clone();
        match entry.kind {
            EntryKind::Node { node, .. } => open_node(&node, window, cx),
            EntryKind::Field {
                target: Some(target),
                ..
            } => open_node(&target.node, window, cx),
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
                if let Some(ancestor_row) = self.visible.iter().position(|ix| *ix == ancestor) {
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

    fn reroot_here(&mut self, _: &RerootHere, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        if let EntryKind::Node { .. } = entry.kind {
            self.with_exploration(cx, |e, cx| e.reroot(&entry.path, cx));
        }
    }

    fn show_on_canvas(&mut self, _: &ShowOnCanvas, window: &mut Window, cx: &mut Context<Self>) {
        let node = match self.selected_entry().map(|entry| &entry.kind) {
            Some(EntryKind::Node { node, .. } | EntryKind::Cycle { node, .. }) => node.clone(),
            Some(EntryKind::Field {
                target: Some(target),
                ..
            }) => target.node.clone(),
            _ => return,
        };
        window.dispatch_action(
            Box::new(ShowNodeOnCanvas {
                type_name: node.type_name,
                id: node.id,
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
        self.exploration
            .as_ref()?
            .read(cx)
            .state()
            .searches
            .get(key)
            .cloned()
    }

    /// Space: the selected node in the inspector, focus staying in the tree.
    fn peek_selected(&mut self, _: &PeekSelected, window: &mut Window, cx: &mut Context<Self>) {
        let node = match self.selected_entry().map(|entry| &entry.kind) {
            Some(EntryKind::Node { node, .. } | EntryKind::Cycle { node, .. }) => node.clone(),
            Some(EntryKind::Field {
                target: Some(target),
                ..
            }) => target.node.clone(),
            _ => return,
        };
        window.dispatch_action(
            Box::new(PeekNode {
                type_name: node.type_name,
                id: node.id,
            }),
            cx,
        );
    }

    /// ⌘↵: a node's own type table filtered to it; on a relationship group, the other type's
    /// table filtered to the nodes related to this one.
    fn open_table(&mut self, _: &OpenTable, window: &mut Window, cx: &mut Context<Self>) {
        let action = match self.selected_entry().map(|entry| &entry.kind) {
            Some(EntryKind::Node { node, .. } | EntryKind::Cycle { node, .. }) => OpenType {
                type_name: node.type_name.clone(),
                at_commit: None,
                filter_id: Some(node.id.clone()),
                via: None,
                preview: false,
            },
            Some(EntryKind::Group { key, .. } | EntryKind::Bucket { key, .. }) => {
                // Seen from the other type, the relationship runs the other way.
                let back = gq_language::RelationshipGroup {
                    edge: key.group.edge.clone(),
                    direction: match key.group.direction {
                        gq_language::Direction::Outgoing => gq_language::Direction::Incoming,
                        gq_language::Direction::Incoming => gq_language::Direction::Outgoing,
                    },
                    other_type: key.node.type_name.clone(),
                };
                OpenType {
                    type_name: key.group.other_type.clone(),
                    at_commit: None,
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
        let colors = cx.theme().colors();
        let selected_bg = colors.element_selected;
        let hover_bg = colors.element_hover;
        let focus_border = colors.border_focused;
        let panel_bg = colors.panel_background;
        let entry = &self.entries[self.visible[row]];
        let Some(exploration) = &self.exploration else {
            return div().into_any_element();
        };
        let state = exploration.read(cx).state();

        let (disclosure, expanded) = match &entry.kind {
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
        };
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
                d.child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
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
                    .gap_1p5()
                    // The root carries the type's mark; below it a dot, since each
                    // relationship already names the type it leads to.
                    .child(if root {
                        type_swatch(&node.type_name)
                    } else {
                        studio_ui::ds::type_dot(&node.type_name)
                    })
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            Label::new(label.unwrap_or_else(|| node.id.clone()))
                                .size(LabelSize::Small)
                                .when(root, |label| label.weight(gpui::FontWeight::MEDIUM))
                                .truncate(),
                        ),
                    )
                    .children(key.map(|key| {
                        div().min_w(rems(2.5)).flex_shrink(1.).child(
                            Label::new(key)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate_start(),
                        )
                    }))
                    .child(div().flex_1())
                    .children(hint.map(|hint| {
                        Label::new(hint)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line()
                    }))
                    .into_any_element()
            }
            EntryKind::Cycle { node, .. } => h_flex()
                .min_w_0()
                .flex_1()
                .gap_1p5()
                .child(Label::new("↺").size(LabelSize::Small).color(Color::Muted))
                .child(type_swatch(&node.type_name))
                .child(
                    div().min_w_0().flex_shrink(1.).child(
                        Label::new(rows::node_text(state, node))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
                .child(
                    Label::new("already above")
                        .size(LabelSize::XSmall)
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
                    .gap_1p5()
                    .child(
                        Label::new(format!("{} →", graph_store_traversal(&key.group.edge)))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    )
                    // The target's dot, as on a neighbour row; none when there is no target.
                    .when(target.is_some(), |row| {
                        row.child(studio_ui::ds::type_dot(&key.group.other_type))
                    })
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            Label::new(value)
                                .size(LabelSize::Small)
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
                let arrow = match key.group.direction {
                    gq_language::Direction::Outgoing => "→",
                    gq_language::Direction::Incoming => "←",
                };
                let count_label = match count {
                    Loadable::Loaded(n) => n.to_string(),
                    Loadable::Failed(_) => "!".to_owned(),
                    Loadable::NotLoaded | Loadable::Loading => "…".to_owned(),
                };
                let grouped_by = exploration
                    .read(cx)
                    .grouping(&key.group)
                    .map(GroupBy::label);
                let group_menu = (!empty)
                    .then(|| self.group_by_menu(row, entry, &key.group, sticky, selected, cx))
                    .flatten();
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1p5()
                    .child(Label::new(arrow).size(LabelSize::Small).color(muted))
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            Label::new(key.group.edge.clone())
                                .size(LabelSize::Small)
                                .color(text_color)
                                .truncate(),
                        ),
                    )
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            Label::new(key.group.other_type.clone())
                                .size(LabelSize::XSmall)
                                .color(muted)
                                .truncate(),
                        ),
                    )
                    .children(grouped_by.map(|property| {
                        div().min_w_0().flex_shrink(1.).child(
                            Label::new(format!("by {property}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Accent)
                                .truncate(),
                        )
                    }))
                    .child(div().flex_1())
                    .children(group_menu)
                    .child(
                        Label::new(count_label)
                            .size(LabelSize::XSmall)
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
                let value = key.within.as_ref().map(|within| &within.value);
                let missing = value.is_none_or(|value| value.is_null());
                // By a relationship of the neighbours: the related node's label, or "No
                // Company" for the neighbours related to none.
                let other = key
                    .within
                    .as_ref()
                    .filter(|within| within.by.on == GroupOn::Relationship)
                    .and_then(|within| {
                        let schema = exploration.read(cx).schema(cx)?;
                        let (direction, edge) =
                            gq_language::builder::parse_relationship_key(&within.by.property)?;
                        let edge = schema.edge(edge)?;
                        Some(match direction {
                            gq_language::Direction::Outgoing => edge.to.clone(),
                            gq_language::Direction::Incoming => edge.from.clone(),
                        })
                    });
                let text = match (&other, missing) {
                    (Some(other), true) => format!("No {other}"),
                    _ => state
                        .bucket_label(key)
                        .map(str::to_owned)
                        .or_else(|| value.map(rows::bucket_text))
                        .unwrap_or_default(),
                };
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1p5()
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            Label::new(text)
                                .size(LabelSize::Small)
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
                    .child(
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    )
                    .into_any_element()
            }
            EntryKind::OtherValues { shown, rest } => {
                Label::new(rows::other_values_label(*shown, *rest))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
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
                    rows::more_label(*shown, *total, exploration.read(cx).page_size(cx))
                };
                Label::new(text)
                    .size(LabelSize::XSmall)
                    .color(Color::Accent)
                    .single_line()
                    .into_any_element()
            }
            EntryKind::Loading { .. } => Label::new("Loading…")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .single_line()
                .into_any_element(),
            EntryKind::Error { target, message } => {
                let target = target.clone();
                let tooltip: SharedString = message.clone().into();
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::XSmall)
                            .color(Color::Error),
                    )
                    .child(
                        div()
                            .id(("error", row))
                            .min_w_0()
                            .flex_shrink(1.)
                            .tooltip(Tooltip::text(tooltip))
                            .child(
                                Label::new(message.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error)
                                    .truncate(),
                            ),
                    )
                    .child(
                        Button::new(("retry", row), "Retry")
                            .label_size(LabelSize::XSmall)
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
                                let arrow = match key.group.direction {
                                    gq_language::Direction::Outgoing => "→",
                                    gq_language::Direction::Incoming => "←",
                                };
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

        h_flex()
            .id((if sticky { "sticky-row" } else { "tree-row" }, row))
            .group("tree-row")
            .w_full()
            .h(px(ROW_HEIGHT))
            .pl(px(LEFT_PAD + entry.depth as f32 * INDENT))
            .pr_2()
            .gap_1()
            .items_center()
            .bg(panel_bg)
            .border_l_2()
            .border_color(gpui::transparent_black())
            .when(selected, |d| d.bg(selected_bg))
            .when(selected && focused, |d| d.border_color(focus_border))
            .hover(|style| style.bg(hover_bg))
            .when_some(prefetch_path, |row, path| {
                row.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    this.prefetch(hovered.then(|| path.clone()), cx)
                }))
            })
            .when_some(card, |d, card| {
                d.tooltip(Tooltip::element(move |_, cx| card.render(cx)))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
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
                if single_activates || event.click_count() >= 2 {
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
        let exploration = self.exploration.clone()?;
        let schema = exploration.read(cx).schema(cx)?;
        let choices = GroupBy::choices(&schema, group);
        if choices.is_empty() {
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
                IconButton::new(id, IconName::ListTree)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .toggle_state(current.is_some())
                    .tooltip(Tooltip::text(tooltip)),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let (exploration, group, path, current, choices) = (
                    exploration.clone(),
                    group.clone(),
                    path.clone(),
                    current.clone(),
                    choices.clone(),
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
                    for (on, title) in [
                        (GroupOn::Node, group.other_type.clone()),
                        (GroupOn::Edge, group.edge.clone()),
                        (
                            GroupOn::Relationship,
                            format!("{}'s relationships", group.other_type),
                        ),
                    ] {
                        let section: Vec<&GroupBy> =
                            choices.iter().filter(|by| by.on == on).collect();
                        if section.is_empty() {
                            continue;
                        }
                        menu = menu.separator().header(title);
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

    fn render_header(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let exploration = self.exploration.as_ref()?;
        let e = exploration.read(cx);
        let state = e.state();
        let newer = e.newer_available();
        let breadcrumb: Vec<(usize, String)> = state
            .breadcrumb
            .iter()
            .enumerate()
            .map(|(index, node)| (index, rows::node_text(state, node)))
            .collect();
        state.root.as_ref()?;
        let filter = self.filter.clone();
        if breadcrumb.is_empty() && !newer && filter.is_empty() {
            return None;
        }
        let field = studio_ui::ds::surface::sunken(cx);
        Some(
            v_flex()
                .px_2()
                .pb_1()
                .gap_1()
                .when(!breadcrumb.is_empty(), |d| {
                    d.child(h_flex().min_w_0().flex_wrap().gap_0p5().children(
                        breadcrumb.into_iter().map(|(index, text)| {
                            studio_ui::ds::button(("crumb", index), format!("{text} ›"))
                                .label_size(LabelSize::XSmall)
                                .style(ButtonStyle::Subtle)
                                .tooltip(Tooltip::text("Explore from here again"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.with_exploration(cx, |e, cx| e.back_to(index, cx));
                                }))
                        }),
                    ))
                })
                .when(newer, |d| {
                    d.child(
                        studio_ui::ds::button("newer", "Newer data · Refresh")
                            .label_size(LabelSize::XSmall)
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .tooltip(Tooltip::text(
                                "The branch moved since this tree was read. Refresh reads it again.",
                            ))
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
                            .px_2()
                            .gap_1p5()
                            .rounded_md()
                            .bg(field)
                            .child(
                                Icon::new(IconName::MagnifyingGlass)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .child(Label::new(filter).size(LabelSize::Small).truncate()),
                            )
                            .child(
                                Label::new("esc")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Disabled),
                            ),
                    )
                })
                .into_any_element(),
        )
    }

    /// Reads ahead what opening `path` will show once the pointer has rested on its row;
    /// leaving the row first reads nothing.
    fn prefetch(&mut self, path: Option<graph_store::ExplorePath>, cx: &mut Context<Self>) {
        let (Some(path), Some(exploration)) = (path, self.exploration.clone()) else {
            self._prefetch = None;
            return;
        };
        self._prefetch = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            exploration.update(cx, |e, cx| e.prefetch(&path, cx));
        }));
    }

    /// Whether some relationship group shown has no relationships.
    fn has_empty_groups(&self) -> bool {
        self.entries.iter().any(|entry| {
            matches!(
                entry.kind,
                EntryKind::Group {
                    count: Loadable::Loaded(0),
                    ..
                }
            )
        })
    }

    /// Closes everything below the root.
    fn collapse_to_root(&mut self, cx: &mut Context<Self>) {
        let root = graph_store::ExplorePath::root();
        self.with_exploration(cx, |e, cx| {
            e.collapse_subtree(&root, cx);
            e.expand(&root, cx);
        });
    }

    fn render_list(
        &self,
        height: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
                        .filter_map(|ix| this.visible.iter().position(|v| *v == ix))
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
        .h(height)
        .into_any_element()
    }
}

fn graph_store_traversal(edge: &str) -> String {
    gq_language::traversal_name(edge)
}

fn open_node(node: &NodeRef, window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        Box::new(OpenNode {
            type_name: node.type_name.clone(),
            id: node.id.clone(),
        }),
        cx,
    );
}

impl Render for RelationshipTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut context = KeyContext::new_with_defaults();
        context.add("Tree");
        if !self.filter.is_empty() {
            context.add("filtering");
        }
        let root = v_flex()
            .id("relationship-tree-root")
            .key_context(context)
            .track_focus(&self.focus_handle)
            .w_full()
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
            .on_action(cx.listener(Self::toggle_empty_groups))
            .on_key_down(cx.listener(Self::key_down));

        let has_root = self
            .exploration
            .as_ref()
            .is_some_and(|exploration| exploration.read(cx).root().is_some());
        if !has_root || self.entries.is_empty() {
            return root
                .child(studio_ui::ds::sidebar_placeholder(
                    "explore-empty",
                    IconName::ListTree,
                    "No node",
                    how_to_explore(cx),
                ))
                .into_any_element();
        }

        let header = self.render_header(cx);
        // The tree fills the sidebar below the section title; other sections scroll above it.
        let chrome = px(38. + 24. + 32.) + if header.is_some() { px(56.) } else { px(0.) };
        let height = (window.viewport_size().height - chrome).max(px(200.));
        let list = self.render_list(height, window, cx);
        root.children(header).child(list).into_any_element()
    }
}

/// The Explore panel's title-row actions: hide relationships with none, and close everything
/// below the root.
pub struct ExploreActions {
    tree: gpui::WeakEntity<RelationshipTree>,
    _observe: Subscription,
}

impl ExploreActions {
    fn new(tree: &Entity<RelationshipTree>, cx: &mut Context<Self>) -> Self {
        Self {
            tree: tree.downgrade(),
            _observe: cx.observe(tree, |_, _, cx| cx.notify()),
        }
    }
}

impl Render for ExploreActions {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(tree) = self.tree.upgrade() else {
            return h_flex();
        };
        let (rooted, hide_empty, any_empty) = {
            let tree = tree.read(cx);
            (
                tree.exploration
                    .as_ref()
                    .is_some_and(|e| e.read(cx).root().is_some()),
                tree.hide_empty,
                tree.has_empty_groups(),
            )
        };
        let weak = self.tree.clone();
        h_flex()
            .gap_0p5()
            // The toggle shows only when it changes something: an empty group, or hiding on.
            .when(hide_empty || any_empty, |row| {
                let tree = weak.clone();
                row.child(
                    IconButton::new(
                        "toggle-empty-groups",
                        if hide_empty {
                            IconName::EyeOff
                        } else {
                            IconName::Eye
                        },
                    )
                    .icon_size(IconSize::XSmall)
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
                    .on_click(move |_, window, cx| {
                        tree.update(cx, |tree, cx| {
                            tree.toggle_empty_groups(&ToggleEmptyGroups, window, cx)
                        })
                        .ok();
                    }),
                )
            })
            .when(rooted, |row| {
                let tree = weak.clone();
                row.child(
                    IconButton::new("collapse-to-root", IconName::ListCollapse)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Collapse all"))
                        .on_click(move |_, _, cx| {
                            tree.update(cx, |tree, cx| tree.collapse_to_root(cx)).ok();
                        }),
                )
            })
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
            .gap_1()
            .max_w(px(320.))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(studio_ui::ds::type_dot(&self.type_name))
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            Label::new(self.label.clone().unwrap_or_else(|| self.id.clone()))
                                .size(LabelSize::Small)
                                .weight(gpui::FontWeight::MEDIUM)
                                .truncate(),
                        ),
                    )
                    .child(
                        Label::new(self.type_name.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    ),
            )
            .when(self.label.is_some(), |card| {
                card.child(
                    Label::new(self.id.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(cx)
                        .truncate(),
                )
            })
            .when(!self.edge.is_empty(), |card| {
                card.child(
                    v_flex()
                        .mt_1()
                        .pt_1()
                        .gap_0p5()
                        .border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                        .children(self.relationship.clone().map(|relationship| {
                            Label::new(relationship)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                        }))
                        .children(self.edge.iter().map(|(name, value)| {
                            let value = rows::edge_value(value);
                            h_flex()
                                .gap_2()
                                .child(
                                    div().w(px(name_width)).flex_none().child(
                                        Label::new(name.clone())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                                )
                                .child(
                                    div().min_w_0().flex_1().child(
                                        Label::new(value).size(LabelSize::XSmall).truncate(),
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
fn how_to_explore(cx: &App) -> String {
    let row = studio_ui::shortcut_text(&studio_actions::table::ExploreRow, cx)
        .map(|keys| format!("Press {keys} on a table row"))
        .unwrap_or_else(|| "Explore a table row".to_owned());
    let palette = studio_ui::shortcut_text(&studio_actions::workspace::ToggleCommandPalette, cx)
        .map(|keys| format!("{keys} and Open Node"))
        .unwrap_or_else(|| "Open Node in the command palette".to_owned());
    format!("{row}, or {palette}, to walk its relationships")
}
