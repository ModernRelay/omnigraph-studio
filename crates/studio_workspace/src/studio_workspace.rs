//! The window frame (spec 002): a title bar with server, graph and branch; a sidebar of
//! sections; a center pane of tabs; an inspector on the right; an activity log at the bottom;
//! a status bar; a modal layer and toasts.
//!
//! Layer 4 (`docs/architecture.md`). A small version of Zed's `workspace` crate, written for
//! Studio rather than forked: Studio has no projects, collaboration or Zed database. Feature
//! crates attach to it with `cx.observe_new::<Workspace>` and `Workspace::register_action`.

mod item;
mod persistence;

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Action, AnyView, App, Context, DismissEvent, Div, Entity, EventEmitter, FocusHandle, Focusable,
    MouseButton, Render, SharedString, Stateful, Subscription, Task, Window, WindowControlArea,
    deferred, div,
};
use graph_store::{ConnectionState, GraphSession, OutcomeKind, SessionEvent};
use studio_actions::workspace::{
    ActivateItem, ActivateLastItem, ActivateNextItem, ActivatePreviousItem, CloseActiveItem,
    Refresh, ToggleActivity, ToggleInspector, ToggleSidebar,
};
use ui::prelude::*;
use ui::{ContextMenu, Indicator, PopoverMenu, Tooltip};

pub use item::{Item, ItemEvent, ItemHandle};
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
    /// Time left on screen; `None` stays until dismissed (errors, spec 002).
    remaining: Option<Duration>,
    hovered: bool,
}

/// How long a success or info toast stays (spec 002); hovering pauses it.
const TOAST_DURATION: Duration = Duration::from_secs(10);
const TOAST_TICK: Duration = Duration::from_millis(250);

struct SidebarSection {
    title: SharedString,
    view: AnyView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceEvent {
    SessionChanged,
}

type ActionRegistration = Rc<dyn Fn(Div, &mut Window, &mut Context<Workspace>) -> Div>;

pub struct Workspace {
    session: Option<Entity<GraphSession>>,
    items: Vec<Box<dyn ItemHandle>>,
    active: usize,
    sidebar_sections: Vec<SidebarSection>,
    inspector: Option<AnyView>,
    activity: Option<AnyView>,
    sidebar_open: bool,
    inspector_open: bool,
    activity_open: bool,
    modal: Option<ActiveModal>,
    toasts: Vec<Toast>,
    next_toast: usize,
    actions: Vec<ActionRegistration>,
    focus_handle: FocusHandle,
    /// A press on a header strip that becomes a window move if the pointer moves.
    drag_pending: bool,
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
const STRIP_HEIGHT: f32 = studio_ui::chrome::STRIP_HEIGHT;
/// Room for the window's traffic lights at the left of the first header strip.
const TRAFFIC_LIGHTS_WIDTH: f32 = 76.;
const SIDEBAR_WIDTH: f32 = 248.;
const INSPECTOR_WIDTH: f32 = 320.;

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            sidebar_sections: Vec::new(),
            inspector: None,
            activity: None,
            sidebar_open: true,
            inspector_open: true,
            activity_open: false,
            modal: None,
            toasts: Vec::new(),
            next_toast: 0,
            actions: Vec::new(),
            focus_handle: cx.focus_handle(),
            drag_pending: false,
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
            sidebar_open: self.sidebar_open,
            inspector_open: self.inspector_open,
            activity_open: self.activity_open,
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
        self.sidebar_open = saved.sidebar_open;
        self.inspector_open = saved.inspector_open;
        self.activity_open = saved.activity_open;
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
            self.inspector = None;
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
        let kind = match latest.outcome {
            // No publish, no toast (constitution VII); the surface that wrote explains it.
            OutcomeKind::Committed { commit: None } => return,
            OutcomeKind::Committed { .. } => ToastKind::Success,
            _ => ToastKind::Error,
        };
        let text = toast_text(latest);
        self.show_toast(kind, text, cx);
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
                ItemEvent::Close => {
                    if let Some(index) = this
                        .items
                        .iter()
                        .position(|i| i.item_id() == item.entity_id())
                    {
                        this.close_item(index, window, cx);
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
        if index < self.items.len() {
            self.active = index;
            let focus = self.items[index].focus_handle(cx);
            window.focus(&focus, cx);
            cx.notify();
        }
    }

    pub fn close_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
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

    pub fn add_sidebar_section(
        &mut self,
        title: impl Into<SharedString>,
        view: AnyView,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_sections.push(SidebarSection {
            title: title.into(),
            view,
        });
        cx.notify();
    }

    pub fn set_inspector(&mut self, view: Option<AnyView>, cx: &mut Context<Self>) {
        self.inspector = view;
        if self.inspector.is_some() {
            self.inspector_open = true;
        }
        cx.notify();
    }

    pub fn set_activity_view(&mut self, view: AnyView, cx: &mut Context<Self>) {
        self.activity = Some(view);
        cx.notify();
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
            && active.view.clone().downcast::<V>().is_ok() {
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
        let id = self.next_toast;
        self.next_toast += 1;
        let remaining = match kind {
            ToastKind::Error => None,
            ToastKind::Info | ToastKind::Success => Some(TOAST_DURATION),
        };
        self.toasts.push(Toast {
            id,
            kind,
            message: message.into(),
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
                            let left = toast.remaining.unwrap_or_default().saturating_sub(TOAST_TICK);
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
        let hairline = studio_ui::chrome::hairline(cx);
        let header = self
            .strip("sidebar-header", cx)
            .pr_1()
            .gap_0p5()
            .border_b_1()
            .border_color(hairline)
            .child(div().w(px(TRAFFIC_LIGHTS_WIDTH)).flex_none())
            .child(
                IconButton::new("toggle-sidebar", IconName::ThreadsSidebarLeftOpen)
                    .icon_size(IconSize::Small)
                    .tooltip(|_, cx| Tooltip::for_action("Hide sidebar", &ToggleSidebar, cx))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleSidebar), cx)),
            )
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
        let sections = v_flex()
            .id("sidebar-sections")
            .flex_1()
            .min_h_0()
            .pb_2()
            .overflow_y_scroll()
            .children(self.sidebar_sections.iter().map(|section| {
                v_flex()
                    .w_full()
                    .child(studio_ui::chrome::section_label(section.title.clone()))
                    .child(section.view.clone())
            }));
        v_flex()
            .id("sidebar")
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .bg(panel)
            .border_r_1()
            .border_color(hairline)
            .child(header)
            .child(sections)
            .child(self.render_sidebar_footer(cx))
    }

    /// Where this window is connected, as Delta's sidebar footer shows the account: a status
    /// dot, the graph and the server. Clicking opens "Open a graph".
    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hairline = studio_ui::chrome::hairline(cx);
        let (color, graph, server, state) = match &self.session {
            Some(session) => {
                let s = session.read(cx);
                let connection = s.connection.read(cx);
                let color = match &connection.state {
                    ConnectionState::Ready => Color::Success,
                    ConnectionState::Connecting | ConnectionState::Idle => Color::Muted,
                    ConnectionState::ContractMismatch { .. } => Color::Warning,
                    _ => Color::Error,
                };
                (
                    color,
                    s.graph_id.clone(),
                    connection.server.name.clone(),
                    connection.state.label(),
                )
            }
            None => (
                Color::Muted,
                "No graph".to_owned(),
                String::new(),
                "Not connected".to_owned(),
            ),
        };
        self.strip("sidebar-footer", cx)
            .px_1()
            .border_t_1()
            .border_color(hairline)
            .child(
                h_flex()
                    .id("graph-switcher")
                    .w_full()
                    .h(px(24.))
                    .px_1p5()
                    .gap_1p5()
                    .items_center()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .tooltip(Tooltip::text(format!("{server} · {state}")))
                    .child(Indicator::dot().color(color))
                    .child(
                        Label::new(graph)
                            .size(LabelSize::Small)
                            .single_line()
                            .truncate(),
                    )
                    .child(
                        Label::new(server)
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
        let (editor_bg, inactive_bg) = (
            colors.editor_background,
            colors.tab_bar_background.opacity(0.35),
        );
        let hairline = studio_ui::chrome::hairline(cx);
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
                    .pl_3()
                    .pr_1()
                    .gap_1p5()
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
                    .children(item.tab_icon(cx).map(|icon| {
                        Icon::new(icon).size(IconSize::XSmall).color(Color::Muted)
                    }))
                    .child(
                        Label::new(item.tab_title(cx))
                            .size(LabelSize::Small)
                            .color(if selected { Color::Default } else { Color::Muted })
                            .single_line(),
                    )
                    .when(dirty, |tab| tab.child(Indicator::dot().color(Color::Warning)))
                    .when(stale && !dirty, |tab| tab.child(Indicator::dot().color(Color::Muted)))
                    .child(
                        IconButton::new(("close", index), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .when(!selected, |button| button.visible_on_hover(group))
                            .tooltip(|_, cx| Tooltip::for_action("Close", &CloseActiveItem, cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_item(index, window, cx)
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_item(index, window, cx)
                    }))
            }))
    }

    /// The document column's header: tabs, then the branch the active tab reads and writes.
    fn render_center_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let header_bg = colors.editor_background.blend(gpui::black().opacity(0.02));
        let hairline = studio_ui::chrome::hairline(cx);
        let mut context = h_flex().gap_1().items_center().pr_1();
        if let Some(session) = &self.session {
            let s = session.read(cx);
            let branch = s.branch.clone();
            let branches = s.branches.loaded().cloned().unwrap_or_default();
            let session_for_menu = session.clone();
            context = context
                .child(
                    PopoverMenu::new("branch-menu")
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
                                    Icon::new(IconName::ChevronDown)
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
                )
                ;
            let running = match (s.reads_in_flight, s.writes_in_flight) {
                (0, 0) => None,
                (reads, 0) => Some(format!(
                    "{reads} quer{} running",
                    if reads == 1 { "y" } else { "ies" }
                )),
                (_, writes) => Some(format!("Writing ({writes})…")),
            };
            context = context.children(
                running.map(|text| Label::new(text).size(LabelSize::XSmall).color(Color::Muted)),
            );
        }
        context = context.child(
            IconButton::new("toggle-activity", IconName::ListTree)
                .icon_size(IconSize::Small)
                .toggle_state(self.activity_open)
                .tooltip(|_, cx| Tooltip::for_action("Activity", &ToggleActivity, cx))
                .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleActivity), cx)),
        );
        if self.inspector.is_some() {
            context = context.child(
                IconButton::new("toggle-inspector", IconName::ThreadsSidebarRightOpen)
                    .icon_size(IconSize::Small)
                    .toggle_state(self.inspector_open)
                    .tooltip(|_, cx| Tooltip::for_action("Inspector", &ToggleInspector, cx))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleInspector), cx)
                    }),
            );
        }
        self.strip("center-header", cx)
            .bg(header_bg)
            .when(!self.sidebar_open || self.session.is_none(), |strip| {
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

    fn render_center(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (editor_background, panel_background) =
            (colors.editor_background, colors.panel_background);
        let hairline = studio_ui::chrome::hairline(cx);
        let body = match self.items.get(self.active) {
            Some(item) => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(item.to_any())
                .into_any_element(),
            None => self.render_empty_center(cx).into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(editor_background)
            .child(self.render_center_header(cx))
            .child(body)
            .when(self.activity_open, |d| {
                d.children(self.activity.clone().map(|activity| {
                    div()
                        .h(px(200.))
                        .flex_none()
                        .overflow_hidden()
                        .bg(panel_background)
                        .border_t_1()
                        .border_color(hairline)
                        .child(activity)
                }))
            })
    }

    fn render_empty_center(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let message = if self.session.is_some() {
            "Pick a type in the sidebar, or press ⌘K."
        } else {
            "Connect to a server to open a graph."
        };
        v_flex()
            .flex_1()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(Label::new(message).color(Color::Muted))
            .when(self.session.is_none(), |d| {
                d.child(
                    Button::new("open-graph", "Open a graph…")
                        .style(ButtonStyle::Filled)
                        .key_binding(ui::KeyBinding::for_action(
                            &studio_actions::workspace::OpenGraph,
                            cx,
                        ))
                        .on_click(|_, window, cx| {
                            window
                                .dispatch_action(Box::new(studio_actions::workspace::OpenGraph), cx)
                        }),
                )
            })
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let inspector = self.inspector.clone()?;
        if !self.inspector_open {
            return None;
        }
        let colors = cx.theme().colors();
        let editor_background = colors.editor_background;
        let hairline = studio_ui::chrome::hairline(cx);
        Some(
            v_flex()
                .w(px(INSPECTOR_WIDTH))
                .h_full()
                .flex_none()
                .bg(editor_background)
                .border_l_1()
                .border_color(hairline)
                .child(
                    self.strip("inspector-header", cx)
                        .px_3()
                        .border_b_1()
                        .border_color(hairline)
                        .child(Label::new("Inspector").size(LabelSize::Small))
                        .child(div().flex_1())
                        .child(
                            IconButton::new("close-inspector", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .tooltip(|_, cx| {
                                    Tooltip::for_action("Hide inspector", &ToggleInspector, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ToggleInspector), cx)
                                }),
                        ),
                )
                .child(
                    div()
                        .id("inspector")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(inspector),
                ),
        )
    }

    fn render_toasts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .absolute()
            .bottom(px(12.))
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
                    .rounded_md()
                    .bg(colors.elevated_surface_background)
                    .border_1()
                    .border_color(colors.border)
                    .shadow_md()
                    .child(Indicator::dot().color(color))
                    // `min_w_0` lets the text wrap inside the toast instead of widening it.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(toast.message.clone()).size(LabelSize::Small)),
                    )
                    .child(
                        IconButton::new(("dismiss-toast", id), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .on_click(cx.listener(move |this, _, _, cx| this.dismiss_toast(id, cx))),
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
            .bg(colors.background)
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
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| {
                this.inspector_open = !this.inspector_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleActivity, _, cx| {
                this.activity_open = !this.activity_open;
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
        root.child(
            h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .when(self.sidebar_open && self.session.is_some(), |d| {
                    d.child(self.render_sidebar(cx))
                })
                .child(self.render_center(cx))
                .children(self.render_inspector(cx)),
        )
        .child(self.render_toasts(cx))
        .children(modal)
    }
}

/// The workspace that owns a window, if any.
pub fn workspace_for(window: &Window) -> Option<Entity<Workspace>> {
    window.root::<Workspace>().flatten()
}
