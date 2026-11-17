//! The contract every tab in the center pane implements (spec 002). A small version of Zed's
//! `workspace::Item`.

use gpui::{
    Action, AnyElement, AnyView, App, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Render, SharedString, Window,
};
use ui::IconName;

use crate::persistence::SavedItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemEvent {
    /// The tab's title, icon or dirty state changed.
    UpdateTab,
    /// The item asks to be closed.
    Close,
    /// What the tab has selected changed, or nothing is selected now: the inspector follows
    /// ([`Item::inspected`], spec 020).
    Selected,
}

/// What a tab has selected, as the inspector shows it (spec 020).
pub enum Inspected {
    /// A view the tab made of what is selected: a table's row, with the values it holds. The
    /// tab hands the same view back while the same thing is selected, so what is being edited
    /// in it stays. `scrolls_itself` is true for a list that draws only what is in view.
    View { view: AnyView, scrolls_itself: bool },
    /// A node. Whoever shows nodes makes its view ([`crate::register_node_inspector`]), so a
    /// tab of another crate needs only name it.
    Node(InspectedNode),
}

/// Makes the inspector's view of a node a tab has selected, in the window's graph.
pub type NodeInspector =
    fn(&InspectedNode, &Entity<graph_store::GraphSession>, &mut App) -> AnyView;

struct NodeInspectors(NodeInspector);

impl gpui::Global for NodeInspectors {}

/// Lets tabs name a node as their selection ([`Inspected::Node`]): `make` builds its view. The
/// crate that shows nodes registers it once, so the tabs of other crates need not depend on it.
pub fn register_node_inspector(cx: &mut App, make: NodeInspector) {
    cx.set_global(NodeInspectors(make));
}

pub(crate) fn node_inspector(cx: &App) -> Option<NodeInspector> {
    cx.try_global::<NodeInspectors>().map(|made| made.0)
}

/// A node a tab has selected: where it is read is the tab's own read context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedNode {
    pub type_name: String,
    pub id: String,
    /// The commit the tab is held at, which the inspector is held at too.
    pub at_commit: Option<String>,
    /// The commit the tab shows while it follows the head: the inspector reads there, as the
    /// tab does, and offers Refresh when the branch has moved past it.
    pub snapshot: Option<String>,
}

/// What a tab has selected, as identifiers only: what a reader of the window is told (a UI
/// tool, spec 032). It never holds a property value or a row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectedIds {
    /// The type of the nodes selected, when they share one.
    pub type_name: Option<String>,
    /// The nodes selected, by id: a table's marked rows or the row under its cursor, a canvas's
    /// marked chips or its selected one, a tree's selected row, a page's node.
    pub ids: Vec<String>,
    /// The node a canvas or a tree walks from.
    pub root: Option<(String, String)>,
}

/// A node in a view's outline: its type, its id, and the name the person sees, when known.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutlineNode {
    pub type_name: String,
    pub id: String,
    pub label: Option<String>,
}

/// An edge drawn between two nodes, from the first's side: `out:WorksAt` from a person to a
/// company. A query's result picture links nodes with no relationship named.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutlineEdge {
    pub from: (String, String),
    pub to: (String, String),
    pub relationship: Option<String>,
}

/// One relationship of a node as a view shows it: open or not, how many it has when counted,
/// and how many of them are shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineGroup {
    pub node: (String, String),
    pub relationship: String,
    pub other_type: String,
    pub count: Option<u64>,
    pub expanded: bool,
    pub shown: usize,
}

/// What a view that walks the graph shows, for a reader that does not draw it (a UI tool,
/// spec 032; a test): identifiers, the names the person already sees, and how the view holds
/// them. Never a property value beyond a name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewOutline {
    pub root: Option<OutlineNode>,
    pub nodes: Vec<OutlineNode>,
    pub edges: Vec<OutlineEdge>,
    pub groups: Vec<OutlineGroup>,
    /// What is selected, as JSON: a node, a relationship, or a "+K more".
    pub selected: Option<serde_json::Value>,
    /// The node cap left nodes out.
    pub capped: bool,
    /// The last expansion's second level was not opened for want of a confirmation: the nodes
    /// and reads it would have asked about.
    pub stopped: Option<(usize, usize)>,
    /// What else the view says: its zoom, its history, nodes hidden.
    pub facts: serde_json::Map<String, serde_json::Value>,
}

/// Why a view would not do what an action asks, as the view stands ([`Item::refuses`]). It
/// states the fact; whoever asked words it for their reader (a UI tool for an agent, spec 032).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// In the view's own words: what the action lacks or names that the view has not.
    Says(String),
    /// Nodes the action names that the view does not show, as "Type id" where the view lists
    /// them.
    NotShown(Vec<String>),
    /// The view shows a query's result whole: it is not a walk, so nothing in it expands or
    /// hides.
    NotAWalk,
    /// The relationship the action names is not open in the view.
    RelationshipClosed,
    /// It would open more than the view opens without asking the person: how many nodes (a
    /// tree's rows) and reads. `confirms` says the action can carry the person's answer.
    AsksFirst {
        nodes: usize,
        reads: usize,
        confirms: bool,
    },
}

/// What a tab adds to the status bar (spec 002): a few facts after the graph and branch, as
/// Zed's status items, and the keys it answers, which the bar lists in a panel behind one
/// button rather than spelling them out. A tab's controls belong in the tab.
#[derive(Default)]
pub struct ItemStatus {
    pub start: Vec<AnyElement>,
    pub end: Vec<AnyElement>,
    pub keys: Vec<KeyHint>,
}

/// One key a tab answers, as its keys panel lists it: what it does, and the action whose
/// binding is shown, looked up from `focus` so the tab's own context decides it.
pub struct KeyHint {
    pub label: SharedString,
    pub action: Box<dyn Action>,
    pub focus: FocusHandle,
}

impl KeyHint {
    pub fn new(label: impl Into<SharedString>, action: &dyn Action, focus: &FocusHandle) -> Self {
        Self {
            label: label.into(),
            action: action.boxed_clone(),
            focus: focus.clone(),
        }
    }
}

impl Clone for KeyHint {
    fn clone(&self) -> Self {
        Self {
            label: self.label.clone(),
            action: self.action.boxed_clone(),
            focus: self.focus.clone(),
        }
    }
}

pub trait Item: Render + Focusable + EventEmitter<ItemEvent> {
    fn tab_title(&self, cx: &App) -> SharedString;

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        None
    }

    /// Items with the same key are one tab: opening the item again activates the existing tab.
    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        None
    }

    /// What this tab shows, in any of its views (spec 020). The sidebar lights its row from this.
    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        None
    }

    /// The views of what this tab shows, in strip order; none or one shows no strip (spec 020).
    /// Asked at every draw, so it reads what it needs cheaply.
    fn views(&self, _cx: &App) -> Vec<studio_actions::ViewLink> {
        Vec::new()
    }

    /// What opens this view of what the tab shows, with the tab's read context.
    fn view_action(&self, _kind: studio_actions::ViewKind, _cx: &App) -> Option<Box<dyn Action>> {
        None
    }

    /// The commit the tab is held at, read-only (spec 009); `None` follows its branch's head.
    fn held_at(&self, _cx: &App) -> Option<String> {
        None
    }

    /// What the tab has selected, as identifiers only: `None` when nothing is or the tab has no
    /// selection to name. Read without making anything.
    fn selected_ids(&self, _cx: &App) -> Option<SelectedIds> {
        None
    }

    /// What the tab shows when it walks the graph (a canvas, a tree), for a reader that does
    /// not draw it.
    fn outline(&self, _cx: &App) -> Option<ViewOutline> {
        None
    }

    /// The tab is still reading or laying out what the last action on it asked for: a reader
    /// that wants what the action did waits while this holds.
    fn settling(&self, _cx: &App) -> bool {
        false
    }

    /// Does now what the tab leaves for its next draw (the reads and the layout a tab owes
    /// while it is not drawn, `docs/drawing.md`), for a reader that waits on
    /// [`Self::settling`] in a window the system is not drawing.
    fn catch_up(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    /// Why the tab would not do what `action` asks, as it stands, or `None` when it would: a
    /// node the action names that the tab does not show, nothing to act on, more than is
    /// opened without asking. `on` is the node the action is to act on, when its caller
    /// selects one first.
    fn refuses(
        &self,
        _action: &dyn Action,
        _on: Option<&studio_actions::NodeId>,
        _cx: &App,
    ) -> Option<Refusal> {
        None
    }

    /// The view this tab is (spec 020): by default the current entry of its view strip.
    fn view_kind(&self, cx: &App) -> Option<studio_actions::ViewKind> {
        self.views(cx)
            .into_iter()
            .find(|link| link.current)
            .map(|link| link.kind)
    }

    /// What the tab has selected, which the inspector shows (spec 020): `None` when nothing is.
    /// Asked while the inspector is shown, when the tab says its selection changed
    /// ([`ItemEvent::Selected`]) and when the tab comes to the front, so a hidden inspector
    /// costs a tab nothing. It must not reach for the workspace, which is asking.
    fn inspected(&mut self, _cx: &mut Context<Self>) -> Option<Inspected> {
        None
    }

    /// The item has unsaved work; closing asks first.
    fn is_dirty(&self, _cx: &App) -> bool {
        false
    }

    /// The item reads an older commit than the branch head.
    fn is_stale(&self, _cx: &App) -> bool {
        false
    }

    /// What reopens this tab after a restart; `None` is not remembered. The kind must be
    /// registered with `register_item_kind`.
    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        None
    }

    /// The tab's part of the status bar; `None` shows only the graph and branch.
    fn status(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<ItemStatus> {
        None
    }

    /// The item draws its costly part as a cached view of its own, so its pane draws it
    /// uncached. A cached view that draws again draws every cached view inside it again too
    /// (GPUI's `prepaint_view`), so a keystroke in one of the item's fields would lay that part
    /// out again if the pane cached the whole item.
    fn caches_its_parts(&self) -> bool {
        false
    }

    /// The tab shows what the window's graph reads from its server, so while the server asks
    /// for a sign-in nobody holds its pane shows the sign-in instead ([`crate::sign_in_prompt`]).
    /// A tab that reads nothing from it (settings, the agent) is drawn as it is.
    fn reads_graph(&self) -> bool {
        true
    }

    /// The tab is shown alone, full screen, in focus mode (spec 002), or no longer is. A tab
    /// that writes prose gives up its chrome while it is; the rest are drawn as they are.
    fn set_focus_mode(&mut self, _on: bool, _window: &mut Window, _cx: &mut Context<Self>) {}
}

pub trait ItemHandle: 'static {
    fn item_id(&self) -> EntityId;
    fn to_any(&self) -> AnyView;
    fn tab_title(&self, cx: &App) -> SharedString;
    fn tab_icon(&self, cx: &App) -> Option<IconName>;
    fn dedupe_key(&self, cx: &App) -> Option<SharedString>;
    fn thing(&self, cx: &App) -> Option<studio_actions::Thing>;
    fn views(&self, cx: &App) -> Vec<studio_actions::ViewLink>;
    fn view_action(&self, kind: studio_actions::ViewKind, cx: &App) -> Option<Box<dyn Action>>;
    fn held_at(&self, cx: &App) -> Option<String>;
    fn selected_ids(&self, cx: &App) -> Option<SelectedIds>;
    fn view_kind(&self, cx: &App) -> Option<studio_actions::ViewKind>;
    fn outline(&self, cx: &App) -> Option<ViewOutline>;
    fn settling(&self, cx: &App) -> bool;
    fn catch_up(&self, window: &mut Window, cx: &mut App);
    fn refuses(
        &self,
        action: &dyn Action,
        on: Option<&studio_actions::NodeId>,
        cx: &App,
    ) -> Option<Refusal>;
    fn inspected(&self, cx: &mut App) -> Option<Inspected>;
    fn is_dirty(&self, cx: &App) -> bool;
    fn is_stale(&self, cx: &App) -> bool;
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn serialize(&self, cx: &App) -> Option<SavedItem>;
    fn status(&self, window: &mut Window, cx: &mut App) -> Option<ItemStatus>;
    fn caches_its_parts(&self, cx: &App) -> bool;
    fn reads_graph(&self, cx: &App) -> bool;
    fn set_focus_mode(&self, on: bool, window: &mut Window, cx: &mut App);
}

impl<T: Item> ItemHandle for Entity<T> {
    fn item_id(&self) -> EntityId {
        self.entity_id()
    }

    fn to_any(&self) -> AnyView {
        self.clone().into()
    }

    fn tab_title(&self, cx: &App) -> SharedString {
        self.read(cx).tab_title(cx)
    }

    fn tab_icon(&self, cx: &App) -> Option<IconName> {
        self.read(cx).tab_icon(cx)
    }

    fn dedupe_key(&self, cx: &App) -> Option<SharedString> {
        self.read(cx).dedupe_key(cx)
    }

    fn thing(&self, cx: &App) -> Option<studio_actions::Thing> {
        self.read(cx).thing(cx)
    }

    fn views(&self, cx: &App) -> Vec<studio_actions::ViewLink> {
        self.read(cx).views(cx)
    }

    fn view_action(&self, kind: studio_actions::ViewKind, cx: &App) -> Option<Box<dyn Action>> {
        self.read(cx).view_action(kind, cx)
    }

    fn held_at(&self, cx: &App) -> Option<String> {
        self.read(cx).held_at(cx)
    }

    fn selected_ids(&self, cx: &App) -> Option<SelectedIds> {
        self.read(cx).selected_ids(cx)
    }

    fn view_kind(&self, cx: &App) -> Option<studio_actions::ViewKind> {
        self.read(cx).view_kind(cx)
    }

    fn outline(&self, cx: &App) -> Option<ViewOutline> {
        self.read(cx).outline(cx)
    }

    fn settling(&self, cx: &App) -> bool {
        self.read(cx).settling(cx)
    }

    fn catch_up(&self, window: &mut Window, cx: &mut App) {
        self.update(cx, |item, cx| item.catch_up(window, cx));
    }

    fn refuses(
        &self,
        action: &dyn Action,
        on: Option<&studio_actions::NodeId>,
        cx: &App,
    ) -> Option<Refusal> {
        self.read(cx).refuses(action, on, cx)
    }

    fn inspected(&self, cx: &mut App) -> Option<Inspected> {
        self.update(cx, |item, cx| item.inspected(cx))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty(cx)
    }

    fn is_stale(&self, cx: &App) -> bool {
        self.read(cx).is_stale(cx)
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        Focusable::focus_handle(self.read(cx), cx)
    }

    fn serialize(&self, cx: &App) -> Option<SavedItem> {
        self.read(cx).serialize(cx)
    }

    fn status(&self, window: &mut Window, cx: &mut App) -> Option<ItemStatus> {
        self.update(cx, |item, cx| item.status(window, cx))
    }

    fn caches_its_parts(&self, cx: &App) -> bool {
        self.read(cx).caches_its_parts()
    }

    fn reads_graph(&self, cx: &App) -> bool {
        self.read(cx).reads_graph()
    }

    fn set_focus_mode(&self, on: bool, window: &mut Window, cx: &mut App) {
        self.update(cx, |item, cx| item.set_focus_mode(on, window, cx));
    }
}
