//! The window frame (spec 002): a title bar with server, graph and branch; a sidebar of
//! sections; a center pane of tabs; an inspector on the right; an activity log at the bottom;
//! a status bar; a modal layer and toasts.
//!
//! Layer 4 (`docs/architecture.md`). A small version of Zed's `workspace` crate, written for
//! Studio rather than forked: Studio has no projects, collaboration or Zed database. Feature
//! crates attach to it with `cx.observe_new::<Workspace>` and `Workspace::register_action`.

mod item;
mod pane;
mod panels;
mod persistence;

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Action, AnyView, App, ClickEvent, Context, DismissEvent, Div, DragMoveEvent, ElementId, Entity,
    EntityId, EventEmitter, FocusHandle, Focusable, MouseButton, Pixels, Point, Render,
    SharedString, Stateful, Subscription, Task, WeakEntity, Window, WindowControlArea, deferred,
    div, point,
};
use graph_store::{ConnectionState, GraphSession, OutcomeKind, SessionEvent};
use studio_actions::graph::{OpenCommit, OpenHistory};
use studio_actions::workspace::{
    ActivateItem, ActivateLastItem, ActivateNextGraph, ActivateNextItem, ActivateNextPane,
    ActivatePreviousGraph, ActivatePreviousItem, ActivatePreviousPane, CloseActiveItem,
    CloseAllItems, CloseGraph, CloseOtherItems, GoBack, GoForward, KeepPreviewTab, Refresh,
    SplitDown, SplitRight, ToggleActivity, ToggleInspector, TogglePinTab, ToggleSidebar,
};
use studio_settings::StudioSettings;
use ui::prelude::*;
use ui::{
    ButtonLike, CommonAnimationExt as _, ContextMenu, Indicator, PopoverMenu, Tooltip,
    right_click_menu,
};

pub use item::{Item, ItemEvent, ItemFooter, ItemHandle};
pub use pane::{Member, Pane, PaneId, SplitAxis, SplitDirection, Tab};
pub use panels::{Dock, PanelEntry, PanelPosition};
pub use persistence::{
    SavedGraph, SavedItem, SavedPane, SavedSession, SavedWorkspace, load_session,
    register_item_kind, restore_enabled, saved_window_bounds,
};

/// A modal: dismissing it emits [`DismissEvent`].
pub trait ModalView: Render + Focusable + EventEmitter<DismissEvent> {}

impl<T: Render + Focusable + EventEmitter<DismissEvent>> ModalView for T {}

struct ActiveModal {
    view: AnyView,
    previous_focus: Option<FocusHandle>,
    _subscription: Subscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

struct Toast {
    id: usize,
    kind: ToastKind,
    message: SharedString,
    /// The one follow-up a toast offers ("View commit"), run by its button.
    action: Option<(SharedString, Box<dyn Action>)>,
    /// Time left on screen; `None` stays until dismissed (errors, spec 002).
    remaining: Option<Duration>,
    hovered: bool,
}

/// How long a success or info toast stays (spec 002); hovering pauses it.
const TOAST_DURATION: Duration = Duration::from_secs(10);
const TOAST_TICK: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceEvent {
    SessionChanged,
}

type ActionRegistration = Rc<dyn Fn(Div, &mut Window, &mut Context<Workspace>) -> Div>;

/// A graph open in this window but not shown (spec 002: several graph sessions as tabs): its
/// tabs, splits and back/forward trail wait here until it is shown again.
struct ParkedGraph {
    session: Entity<GraphSession>,
    panes: Vec<Pane<Box<dyn ItemHandle>>>,
    center: Member,
    active_pane: PaneId,
    next_pane: PaneId,
    back: Vec<EntityId>,
    forward: Vec<EntityId>,
    saved: Option<SavedWorkspace>,
}

pub struct Workspace {
    session: Option<Entity<GraphSession>>,
    /// The other graphs open in this window, and every open graph's session in the order they
    /// were opened (the switcher's order).
    parked: Vec<ParkedGraph>,
    graph_order: Vec<EntityId>,
    /// The open graphs as last written for the next start.
    saved_session: Option<SavedSession>,
    /// The document area: panes of tabs in a tree of splits (spec 002, "Frame").
    panes: Vec<Pane<Box<dyn ItemHandle>>>,
    center: Member,
    active_pane: PaneId,
    next_pane: PaneId,
    /// Whether opening an item another pane shows activates it there. Off while restoring and
    /// splitting, which put copies in the pane being filled.
    dedupe_across_panes: bool,
    /// Panels: the left dock (types, explore, branches), the right (inspector) and the bottom
    /// (activity), after Zed's docks and Obsidian's side docks.
    left: Dock,
    right: Dock,
    bottom: Dock,
    modal: Option<ActiveModal>,
    toasts: Vec<Toast>,
    next_toast: usize,
    actions: Vec<ActionRegistration>,
    focus_handle: FocusHandle,
    /// A press on a header strip that becomes a window move if the pointer moves.
    drag_pending: bool,
    /// Where the traffic lights were last placed; they move with the window style.
    traffic_lights: Option<Point<Pixels>>,
    /// Tabs visited before the active one, most recent last (⌘[), and those stepped back
    /// from (⌘]).
    back: Vec<EntityId>,
    forward: Vec<EntityId>,
    /// The settings file problem last shown, so a toast appears once per problem.
    settings_error: Option<String>,
    _settings_subscription: Subscription,
    /// What was last written for this graph, so unchanged state is not rewritten.
    saved: Option<SavedWorkspace>,
    _save: Option<Task<()>>,
    _save_bounds: Option<Task<()>>,
    _bounds_subscription: Subscription,
    _session_subscription: Option<Subscription>,
    _item_subscriptions: Vec<(gpui::EntityId, Subscription)>,
}

/// How long state must be still before it is written.
const SAVE_DELAY: Duration = Duration::from_millis(500);
/// Height of a column's header strip (spec 002, after Delta).
const STRIP_HEIGHT: f32 = studio_ui::ds::size::STRIP;
/// Room for the window's traffic lights at the left of the first header strip.
const TRAFFIC_LIGHTS_WIDTH: f32 = 76.;
/// The traffic lights' offset from the top of that strip: centers the 16-point buttons in it.
const TRAFFIC_LIGHTS_TOP: f32 = (STRIP_HEIGHT - 16.) / 2.;
/// How many visited tabs ⌘[ remembers.
const NAV_HISTORY: usize = 100;
/// The id of the right dock's inspector panel.
const INSPECTOR_PANEL: &str = "inspector";

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Tab for Box<dyn ItemHandle> {
    fn tab_id(&self) -> EntityId {
        self.item_id()
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // A settings file that cannot be read is named in a toast that stays until dismissed.
        let settings_subscription =
            cx.observe_global::<StudioSettings>(|this, cx| this.show_settings_error(cx));
        let bounds_subscription = cx.observe_window_bounds(window, |this, window, cx| {
            let bounds = window.bounds();
            this._save_bounds = Some(cx.spawn(async move |_, cx| {
                cx.background_executor().timer(SAVE_DELAY).await;
                persistence::save_window_bounds(bounds);
            }));
        });
        Self {
            session: None,
            parked: Vec::new(),
            graph_order: Vec::new(),
            saved_session: None,
            panes: vec![Pane::new(0)],
            center: Member::Pane(0),
            active_pane: 0,
            next_pane: 1,
            dedupe_across_panes: true,
            left: Dock::new(PanelPosition::Left, true),
            right: Dock::new(PanelPosition::Right, true),
            bottom: Dock::new(PanelPosition::Bottom, false),
            modal: None,
            toasts: Vec::new(),
            next_toast: 0,
            actions: Vec::new(),
            focus_handle: cx.focus_handle(),
            drag_pending: false,
            traffic_lights: None,
            back: Vec::new(),
            forward: Vec::new(),
            settings_error: None,
            _settings_subscription: settings_subscription,
            saved: None,
            _save: None,
            _save_bounds: None,
            _bounds_subscription: bounds_subscription,
            _session_subscription: None,
            _item_subscriptions: Vec::new(),
        }
    }

    /// The server URL and graph id the open session belongs to.
    fn graph_key(&self, cx: &App) -> Option<(String, String)> {
        let session = self.session.as_ref()?.read(cx);
        let url = session.connection.read(cx).server.url.clone();
        Some((url, session.graph_id.clone()))
    }

    fn snapshot(&self, cx: &App) -> SavedWorkspace {
        let order = self.center.panes();
        let mut panes = Vec::new();
        let mut active_pane = 0;
        for (position, id) in order.iter().enumerate() {
            let Some(pane) = self.pane(*id) else { continue };
            let mut saved = SavedPane::default();
            for (index, item) in pane.items.iter().enumerate() {
                if let Some(item) = item.serialize(cx) {
                    if index == pane.active {
                        saved.active = saved.items.len();
                    }
                    if pane.is_pinned(index) {
                        saved.pinned += 1;
                    }
                    saved.items.push(item);
                }
            }
            if *id == self.active_pane {
                active_pane = position;
            }
            panes.push(saved);
        }
        let layout = self
            .center
            .map(&|id| order.iter().position(|p| *p == id).unwrap_or_default());
        SavedWorkspace {
            items: Vec::new(),
            active: 0,
            panes,
            layout: Some(layout),
            active_pane,
            sidebar_open: self.left.open,
            inspector_open: self.right.open,
            activity_open: self.bottom.open,
            left_panel: self.left.active_id().map(|id| id.to_string()),
            right_panel: self.right.active_id().map(|id| id.to_string()),
            bottom_panel: self.bottom.active_id().map(|id| id.to_string()),
            left_size: Some(self.left.size),
            right_size: Some(self.right.size),
            bottom_size: Some(self.bottom.size),
        }
    }

    /// The open graphs as Studio reopens them at its next start.
    fn session_snapshot(&self, cx: &App) -> SavedSession {
        let open = self.open_graphs();
        let shown = open.iter().position(|(_, shown)| *shown).unwrap_or(0);
        SavedSession {
            graphs: open
                .iter()
                .map(|(session, _)| {
                    let session = session.read(cx);
                    SavedGraph {
                        server_url: session.connection.read(cx).server.url.clone(),
                        graph: session.graph_id.clone(),
                        branch: session.branch.clone(),
                    }
                })
                .collect(),
            shown,
        }
    }

    /// Writes the workspace's state shortly after it stops changing.
    fn save_if_changed(&mut self, cx: &mut Context<Self>) {
        if persistence::restore_enabled() {
            let open = self.session_snapshot(cx);
            if self.saved_session.as_ref() != Some(&open) {
                persistence::save_session(&open);
                self.saved_session = Some(open);
            }
        }
        let Some((url, graph)) = self.graph_key(cx) else {
            return;
        };
        let snapshot = self.snapshot(cx);
        if self.saved.as_ref() == Some(&snapshot) {
            return;
        }
        self.saved = Some(snapshot.clone());
        self._save = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            persistence::save(&url, &graph, &snapshot);
        }));
    }

    /// Reopens the tabs and panels remembered for the open graph.
    fn restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((url, graph)) = self.graph_key(cx) else {
            return;
        };
        let Some(saved) = persistence::load(&url, &graph) else {
            return;
        };
        self.left.open = saved.sidebar_open;
        self.right.open = saved.inspector_open;
        self.bottom.open = saved.activity_open;
        for (dock, size) in [
            (&mut self.left, saved.left_size),
            (&mut self.right, saved.right_size),
            (&mut self.bottom, saved.bottom_size),
        ] {
            if let Some(size) = size {
                dock.size = size;
            }
        }
        for (dock, id) in [
            (&mut self.left, &saved.left_panel),
            (&mut self.right, &saved.right_panel),
            (&mut self.bottom, &saved.bottom_panel),
        ] {
            if let Some(id) = id {
                let open = dock.open;
                dock.activate(id);
                dock.open = open;
            }
        }
        // A file from before splits has its tabs at the top level: one pane.
        let (layout, panes) = if saved.panes.is_empty() {
            let pane = SavedPane {
                items: saved.items.clone(),
                active: saved.active,
                pinned: 0,
            };
            (Member::Pane(0), vec![pane])
        } else {
            let mut ids = saved.layout.as_ref().map(Member::panes).unwrap_or_default();
            ids.sort_unstable();
            match &saved.layout {
                Some(layout) if ids == (0..saved.panes.len()).collect::<Vec<_>>() => {
                    (layout.clone(), saved.panes.clone())
                }
                // A layout that does not name every pane once: the first pane alone.
                _ => (Member::Pane(0), vec![saved.panes[0].clone()]),
            }
        };
        self.panes = (0..panes.len()).map(Pane::new).collect();
        self.center = layout;
        self.next_pane = panes.len();
        self.dedupe_across_panes = false;
        for (id, saved_pane) in panes.iter().enumerate() {
            self.active_pane = id;
            for item in &saved_pane.items {
                if let Some(open) = persistence::opener(cx, &item.kind) {
                    open(&item.data, self, window, cx);
                }
            }
            if let Some(pane) = self.pane_mut(id) {
                pane.pinned = saved_pane.pinned.min(pane.items.len());
                pane.preview = None;
                pane.active = saved_pane.active.min(pane.items.len().saturating_sub(1));
            }
        }
        self.dedupe_across_panes = true;
        // A pane whose tabs could not be reopened is dropped.
        for id in self.center.panes() {
            if self.pane(id).is_some_and(|p| p.items.is_empty()) && self.panes.len() > 1 {
                self.remove_pane(id);
            }
        }
        let order = self.center.panes();
        let active = order
            .get(saved.active_pane)
            .copied()
            .unwrap_or_else(|| self.center.top_left());
        let index = self.pane(active).map_or(0, |p| p.active);
        self.activate_in(active, index, false, window, cx);
        self.saved = Some(saved);
    }

    // ---- session ----

    pub fn session(&self) -> Option<&Entity<GraphSession>> {
        self.session.as_ref()
    }

    /// Opens a graph session in this window. Another graph's tabs are kept: that graph is
    /// parked and comes back from the graph switcher.
    pub fn set_session(
        &mut self,
        session: Entity<GraphSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self
            .session
            .as_ref()
            .is_some_and(|current| same_graph(current, &session, cx));
        if shown {
            self.subscribe_session(session, window, cx);
            cx.emit(WorkspaceEvent::SessionChanged);
            cx.notify();
            return;
        }
        let parked = self
            .parked
            .iter()
            .position(|parked| same_graph(&parked.session, &session, cx))
            .map(|index| self.parked.remove(index));
        if let Some(current) = self.park(cx) {
            self.parked.push(current);
        }
        if let Some(parked) = parked {
            self.show_parked(parked, window, cx);
            return;
        }
        self.graph_order.push(session.entity_id());
        self.subscribe_session(session, window, cx);
        if persistence::restore_enabled() {
            self.saved = None;
            self.restore(window, cx);
        }
        // Focus the graph's active tab, else the workspace: a parked graph's tab may still hold
        // the focus, and actions dispatched from an element that is not drawn reach nothing.
        let index = self.pane(self.active_pane).map_or(0, |p| p.active);
        self.activate_in(self.active_pane, index, false, window, cx);
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    fn subscribe_session(
        &mut self,
        session: Entity<GraphSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._session_subscription = Some(cx.subscribe_in(
            &session,
            window,
            |this, _session, event: &SessionEvent, _window, cx| {
                if let SessionEvent::ActivityChanged = event {
                    this.toast_for_latest_outcome(cx);
                }
                cx.notify();
            },
        ));
        self.session = Some(session);
    }

    /// Writes the shown graph's state now, rather than after the save delay.
    fn save_now(&mut self, cx: &mut Context<Self>) {
        let Some((url, graph)) = self.graph_key(cx) else {
            return;
        };
        let snapshot = self.snapshot(cx);
        self._save = None;
        persistence::save(&url, &graph, &snapshot);
        self.saved = Some(snapshot);
    }

    /// Takes the shown graph out of the window, saved, leaving no graph shown.
    fn park(&mut self, cx: &mut Context<Self>) -> Option<ParkedGraph> {
        self.save_now(cx);
        let session = self.session.take()?;
        self._session_subscription = None;
        self.right.remove(INSPECTOR_PANEL);
        // A modal open now must not hand the focus back to a tab of this graph.
        if let Some(modal) = &mut self.modal {
            modal.previous_focus = None;
        }
        Some(ParkedGraph {
            session,
            panes: std::mem::replace(&mut self.panes, vec![Pane::new(0)]),
            center: std::mem::replace(&mut self.center, Member::Pane(0)),
            active_pane: std::mem::replace(&mut self.active_pane, 0),
            next_pane: std::mem::replace(&mut self.next_pane, 1),
            back: std::mem::take(&mut self.back),
            forward: std::mem::take(&mut self.forward),
            saved: self.saved.take(),
        })
    }

    fn show_parked(&mut self, parked: ParkedGraph, window: &mut Window, cx: &mut Context<Self>) {
        self.panes = parked.panes;
        self.center = parked.center;
        self.active_pane = parked.active_pane;
        self.next_pane = parked.next_pane;
        self.back = parked.back;
        self.forward = parked.forward;
        self.saved = parked.saved;
        self.subscribe_session(parked.session, window, cx);
        let index = self.pane(self.active_pane).map_or(0, |p| p.active);
        self.activate_in(self.active_pane, index, false, window, cx);
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    /// The graphs open in this window, in the order they were opened, and which is shown.
    pub fn open_graphs(&self) -> Vec<(Entity<GraphSession>, bool)> {
        self.graph_order
            .iter()
            .filter_map(|id| {
                if let Some(session) = self.session.as_ref().filter(|s| s.entity_id() == *id) {
                    return Some((session.clone(), true));
                }
                self.parked
                    .iter()
                    .find(|parked| parked.session.entity_id() == *id)
                    .map(|parked| (parked.session.clone(), false))
            })
            .collect()
    }

    /// Shows the open graph whose session is `id`.
    pub fn show_graph(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.as_ref().map(Entity::entity_id) == Some(id) {
            return;
        }
        let Some(index) = self.parked.iter().position(|p| p.session.entity_id() == id) else {
            return;
        };
        let target = self.parked.remove(index);
        if let Some(current) = self.park(cx) {
            self.parked.push(current);
        }
        self.show_parked(target, window, cx);
    }

    /// Shows the next (or previous) open graph.
    fn show_graph_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let order: Vec<EntityId> = self
            .open_graphs()
            .iter()
            .map(|(session, _)| session.entity_id())
            .collect();
        let Some(current) = self.session.as_ref().map(Entity::entity_id) else {
            return;
        };
        let Some(position) = order.iter().position(|id| *id == current) else {
            return;
        };
        let next = if forward {
            order[(position + 1) % order.len()]
        } else {
            order[(position + order.len() - 1) % order.len()]
        };
        self.show_graph(next, window, cx);
    }

    /// Closes the shown graph's tabs in this window and shows the graph opened before it. Its
    /// tabs are saved first, so opening it again brings them back. A graph with a write still
    /// running stays open: closing it could drop the write's outcome.
    pub fn close_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if session.read(cx).writes_in_flight > 0 {
            let graph = session.read(cx).graph_id.clone();
            self.show_toast(
                ToastKind::Error,
                format!("A write to {graph} is still running. Close the graph when it finishes."),
                cx,
            );
            return;
        }
        let id = session.entity_id();
        if let Some(closed) = self.park(cx) {
            for item in closed.panes.iter().flat_map(|pane| pane.items.iter()) {
                let item = item.item_id();
                self._item_subscriptions.retain(|(id, _)| *id != item);
            }
        }
        self.graph_order.retain(|open| *open != id);
        let next = self.graph_order.last().copied().and_then(|next| {
            let index = self
                .parked
                .iter()
                .position(|p| p.session.entity_id() == next)?;
            Some(self.parked.remove(index))
        });
        match next {
            Some(next) => self.show_parked(next, window, cx),
            None => {
                window.focus(&self.focus_handle, cx);
                cx.emit(WorkspaceEvent::SessionChanged);
                cx.notify();
            }
        }
    }

    fn toast_for_latest_outcome(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let session = session.read(cx);
        if session.writes_in_flight > 0 {
            return;
        }
        let Some(latest) = session.outcomes.last() else {
            return;
        };
        let shown = self
            .toasts
            .iter()
            .any(|t| t.message.as_ref() == toast_text(latest).as_str());
        if shown {
            return;
        }
        let elapsed = latest.at.elapsed().unwrap_or_default();
        if elapsed > Duration::from_secs(2) {
            return;
        }
        let text = toast_text(latest);
        match &latest.outcome {
            // No publish, no toast (constitution VII); the surface that wrote explains it.
            OutcomeKind::Committed { commit: None } => {}
            OutcomeKind::Committed {
                commit: Some(commit),
            } => {
                let action = Box::new(OpenCommit {
                    commit_id: commit.clone(),
                });
                self.show_toast_with_action(ToastKind::Success, text, "View commit", action, cx);
            }
            // The write may or may not have landed: the branch's history says which.
            OutcomeKind::Unknown { .. } => {
                self.show_toast_with_action(
                    ToastKind::Error,
                    text,
                    "Open history",
                    Box::new(OpenHistory),
                    cx,
                );
            }
            OutcomeKind::Refused { .. } => self.show_toast(ToastKind::Error, text, cx),
        }
    }

    // ---- panes and items ----

    fn pane(&self, id: PaneId) -> Option<&Pane<Box<dyn ItemHandle>>> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane<Box<dyn ItemHandle>>> {
        self.panes.iter_mut().find(|pane| pane.id == id)
    }

    /// The pane and position of an open item.
    fn find_item(&self, id: EntityId) -> Option<(PaneId, usize)> {
        self.panes
            .iter()
            .find_map(|pane| pane.position(id).map(|index| (pane.id, index)))
    }

    /// The tab with `key`: in the active pane, else (unless restoring or splitting) another.
    fn find_key(&self, key: &SharedString, cx: &App) -> Option<(PaneId, usize)> {
        let in_pane = |pane: &Pane<Box<dyn ItemHandle>>| {
            pane.items
                .iter()
                .position(|item| item.dedupe_key(cx).as_ref() == Some(key))
                .map(|index| (pane.id, index))
        };
        self.pane(self.active_pane).and_then(in_pane).or_else(|| {
            self.dedupe_across_panes
                .then(|| self.panes.iter().find_map(in_pane))
                .flatten()
        })
    }

    /// Adds an item as a tab of the active pane, or activates the tab with the same dedupe key.
    pub fn add_item(
        &mut self,
        item: Box<dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = item.dedupe_key(cx)
            && let Some((pane, index)) = self.find_key(&key, cx)
        {
            self.activate_in(pane, index, true, window, cx);
            return;
        }
        self.insert(item, false, window, cx);
    }

    /// Opens an item of type `T`, unless a tab with `key` is open, which is activated instead.
    pub fn open_item<T: Item>(
        &mut self,
        key: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Window, &mut Context<T>) -> T,
    ) -> Option<Entity<T>> {
        self.open_item_with(key, false, window, cx, build)
    }

    /// As [`Self::open_item`]; a `preview` tab takes the place of the pane's preview tab, so
    /// browsing does not pile up tabs (spec 002). Opening it again without `preview` keeps it.
    pub fn open_item_with<T: Item>(
        &mut self,
        key: impl Into<SharedString>,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Window, &mut Context<T>) -> T,
    ) -> Option<Entity<T>> {
        let key = key.into();
        if let Some((pane, index)) = self.find_key(&key, cx) {
            let item = &self.pane(pane)?.items[index];
            let (id, view) = (item.item_id(), item.to_any());
            if !preview && let Some(pane) = self.pane_mut(pane) {
                pane.keep(id);
            }
            self.activate_in(pane, index, true, window, cx);
            return view.downcast::<T>().ok();
        }
        let item = cx.new(|cx| build(window, cx));
        let subscription = cx.subscribe_in(
            &item,
            window,
            |this, item, event: &ItemEvent, window, cx| match event {
                ItemEvent::UpdateTab => {
                    // Work in a preview tab keeps it.
                    let id = item.entity_id();
                    if item.read(cx).is_dirty(cx)
                        && let Some((pane, _)) = this.find_item(id)
                        && let Some(pane) = this.pane_mut(pane)
                    {
                        pane.keep(id);
                    }
                    cx.notify()
                }
                // The item asked to close; it has already decided.
                ItemEvent::Close => {
                    if let Some((pane, index)) = this.find_item(item.entity_id()) {
                        this.force_close(pane, index, window, cx);
                    }
                }
            },
        );
        self._item_subscriptions
            .push((item.entity_id(), subscription));
        self.insert(Box::new(item.clone()), preview, window, cx);
        Some(item)
    }

    /// Puts an item in the active pane and activates it.
    fn insert(
        &mut self,
        item: Box<dyn ItemHandle>,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane_id = self.active_pane;
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        // A preview with work in it is not replaced.
        if let Some(current) = pane.preview
            && let Some(index) = pane.position(current)
            && pane.items[index].is_dirty(cx)
        {
            pane.keep(current);
        }
        let (index, replaced) = pane.add(item, preview);
        if let Some(replaced) = replaced {
            let id = replaced.item_id();
            self._item_subscriptions.retain(|(item, _)| *item != id);
            self.back.retain(|visited| *visited != id);
        }
        self.activate_in(pane_id, index, true, window, cx);
    }

    /// Every tab of the shown graph, pane by pane.
    pub fn all_items(&self) -> impl Iterator<Item = &dyn ItemHandle> {
        self.center
            .panes()
            .into_iter()
            .filter_map(|id| self.pane(id))
            .flat_map(|pane| pane.items.iter().map(|item| item.as_ref()))
    }

    /// Activates a tab of the shown graph wherever it is; false when there is none.
    pub fn activate_item_by_id(
        &mut self,
        id: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.find_item(id) {
            Some((pane, index)) => {
                self.activate_in(pane, index, true, window, cx);
                true
            }
            None => false,
        }
    }

    /// The active pane's tabs.
    pub fn items(&self) -> &[Box<dyn ItemHandle>] {
        self.pane(self.active_pane)
            .map_or(&[], |pane| pane.items.as_slice())
    }

    pub fn active_item(&self) -> Option<&dyn ItemHandle> {
        self.pane(self.active_pane)?
            .active_item()
            .map(|item| item.as_ref())
    }

    /// Activates a tab of the active pane.
    pub fn activate_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_in(self.active_pane, index, true, window, cx);
    }

    /// Activates a pane's tab; `record` remembers the tab left for ⌘[ and forgets the ⌘] trail.
    fn activate_in(
        &mut self,
        pane_id: PaneId,
        index: usize,
        record: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(next) = self
            .pane(pane_id)
            .and_then(|pane| pane.items.get(index))
            .map(|item| item.item_id())
        else {
            // An empty pane takes the focus itself.
            if self.pane(pane_id).is_some() {
                self.active_pane = pane_id;
                window.focus(&self.focus_handle, cx);
                cx.notify();
            }
            return;
        };
        if record
            && let Some(current) = self.active_item().map(|item| item.item_id())
            && current != next
        {
            self.back.retain(|visited| *visited != current);
            self.back.push(current);
            if self.back.len() > NAV_HISTORY {
                self.back.remove(0);
            }
            self.forward.clear();
        }
        self.active_pane = pane_id;
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.active = index;
            let focus = pane.items[index].focus_handle(cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// ⌘[ and ⌘]: the previous or next tab in the order they were visited, across panes,
    /// skipping closed ones.
    fn navigate(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        loop {
            let next = if back {
                self.back.pop()
            } else {
                self.forward.pop()
            };
            let Some(id) = next else { return };
            let Some((pane, index)) = self.find_item(id) else {
                continue;
            };
            let current = self.active_item().map(|item| item.item_id());
            if current == Some(id) {
                continue;
            }
            if let Some(current) = current {
                if back {
                    self.forward.push(current);
                } else {
                    self.back.push(current);
                }
            }
            self.activate_in(pane, index, false, window, cx);
            return;
        }
    }

    /// Closes a tab of the active pane; one with unsaved work or a write in flight asks first.
    pub fn close_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_in(self.active_pane, index, window, cx);
    }

    fn close_in(
        &mut self,
        pane: PaneId,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.pane(pane).and_then(|p| p.items.get(index)) else {
            return;
        };
        if !item.is_dirty(cx) {
            self.force_close(pane, index, window, cx);
            return;
        }
        let item_id = item.item_id();
        let title = item.tab_title(cx);
        let workspace = cx.entity().downgrade();
        self.toggle_modal(window, cx, move |_, cx| ConfirmClose {
            workspace,
            item_id,
            title,
            focus_handle: cx.focus_handle(),
        });
    }

    /// Closes the tabs with these ids that have nothing unsaved; the others stay open, with
    /// their dot.
    fn close_clean(&mut self, ids: Vec<EntityId>, window: &mut Window, cx: &mut Context<Self>) {
        for id in ids {
            if let Some((pane, index)) = self.find_item(id)
                && !self.pane(pane).is_some_and(|p| p.items[index].is_dirty(cx))
            {
                self.force_close(pane, index, window, cx);
            }
        }
    }

    /// Closes a tab of the active pane without asking.
    pub fn force_close_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.force_close(self.active_pane, index, window, cx);
    }

    /// Closes a tab without asking. A pane left empty closes too, unless it is the only one.
    fn force_close(
        &mut self,
        pane_id: PaneId,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(removed) = self.pane_mut(pane_id).and_then(|pane| pane.remove(index)) else {
            return;
        };
        self._item_subscriptions
            .retain(|(id, _)| *id != removed.item_id());
        let emptied = self.pane(pane_id).is_some_and(|p| p.items.is_empty());
        if emptied && self.panes.len() > 1 {
            self.remove_pane(pane_id);
        }
        if pane_id == self.active_pane || !self.center.contains(pane_id) {
            let active = self.active_pane;
            let index = self.pane(active).map_or(0, |p| p.active);
            self.activate_in(active, index, false, window, cx);
        }
        cx.notify();
    }

    /// Takes a pane out of the split tree; its neighbour in reading order becomes active.
    fn remove_pane(&mut self, id: PaneId) {
        let order = self.center.panes();
        if !self.center.remove(id) {
            return;
        }
        self.panes.retain(|pane| pane.id != id);
        if self.active_pane == id {
            let position = order.iter().position(|p| *p == id).unwrap_or_default();
            self.active_pane = order
                .get(position.wrapping_sub(1))
                .or_else(|| order.get(position + 1))
                .copied()
                .filter(|p| *p != id)
                .unwrap_or_else(|| self.center.top_left());
        }
    }

    /// Splits the active pane; the new pane opens a copy of the active tab, when the tab can
    /// be reopened, and becomes active.
    pub fn split(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let at = self.active_pane;
        let new = self.next_pane;
        if !self.center.split(at, new, direction) {
            return;
        }
        self.next_pane += 1;
        self.panes.push(Pane::new(new));
        let copy = self.active_item().and_then(|item| item.serialize(cx));
        self.active_pane = new;
        if let Some(copy) = copy
            && let Some(open) = persistence::opener(cx, &copy.kind)
        {
            self.dedupe_across_panes = false;
            open(&copy.data, self, window, cx);
            self.dedupe_across_panes = true;
        }
        let index = self.pane(new).map_or(0, |p| p.active);
        self.activate_in(new, index, true, window, cx);
    }

    /// Moves to the next (or previous) pane in reading order.
    fn activate_pane_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.center.panes();
        let Some(position) = order.iter().position(|p| *p == self.active_pane) else {
            return;
        };
        let next = if forward {
            order[(position + 1) % order.len()]
        } else {
            order[(position + order.len() - 1) % order.len()]
        };
        let index = self.pane(next).map_or(0, |p| p.active);
        self.activate_in(next, index, true, window, cx);
    }

    fn toggle_pin(&mut self, pane: PaneId, index: usize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane) {
            pane.toggle_pin(index);
        }
        cx.notify();
    }

    // ---- docks ----

    fn dock_mut(&mut self, position: PanelPosition) -> &mut Dock {
        match position {
            PanelPosition::Left => &mut self.left,
            PanelPosition::Right => &mut self.right,
            PanelPosition::Bottom => &mut self.bottom,
        }
    }

    /// Adds a panel to a dock, or replaces the view of the panel with the same id there.
    pub fn add_panel(
        &mut self,
        position: PanelPosition,
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        icon: IconName,
        view: AnyView,
        cx: &mut Context<Self>,
    ) {
        self.dock_mut(position).insert(PanelEntry {
            id: id.into(),
            title: title.into(),
            icon,
            view,
        });
        cx.notify();
    }

    /// Shows a panel: makes it its dock's active panel and opens the dock.
    pub fn activate_panel(&mut self, id: &str, cx: &mut Context<Self>) {
        for dock in [&mut self.left, &mut self.right, &mut self.bottom] {
            if dock.activate(id) {
                break;
            }
        }
        cx.notify();
    }

    /// The inspector: the right dock's panel for the selected row or node. `None` removes it.
    pub fn set_inspector(&mut self, view: Option<AnyView>, cx: &mut Context<Self>) {
        match view {
            Some(view) => {
                self.right.insert(PanelEntry {
                    id: INSPECTOR_PANEL.into(),
                    title: "Inspector".into(),
                    icon: IconName::Info,
                    view,
                });
                // Selection shows the inspector but never reopens a dock the user closed.
                let open = self.right.open;
                self.right.activate(INSPECTOR_PANEL);
                self.right.open = open;
            }
            None => self.right.remove(INSPECTOR_PANEL),
        }
        cx.notify();
    }

    /// The activity log: the bottom dock's panel.
    pub fn set_activity_view(&mut self, view: AnyView, cx: &mut Context<Self>) {
        let open = self.bottom.open;
        self.add_panel(
            PanelPosition::Bottom,
            "activity",
            "Activity",
            IconName::HistoryRerun,
            view,
            cx,
        );
        self.bottom.open = open;
    }

    // ---- modals and toasts ----

    /// Shows a modal, or closes it when a modal of the same type is open.
    pub fn toggle_modal<V: ModalView>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
    ) {
        if let Some(active) = &self.modal
            && active.view.clone().downcast::<V>().is_ok()
        {
            self.hide_modal(window, cx);
            return;
        }
        let previous_focus = window.focused(cx);
        let view = cx.new(|cx| build(window, cx));
        let subscription =
            cx.subscribe_in(&view, window, |this, _, _: &DismissEvent, window, cx| {
                this.hide_modal(window, cx);
            });
        let focus = view.focus_handle(cx);
        self.modal = Some(ActiveModal {
            view: view.into(),
            previous_focus,
            _subscription: subscription,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    pub fn hide_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(modal) = self.modal.take() {
            match modal.previous_focus {
                Some(focus) => window.focus(&focus, cx),
                // What had the focus left with its graph: focus the shown graph's tab.
                None => {
                    let index = self.pane(self.active_pane).map_or(0, |p| p.active);
                    self.activate_in(self.active_pane, index, false, window, cx);
                }
            }
            cx.notify();
        }
    }

    pub fn has_modal(&self) -> bool {
        self.modal.is_some()
    }

    pub fn show_toast(
        &mut self,
        kind: ToastKind,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.push_toast(kind, message.into(), None, cx);
    }

    /// A toast with one follow-up button, such as "View commit" (spec 002).
    pub fn show_toast_with_action(
        &mut self,
        kind: ToastKind,
        message: impl Into<SharedString>,
        label: impl Into<SharedString>,
        action: Box<dyn Action>,
        cx: &mut Context<Self>,
    ) {
        self.push_toast(kind, message.into(), Some((label.into(), action)), cx);
    }

    fn show_settings_error(&mut self, cx: &mut Context<Self>) {
        let error = studio_settings::file_error(cx);
        if error == self.settings_error {
            return;
        }
        self.settings_error = error.clone();
        if let Some(error) = error {
            self.show_toast_with_action(
                ToastKind::Error,
                error,
                "Open settings",
                Box::new(studio_actions::workspace::OpenSettings),
                cx,
            );
        }
    }

    fn push_toast(
        &mut self,
        kind: ToastKind,
        message: SharedString,
        action: Option<(SharedString, Box<dyn Action>)>,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_toast;
        self.next_toast += 1;
        let remaining = match kind {
            ToastKind::Error => None,
            ToastKind::Info | ToastKind::Success => Some(TOAST_DURATION),
        };
        self.toasts.push(Toast {
            id,
            kind,
            message,
            action,
            remaining,
            hovered: false,
        });
        cx.notify();
        if remaining.is_none() {
            return;
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TOAST_TICK).await;
                let gone = this
                    .update(cx, |this, cx| {
                        let Some(toast) = this.toasts.iter_mut().find(|t| t.id == id) else {
                            return true;
                        };
                        if !toast.hovered {
                            let left = toast
                                .remaining
                                .unwrap_or_default()
                                .saturating_sub(TOAST_TICK);
                            toast.remaining = Some(left);
                            if left.is_zero() {
                                this.toasts.retain(|t| t.id != id);
                                cx.notify();
                                return true;
                            }
                        }
                        false
                    })
                    .unwrap_or(true);
                if gone {
                    break;
                }
            }
        })
        .detach();
    }

    pub fn dismiss_toast(&mut self, id: usize, cx: &mut Context<Self>) {
        self.toasts.retain(|t| t.id != id);
        cx.notify();
    }

    // ---- actions ----

    /// Registers a handler for an action anywhere in the window. Feature crates call this from
    /// `cx.observe_new::<Workspace>`.
    pub fn register_action<A: Action>(
        &mut self,
        handler: impl Fn(&mut Workspace, &A, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let handler = Rc::new(handler);
        self.actions.push(Rc::new(move |div, _window, cx| {
            let handler = handler.clone();
            div.on_action(cx.listener(move |workspace, action: &A, window, cx| {
                handler(workspace, action, window, cx)
            }))
        }));
    }

    // ---- rendering ----

    /// A header or footer strip of a column (spec 002, after Delta): fixed height, and a place
    /// to drag the window from. As in Zed's title bar, the move starts on the first mouse move
    /// after a press, so clicks on the strip's buttons still land.
    fn strip(&self, id: impl Into<ElementId>, cx: &mut Context<Self>) -> Stateful<Div> {
        h_flex()
            .id(id)
            .window_control_area(WindowControlArea::Drag)
            .h(px(STRIP_HEIGHT))
            .w_full()
            .flex_none()
            .items_center()
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.drag_pending = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag_pending = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag_pending = true),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.drag_pending {
                    this.drag_pending = false;
                    window.start_window_move();
                }
            }))
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let panel = colors.panel_background;
        let hairline = studio_ui::ds::surface::rule(cx);
        let header = self
            .strip("sidebar-header", cx)
            .pr_2()
            .gap_0p5()
            .when(!studio_ui::ds::is_glass(cx), |d| {
                d.border_b_1().border_color(hairline)
            })
            .child(div().w(px(TRAFFIC_LIGHTS_WIDTH)).flex_none())
            .child(
                IconButton::new("toggle-sidebar", IconName::ThreadsSidebarLeftOpen)
                    .icon_size(IconSize::Small)
                    .tooltip(|_, cx| Tooltip::for_action("Hide sidebar", &ToggleSidebar, cx))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleSidebar), cx)),
            )
            .child(div().w(px(studio_ui::ds::space::SM)))
            .child(self.render_icon_tabs(&self.left, cx))
            .child(div().flex_1())
            .child(
                IconButton::new("search", IconName::MagnifyingGlass)
                    .icon_size(IconSize::Small)
                    .tooltip(|_, cx| {
                        Tooltip::for_action(
                            "Search types, queries and commands",
                            &studio_actions::workspace::ToggleCommandPalette,
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(
                            Box::new(studio_actions::workspace::ToggleCommandPalette),
                            cx,
                        )
                    }),
            );
        let body = v_flex()
            .id("sidebar-panel")
            .flex_1()
            .min_h_0()
            .pb_2()
            .overflow_y_scroll()
            .children(self.left.active_panel().map(|panel| {
                v_flex()
                    .w_full()
                    .child(studio_ui::ds::section_label(panel.title.clone()))
                    .child(panel.view.clone())
            }));
        let glass = studio_ui::ds::is_glass(cx);
        let column = if glass {
            studio_ui::ds::sheet(cx)
        } else {
            div().bg(panel).border_r_1().border_color(hairline)
        };
        column
            .id("sidebar")
            .relative()
            .flex()
            .flex_col()
            .w(px(self.left.size))
            .h_full()
            .flex_none()
            .child(header)
            .child(body)
            .child(self.render_sidebar_footer(cx))
            .child(resize_handle(PanelPosition::Left))
    }

    /// A dock's panels as icon tabs (the left dock, as Obsidian and Delta switch sidebar modes):
    /// the active one pressed, each named in its tooltip.
    fn render_icon_tabs(&self, dock: &Dock, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap(px(studio_ui::ds::space::XXS))
            .when(dock.panels.len() > 1, |tabs| {
                tabs.children(dock.panels.iter().enumerate().map(|(index, panel)| {
                    let id = panel.id.clone();
                    let title = panel.title.clone();
                    IconButton::new(("dock-tab", index), panel.icon)
                        .icon_size(IconSize::Small)
                        .toggle_state(index == dock.active)
                        .tooltip(Tooltip::text(title))
                        .on_click(cx.listener(move |this, _, _, cx| this.activate_panel(&id, cx)))
                }))
            })
    }

    /// A dock's panels as title tabs (the right and bottom docks), or its one panel's title.
    fn render_title_tabs(&self, dock: &Dock, cx: &mut Context<Self>) -> impl IntoElement {
        let choice = dock.panels.len() > 1;
        h_flex()
            .h_full()
            .gap(px(studio_ui::ds::space::XS))
            .items_center()
            .children(dock.panels.iter().enumerate().map(|(index, panel)| {
                let id = panel.id.clone();
                studio_ui::ds::title_tab(
                    ("dock-title", index),
                    panel.icon,
                    panel.title.clone(),
                    index == dock.active,
                    choice,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.activate_panel(&id, cx)))
            }))
    }

    /// The server this window is connected to, as Delta's sidebar footer shows the account: a
    /// status dot, the server and its state. Clicking opens "Open a graph". The graph and branch
    /// are in the document column's footer, on the same baseline.
    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hairline = studio_ui::ds::surface::rule(cx);
        let (color, server, url, state) = match &self.session {
            Some(session) => {
                let connection = session.read(cx).connection.read(cx);
                let color = match &connection.state {
                    ConnectionState::Ready => Color::Success,
                    ConnectionState::Connecting | ConnectionState::Idle => Color::Muted,
                    ConnectionState::ContractMismatch { .. } => Color::Warning,
                    _ => Color::Error,
                };
                (
                    color,
                    connection.server.name.clone(),
                    connection.server.url.clone(),
                    connection.state.label(),
                )
            }
            None => (
                Color::Muted,
                "No server".to_owned(),
                String::new(),
                "Not connected".to_owned(),
            ),
        };
        let open = self.open_graphs();
        let others = open.len().saturating_sub(1);
        let workspace = cx.entity().downgrade();
        let entries: Vec<(EntityId, String, bool)> = open
            .iter()
            .map(|(session, shown)| {
                let s = session.read(cx);
                let server = s.connection.read(cx).server.name.clone();
                (
                    session.entity_id(),
                    format!("{server} / {}", s.graph_id),
                    *shown,
                )
            })
            .collect();
        let trigger = ButtonLike::new("graph-switcher")
            .full_width()
            .tooltip(Tooltip::text(format!("{url} · {state} · Switch graph")))
            .child(
                h_flex()
                    .w_full()
                    .px(px(studio_ui::ds::space::XS))
                    .gap(px(studio_ui::ds::space::SM))
                    .items_center()
                    .child(Indicator::dot().color(color))
                    .child(
                        Label::new(server)
                            .size(LabelSize::Small)
                            .single_line()
                            .truncate(),
                    )
                    .child(
                        Label::new(state)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                    )
                    .child(div().flex_1())
                    .when(others > 0, |row| {
                        row.child(studio_ui::ds::sidebar_count(format!(
                            "+{others} graph{}",
                            if others == 1 { "" } else { "s" }
                        )))
                    })
                    .child(
                        Icon::new(IconName::ChevronUpDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
        self.strip("sidebar-footer", cx)
            .px(px(studio_ui::ds::space::XS))
            .border_t_1()
            .border_color(hairline)
            .child(
                PopoverMenu::new("graph-switcher-menu")
                    .full_width(true)
                    .anchor(gpui::Anchor::BottomLeft)
                    .trigger(trigger)
                    .menu(move |window, cx| {
                        let entries = entries.clone();
                        let workspace = workspace.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            if !entries.is_empty() {
                                menu = menu.header("Open in this window");
                            }
                            for (id, label, shown) in entries {
                                let workspace = workspace.clone();
                                menu = menu.toggleable_entry(
                                    label,
                                    shown,
                                    IconPosition::Start,
                                    None,
                                    move |window, cx| {
                                        workspace
                                            .update(cx, |w, cx| w.show_graph(id, window, cx))
                                            .ok();
                                    },
                                );
                            }
                            menu.separator()
                                .action(
                                    "Open Another Graph…",
                                    Box::new(studio_actions::workspace::OpenGraph),
                                )
                                .action("Close Graph", Box::new(CloseGraph))
                        }))
                    }),
            )
    }

    /// A pane's tabs, as Delta draws them: text tabs separated by hairlines, the active one
    /// joined to the content below it. Pinned tabs come first with a pin; a preview tab's title
    /// is in italics until it is kept (a double click, or work in it).
    fn render_tabs(&self, pane_id: PaneId, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        // In the glass style the tab strip is part of the sheet: the active tab is clear and the
        // others are barely shaded.
        let (editor_bg, inactive_bg) = if studio_ui::ds::is_glass(cx) {
            (gpui::transparent_black(), gpui::black().opacity(0.025))
        } else {
            (
                colors.editor_background,
                colors.tab_bar_background.opacity(0.35),
            )
        };
        let hairline = studio_ui::ds::surface::rule(cx);
        let hover_bg = colors.ghost_element_hover;
        let split = self.panes.len() > 1;
        let focused_pane = pane_id == self.active_pane;
        let workspace = cx.entity().downgrade();
        let Some(pane) = self.pane(pane_id) else {
            return h_flex().into_any_element();
        };
        h_flex()
            .h_full()
            .min_w_0()
            .overflow_hidden()
            .children(pane.items.iter().enumerate().map(|(index, item)| {
                let selected = index == pane.active;
                let pinned = pane.is_pinned(index);
                let preview = pane.is_preview(index);
                let group: SharedString = format!("tab-{pane_id}-{index}").into();
                let dirty = item.is_dirty(cx);
                let stale = item.is_stale(cx);
                let item_id = item.item_id();
                // With a split, the active pane's tab reads at full strength and the others'
                // active tabs are muted, so the focused document is clear.
                let strong = selected && (!split || focused_pane);
                // The strip's height, not `h_full`: the right-click wrapper lays out at its
                // content's height, so a percentage would shrink the tab to its label.
                let tab = h_flex()
                    .id(SharedString::from(format!("tab-{pane_id}-{index}")))
                    .group(group.clone())
                    .h(px(STRIP_HEIGHT))
                    .pl(px(studio_ui::ds::space::LG))
                    .pr(px(studio_ui::ds::space::SM))
                    .gap(px(studio_ui::ds::space::MD))
                    .items_center()
                    .flex_none()
                    .relative()
                    .border_r_1()
                    .border_color(hairline)
                    .when(selected, |tab| tab.bg(editor_bg))
                    .when(!selected, |tab| {
                        tab.bg(inactive_bg)
                            .hover(move |style| style.bg(hover_bg))
                            // The rule under inactive tabs; the active tab joins the content.
                            .child(
                                div()
                                    .absolute()
                                    .bottom_0()
                                    .left_0()
                                    .w_full()
                                    .h(px(1.))
                                    .bg(hairline),
                            )
                    })
                    .cursor_pointer()
                    .children(
                        item.tab_icon(cx)
                            .map(|icon| Icon::new(icon).size(IconSize::XSmall).color(Color::Muted)),
                    )
                    .child(
                        Label::new(item.tab_title(cx))
                            .size(LabelSize::Small)
                            .color(if strong { Color::Default } else { Color::Muted })
                            .when(preview, |label| label.italic())
                            .single_line(),
                    )
                    .when(dirty, |tab| {
                        tab.child(Indicator::dot().color(Color::Warning))
                    })
                    .when(stale && !dirty, |tab| {
                        tab.child(Indicator::dot().color(Color::Muted))
                    })
                    .map(|tab| {
                        if pinned {
                            tab.child(
                                IconButton::new(
                                    SharedString::from(format!("unpin-{pane_id}-{index}")),
                                    IconName::Pin,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Unpin"))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.toggle_pin(pane_id, index, cx),
                                )),
                            )
                        } else {
                            tab.child(
                                IconButton::new(
                                    SharedString::from(format!("close-{pane_id}-{index}")),
                                    IconName::Close,
                                )
                                .icon_size(IconSize::XSmall)
                                .when(!selected, |button| button.visible_on_hover(group))
                                .tooltip(|_, cx| Tooltip::for_action("Close", &CloseActiveItem, cx))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.close_in(pane_id, index, window, cx)
                                    },
                                )),
                            )
                        }
                    })
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.click_count() == 2
                            && let Some(pane) = this.pane_mut(pane_id)
                        {
                            pane.keep(item_id);
                        }
                        this.activate_in(pane_id, index, true, window, cx)
                    }));
                let workspace = workspace.clone();
                right_click_menu(SharedString::from(format!("tab-menu-{pane_id}-{index}")))
                    .trigger(move |_, _, _| tab)
                    .menu(move |window, cx| {
                        let workspace = workspace.clone();
                        ContextMenu::build(window, cx, move |menu, _, _| {
                            tab_menu(menu, workspace.clone(), item_id, pinned)
                        })
                    })
            }))
            .into_any_element()
    }

    /// A pane's header strip: its tabs, then, on the top-right pane, the toggles of the right
    /// and bottom docks. The top-left pane makes room for the traffic lights when the sidebar
    /// is hidden.
    fn render_pane_header(&self, pane_id: PaneId, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let header_bg = if studio_ui::ds::is_glass(cx) {
            gpui::transparent_black()
        } else {
            colors.editor_background.blend(gpui::black().opacity(0.02))
        };
        let hairline = studio_ui::ds::surface::rule(cx);
        let top_right = self.center.top_right() == pane_id;
        let top_left = self.center.top_left() == pane_id;
        let mut context = h_flex().gap_1().items_center().pr_1();
        if top_right {
            context = context.child(
                studio_ui::ds::icon_button("toggle-activity", IconName::ListTree)
                    .icon_size(IconSize::Small)
                    .toggle_state(self.bottom.open)
                    .tooltip(|_, cx| Tooltip::for_action("Activity", &ToggleActivity, cx))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleActivity), cx)),
            );
            if !self.right.panels.is_empty() {
                context = context.child(
                    studio_ui::ds::icon_button(
                        "toggle-inspector",
                        IconName::ThreadsSidebarRightOpen,
                    )
                    .icon_size(IconSize::Small)
                    .toggle_state(self.right.open)
                    .tooltip(|_, cx| Tooltip::for_action("Inspector", &ToggleInspector, cx))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleInspector), cx)
                    }),
                );
            }
        }
        self.strip(SharedString::from(format!("pane-header-{pane_id}")), cx)
            .bg(header_bg)
            .when(
                top_left && (!self.left.visible() || self.session.is_none()),
                |strip| {
                    strip.child(
                        div()
                            .w(px(TRAFFIC_LIGHTS_WIDTH))
                            .h_full()
                            .flex_none()
                            .border_b_1()
                            .border_color(hairline),
                    )
                },
            )
            .child(self.render_tabs(pane_id, cx))
            .child(
                h_flex()
                    .flex_1()
                    .h_full()
                    .justify_end()
                    .items_center()
                    .border_b_1()
                    .border_color(hairline)
                    .child(context),
            )
    }

    /// One pane: its header and its active tab. Pressing anywhere in it makes it the active
    /// pane.
    fn render_pane(&self, pane_id: PaneId, cx: &mut Context<Self>) -> AnyElement {
        let body = match self.pane(pane_id).and_then(|pane| pane.active_item()) {
            Some(item) => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(item.to_any())
                .into_any_element(),
            None if self.panes.len() == 1 => self.render_empty_center(cx).into_any_element(),
            None => studio_ui::ds::empty_state(IconName::Sparkle, "No tab open").into_any_element(),
        };
        v_flex()
            .id(SharedString::from(format!("pane-{pane_id}")))
            .size_full()
            .min_w_0()
            .min_h_0()
            .capture_any_mouse_down(cx.listener(move |this, _, _, cx| {
                if this.active_pane != pane_id {
                    this.active_pane = pane_id;
                    cx.notify();
                }
            }))
            .child(self.render_pane_header(pane_id, cx))
            .child(body)
            .into_any_element()
    }

    /// The split tree: panes side by side or stacked, each with its share, a hairline between
    /// them that drags to resize.
    fn render_member(
        &self,
        member: &Member,
        path: Vec<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Member::Split {
            axis,
            members,
            flexes,
        } = member
        else {
            let Member::Pane(id) = member else {
                unreachable!()
            };
            return self.render_pane(*id, cx);
        };
        let axis = *axis;
        let hairline = studio_ui::ds::surface::rule(cx);
        let horizontal = axis == SplitAxis::Horizontal;
        let split_id = SharedString::from(format!(
            "split-{}",
            path.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join("-")
        ));
        // Members stretch across the split's other axis.
        let mut container = div()
            .flex()
            .when(horizontal, |d| d.flex_row())
            .when(!horizontal, |d| d.flex_col())
            .id(split_id)
            .size_full()
            .min_w_0()
            .min_h_0();
        for (index, (child, flex)) in members.iter().zip(flexes).enumerate() {
            let mut child_path = path.clone();
            child_path.push(index);
            let mut cell = div()
                .relative()
                .flex()
                .flex_col()
                .min_w_0()
                .min_h_0()
                .flex_basis(px(0.))
                .flex_grow(*flex)
                .flex_shrink(1.)
                .child(self.render_member(child, child_path, cx));
            if index > 0 {
                cell = if horizontal {
                    cell.border_l_1()
                } else {
                    cell.border_t_1()
                }
                .border_color(hairline)
                .child(split_handle(path.clone(), index - 1, axis));
            }
            container = container.child(cell);
        }
        let path_for_drag = path.clone();
        container
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedSplit>, _, cx| {
                    let drag = event.drag(cx);
                    if drag.path != path_for_drag {
                        return;
                    }
                    let bounds = event.bounds;
                    let (position, start, length) = if drag.axis == SplitAxis::Horizontal {
                        (event.event.position.x, bounds.origin.x, bounds.size.width)
                    } else {
                        (event.event.position.y, bounds.origin.y, bounds.size.height)
                    };
                    let Some(flexes) = this.center.flexes_at(&drag.path) else {
                        return;
                    };
                    let fraction = f32::from(position - start) / f32::from(length).max(1.);
                    if let Some(next) = resized(flexes, drag.boundary, fraction) {
                        this.center.set_flexes(&drag.path, next);
                        cx.notify();
                    }
                }),
            )
            .into_any_element()
    }

    /// The document column's footer, after Delta's: where the open tab reads and writes (graph
    /// and branch, the branch a menu), the tab's own facts, then what is running and the tab's
    /// controls. It sits on the same baseline as the sidebar's footer.
    fn render_center_footer(
        &self,
        footer: Option<ItemFooter>,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let session = self.session.as_ref()?;
        let s = session.read(cx);
        let graph = s.graph_id.clone();
        let branch = s.branch.clone();
        let unavailable = s.unavailable.clone();
        let branches = s.branches.loaded().cloned().unwrap_or_default();
        let running = match (s.reads_in_flight, s.writes_in_flight) {
            (0, 0) => None,
            (reads, 0) => Some(format!(
                "{reads} quer{} running",
                if reads == 1 { "y" } else { "ies" }
            )),
            (_, writes) => Some(format!("Writing ({writes})…")),
        };
        let session_for_menu = session.clone();
        let footer = footer.unwrap_or_default();
        let hairline = studio_ui::ds::surface::rule(cx);
        let context = h_flex()
            .gap(px(studio_ui::ds::space::XS))
            .items_center()
            .flex_none()
            .child(
                h_flex()
                    .id("footer-graph")
                    .h(px(24.))
                    .px(px(studio_ui::ds::space::SM))
                    .gap(px(studio_ui::ds::space::SM))
                    .items_center()
                    .rounded(px(studio_ui::ds::radius::SM))
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .map(|button| match unavailable.clone() {
                        Some(reason) => button.tooltip(Tooltip::text(format!(
                            "This graph is unavailable: {reason}. Reads resume when it is ready."
                        ))),
                        None => button.tooltip(|_, cx| {
                            Tooltip::for_action(
                                "Open another graph",
                                &studio_actions::workspace::OpenGraph,
                                cx,
                            )
                        }),
                    })
                    .child(
                        Icon::new(IconName::DatabaseZap)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when(unavailable.is_some(), |button| {
                        button.child(Indicator::dot().color(Color::Warning))
                    })
                    .child(Label::new(graph).size(LabelSize::Small).single_line())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(studio_actions::workspace::OpenGraph), cx)
                    }),
            )
            .child(
                Label::new("/")
                    .size(LabelSize::Small)
                    .color(Color::Disabled),
            )
            .child(
                PopoverMenu::new("branch-menu")
                    .anchor(gpui::Anchor::BottomLeft)
                    .trigger(
                        studio_ui::ds::button("branch", branch.clone())
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text(format!(
                                "Reads and edits in this window use {branch}. Switch branch."
                            )))
                            .start_icon(
                                Icon::new(IconName::GitBranch)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .end_icon(
                                Icon::new(IconName::ChevronUpDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .menu(move |window, cx| {
                        let session = session_for_menu.clone();
                        let branches = branches.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            menu = menu.header("Branches");
                            for branch in branches {
                                let session = session.clone();
                                let name = branch.clone();
                                menu = menu.entry(branch, None, move |_, cx| {
                                    session.update(cx, |s, cx| s.set_branch(name.clone(), cx));
                                });
                            }
                            menu
                        }))
                    }),
            );
        let has_start = !footer.start.is_empty();
        Some(
            self.strip("center-footer", cx)
                .pl(px(studio_ui::ds::space::XS))
                .pr(px(studio_ui::ds::space::EDGE))
                .gap(px(studio_ui::ds::space::LG))
                .border_t_1()
                .border_color(hairline)
                .child(context)
                .when(has_start, |strip| strip.child(studio_ui::ds::rule_v(cx)))
                .children(footer.start)
                .child(div().flex_1())
                .children(running.map(|text| {
                    h_flex()
                        .gap(px(studio_ui::ds::space::XS))
                        .items_center()
                        .flex_none()
                        .child(
                            Icon::new(IconName::ArrowCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Muted)
                                .with_rotate_animation(2),
                        )
                        .child(studio_ui::ds::text::meta(text).single_line())
                }))
                .children(footer.end),
        )
    }

    fn render_center(
        &self,
        footer: Option<ItemFooter>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (editor_background, panel_background) =
            (colors.editor_background, colors.panel_background);
        let hairline = studio_ui::ds::surface::rule(cx);
        let center = self.center.clone();
        let body =
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(self.render_member(&center, Vec::new(), cx));
        let column = if studio_ui::ds::is_glass(cx) {
            studio_ui::ds::sheet(cx)
        } else {
            div().bg(editor_background)
        };
        column
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(body)
            .when(self.bottom.visible(), |d| {
                d.child(
                    v_flex()
                        .h(px(self.bottom.size))
                        .relative()
                        .flex_none()
                        .overflow_hidden()
                        .when(!studio_ui::ds::is_glass(cx), |d| d.bg(panel_background))
                        .border_t_1()
                        .border_color(hairline)
                        .child(
                            h_flex()
                                .h(px(studio_ui::ds::size::STRIP))
                                .flex_none()
                                .px(px(studio_ui::ds::space::SM))
                                .gap(px(studio_ui::ds::space::SM))
                                .items_center()
                                .border_b_1()
                                .border_color(hairline)
                                .child(self.render_title_tabs(&self.bottom, cx))
                                .child(div().flex_1())
                                .child(
                                    studio_ui::ds::icon_button(
                                        "close-bottom-dock",
                                        IconName::Close,
                                    )
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(|_, cx| {
                                        Tooltip::for_action("Hide", &ToggleActivity, cx)
                                    })
                                    .on_click(
                                        |_, window, cx| {
                                            window.dispatch_action(Box::new(ToggleActivity), cx)
                                        },
                                    ),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .children(self.bottom.active_panel().map(|p| p.view.clone())),
                        )
                        .child(resize_handle(PanelPosition::Bottom)),
                )
            })
            .children(self.render_center_footer(footer, cx))
    }

    /// No tab open: an icon, one line, and what fills the page (Zed's empty panels).
    fn render_empty_center(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        let action_button = |id: &'static str,
                             label: &'static str,
                             icon: IconName,
                             action: Box<dyn Action>,
                             cx: &mut Context<Self>| {
            let binding = ui::KeyBinding::for_action_in(action.as_ref(), &focus, cx);
            studio_ui::ds::button(id, label)
                .full_width()
                .style(ButtonStyle::Outlined)
                .label_size(LabelSize::Small)
                .start_icon(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                .key_binding(binding)
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        };
        if self.session.is_none() {
            return studio_ui::ds::empty_state(IconName::Server, "No graph open")
                .child(div().w(px(240.)).child(action_button(
                    "open-graph",
                    "Open a graph",
                    IconName::FolderOpen,
                    Box::new(studio_actions::workspace::OpenGraph),
                    cx,
                )))
                .into_any_element();
        }
        studio_ui::ds::empty_state(IconName::Sparkle, "No tab open")
            .child(
                v_flex()
                    .w(px(240.))
                    .gap_1()
                    .child(action_button(
                        "search",
                        "Search",
                        IconName::MagnifyingGlass,
                        Box::new(studio_actions::workspace::ToggleCommandPalette),
                        cx,
                    ))
                    .child(action_button(
                        "new-query",
                        "New query",
                        IconName::Code,
                        Box::new(studio_actions::graph::NewQuery),
                        cx,
                    ))
                    .child(action_button(
                        "history",
                        "History",
                        IconName::GitBranch,
                        Box::new(studio_actions::graph::OpenHistory),
                        cx,
                    ))
                    .child(action_button(
                        "agent",
                        "Agent",
                        IconName::ZedAssistant,
                        Box::new(studio_actions::agent::OpenAgent),
                        cx,
                    )),
            )
            .into_any_element()
    }
    /// The right dock: a header strip with its panels' tabs, then the active panel.
    fn render_right_dock(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.right.visible() {
            return None;
        }
        let panel = self.right.active_panel()?.view.clone();
        let colors = cx.theme().colors();
        let editor_background = colors.editor_background;
        let hairline = studio_ui::ds::surface::rule(cx);
        let column = if studio_ui::ds::is_glass(cx) {
            studio_ui::ds::sheet(cx)
        } else {
            div()
                .bg(editor_background)
                .border_l_1()
                .border_color(hairline)
        };
        Some(
            column
                .relative()
                .flex()
                .flex_col()
                .w(px(self.right.size))
                .h_full()
                .flex_none()
                .child(resize_handle(PanelPosition::Right))
                .child(
                    self.strip("right-dock-header", cx)
                        .px(px(studio_ui::ds::space::SM))
                        .border_b_1()
                        .border_color(hairline)
                        .child(self.render_title_tabs(&self.right, cx))
                        .child(div().flex_1())
                        .child(
                            studio_ui::ds::icon_button("close-right-dock", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .tooltip(|_, cx| Tooltip::for_action("Hide", &ToggleInspector, cx))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ToggleInspector), cx)
                                }),
                        ),
                )
                .child(
                    div()
                        .id("right-dock-panel")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(panel),
                ),
        )
    }

    /// Follows a dock edge being dragged. Sizes are measured from the window's edges, inside the
    /// glass style's gap and above the document footer.
    fn resize_dock(
        &mut self,
        event: &DragMoveEvent<DraggedDock>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let gap = if studio_ui::ds::is_glass(cx) {
            studio_ui::ds::size::SHEET_GAP
        } else {
            0.
        };
        let bounds = event.bounds;
        let pointer = event.event.position;
        let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let x = f32::from(pointer.x - bounds.origin.x);
        let y = f32::from(pointer.y - bounds.origin.y);
        match event.drag(cx).0 {
            PanelPosition::Left => self.left.resize(x - gap, width),
            PanelPosition::Right => self.right.resize(width - gap - x, width),
            PanelPosition::Bottom => {
                let footer = if self.session.is_some() {
                    STRIP_HEIGHT
                } else {
                    0.
                };
                self.bottom.resize(height - gap - footer - y, height)
            }
        }
        cx.notify();
    }

    fn render_toasts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        // Under the header strips, so a toast never covers a document's footer or composer.
        v_flex()
            .absolute()
            .top(px(STRIP_HEIGHT + 8.))
            .right_3()
            .gap_2()
            .children(self.toasts.iter().map(|toast| {
                let color = match toast.kind {
                    ToastKind::Info => Color::Muted,
                    ToastKind::Success => Color::Success,
                    ToastKind::Error => Color::Error,
                };
                let id = toast.id;
                h_flex()
                    .id(("toast", id))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if let Some(toast) = this.toasts.iter_mut().find(|t| t.id == id) {
                            toast.hovered = *hovered;
                        }
                        cx.notify();
                    }))
                    .gap_2()
                    .px_3()
                    .py_2()
                    .max_w(px(420.))
                    .rounded(px(studio_ui::ds::radius::MD))
                    .map(|toast| {
                        if studio_ui::ds::is_glass(cx) {
                            // A small floating glass surface, as Tower floats its pills.
                            toast
                                .bg(gpui::rgba(0xfcfbfce6))
                                .shadow(studio_ui::ds::elevation::float())
                        } else {
                            toast
                                .bg(colors.elevated_surface_background)
                                .border_1()
                                .border_color(colors.border_variant)
                                .shadow(studio_ui::ds::elevation::overlay())
                        }
                    })
                    .child(Indicator::dot().color(color))
                    // `min_w_0` lets the text wrap inside the toast instead of widening it.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(toast.message.clone()).size(LabelSize::Small)),
                    )
                    .children(toast.action.as_ref().map(|(label, action)| {
                        let action = action.boxed_clone();
                        studio_ui::ds::button(("toast-action", id), label.clone())
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.dismiss_toast(id, cx);
                                window.dispatch_action(action.boxed_clone(), cx);
                            }))
                    }))
                    .child(
                        studio_ui::ds::icon_button(("dismiss-toast", id), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.dismiss_toast(id, cx)),
                            ),
                    )
            }))
    }
}

/// Whether two sessions are the same graph on the same server.
fn same_graph(a: &Entity<GraphSession>, b: &Entity<GraphSession>, cx: &App) -> bool {
    let (a, b) = (a.read(cx), b.read(cx));
    a.graph_id == b.graph_id && a.connection == b.connection
}

fn toast_text(entry: &graph_store::OutcomeEntry) -> String {
    match &entry.outcome {
        // A mutation that matched nothing answers without a commit: nothing was published.
        OutcomeKind::Committed { commit: None } => {
            format!(
                "{} · matched nothing; nothing was written",
                entry.description
            )
        }
        OutcomeKind::Committed { .. } => {
            format!("{} · committed to {}", entry.description, entry.branch)
        }
        OutcomeKind::Refused { message } => format!(
            "{} · refused: {message}. Nothing was written.",
            entry.description
        ),
        OutcomeKind::Unknown { .. } => format!(
            "{} · outcome unknown. Check History before trying again.",
            entry.description
        ),
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Every change to tabs or panels re-renders the workspace, so this sees all of them.
        self.save_if_changed(cx);
        // As Zed's workspace does: the window's rem size is the UI font size, which every `ui`
        // size is relative to, and the root carries the UI font.
        let ui_font = theme_settings::setup_ui_font(window, cx);
        let colors = cx.theme().colors();
        let mut root = v_flex()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .font(ui_font)
            .text_ui(cx)
            .map(|root| {
                if studio_ui::ds::is_glass(cx) {
                    // The page, then the wash painted once behind everything.
                    root.bg(studio_ui::ds::surface::page(cx))
                        .child(studio_ui::ds::wash())
                } else {
                    root.bg(colors.background)
                }
            })
            .text_color(colors.text)
            .on_action(cx.listener(|this, _: &CloseActiveItem, window, cx| {
                let index = this.pane(this.active_pane).map_or(0, |p| p.active);
                this.close_item(index, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateNextItem, window, cx| {
                let (len, active) = this
                    .pane(this.active_pane)
                    .map_or((0, 0), |p| (p.items.len(), p.active));
                if len > 0 {
                    this.activate_item((active + 1) % len, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ActivatePreviousItem, window, cx| {
                let (len, active) = this
                    .pane(this.active_pane)
                    .map_or((0, 0), |p| (p.items.len(), p.active));
                if len > 0 {
                    this.activate_item((active + len - 1) % len, window, cx);
                }
            }))
            .on_action(cx.listener(|this, action: &ActivateItem, window, cx| {
                this.activate_item(action.index, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateLastItem, window, cx| {
                if let Some(last) = this.items().len().checked_sub(1) {
                    this.activate_item(last, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ActivateNextGraph, window, cx| {
                this.show_graph_step(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivatePreviousGraph, window, cx| {
                this.show_graph_step(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseGraph, window, cx| this.close_graph(window, cx)))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.split(SplitDirection::Right, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                this.split(SplitDirection::Down, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateNextPane, window, cx| {
                this.activate_pane_step(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivatePreviousPane, window, cx| {
                this.activate_pane_step(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &TogglePinTab, _, cx| {
                let pane = this.active_pane;
                let index = this.pane(pane).map_or(0, |p| p.active);
                this.toggle_pin(pane, index, cx);
            }))
            .on_action(cx.listener(|this, _: &KeepPreviewTab, _, cx| {
                if let Some(id) = this.active_item().map(|item| item.item_id())
                    && let Some(pane) = this.pane_mut(this.active_pane)
                {
                    pane.keep(id);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &CloseOtherItems, window, cx| {
                let ids = this
                    .pane(this.active_pane)
                    .map(|p| p.others(p.active))
                    .unwrap_or_default();
                this.close_clean(ids, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseAllItems, window, cx| {
                let ids = this
                    .pane(this.active_pane)
                    .map(|p| p.all_unpinned())
                    .unwrap_or_default();
                this.close_clean(ids, window, cx);
            }))
            .on_action(cx.listener(
                |this, action: &studio_actions::workspace::ActivatePanel, _, cx| {
                    this.activate_panel(&action.id, cx)
                },
            ))
            .on_action(cx.listener(|this, _: &GoBack, window, cx| this.navigate(true, window, cx)))
            .on_action(
                cx.listener(|this, _: &GoForward, window, cx| this.navigate(false, window, cx)),
            )
            .on_drag_move(cx.listener(Self::resize_dock))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.left.open = !this.left.open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| {
                this.right.open = !this.right.open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleActivity, _, cx| {
                this.bottom.open = !this.bottom.open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                if let Some(session) = &this.session {
                    session.update(cx, |s, cx| s.reload(cx));
                }
            }));
        for registration in self.actions.clone() {
            root = registration(root, window, cx);
        }
        let modal = self.modal.as_ref().map(|modal| {
            deferred(
                div()
                    .absolute()
                    .size_full()
                    .top_0()
                    .left_0()
                    .flex()
                    .justify_center()
                    .pt(px(80.))
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.hide_modal(window, cx)),
                    )
                    .child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(modal.view.clone()),
                    ),
            )
            .with_priority(1)
        });
        // Columns, each with its own header and footer strip, as Delta lays out its window.
        // Glass: the sidebar, the document and the inspector are three sheets with one gap
        // around and between them. Flat: columns split by hairlines.
        let glass = studio_ui::ds::is_glass(cx);
        let sidebar_shown = self.left.visible() && self.session.is_some();
        let gap = px(studio_ui::ds::size::SHEET_GAP);
        // The traffic lights sit centered in the first header strip, which the glass style insets.
        let lights = if glass {
            point(gap + px(12.), gap + px(TRAFFIC_LIGHTS_TOP))
        } else {
            point(px(12.), px(TRAFFIC_LIGHTS_TOP))
        };
        if self.traffic_lights != Some(lights) {
            self.traffic_lights = Some(lights);
            window.set_traffic_light_position(lights);
        }
        let footer = self
            .pane(self.active_pane)
            .and_then(|pane| pane.active_item())
            .and_then(|item| item.footer(window, cx));
        root.child(
            h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .when(glass, |d| d.p(gap).gap(gap))
                .when(sidebar_shown, |d| d.child(self.render_sidebar(cx)))
                .child(self.render_center(footer, cx))
                .children(self.render_right_dock(cx)),
        )
        .child(self.render_toasts(cx))
        .children(modal)
    }
}

/// A dock edge being dragged.
#[derive(Clone, Copy)]
struct DraggedDock(PanelPosition);

impl Render for DraggedDock {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// The hairline between two panes of a split being dragged: the split's path in the tree and
/// the boundary after member `boundary`.
#[derive(Clone)]
struct DraggedSplit {
    path: Vec<usize>,
    boundary: usize,
    axis: SplitAxis,
}

impl Render for DraggedSplit {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// The strip on a pane's leading edge that drags the boundary before it.
fn split_handle(path: Vec<usize>, boundary: usize, axis: SplitAxis) -> impl IntoElement {
    let drag = DraggedSplit {
        path,
        boundary,
        axis,
    };
    let handle = div()
        .id(SharedString::from(format!(
            "split-handle-{}-{boundary}",
            drag.path
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join("-")
        )))
        .absolute()
        .occlude();
    let handle = match axis {
        SplitAxis::Horizontal => handle
            .top_0()
            .left(px(-3.))
            .h_full()
            .w(px(6.))
            .cursor_col_resize(),
        SplitAxis::Vertical => handle
            .left_0()
            .top(px(-3.))
            .w_full()
            .h(px(6.))
            .cursor_row_resize(),
    };
    handle.on_drag(drag, |drag, _, _, cx| {
        cx.stop_propagation();
        cx.new(|_| drag.clone())
    })
}

/// The shares after moving the boundary after member `boundary` to `fraction` of the split's
/// length, keeping every member at least a tenth of the pair being resized.
fn resized(flexes: &[f32], boundary: usize, fraction: f32) -> Option<Vec<f32>> {
    if boundary + 1 >= flexes.len() {
        return None;
    }
    let total: f32 = flexes.iter().sum();
    let before: f32 = flexes[..boundary].iter().sum();
    let pair = flexes[boundary] + flexes[boundary + 1];
    let min = pair * 0.1;
    let first = (fraction * total - before).clamp(min, pair - min);
    let mut next = flexes.to_vec();
    next[boundary] = first;
    next[boundary + 1] = pair - first;
    Some(next)
}

/// A tab's right-click menu, after Zed's: close variants, pin, then splits.
fn tab_menu(
    menu: ContextMenu,
    workspace: WeakEntity<Workspace>,
    item: EntityId,
    pinned: bool,
) -> ContextMenu {
    let on = |f: fn(&mut Workspace, PaneId, usize, &mut Window, &mut Context<Workspace>)| {
        let workspace = workspace.clone();
        move |window: &mut Window, cx: &mut App| {
            workspace
                .update(cx, |workspace, cx| {
                    if let Some((pane, index)) = workspace.find_item(item) {
                        f(workspace, pane, index, window, cx);
                    }
                })
                .ok();
        }
    };
    menu.entry(
        "Close",
        None,
        on(|w, pane, index, window, cx| w.close_in(pane, index, window, cx)),
    )
    .entry(
        "Close Others",
        None,
        on(|w, pane, index, window, cx| {
            let ids = w.pane(pane).map(|p| p.others(index)).unwrap_or_default();
            w.close_clean(ids, window, cx);
        }),
    )
    .entry(
        "Close to the Right",
        None,
        on(|w, pane, index, window, cx| {
            let ids = w
                .pane(pane)
                .map(|p| p.to_the_right(index))
                .unwrap_or_default();
            w.close_clean(ids, window, cx);
        }),
    )
    .entry(
        "Close All",
        None,
        on(|w, pane, _, window, cx| {
            let ids = w.pane(pane).map(|p| p.all_unpinned()).unwrap_or_default();
            w.close_clean(ids, window, cx);
        }),
    )
    .separator()
    .entry(
        if pinned { "Unpin Tab" } else { "Pin Tab" },
        None,
        on(|w, pane, index, _, cx| w.toggle_pin(pane, index, cx)),
    )
    .separator()
    .entry(
        "Split Right",
        None,
        on(|w, pane, index, window, cx| {
            w.activate_in(pane, index, true, window, cx);
            w.split(SplitDirection::Right, window, cx)
        }),
    )
    .entry(
        "Split Down",
        None,
        on(|w, pane, index, window, cx| {
            w.activate_in(pane, index, true, window, cx);
            w.split(SplitDirection::Down, window, cx)
        }),
    )
}

/// The strip along a dock's inner edge that resizes it.
fn resize_handle(position: PanelPosition) -> impl IntoElement {
    let handle = div()
        .id(match position {
            PanelPosition::Left => "resize-left-dock",
            PanelPosition::Right => "resize-right-dock",
            PanelPosition::Bottom => "resize-bottom-dock",
        })
        .absolute()
        .occlude();
    let handle = match position {
        PanelPosition::Left => handle
            .top_0()
            .right_0()
            .h_full()
            .w(px(5.))
            .cursor_col_resize(),
        PanelPosition::Right => handle
            .top_0()
            .left_0()
            .h_full()
            .w(px(5.))
            .cursor_col_resize(),
        PanelPosition::Bottom => handle
            .top_0()
            .left_0()
            .w_full()
            .h(px(5.))
            .cursor_row_resize(),
    };
    handle.on_drag(DraggedDock(position), |dock, _, _, cx| {
        cx.stop_propagation();
        cx.new(|_| *dock)
    })
}

/// Asks before closing a tab that has unsaved work or a write in flight.
struct ConfirmClose {
    workspace: WeakEntity<Workspace>,
    item_id: EntityId,
    title: SharedString,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for ConfirmClose {}

impl Focusable for ConfirmClose {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ConfirmClose {
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let item_id = self.item_id;
        cx.emit(DismissEvent);
        self.workspace
            .update(cx, |workspace, cx| {
                if let Some((pane, index)) = workspace.find_item(item_id) {
                    workspace.force_close(pane, index, window, cx);
                }
            })
            .ok();
    }
}

impl Render for ConfirmClose {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        studio_ui::ds::card(cx)
            .key_context("ConfirmClose")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(px(380.))
            .child(
                studio_ui::ds::card_body()
                    .gap(px(studio_ui::ds::space::SM))
                    .child(studio_ui::ds::text::title(format!("Close {}?", self.title)))
                    .child(studio_ui::ds::text::muted(
                        "It has unsaved changes or a write still running. A write in flight \
                         still finishes; the activity log keeps its outcome.",
                    )),
            )
            .child(
                studio_ui::ds::card_footer(cx)
                    .justify_end()
                    .gap(px(studio_ui::ds::space::SM))
                    .child(
                        studio_ui::ds::button("keep-open", "Keep open")
                            .style(ButtonStyle::Outlined)
                            .key_binding(ui::KeyBinding::for_action_in(&menu::Cancel, &focus, cx))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        studio_ui::ds::button("close-anyway", "Close")
                            .style(ButtonStyle::Filled)
                            .key_binding(ui::KeyBinding::for_action_in(&menu::Confirm, &focus, cx))
                            .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                    ),
            )
    }
}

/// The workspace that owns a window, if any.
pub fn workspace_for(window: &Window) -> Option<Entity<Workspace>> {
    window.root::<Workspace>().flatten()
}
