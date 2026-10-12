//! The window frame (spec 002): a title bar with server, graph and branch; a sidebar of
//! sections; a center pane of tabs; an inspector on the right; an activity log at the bottom;
//! a status bar; a modal layer and toasts.
//!
//! Layer 4 (`docs/architecture.md`). A small version of Zed's `workspace` crate, written for
//! Studio rather than forked: Studio has no projects, collaboration or Zed database. Feature
//! crates attach to it with `cx.observe_new::<Workspace>` and `Workspace::register_action`.

mod item;
mod panels;
mod persistence;

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Action, AnyView, App, Context, DismissEvent, Div, DragMoveEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, MouseButton, Pixels, Point, Render, SharedString,
    Stateful, Subscription, Task, WeakEntity, Window, WindowControlArea, deferred, div, point,
};
use graph_store::{ConnectionState, GraphSession, OutcomeKind, SessionEvent};
use studio_actions::graph::{OpenCommit, OpenHistory};
use studio_actions::workspace::{
    ActivateItem, ActivateLastItem, ActivateNextItem, ActivatePreviousItem, CloseActiveItem,
    GoBack, GoForward, Refresh, ToggleActivity, ToggleInspector, ToggleSidebar,
};
use studio_settings::StudioSettings;
use ui::prelude::*;
use ui::{CommonAnimationExt as _, ContextMenu, Indicator, PopoverMenu, Tooltip};

pub use item::{Item, ItemEvent, ItemFooter, ItemHandle};
pub use panels::{Dock, PanelEntry, PanelPosition};
pub use persistence::{SavedItem, SavedWorkspace, register_item_kind, saved_window_bounds};

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

pub struct Workspace {
    session: Option<Entity<GraphSession>>,
    items: Vec<Box<dyn ItemHandle>>,
    active: usize,
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
            items: Vec::new(),
            active: 0,
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
        let mut items = Vec::new();
        let mut active = 0;
        for (index, item) in self.items.iter().enumerate() {
            if let Some(saved) = item.serialize(cx) {
                if index == self.active {
                    active = items.len();
                }
                items.push(saved);
            }
        }
        SavedWorkspace {
            items,
            active,
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

    /// Writes the workspace's state shortly after it stops changing.
    fn save_if_changed(&mut self, cx: &mut Context<Self>) {
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
        for item in &saved.items {
            if let Some(open) = persistence::opener(cx, &item.kind) {
                open(&item.data, self, window, cx);
            }
        }
        if !self.items.is_empty() {
            self.activate_item(saved.active.min(self.items.len() - 1), window, cx);
        }
        self.saved = Some(saved);
    }

    // ---- session ----

    pub fn session(&self) -> Option<&Entity<GraphSession>> {
        self.session.as_ref()
    }

    /// Opens a graph session in this window. Tabs from another graph are closed.
    pub fn set_session(
        &mut self,
        session: Entity<GraphSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same_graph = self.session.as_ref().is_some_and(|current| {
            let current = current.read(cx);
            let next = session.read(cx);
            current.graph_id == next.graph_id && current.connection == next.connection
        });
        if !same_graph {
            self.items.clear();
            self._item_subscriptions.clear();
            self.active = 0;
            self.right.remove(INSPECTOR_PANEL);
        }
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
        if !same_graph && persistence::restore_enabled() {
            self.saved = None;
            self.restore(window, cx);
        }
        // With nothing focused, GPUI dispatches actions to the window's root view, which is
        // outside the element that carries the workspace's action handlers.
        if window.focused(cx).is_none() {
            window.focus(&self.focus_handle, cx);
        }
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
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

    // ---- items ----

    /// Adds an item as a tab, or activates the tab with the same dedupe key.
    pub fn add_item(
        &mut self,
        item: Box<dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = item.dedupe_key(cx)
            && let Some(index) = self
                .items
                .iter()
                .position(|existing| existing.dedupe_key(cx).as_ref() == Some(&key))
        {
            self.activate_item(index, window, cx);
            return;
        }
        self.items.push(item);
        let index = self.items.len() - 1;
        self.activate_item(index, window, cx);
    }

    /// Opens an item of type `T`, unless a tab with `key` is open, which is activated instead.
    pub fn open_item<T: Item>(
        &mut self,
        key: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Window, &mut Context<T>) -> T,
    ) -> Option<Entity<T>> {
        let key = key.into();
        if let Some(index) = self
            .items
            .iter()
            .position(|existing| existing.dedupe_key(cx).as_ref() == Some(&key))
        {
            self.activate_item(index, window, cx);
            return self.items[index].to_any().downcast::<T>().ok();
        }
        let item = cx.new(|cx| build(window, cx));
        let subscription = cx.subscribe_in(
            &item,
            window,
            |this, item, event: &ItemEvent, window, cx| match event {
                ItemEvent::UpdateTab => cx.notify(),
                // The item asked to close; it has already decided.
                ItemEvent::Close => {
                    if let Some(index) = this
                        .items
                        .iter()
                        .position(|i| i.item_id() == item.entity_id())
                    {
                        this.force_close_item(index, window, cx);
                    }
                }
            },
        );
        self._item_subscriptions
            .push((item.entity_id(), subscription));
        self.items.push(Box::new(item.clone()));
        let index = self.items.len() - 1;
        self.activate_item(index, window, cx);
        Some(item)
    }

    pub fn items(&self) -> &[Box<dyn ItemHandle>] {
        &self.items
    }

    pub fn active_item(&self) -> Option<&dyn ItemHandle> {
        self.items.get(self.active).map(|item| item.as_ref())
    }

    pub fn activate_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.activate(index, true, window, cx);
    }

    /// Activates a tab; `record` remembers the tab left for ⌘[ and forgets the ⌘] trail.
    fn activate(
        &mut self,
        index: usize,
        record: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.items.len() {
            return;
        }
        if record
            && index != self.active
            && let Some(current) = self.items.get(self.active)
        {
            let id = current.item_id();
            self.back.retain(|visited| *visited != id);
            self.back.push(id);
            if self.back.len() > NAV_HISTORY {
                self.back.remove(0);
            }
            self.forward.clear();
        }
        self.active = index;
        let focus = self.items[index].focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// ⌘[ and ⌘]: the previous or next tab in the order they were visited, skipping closed
    /// ones.
    fn navigate(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        loop {
            let next = if back {
                self.back.pop()
            } else {
                self.forward.pop()
            };
            let Some(id) = next else { return };
            let Some(index) = self.items.iter().position(|item| item.item_id() == id) else {
                continue;
            };
            if index == self.active {
                continue;
            }
            if let Some(current) = self.items.get(self.active) {
                let current = current.item_id();
                if back {
                    self.forward.push(current);
                } else {
                    self.back.push(current);
                }
            }
            self.activate(index, false, window, cx);
            return;
        }
    }

    /// Closes a tab; one with unsaved work or a write in flight asks first.
    pub fn close_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(index) else {
            return;
        };
        if !item.is_dirty(cx) {
            self.force_close_item(index, window, cx);
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

    /// Closes a tab without asking.
    pub fn force_close_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.items.len() {
            return;
        }
        let removed = self.items.remove(index);
        self._item_subscriptions
            .retain(|(id, _)| *id != removed.item_id());
        if self.active >= self.items.len() {
            self.active = self.items.len().saturating_sub(1);
        }
        if let Some(item) = self.items.get(self.active) {
            let focus = item.focus_handle(cx);
            window.focus(&focus, cx);
        } else {
            window.focus(&self.focus_handle, cx);
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
            if let Some(focus) = modal.previous_focus {
                window.focus(&focus, cx);
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
    fn strip(&self, id: &'static str, cx: &mut Context<Self>) -> Stateful<Div> {
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
        self.strip("sidebar-footer", cx)
            .px(px(studio_ui::ds::space::XS))
            .border_t_1()
            .border_color(hairline)
            .child(
                h_flex()
                    .id("graph-switcher")
                    .w_full()
                    .h(px(24.))
                    .px(px(studio_ui::ds::space::SM))
                    .gap(px(studio_ui::ds::space::SM))
                    .items_center()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .tooltip(Tooltip::text(format!("{url} · {state}")))
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
                    .child(
                        Icon::new(IconName::ChevronUpDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(studio_actions::workspace::OpenGraph), cx)
                    }),
            )
    }

    /// The tabs, as Delta draws them: text tabs separated by hairlines, the active one joined
    /// to the content below it.
    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        h_flex()
            .h_full()
            .min_w_0()
            .overflow_hidden()
            .children(self.items.iter().enumerate().map(|(index, item)| {
                let selected = index == self.active;
                let group: SharedString = format!("tab-{index}").into();
                let dirty = item.is_dirty(cx);
                let stale = item.is_stale(cx);
                h_flex()
                    .id(("tab", index))
                    .group(group.clone())
                    .h_full()
                    .pl_4()
                    .pr_1p5()
                    .gap_2()
                    .items_center()
                    .flex_none()
                    .relative()
                    .border_r_1()
                    .border_color(hairline)
                    .when(selected, |tab| tab.bg(editor_bg))
                    .when(!selected, |tab| {
                        tab.bg(inactive_bg)
                            .hover(|style| style.bg(colors.ghost_element_hover))
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
                            .color(if selected {
                                Color::Default
                            } else {
                                Color::Muted
                            })
                            .single_line(),
                    )
                    .when(dirty, |tab| {
                        tab.child(Indicator::dot().color(Color::Warning))
                    })
                    .when(stale && !dirty, |tab| {
                        tab.child(Indicator::dot().color(Color::Muted))
                    })
                    .child(
                        IconButton::new(("close", index), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .when(!selected, |button| button.visible_on_hover(group))
                            .tooltip(|_, cx| Tooltip::for_action("Close", &CloseActiveItem, cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_item(index, window, cx)
                            })),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.activate_item(index, window, cx)
                        }),
                    )
            }))
    }

    /// The document column's header: the tabs, then the toggles of the right and bottom docks.
    fn render_center_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let header_bg = if studio_ui::ds::is_glass(cx) {
            gpui::transparent_black()
        } else {
            colors.editor_background.blend(gpui::black().opacity(0.02))
        };
        let hairline = studio_ui::ds::surface::rule(cx);
        let mut context = h_flex().gap_1().items_center().pr_1();
        context = context.child(
            IconButton::new("toggle-activity", IconName::ListTree)
                .icon_size(IconSize::Small)
                .toggle_state(self.bottom.open)
                .tooltip(|_, cx| Tooltip::for_action("Activity", &ToggleActivity, cx))
                .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleActivity), cx)),
        );
        if !self.right.panels.is_empty() {
            context = context.child(
                IconButton::new("toggle-inspector", IconName::ThreadsSidebarRightOpen)
                    .icon_size(IconSize::Small)
                    .toggle_state(self.right.open)
                    .tooltip(|_, cx| Tooltip::for_action("Inspector", &ToggleInspector, cx))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleInspector), cx)
                    }),
            );
        }
        self.strip("center-header", cx)
            .bg(header_bg)
            .when(!self.left.visible() || self.session.is_none(), |strip| {
                strip.child(
                    div()
                        .w(px(TRAFFIC_LIGHTS_WIDTH))
                        .h_full()
                        .flex_none()
                        .border_b_1()
                        .border_color(hairline),
                )
            })
            .child(self.render_tabs(cx))
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
                        Button::new("branch", branch.clone())
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
        let body = match self.items.get(self.active) {
            Some(item) => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(item.to_any())
                .into_any_element(),
            None => self.render_empty_center(cx).into_any_element(),
        };
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
            .child(self.render_center_header(cx))
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
                                    IconButton::new("close-bottom-dock", IconName::Close)
                                        .icon_size(IconSize::XSmall)
                                        .tooltip(|_, cx| {
                                            Tooltip::for_action("Hide", &ToggleActivity, cx)
                                        })
                                        .on_click(|_, window, cx| {
                                            window.dispatch_action(Box::new(ToggleActivity), cx)
                                        }),
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
            Button::new(id, label)
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
                            IconButton::new("close-right-dock", IconName::Close)
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
                        Button::new(("toast-action", id), label.clone())
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.dismiss_toast(id, cx);
                                window.dispatch_action(action.boxed_clone(), cx);
                            }))
                    }))
                    .child(
                        IconButton::new(("dismiss-toast", id), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.dismiss_toast(id, cx)),
                            ),
                    )
            }))
    }
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
                let index = this.active;
                this.close_item(index, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateNextItem, window, cx| {
                if !this.items.is_empty() {
                    let next = (this.active + 1) % this.items.len();
                    this.activate_item(next, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ActivatePreviousItem, window, cx| {
                if !this.items.is_empty() {
                    let previous = (this.active + this.items.len() - 1) % this.items.len();
                    this.activate_item(previous, window, cx);
                }
            }))
            .on_action(cx.listener(|this, action: &ActivateItem, window, cx| {
                this.activate_item(action.index, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateLastItem, window, cx| {
                if let Some(last) = this.items.len().checked_sub(1) {
                    this.activate_item(last, window, cx);
                }
            }))
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
            .items
            .get(self.active)
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
                if let Some(index) = workspace.items.iter().position(|i| i.item_id() == item_id) {
                    workspace.force_close_item(index, window, cx);
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
                        Button::new("keep-open", "Keep open")
                            .style(ButtonStyle::Outlined)
                            .key_binding(ui::KeyBinding::for_action_in(&menu::Cancel, &focus, cx))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("close-anyway", "Close")
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
