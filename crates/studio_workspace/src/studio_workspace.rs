//! The window frame (spec 002): columns with header strips (a sidebar dock on the left, panes
//! of tabs in the center over a bottom dock, an inspector dock on the right) over one status
//! bar that holds every panel's button, the server, graph and branch, and the tab's facts, as
//! Zed lays out its docks and status bar; a modal layer and toasts.
//!
//! Layer 4 (`docs/architecture.md`). A small version of Zed's `workspace` crate, written for
//! Studio rather than forked: Studio has no projects, collaboration or Zed database. Feature
//! crates attach to it with `cx.observe_new::<Workspace>` and `Workspace::register_action`.

mod item;
mod pane;
mod panels;
mod persistence;
mod process_stats;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Action, AnyView, App, Bounds, ClickEvent, Context, DismissEvent, Div, DragMoveEvent, ElementId,
    Entity, EntityId, EventEmitter, FocusHandle, Focusable, MouseButton, Pixels, Point, Render,
    ScrollHandle, SharedString, Stateful, Subscription, Task, WeakEntity, Window,
    WindowControlArea, WindowHandle, deferred, div, point,
};
use graph_store::{ConnectionRegistry, ConnectionState, GraphSession, OutcomeKind, SessionEvent};
use studio_actions::graph::{OpenCommit, OpenHistory};
use studio_actions::workspace::{
    ActivateItem, ActivateLastItem, ActivateNextGraph, ActivateNextItem, ActivateNextPane,
    ActivatePreviousGraph, ActivatePreviousItem, ActivatePreviousPane, CloseActiveItem,
    CloseAllItems, CloseGraph, CloseOtherItems, GoBack, GoForward, KeepPreviewTab, Refresh,
    SplitDown, SplitLeft, SplitRight, SplitUp, ToggleActivity, ToggleInspector, TogglePinTab,
    ToggleSidebar, ToggleZoom,
};
use studio_settings::{BottomDockLayout, StudioSettings};
use studio_ui::ds::IconButton;
use studio_ui::menu::{ContextMenu, ContextMenuEntry, ContextMenuItem};
use ui::prelude::*;
use ui::{ButtonLike, Indicator, PopoverMenu, Tooltip, right_click_menu};

use item::ItemHandle;
pub use item::{Item, ItemEvent, ItemFooter, KeyHint};
use pane::{Member, Pane, PaneId, SplitAxis, SplitDirection, Tab};
pub use panels::PanelPosition;
use panels::{Dock, PanelEntry};
pub use persistence::{
    SavedGraph, SavedItem, SavedPane, SavedSession, SavedWindow, SavedWorkspace,
    keep_saved_windows, load_windows, register_item_kind, saved_window_bounds,
};

/// A modal: dismissing it emits [`DismissEvent`]. Following Zed's modal layer, the workspace
/// asks [`ModalView::on_before_dismiss`] before every close.
pub trait ModalView: Render + Focusable + EventEmitter<DismissEvent> {
    /// Whether the modal may close now: asked on ⎋, a click outside, the same command again,
    /// and the modal's own dismissal. A dialog with a write running says no, since closing it
    /// would lose the outcome it shows.
    fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }

    /// Where the modal sits; a dialog unless it says otherwise.
    fn placement(&self) -> ModalPlacement {
        ModalPlacement::Dialog
    }
}

/// Where a modal sits (spec 002, "Modals"), after Tower's: a dialog over a dim, a quarter of
/// the way down the space it leaves free; the command palette near the top over a light veil.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModalPlacement {
    #[default]
    Dialog,
    Palette,
}

/// Asks the open modal [`ModalView::on_before_dismiss`].
type BeforeDismiss = Box<dyn Fn(&mut Window, &mut App) -> bool>;

struct ActiveModal {
    view: AnyView,
    placement: ModalPlacement,
    previous_focus: Option<FocusHandle>,
    before_dismiss: BeforeDismiss,
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
/// Toasts shown at once.
const TOASTS_SHOWN: usize = 4;

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
    saved: Option<SavedWorkspace>,
    /// False for a graph reopened behind the shown one: `saved` is its saved state, whose tabs
    /// open when it is first shown, so a start does not read every table of every graph a
    /// window had open.
    restored: bool,
}

/// What fills the document area while zoomed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zoomed {
    Pane(PaneId),
    Dock(PanelPosition),
}

pub struct Workspace {
    session: Option<Entity<GraphSession>>,
    /// The other graphs open in this window, and every open graph's session in the order they
    /// were opened (the switcher's order).
    parked: Vec<ParkedGraph>,
    graph_order: Vec<EntityId>,
    /// The open graphs as last written for the next start, and this window's place among the
    /// windows reopened then.
    saved_session: Option<SavedSession>,
    window_slot: u64,
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
    /// A pane or a dock drawn over the whole document area, as Zed zooms one (⇧⎋).
    zoomed: Option<Zoomed>,
    /// Each pane's tab strip scroll, and the tab it was last scrolled to show.
    tab_scrolls: RefCell<HashMap<PaneId, ScrollHandle>>,
    tabs_scrolled_to: RefCell<HashMap<PaneId, EntityId>>,
    /// Where a dragged tab would land: a pane, split toward an edge or not.
    drop_target: Option<(PaneId, Option<SplitDirection>)>,
    /// A press landed on a tab: its strip does not start a window move.
    press_on_tab: bool,
    modal: Option<ActiveModal>,
    /// Each dock panel's scrolling host, by the panel view's entity: the host is cached, so a
    /// panel is drawn again only when it notifies.
    dock_hosts: HashMap<EntityId, Entity<DockHost>>,
    toasts: Vec<Toast>,
    next_toast: usize,
    actions: Vec<ActionRegistration>,
    focus_handle: FocusHandle,
    /// A press on a header strip that becomes a window move if the pointer moves.
    drag_pending: bool,
    /// Where the traffic lights were last placed; they move with the window style.
    traffic_lights: Option<Point<Pixels>>,
    /// Whether the window shows traffic lights the top-left strips make room for, read each
    /// frame: on macOS, and not in full screen, where they hide with the menu bar.
    lights_shown: bool,
    /// The settings file problem last shown, so a toast appears once per problem.
    settings_error: Option<String>,
    /// What the CLI's files could not give, and the servers not added at start, as last shown.
    cli_errors: Vec<String>,
    _settings_subscription: Subscription,
    /// Studio's memory and CPU for the footer while `studio.show_performance` is on (the
    /// default), read every two seconds while the window is in front.
    performance: Option<Performance>,
    /// Whether this window is in front, which the memory and CPU reading waits for.
    window_active: bool,
    _performance: Option<Task<()>>,
    _performance_subscription: Subscription,
    /// What was last written for this graph, so unchanged state is not rewritten.
    saved: Option<SavedWorkspace>,
    _save: Option<Task<()>>,
    /// The next comparison of the state with what was last written.
    _save_check: Option<Task<()>>,
    _save_bounds: Option<Task<()>>,
    /// The window's place and size the debounced save above has still to write.
    pending_bounds: Option<Bounds<Pixels>>,
    _bounds_subscription: Subscription,
    _quit_subscription: Subscription,
    _activation_subscription: Subscription,
    _registry_subscription: Option<Subscription>,
    /// Each open graph's session, shown or parked: a write's outcome is toasted whichever
    /// graph is shown when it lands.
    _graph_subscriptions: HashMap<EntityId, Subscription>,
    /// How many of each open graph's outcomes ([`GraphSession::outcomes_logged`]) have been
    /// toasted or passed over.
    outcomes_toasted: HashMap<EntityId, u64>,
    _item_subscriptions: Vec<(gpui::EntityId, Subscription)>,
}

/// How often, at most, the workspace's state is compared with what was last written.
const SAVE_CHECK: Duration = Duration::from_millis(250);
/// How long state must be still before it is written.
const SAVE_DELAY: Duration = Duration::from_millis(500);
/// Height of a column's header strip (spec 002, after Delta), and of the status bar under the
/// columns (after Zed's).
const HEADER_HEIGHT: f32 = studio_ui::ds::size::HEADER_STRIP;
const STATUS_BAR_HEIGHT: f32 = studio_ui::ds::size::STATUS_BAR;
/// A tab's slots before and after its content, as Zed's `Tab`: its state dot, its close button.
const TAB_START_SLOT: f32 = 12.;
const TAB_END_SLOT: f32 = 14.;
/// How far a zoomed pane or panel sits inside the document area, as Zed's `zoomed_padding`.
const ZOOM_INSET: f32 = studio_ui::ds::size::SHEET_GAP;
/// Room for the window's traffic lights at the left of the first header strip.
const TRAFFIC_LIGHTS_WIDTH: f32 = 76.;
/// The traffic lights' offset from the top of that strip: centers the 16-point buttons in it.
const TRAFFIC_LIGHTS_TOP: f32 = (HEADER_HEIGHT - 16.) / 2.;

/// Where the traffic lights sit: centered in the first header strip, which the glass style
/// insets. A window opens with them there, so they do not move on its first draw.
pub fn traffic_light_position(glass: bool) -> Point<Pixels> {
    let gap = if glass {
        px(studio_ui::ds::size::SHEET_GAP)
    } else {
        px(0.)
    };
    point(gap + px(12.), gap + px(TRAFFIC_LIGHTS_TOP))
}
/// A dock panel inside a scrolling host of its own. The host is what is cached (a cached view
/// needs a definite size, which a panel measured by its content lacks), as Zed caches each dock
/// and pane: a panel that notifies marks its host dirty and is drawn again; otherwise the frame
/// replays it.
struct DockHost {
    view: AnyView,
    /// The panel scrolls itself (a list that draws only its rows in view), so the host gives
    /// it its size rather than scrolling it.
    scrolls_itself: bool,
}

impl Render for DockHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        if self.scrolls_itself {
            return div().id("dock-host").size_full().child(self.view.clone());
        }
        div()
            .id("dock-host")
            .size_full()
            .pb(px(studio_ui::ds::space::MD))
            .overflow_y_scroll()
            .child(self.view.clone())
    }
}

/// The layout of a cached view that fills its area. Caching needs a definite size: the view
/// is laid out from this style, not measured from its contents.
fn cached_fill() -> gpui::StyleRefinement {
    gpui::StyleRefinement::default().v_flex().size_full()
}

/// The id of the right dock's inspector panel.
const INSPECTOR_PANEL: &str = "inspector";
const ACTIVITY_PANEL: &str = "activity";

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
        // A moved panel goes where the settings now place it, and the frame follows the layout
        // settings (the bottom panel's span).
        let settings_subscription = cx.observe_global::<StudioSettings>(|this, cx| {
            this.show_settings_error(cx);
            this.place_panels(cx);
            cx.notify();
        });
        let performance_subscription =
            cx.observe_global::<StudioSettings>(|this, cx| this.watch_performance(cx));
        let window_slot = persistence::window_opened(window, cx);
        // A window with a write still running stays open until the write ends; one with unsaved
        // tabs asks first.
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |workspace, cx| match workspace.running_write(cx) {
                None => {
                    let unsaved = workspace.unsaved_tabs(cx);
                    if !unsaved.is_empty() {
                        workspace.confirm_close(Closing::Window, unsaved, window, cx);
                        return false;
                    }
                    // Closing drops the save waiting for its delay: written now, in turn on
                    // the write thread; only a quit waits for the disk.
                    workspace.flush_saves(cx);
                    true
                }
                Some(graph) => {
                    workspace.show_toast(
                        ToastKind::Error,
                        format!(
                            "A write to {graph} is still running. Close the window when it finishes."
                        ),
                        cx,
                    );
                    false
                }
            })
            .unwrap_or(true)
        });
        let bounds_subscription = cx.observe_window_bounds(window, move |this, window, cx| {
            let bounds = window.bounds();
            this.pending_bounds = Some(bounds);
            this._save_bounds = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SAVE_DELAY).await;
                this.update(cx, |this, cx| this.save_bounds(cx)).ok();
            }));
        });
        // So are the CLI's files when they cannot be read (servers read before stand meanwhile).
        let registry = ConnectionRegistry::try_global(cx);
        let registry_subscription = registry.as_ref().map(|registry| {
            cx.observe(registry, |this, registry, cx| {
                this.show_cli_errors(&registry, cx)
            })
        });
        // The window's graphs ask after their branch heads at once when it becomes active and a
        // poll came while no window was.
        let activation_subscription = cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            if !this.window_active {
                this.pause_performance();
                return;
            }
            let sessions: Vec<_> = this
                .session
                .iter()
                .cloned()
                .chain(this.parked.iter().map(|parked| parked.session.clone()))
                .collect();
            for session in sessions {
                session.update(cx, |session, cx| session.window_activated(cx));
            }
        });
        let mut this = Self {
            session: None,
            parked: Vec::new(),
            graph_order: Vec::new(),
            saved_session: None,
            window_slot,
            panes: vec![Pane::new(0)],
            center: Member::Pane(0),
            active_pane: 0,
            next_pane: 1,
            dedupe_across_panes: true,
            left: Dock::new(PanelPosition::Left, true),
            right: Dock::new(PanelPosition::Right, true),
            bottom: Dock::new(PanelPosition::Bottom, false),
            zoomed: None,
            tab_scrolls: RefCell::default(),
            tabs_scrolled_to: RefCell::default(),
            drop_target: None,
            press_on_tab: false,
            modal: None,
            dock_hosts: HashMap::new(),
            toasts: Vec::new(),
            next_toast: 0,
            actions: Vec::new(),
            focus_handle: cx.focus_handle(),
            drag_pending: false,
            traffic_lights: None,
            lights_shown: cfg!(target_os = "macos"),
            settings_error: None,
            cli_errors: Vec::new(),
            _settings_subscription: settings_subscription,
            performance: None,
            window_active: window.is_window_active(),
            _performance: None,
            _performance_subscription: performance_subscription,
            saved: None,
            _save: None,
            _save_check: None,
            _save_bounds: None,
            pending_bounds: None,
            _bounds_subscription: bounds_subscription,
            // GPUI drops every window once the quit handlers ran: a save still waiting for its
            // delay would go with them.
            _quit_subscription: cx.on_app_quit(|this, cx| {
                this.flush_saves(cx);
                studio_settings::writes_done();
                async {}
            }),
            _activation_subscription: activation_subscription,
            _registry_subscription: registry_subscription,
            _graph_subscriptions: HashMap::new(),
            outcomes_toasted: HashMap::new(),
            _item_subscriptions: Vec::new(),
        };
        if let Some(registry) = registry {
            this.show_cli_errors(&registry, cx);
        }
        this.watch_performance(cx);
        this
    }

    /// Starts or stops reading Studio's memory and CPU as `studio.show_performance` says. The
    /// reading waits while another window is in front, and while it runs it draws the
    /// workspace again every two seconds (a cached tab's parts keep their last frame).
    fn watch_performance(&mut self, cx: &mut Context<Self>) {
        let on = cx
            .try_global::<StudioSettings>()
            .is_some_and(|settings| settings.studio.show_performance);
        if on == self._performance.is_some() {
            return;
        }
        if !on {
            self._performance = None;
            self.performance = None;
            cx.notify();
            return;
        }
        self._performance = Some(cx.spawn(async move |this, cx| {
            loop {
                let read = this.update(cx, |this, cx| {
                    if !this.window_active {
                        this.pause_performance();
                        return;
                    }
                    let sample = process_stats::Sample::now();
                    this.performance = Some(Performance::next(this.performance.as_ref(), sample));
                    cx.notify();
                });
                if read.is_err() {
                    return;
                }
                cx.background_executor().timer(PERFORMANCE_INTERVAL).await;
            }
        }));
    }

    /// The window went to the background: a reading after it would average CPU over the time
    /// away, so the next one starts over, keeping the figure shown until then. Marked when the
    /// window leaves, not at the next tick, which may come after it is back.
    fn pause_performance(&mut self) {
        if let Some(last) = &mut self.performance {
            last.paused = true;
        }
    }

    /// Studio's memory and CPU, muted at the end of the footer; in the warning colour past
    /// 2 GB or half a core.
    fn render_performance(&self) -> Option<impl IntoElement> {
        let reading = self.performance.as_ref()?;
        let memory = reading.sample.memory;
        let mut parts = Vec::new();
        if let Some((now, _)) = memory {
            parts.push(studio_ui::format_bytes(now));
        }
        if let Some(cpu) = reading.cpu {
            parts.push(format!("{}% CPU", (cpu * 100.).round() as u32));
        }
        if parts.is_empty() {
            return None;
        }
        let busy = memory.is_some_and(|(now, _)| now > 2 << 30) || reading.cpu > Some(0.5);
        let tooltip = match memory {
            Some((now, peak)) => format!(
                "Studio holds {} of memory, at most {} so far. CPU is how much of one core \
                 Studio used over the last two seconds; agents run as their own processes and \
                 are not counted.",
                studio_ui::format_bytes(now),
                studio_ui::format_bytes(peak)
            ),
            None => "CPU is how much of one core Studio used over the last two seconds; agents \
                     run as their own processes and are not counted."
                .to_owned(),
        };
        Some(
            div()
                .id("footer-performance")
                .flex_none()
                .tooltip(Tooltip::text(tooltip))
                .child(
                    studio_ui::ds::text::meta(parts.join(" · "))
                        .color(if busy { Color::Warning } else { Color::Muted })
                        .single_line(),
                ),
        )
    }

    /// The server URL and graph id the open session belongs to.
    fn graph_key(&self, cx: &App) -> Option<(String, String)> {
        self.session
            .as_ref()
            .map(|session| graph_key_of(session, cx))
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

    /// Records the graphs the window has open, for the next start, when they changed.
    fn save_open_graphs(&mut self, cx: &mut Context<Self>) {
        if persistence::restore_enabled() {
            let open = self.session_snapshot(cx);
            if self.saved_session.as_ref() != Some(&open) {
                persistence::window_graphs_changed(self.window_slot, &open, cx);
                self.saved_session = Some(open);
            }
        }
    }

    /// Writes the workspace's state shortly after it stops changing.
    fn save_if_changed(&mut self, cx: &mut Context<Self>) {
        self.save_open_graphs(cx);
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

    /// The docks a graph's saved state names: open or hidden, their sizes and active panels.
    /// A saved graph's docks, their sizes held within `window` as a drag holds them: a size
    /// saved on a larger display does not squeeze the document on a smaller one.
    fn apply_docks(&mut self, saved: &SavedWorkspace, window: gpui::Size<Pixels>) {
        self.left.open = saved.sidebar_open;
        self.right.open = saved.inspector_open;
        self.bottom.open = saved.activity_open;
        self.zoomed = None;
        let (width, height) = (f32::from(window.width), f32::from(window.height));
        for (dock, size, available) in [
            (&mut self.left, saved.left_size, width),
            (&mut self.right, saved.right_size, width),
            (&mut self.bottom, saved.bottom_size, height),
        ] {
            if let Some(size) = size.filter(|size| size.is_finite()) {
                dock.resize(size, available);
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
    }

    /// Reopens the tabs and panels remembered for the open graph.
    fn restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((url, graph)) = self.graph_key(cx) else {
            return;
        };
        if let Some(saved) = persistence::load(&url, &graph) {
            self.restore_saved(saved, window, cx);
        }
    }

    /// Reopens the tabs and panels of a graph's saved state, in the shown graph's place.
    fn restore_saved(
        &mut self,
        saved: SavedWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_docks(&saved, window.viewport_size());
        let mut ids = saved.layout.as_ref().map(Member::panes).unwrap_or_default();
        ids.sort_unstable();
        let (layout, panes) = match &saved.layout {
            Some(layout)
                if ids == (0..saved.panes.len()).collect::<Vec<_>>() && layout.is_well_formed() =>
            {
                (layout.clone(), saved.panes.clone())
            }
            // A layout that does not name every pane once, or whose shares do not fit its
            // panes: the first pane alone.
            _ => (
                Member::Pane(0),
                vec![saved.panes.first().cloned().unwrap_or_default()],
            ),
        };
        let shown = std::mem::replace(&mut self.panes, (0..panes.len()).map(Pane::new).collect());
        self.forget_items(&shown);
        self.center = layout;
        self.next_pane = panes.len();
        self.dedupe_across_panes = false;
        for (id, saved_pane) in panes.iter().enumerate() {
            // A saved tab can decline to open (a type the schema dropped, a kind removed): the
            // pins and the active tab are counted over the tabs that opened, not by position.
            let mut pinned = 0;
            let mut active = 0;
            for (index, item) in saved_pane.items.iter().enumerate() {
                // Set for each tab: one that opens once (a conversation) shows the tab another
                // pane holds, which makes that pane the active one.
                self.active_pane = id;
                let before = self.pane(id).map_or(0, |pane| pane.items.len());
                if let Some(open) = persistence::opener(cx, &item.kind) {
                    open(&item.data, self, window, cx);
                }
                let now = self.pane(id).map_or(0, |pane| pane.items.len());
                if now > before {
                    pinned += usize::from(index < saved_pane.pinned);
                    // The saved active tab, or the nearest one before it that opened.
                    if index <= saved_pane.active {
                        active = now - 1;
                    }
                }
            }
            if let Some(pane) = self.pane_mut(id) {
                pane.pinned = pinned.min(pane.items.len());
                pane.preview = None;
                pane.active = active.min(pane.items.len().saturating_sub(1));
            }
        }
        self.dedupe_across_panes = true;
        // The saved active pane, by its place in the saved order, before empty panes go.
        let wanted = self.center.panes().get(saved.active_pane).copied();
        // A pane whose tabs could not be reopened is dropped.
        for id in self.center.panes() {
            if self.pane(id).is_some_and(|p| p.items.is_empty()) && self.panes.len() > 1 {
                self.remove_pane(id);
            }
        }
        let active = wanted
            .filter(|id| self.pane(*id).is_some())
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
        // A graph the window has open keeps its session, its tabs and its place among the
        // graphs: it is shown, on the branch the new session is for. A second session for it
        // would leave its tabs on the first, and a write in flight unseen.
        let (url, graph) = graph_key_of(&session, cx);
        if let Some(open) = self.open_session_for(&url, &graph, cx) {
            let branch = session.read(cx).branch.clone();
            open.update(cx, |open, cx| open.set_branch(branch, cx));
            self.show_graph(open.entity_id(), window, cx);
            return;
        }
        if let Some(current) = self.park(cx) {
            self.parked.push(current);
        }
        self.graph_order.push(session.entity_id());
        self.subscribe_session(session, cx);
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

    /// Opens a graph session in this window behind the shown graph, as the graph switcher shows
    /// it. Its saved tabs open when it is first shown. A graph the window has open already is
    /// left as it is.
    pub fn add_session_behind(&mut self, session: Entity<GraphSession>, cx: &mut Context<Self>) {
        let saved = persistence::restore_enabled()
            .then(|| {
                let (url, graph) = graph_key_of(&session, cx);
                persistence::load(&url, &graph)
            })
            .flatten();
        self.add_behind(session, saved, cx);
    }

    fn add_behind(
        &mut self,
        session: Entity<GraphSession>,
        saved: Option<SavedWorkspace>,
        cx: &mut Context<Self>,
    ) {
        let (url, graph) = graph_key_of(&session, cx);
        if self.open_session_for(&url, &graph, cx).is_some() {
            return;
        }
        self.graph_order.push(session.entity_id());
        self.watch_graph(&session, cx);
        self.parked.push(ParkedGraph {
            session,
            panes: vec![Pane::new(0)],
            center: Member::Pane(0),
            active_pane: 0,
            next_pane: 1,
            saved,
            restored: false,
        });
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    /// Shows `session`'s graph as the window's.
    fn subscribe_session(&mut self, session: Entity<GraphSession>, cx: &mut Context<Self>) {
        self.watch_graph(&session, cx);
        self.session = Some(session);
    }

    /// Follows an open graph's session for as long as the window has it open: its writes'
    /// outcomes are toasted, and the shown one's other events drawn.
    fn watch_graph(&mut self, session: &Entity<GraphSession>, cx: &mut Context<Self>) {
        let id = session.entity_id();
        if self._graph_subscriptions.contains_key(&id) {
            return;
        }
        // Outcomes from before the graph opened here are the activity log's, not toasts.
        self.outcomes_toasted
            .insert(id, session.read(cx).outcomes_logged);
        let subscription = cx.subscribe(session, |this, session, event: &SessionEvent, cx| {
            let shown = this.session.as_ref() == Some(&session);
            match event {
                SessionEvent::ActivityChanged => this.toast_outcomes(&session, cx),
                SessionEvent::BranchMissing { branch } if shown => this.show_toast(
                    ToastKind::Info,
                    format!("The branch {branch} is gone, so main is shown."),
                    cx,
                ),
                _ => {}
            }
            if shown {
                cx.notify();
            }
        });
        self._graph_subscriptions.insert(id, subscription);
    }

    /// Writes what is waiting for the save delay now, and waits for the writes: the window or
    /// the app is going.
    fn flush_saves(&mut self, cx: &mut Context<Self>) {
        self.save_open_graphs(cx);
        self.save_now(cx);
        // A move or resize just before the window closes or Studio quits is kept too.
        self._save_bounds = None;
        self.save_bounds(cx);
    }

    /// Writes the window's place and size, when they changed since last written.
    fn save_bounds(&mut self, cx: &mut Context<Self>) {
        if let Some(bounds) = self.pending_bounds.take() {
            persistence::save_window_bounds(bounds);
            persistence::window_moved(self.window_slot, bounds, cx);
        }
    }

    /// Writes the shown graph's state now, rather than after the save delay, when a save is
    /// waiting or the state is not what was last written.
    fn save_now(&mut self, cx: &mut Context<Self>) {
        let Some((url, graph)) = self.graph_key(cx) else {
            return;
        };
        let pending = self._save.take().is_some();
        let snapshot = self.snapshot(cx);
        if pending || self.saved.as_ref() != Some(&snapshot) {
            persistence::save(&url, &graph, &snapshot);
            self.saved = Some(snapshot);
        }
    }

    /// Takes the shown graph out of the window, saved, leaving no graph shown.
    fn park(&mut self, cx: &mut Context<Self>) -> Option<ParkedGraph> {
        self.save_now(cx);
        let session = self.session.take()?;
        // The inspector shows the graph's rows, and holds its session.
        self.remove_panel(INSPECTOR_PANEL);
        self.zoomed = None;
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
            saved: self.saved.take(),
            restored: true,
        })
    }

    /// Drops the subscriptions to the tabs of `panes`, which are going.
    fn forget_items(&mut self, panes: &[Pane<Box<dyn ItemHandle>>]) {
        let going: HashSet<EntityId> = panes
            .iter()
            .flat_map(|pane| pane.items.iter().map(|item| item.item_id()))
            .collect();
        self._item_subscriptions
            .retain(|(id, _)| !going.contains(id));
    }

    fn show_parked(&mut self, parked: ParkedGraph, window: &mut Window, cx: &mut Context<Self>) {
        // The tabs shown with no graph open go.
        let shown = std::mem::take(&mut self.panes);
        self.forget_items(&shown);
        self.panes = parked.panes;
        self.center = parked.center;
        self.active_pane = parked.active_pane;
        self.next_pane = parked.next_pane;
        self.subscribe_session(parked.session, cx);
        if parked.restored {
            // Its docks as it left them, not the docks of the graph shown before.
            if let Some(saved) = &parked.saved {
                self.apply_docks(saved, window.viewport_size());
            }
            self.saved = parked.saved;
        } else {
            self.saved = None;
            if let Some(saved) = parked.saved {
                self.restore_saved(saved, window, cx);
            }
        }
        let index = self.pane(self.active_pane).map_or(0, |p| p.active);
        self.activate_in(self.active_pane, index, false, window, cx);
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    /// The session this window has open for `graph` on the server at `url`. A graph is matched
    /// by its server's address, as its saved state is kept, so one open under another name the
    /// CLI gives that server is the same graph.
    pub fn open_session_for(
        &self,
        url: &str,
        graph: &str,
        cx: &App,
    ) -> Option<Entity<GraphSession>> {
        self.session
            .iter()
            .chain(self.parked.iter().map(|parked| &parked.session))
            .find(|open| {
                let open = open.read(cx);
                open.graph_id == graph && open.connection.read(cx).server.url == url
            })
            .cloned()
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

    /// The titles of this window's tabs with unsaved work, the shown graph's and each parked
    /// graph's: closing the window or quitting drops it.
    pub fn unsaved_tabs(&self, cx: &App) -> Vec<SharedString> {
        let parked = self.parked.iter().flat_map(|parked| {
            parked
                .panes
                .iter()
                .flat_map(|pane| pane.items.iter().map(|item| item.as_ref()))
        });
        self.all_items()
            .chain(parked)
            .filter(|item| item.is_dirty(cx))
            .map(|item| item.tab_title(cx))
            .collect()
    }

    /// Asks before `closing` drops the unsaved work of the tabs named. A close asked while
    /// another waits for its answer asks the wider of the two (Quit over the window, the
    /// window over the graph, the graph over a tab): ⌘Q over a tab's question asks about
    /// quitting, where ⌘W over the window's question leaves it.
    fn confirm_close(
        &mut self,
        closing: Closing,
        unsaved: Vec<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let asked = self
            .modal
            .as_ref()
            .and_then(|modal| modal.view.clone().downcast::<ConfirmClose>().ok())
            .map(|dialog| dialog.read(cx).closing);
        if asked.is_some_and(|asked| asked.reach() > closing.reach()) {
            return;
        }
        let workspace = cx.entity().downgrade();
        self.show_modal(window, cx, move |_, cx| ConfirmClose {
            workspace,
            closing,
            unsaved,
            focus_handle: cx.focus_handle(),
        });
    }

    /// A graph of this window, shown or parked, with a write still running: closing the window
    /// or quitting would drop the write's outcome.
    pub fn running_write(&self, cx: &App) -> Option<SharedString> {
        self.session
            .iter()
            .chain(self.parked.iter().map(|parked| &parked.session))
            .map(|session| session.read(cx))
            .find(|session| session.writes_in_flight > 0)
            .map(|session| session.graph_id.clone().into())
    }

    /// Closes the shown graph's tabs in this window and shows the graph opened before it. Its
    /// tabs are saved first, so opening it again brings them back. A graph with a write still
    /// running stays open: closing it could drop the write's outcome. Tabs with unsaved work
    /// are named first, in a dialog that closes the graph once confirmed.
    pub fn close_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_graph_asking(true, window, cx);
    }

    fn close_graph_asking(&mut self, ask: bool, window: &mut Window, cx: &mut Context<Self>) {
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
        let unsaved: Vec<SharedString> = self
            .all_items()
            .filter(|item| item.is_dirty(cx))
            .map(|item| item.tab_title(cx))
            .collect();
        if ask && !unsaved.is_empty() {
            self.confirm_close(Closing::Graph, unsaved, window, cx);
            return;
        }
        let id = session.entity_id();
        if let Some(closed) = self.park(cx) {
            self.forget_items(&closed.panes);
        }
        self.graph_order.retain(|open| *open != id);
        self._graph_subscriptions.remove(&id);
        self.outcomes_toasted.remove(&id);
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

    /// Toasts the outcomes `session` logged since the last asked: each refusal and each
    /// outcome nobody knows, and a commit once no write of its graph is out, so a run of fixes
    /// toasts its last. A graph not shown names itself.
    fn toast_outcomes(&mut self, session: &Entity<GraphSession>, cx: &mut Context<Self>) {
        let id = session.entity_id();
        let read = session.read(cx);
        let logged = read.outcomes_logged;
        let seen = self.outcomes_toasted.insert(id, logged).unwrap_or(logged);
        let new = (logged.saturating_sub(seen) as usize).min(read.outcomes.len());
        let writing = read.writes_in_flight > 0;
        let fresh = read.outcomes[read.outcomes.len() - new..].to_vec();
        let graph = (self.session.as_ref() != Some(session)).then(|| read.graph_id.clone());
        for (ix, entry) in fresh.iter().enumerate() {
            if matches!(entry.outcome, OutcomeKind::Committed { .. })
                && (writing || ix + 1 < fresh.len())
            {
                continue;
            }
            self.toast_outcome(entry, graph.as_deref(), cx);
        }
    }

    fn toast_outcome(
        &mut self,
        latest: &graph_store::OutcomeEntry,
        graph: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let text = match graph {
            Some(graph) => format!("{graph}: {}", toast_text(latest)),
            None => toast_text(latest),
        };
        if self.toasts.iter().any(|t| t.message.as_ref() == text) {
            return;
        }
        match &latest.outcome {
            // No publish, no toast (constitution VII); the surface that wrote explains it.
            OutcomeKind::Committed { commit: None } => {}
            OutcomeKind::Committed {
                commit: Some(commit),
            } => {
                let (label, action) = committed_follow_up(latest, commit);
                self.show_toast_with_action(ToastKind::Success, text, label, action, cx);
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
        Some(self.add_new(preview, window, cx, build))
    }

    /// Opens an item of type `T` in a new tab whatever is open: one that has no key yet, as a
    /// new conversation before the agent names it.
    pub fn add_item<T: Item>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Window, &mut Context<T>) -> T,
    ) -> Entity<T> {
        self.add_new(false, window, cx, build)
    }

    /// Makes an item, follows its events and puts it in the active pane.
    fn add_new<T: Item>(
        &mut self,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Window, &mut Context<T>) -> T,
    ) -> Entity<T> {
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
        item
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
            pane.forget(id);
            self._item_subscriptions.retain(|(item, _)| *item != id);
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

    /// Activates a pane's tab; `record` notes the tab that pane leaves for its Back trail and
    /// forgets its Forward trail.
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
        // Showing a tab of another pane than the zoomed one brings the panes back.
        if matches!(self.zoomed, Some(Zoomed::Pane(zoomed)) if zoomed != pane_id) {
            self.zoomed = None;
        }
        self.active_pane = pane_id;
        if let Some(pane) = self.pane_mut(pane_id) {
            if record
                && let Some(current) = pane.active_item().map(|item| item.item_id())
                && current != next
            {
                pane.record(current);
            }
            pane.active = index;
            let focus = pane.items[index].focus_handle(cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Back and Forward (⌘[ and ⌘], and the arrows at the start of a pane's tabs): the tab a
    /// pane showed before, or the one it stepped back from, as Zed keeps history per pane.
    fn navigate(
        &mut self,
        pane_id: PaneId,
        back: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.pane_mut(pane_id).and_then(|pane| pane.step(back)) else {
            return;
        };
        self.activate_in(pane_id, index, false, window, cx);
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
        let closing = Closing::Tab(item.item_id());
        let title = item.tab_title(cx);
        self.confirm_close(closing, vec![title], window, cx);
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
        if self.zoomed == Some(Zoomed::Pane(id)) {
            self.zoomed = None;
        }
        self.panes.retain(|pane| pane.id != id);
        self.tab_scrolls.borrow_mut().remove(&id);
        self.tabs_scrolled_to.borrow_mut().remove(&id);
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
    /// be reopened, and becomes active. A tab that opens once in a window (a conversation)
    /// stays where it is, and the new pane opens empty.
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

    /// A tab dropped on a tab (`Some(index)`) or after a strip's tabs (`None`): moved within its
    /// pane, or into another pane, which shows it. A pane left with no tab closes.
    fn drop_tab(
        &mut self,
        dragged: &DraggedTab,
        to_pane: PaneId,
        to_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drop_target = None;
        let Some((from_pane, from_index)) = self.find_item(dragged.item) else {
            return;
        };
        if from_pane == to_pane {
            let Some(pane) = self.pane_mut(to_pane) else {
                return;
            };
            pane.keep(dragged.item);
            let to = to_index.unwrap_or(pane.items.len().saturating_sub(1));
            let index = pane.move_to(from_index, to);
            self.activate_in(to_pane, index, true, window, cx);
            return;
        }
        let Some(item) = self.pane_mut(from_pane).and_then(|pane| {
            pane.forget(dragged.item);
            pane.remove(from_index)
        }) else {
            return;
        };
        let Some(pane) = self.pane_mut(to_pane) else {
            return;
        };
        let index = pane.insert_at(item, to_index.unwrap_or(usize::MAX));
        if self
            .pane(from_pane)
            .is_some_and(|pane| pane.items.is_empty())
        {
            self.remove_pane(from_pane);
        }
        self.activate_in(to_pane, index, true, window, cx);
    }

    /// A tab dropped near a pane's edge: a new pane on that side holds it, as Zed splits on a
    /// drop. The only tab of a pane dropped on its own pane stays.
    fn drop_split(
        &mut self,
        dragged: &DraggedTab,
        at: PaneId,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((from_pane, from_index)) = self.find_item(dragged.item) else {
            return;
        };
        if from_pane == at && self.pane(at).is_some_and(|pane| pane.items.len() < 2) {
            return;
        }
        let new = self.next_pane;
        if !self.center.split(at, new, direction) {
            return;
        }
        self.next_pane += 1;
        let Some(item) = self.pane_mut(from_pane).and_then(|pane| {
            pane.forget(dragged.item);
            pane.remove(from_index)
        }) else {
            return;
        };
        let mut pane = Pane::new(new);
        pane.insert_at(item, 0);
        self.panes.push(pane);
        if self
            .pane(from_pane)
            .is_some_and(|pane| pane.items.is_empty())
        {
            self.remove_pane(from_pane);
        }
        self.activate_in(new, 0, true, window, cx);
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

    /// Adds a panel to its dock, or replaces the view of the panel with the same id there.
    /// `position` is its own dock; it goes to another when the person moved it there
    /// (`panels.<id>.dock` in the settings).
    pub fn add_panel(
        &mut self,
        position: PanelPosition,
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        icon: IconName,
        view: AnyView,
        cx: &mut Context<Self>,
    ) {
        let id = id.into();
        self.host_for(&view, cx);
        let dock = self
            .dock_of(&id)
            .unwrap_or_else(|| placed(&id, position, cx));
        self.dock_mut(dock).insert(PanelEntry {
            id,
            title: title.into(),
            icon,
            home: position,
            view,
            actions: None,
            focus: None,
        });
        // The view it replaced goes with its host.
        self.drop_stale_hosts();
        cx.notify();
    }

    /// The dock that holds a panel.
    fn dock_of(&self, id: &str) -> Option<PanelPosition> {
        [&self.left, &self.right, &self.bottom]
            .into_iter()
            .find(|dock| dock.position_of(id).is_some())
            .map(|dock| dock.position)
    }

    /// Moves a panel to another dock, as Zed moves a panel whose `dock` setting changed: a panel
    /// that showed keeps showing there, opening that dock.
    fn move_panel(&mut self, id: &str, to: PanelPosition, cx: &mut Context<Self>) {
        let Some(from) = self.dock_of(id) else {
            return;
        };
        if from == to {
            return;
        }
        let dock = self.dock_mut(from);
        let shown = dock.open && dock.active_id().as_deref() == Some(id);
        let Some(panel) = dock.take(id) else {
            return;
        };
        if self.zoomed == Some(Zoomed::Dock(from)) && self.dock_mut(from).panels.is_empty() {
            self.zoomed = None;
        }
        let target = self.dock_mut(to);
        target.insert(panel);
        if shown {
            target.activate(id);
        }
        cx.notify();
    }

    /// Puts every panel where the settings place it, after they change.
    fn place_panels(&mut self, cx: &mut Context<Self>) {
        let moves: Vec<(SharedString, PanelPosition)> = [&self.left, &self.right, &self.bottom]
            .into_iter()
            .flat_map(|dock| {
                dock.panels.iter().filter_map(|panel| {
                    let to = placed(&panel.id, panel.home, cx);
                    (to != dock.position).then(|| (panel.id.clone(), to))
                })
            })
            .collect();
        for (id, to) in moves {
            self.move_panel(&id, to, cx);
        }
    }

    /// The person's choice from a panel button's menu: moves the panel and remembers where, or
    /// forgets the choice when the panel goes back to its own dock.
    fn dock_panel(&mut self, id: SharedString, to: PanelPosition, cx: &mut Context<Self>) {
        let home = [&self.left, &self.right, &self.bottom]
            .into_iter()
            .flat_map(|dock| dock.panels.iter())
            .find(|panel| panel.id == id)
            .map(|panel| panel.home);
        let Some(home) = home else {
            return;
        };
        self.move_panel(&id, to, cx);
        let key = id.to_string();
        studio_settings::update(cx, move |settings| {
            if to == home {
                settings.panels.remove(&key);
            } else {
                settings.panels.entry(key).or_default().dock = Some(to.side());
            }
        });
    }

    /// Makes sure a dock panel has its cached host.
    fn host_for(&mut self, view: &AnyView, cx: &mut Context<Self>) {
        let view = view.clone();
        self.dock_hosts.entry(view.entity_id()).or_insert_with(|| {
            cx.new(|_| DockHost {
                view,
                scrolls_itself: false,
            })
        });
    }

    /// Drops the hosts of panels gone from the docks, whenever a panel goes or is replaced. A
    /// host holds its panel's view and what the view holds: an inspector holds its graph's
    /// session, whose poll of the server runs for as long as the session lives.
    fn drop_stale_hosts(&mut self) {
        let live: HashSet<EntityId> = [&self.left, &self.right, &self.bottom]
            .iter()
            .flat_map(|dock| dock.panels.iter().map(|panel| panel.view.entity_id()))
            .collect();
        self.dock_hosts.retain(|id, _| live.contains(id));
    }

    /// Takes a panel out of its dock, with its host.
    fn remove_panel(&mut self, id: &str) {
        if let Some(position) = self.dock_of(id) {
            self.dock_mut(position).remove(id);
        }
        self.drop_stale_hosts();
    }

    /// Lets a side panel scroll itself, as a list that draws only the rows in view does: its
    /// host gives it the dock's size instead of scrolling it.
    pub fn set_panel_scrolls_itself(&mut self, view: &AnyView, cx: &mut Context<Self>) {
        if let Some(host) = self.dock_hosts.get(&view.entity_id()) {
            host.update(cx, |host, cx| {
                host.scrolls_itself = true;
                cx.notify();
            });
        }
    }

    /// A dock panel's view, cached inside its scrolling host.
    fn hosted(&self, view: &AnyView) -> AnyElement {
        match self.dock_hosts.get(&view.entity_id()) {
            Some(host) => host.clone().cached(cached_fill()).into_any_element(),
            None => view.clone().into_any_element(),
        }
    }

    /// Puts small buttons on the right of a panel's title line.
    pub fn set_panel_actions(&mut self, id: &str, actions: AnyView, cx: &mut Context<Self>) {
        for dock in [&mut self.left, &mut self.right, &mut self.bottom] {
            if dock.set_actions(id, actions.clone()) {
                break;
            }
        }
        cx.notify();
    }

    /// What takes the keyboard when the panel is shown from its button or its key.
    pub fn set_panel_focus(
        &mut self,
        id: &str,
        focus: impl Fn(&App) -> Option<FocusHandle> + 'static,
        cx: &mut Context<Self>,
    ) {
        let focus: panels::PanelFocus = std::rc::Rc::new(focus);
        for dock in [&mut self.left, &mut self.right, &mut self.bottom] {
            if let Some(index) = dock.position_of(id) {
                dock.panels[index].focus = Some(focus.clone());
                break;
            }
        }
        cx.notify();
    }

    /// Shows a panel, as [`Self::activate_panel`], and gives it the keyboard when it takes it:
    /// a panel asked for by its button or its key, as Zed focuses a dock's panel.
    pub fn show_panel(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_panel(id, cx);
        let focus = [&self.left, &self.right, &self.bottom]
            .into_iter()
            .find_map(|dock| dock.panels.iter().find(|panel| panel.id.as_ref() == id))
            .and_then(|panel| panel.focus.clone())
            .and_then(|focus| focus(cx));
        if let Some(focus) = focus {
            window.focus(&focus, cx);
        }
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
                self.host_for(&view, cx);
                let position = self
                    .dock_of(INSPECTOR_PANEL)
                    .unwrap_or_else(|| placed(INSPECTOR_PANEL, PanelPosition::Right, cx));
                let dock = self.dock_mut(position);
                dock.insert(PanelEntry {
                    id: INSPECTOR_PANEL.into(),
                    title: "Inspector".into(),
                    icon: IconName::Info,
                    home: PanelPosition::Right,
                    view,
                    actions: None,
                    focus: None,
                });
                // Selection shows the inspector but never reopens a dock the user closed.
                let open = dock.open;
                dock.activate(INSPECTOR_PANEL);
                dock.open = open;
                // The inspector it replaced goes with its host.
                self.drop_stale_hosts();
            }
            None => self.remove_panel(INSPECTOR_PANEL),
        }
        cx.notify();
    }

    /// Opens the inspector's dock on the inspector. An explicit peek (Space, a click) shows it
    /// even when the person hid the dock; a peek that follows the cursor does not.
    pub fn reveal_inspector(&mut self, cx: &mut Context<Self>) {
        if let Some(position) = self.dock_of(INSPECTOR_PANEL) {
            self.dock_mut(position).activate(INSPECTOR_PANEL);
            cx.notify();
        }
    }

    /// The activity log: the bottom dock's panel.
    pub fn set_activity_view(&mut self, view: AnyView, cx: &mut Context<Self>) {
        let position = self
            .dock_of(ACTIVITY_PANEL)
            .unwrap_or_else(|| placed(ACTIVITY_PANEL, PanelPosition::Bottom, cx));
        let open = self.dock_mut(position).open;
        self.add_panel(
            PanelPosition::Bottom,
            ACTIVITY_PANEL,
            "Activity",
            IconName::BoltOutlined,
            view,
            cx,
        );
        self.dock_mut(position).open = open;
    }

    // ---- modals and toasts ----

    /// Shows a modal, or closes it when a modal of the same type is open.
    pub fn toggle_modal<V: ModalView>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
    ) {
        let same = self
            .modal
            .as_ref()
            .is_some_and(|active| active.view.clone().downcast::<V>().is_ok());
        if same {
            self.hide_modal(window, cx);
        } else {
            self.show_modal(window, cx, build);
        }
    }

    /// Shows a modal in place of the one open, of whatever type.
    pub fn show_modal<V: ModalView>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
    ) {
        // As Zed's modal layer: the open modal is asked first, whether a command closes it or
        // another replaces it; a modal that says no stays, and the replacement is not shown.
        // The focus to return to is the one before the first modal, not the modal's own.
        let mut previous_focus = window.focused(cx);
        if let Some(active) = &self.modal {
            previous_focus = active.previous_focus.clone();
            if !self.hide_modal(window, cx) {
                return;
            }
        }
        let view = cx.new(|cx| build(window, cx));
        let subscription =
            cx.subscribe_in(&view, window, |this, _, _: &DismissEvent, window, cx| {
                this.hide_modal(window, cx);
            });
        let focus = view.focus_handle(cx);
        let placement = view.read(cx).placement();
        let asked = view.clone();
        let before_dismiss = Box::new(move |window: &mut Window, cx: &mut App| {
            asked.update(cx, |modal, cx| modal.on_before_dismiss(window, cx))
        });
        self.modal = Some(ActiveModal {
            view: view.into(),
            placement,
            previous_focus,
            before_dismiss,
            _subscription: subscription,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Closes the modal if it agrees ([`ModalView::on_before_dismiss`]); returns whether no
    /// modal is open now.
    pub fn hide_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(modal) = &self.modal
            && !(modal.before_dismiss)(window, cx)
        {
            return false;
        }
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
        true
    }

    #[cfg(test)]
    fn has_modal(&self) -> bool {
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

    /// Names each new error from the CLI's files, and each server that could not be added at
    /// start, in a toast that stays until dismissed.
    fn show_cli_errors(&mut self, registry: &Entity<ConnectionRegistry>, cx: &mut Context<Self>) {
        let registry = registry.read(cx);
        let errors: Vec<String> = registry
            .cli_errors()
            .iter()
            .map(|error| format!("OmniGraph CLI config: {error}"))
            .chain(
                registry
                    .start_errors()
                    .iter()
                    .map(|error| format!("A server could not be added: {error}")),
            )
            .collect();
        for error in &errors {
            if !self.cli_errors.contains(error) {
                self.show_toast(ToastKind::Error, error.clone(), cx);
            }
        }
        self.cli_errors = errors;
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
        // A few at a time, the oldest going first: an error stays until closed, and many
        // would cover the window.
        if self.toasts.len() >= TOASTS_SHOWN {
            self.toasts.remove(0);
        }
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
    fn strip(
        &self,
        id: impl Into<ElementId>,
        height: f32,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        h_flex()
            .id(id)
            .window_control_area(WindowControlArea::Drag)
            .h(px(height))
            .w_full()
            .flex_none()
            .items_center()
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.drag_pending = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.drag_pending = false;
                    this.press_on_tab = false;
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    // A tab under the press keeps it for its own drag.
                    this.drag_pending = !std::mem::take(&mut this.press_on_tab);
                }),
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

    /// Room for the traffic lights at the start of a top-left strip (`lights_room`).
    fn lights_room(&self, zoomed: bool, cx: &App) -> Option<f32> {
        lights_room(self.lights_shown, zoomed, cx)
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = studio_ui::ds::surface::chrome(cx);
        let hairline = studio_ui::ds::surface::rule(cx);
        // The column's header is its panel's own, as a Zed panel draws its header: the panel's
        // title and its buttons. Panels are switched, and the sidebar shown and hidden, from
        // the status bar.
        let header = self
            .strip("sidebar-header", HEADER_HEIGHT, cx)
            .pr(px(studio_ui::ds::space::SM))
            .gap(px(studio_ui::ds::space::XS))
            .when(!studio_ui::ds::is_glass(cx), |d| {
                d.border_b_1().border_color(hairline)
            })
            .map(|strip| match self.lights_room(false, cx) {
                Some(room) => strip.child(div().w(px(room)).flex_none()),
                None => strip.pl(px(studio_ui::ds::space::EDGE)),
            })
            .children(self.left.active_panel().map(dock_title))
            .child(
                IconButton::new("search", IconName::MagnifyingGlass)
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
        // The content scrolls in its cached host.
        let body = v_flex().id("sidebar-panel").flex_1().min_h_0().children(
            self.left
                .active_panel()
                .map(|panel| div().size_full().child(self.panel_body(panel))),
        );
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
            .child(resize_handle(PanelPosition::Left))
    }

    /// The status bar's first button, as Zed's sidebar toggle: shows or hides the sidebar, its
    /// icon drawn as the sidebar is.
    fn render_sidebar_toggle(&self) -> impl IntoElement {
        let open = self.left.open;
        IconButton::new(
            "toggle-sidebar",
            if open {
                IconName::ThreadsSidebarLeftOpen
            } else {
                IconName::ThreadsSidebarLeftClosed
            },
        )
        .tooltip(move |_, cx| {
            Tooltip::for_action(
                if open { "Hide sidebar" } else { "Show sidebar" },
                &ToggleSidebar,
                cx,
            )
        })
        .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleSidebar), cx))
    }

    /// A dock's panels as buttons in the status bar, as Zed's `PanelButtons`: one icon each.
    /// The active panel of an open dock is lit, and clicking it hides the dock; clicking any
    /// other shows its panel, opening the dock.
    fn render_panel_buttons(
        &self,
        position: PanelPosition,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dock = match position {
            PanelPosition::Left => &self.left,
            PanelPosition::Right => &self.right,
            PanelPosition::Bottom => &self.bottom,
        };
        let hide_label = match position {
            PanelPosition::Left => "Hide sidebar",
            PanelPosition::Right => "Hide inspector",
            PanelPosition::Bottom => "Hide bottom panel",
        };
        let workspace = cx.entity().downgrade();
        h_flex()
            .gap(px(studio_ui::ds::space::XS))
            .children(dock.panels.iter().enumerate().map(|(index, panel)| {
                let lit = dock.open && index == dock.active;
                let id = panel.id.clone();
                let title = panel.title.clone();
                let icon = panel.icon;
                let tooltip_id = id.clone();
                let workspace = workspace.clone();
                let this = cx.entity().downgrade();
                let (anchor, attach) = match position {
                    PanelPosition::Left => (gpui::Anchor::BottomLeft, gpui::Anchor::TopLeft),
                    PanelPosition::Bottom | PanelPosition::Right => {
                        (gpui::Anchor::BottomRight, gpui::Anchor::TopRight)
                    }
                };
                let button_id = id.clone();
                // As Zed's panel buttons: the right-click menu moves the panel to another dock,
                // and no tooltip covers it while it is open.
                right_click_menu(SharedString::from(format!("panel-menu-{id}")))
                    .anchor(anchor)
                    .attach(attach)
                    .trigger(move |menu_open, _, _| {
                        let title = title.clone();
                        let tooltip_id = tooltip_id.clone();
                        let id = button_id.clone();
                        let this = this.clone();
                        IconButton::new(
                            SharedString::from(format!("panel-button-{id}-{}", lit as u8)),
                            icon,
                        )
                        .toggle_state(lit)
                        .when(!menu_open, |button| {
                            button.tooltip(move |_, cx| {
                                if lit {
                                    Tooltip::for_action(
                                        hide_label,
                                        toggle_action(position).as_ref(),
                                        cx,
                                    )
                                } else {
                                    Tooltip::for_action(
                                        title.clone(),
                                        &studio_actions::workspace::ActivatePanel {
                                            id: tooltip_id.to_string(),
                                        },
                                        cx,
                                    )
                                }
                            })
                        })
                        .on_click(move |_, window, cx| {
                            this.update(cx, |w, cx| {
                                w.press_panel_button(position, &id, window, cx)
                            })
                            .ok();
                        })
                    })
                    .menu(move |window, cx| {
                        let workspace = workspace.clone();
                        let id = id.clone();
                        ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for to in PanelPosition::ALL {
                                let workspace = workspace.clone();
                                let id = id.clone();
                                menu = menu.toggleable_entry(
                                    to.label(),
                                    to == position,
                                    IconPosition::Start,
                                    None,
                                    move |_, cx| {
                                        workspace
                                            .update(cx, |w, cx| w.dock_panel(id.clone(), to, cx))
                                            .ok();
                                    },
                                );
                            }
                            menu
                        })
                    })
            }))
    }

    /// A panel's status-bar button: hides the dock when the panel is the one it shows, else
    /// shows the panel, opening its dock.
    fn press_panel_button(
        &mut self,
        position: PanelPosition,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dock = self.dock_mut(position);
        if dock.open && dock.active_id().as_deref() == Some(id) {
            self.set_dock_open(position, false, window, cx);
        } else {
            self.show_panel(id, window, cx);
        }
    }

    /// Opens or hides a dock; hiding the zoomed one ends the zoom.
    fn set_dock_open(
        &mut self,
        position: PanelPosition,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !open && self.dock_has_focus(position, window, cx) {
            self.focus_active_tab(window, cx);
        }
        self.dock_mut(position).open = open;
        if !open && self.zoomed == Some(Zoomed::Dock(position)) {
            self.zoomed = None;
        }
        cx.notify();
    }

    /// ⇧⎋: zooms the active pane over the document area, or ends any zoom.
    fn toggle_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.zoomed = match self.zoomed {
            Some(_) => None,
            None => Some(Zoomed::Pane(self.active_pane)),
        };
        // The docks are under the zoomed pane now.
        if self.zoomed.is_some()
            && [
                PanelPosition::Left,
                PanelPosition::Right,
                PanelPosition::Bottom,
            ]
            .into_iter()
            .any(|position| self.dock_has_focus(position, window, cx))
        {
            self.focus_active_tab(window, cx);
        }
        cx.notify();
    }

    /// Whether the keyboard is in a panel of the dock at `position`.
    fn dock_has_focus(&self, position: PanelPosition, window: &Window, cx: &App) -> bool {
        let dock = match position {
            PanelPosition::Left => &self.left,
            PanelPosition::Right => &self.right,
            PanelPosition::Bottom => &self.bottom,
        };
        dock.panels
            .iter()
            .filter_map(|panel| panel.focus.as_ref()?(cx))
            .any(|focus| focus.contains_focused(window, cx))
    }

    /// Gives the keyboard to the active tab: the dock that had it is hidden or covered, and the
    /// keys would go on driving a panel nobody sees.
    fn focus_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.pane(self.active_pane).map_or(0, |p| p.active);
        self.activate_in(self.active_pane, index, false, window, cx);
    }

    /// The server this window is connected to: a status dot, the server and its state, as
    /// Delta's sidebar shows the account. It opens the graph switcher: the graphs open in this
    /// window, another graph, closing this one.
    fn render_server_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (color, server, url, state) = match &self.session {
            Some(session) => {
                let connection = session.read(cx).connection.read(cx);
                let color = connection_color(&connection.state);
                // The server's round trip from the last health check, beside its state.
                let state = match (&connection.state, connection.latency) {
                    (ConnectionState::Ready, Some(latency)) => {
                        format!("{} · {} ms", connection.state.label(), latency.as_millis())
                    }
                    (state, _) => state.label(),
                };
                (
                    color,
                    connection.server.name.clone(),
                    connection.server.url.clone(),
                    state,
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
            .tooltip(Tooltip::text(format!("{url} · {state} · Switch graph")))
            .child(
                h_flex()
                    .px(px(studio_ui::ds::space::XS))
                    .gap(px(studio_ui::ds::space::SM))
                    .items_center()
                    .child(Indicator::dot().color(color))
                    .child(studio_ui::ds::text::body(server).single_line())
                    .child(studio_ui::ds::text::muted(state).single_line())
                    .when(others > 0, |row| {
                        row.child(studio_ui::ds::sidebar_count(format!(
                            "+{others} graph{}",
                            if others == 1 { "" } else { "s" }
                        )))
                    })
                    .child(studio_ui::ds::icon::meta(IconName::ChevronUpDown)),
            );
        PopoverMenu::new("graph-switcher-menu")
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
            })
    }

    /// A pane's tabs, as Delta draws them: text tabs separated by hairlines, the active one
    /// joined to the content below it. Pinned tabs come first with a pin; a preview tab's title
    /// is in italics until it is kept (a double click, or work in it).
    fn render_tabs(&self, pane_id: PaneId, cx: &mut Context<Self>) -> impl IntoElement {
        // In the glass style the tab strip is part of the sheet: the active tab is clear and the
        // others are barely shaded.
        let (editor_bg, inactive_bg) = (
            studio_ui::ds::surface::tab_active(cx),
            studio_ui::ds::surface::tab_inactive(cx),
        );
        let hairline = studio_ui::ds::surface::rule(cx);
        let hover_bg = studio_ui::ds::surface::highlight(cx);
        let split = self.panes.len() > 1;
        let focused_pane = pane_id == self.active_pane;
        let in_corner = first_tab_in_sheet_corner(
            studio_ui::ds::is_glass(cx),
            self.center.top_left() == pane_id,
            self.left.visible(),
            self.session.is_some(),
        );
        let workspace = cx.entity().downgrade();
        let Some(pane) = self.pane(pane_id) else {
            return h_flex().into_any_element();
        };
        let drop_bg = studio_ui::ds::surface::drop_target(cx);
        // Tabs past the strip's width scroll, as Zed's do; the active tab is brought into view
        // when it changes.
        let scroll = self
            .tab_scrolls
            .borrow_mut()
            .entry(pane_id)
            .or_default()
            .clone();
        let active_id = pane.active_item().map(|item| item.item_id());
        if self.tabs_scrolled_to.borrow().get(&pane_id).copied() != active_id {
            scroll.scroll_to_item(pane.active);
            self.tabs_scrolled_to
                .borrow_mut()
                .insert(pane_id, active_id.unwrap_or(EntityId::from(0)));
        }
        h_flex()
            .id(SharedString::from(format!("tabs-{pane_id}")))
            .h_full()
            .flex_1()
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&scroll)
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
                // As Zed's `Tab`: a slot before the content for the tab's state (unsaved,
                // newer data) and one after it for its close or pin button, both kept when
                // empty so the tabs' contents line up.
                let indicator = if dirty {
                    Some(Indicator::dot().color(Color::Warning))
                } else if stale {
                    Some(Indicator::dot().color(Color::Muted))
                } else {
                    None
                };
                let end_button = if pinned {
                    studio_ui::ds::inline_icon_button(
                        SharedString::from(format!("unpin-{pane_id}-{index}")),
                        IconName::Pin,
                    )
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Unpin"))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_pin(pane_id, index, cx)),
                    )
                } else {
                    studio_ui::ds::inline_icon_button(
                        SharedString::from(format!("close-{pane_id}-{index}")),
                        IconName::Close,
                    )
                    // On hover only, the active tab's too, as Zed's `show_close_button: hover`:
                    // its slot stays, so the title sits evenly between the tab's edges.
                    .visible_on_hover(group.clone())
                    .tooltip(|_, cx| Tooltip::for_action("Close", &CloseActiveItem, cx))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_in(pane_id, index, window, cx)
                    }))
                };
                // The strip's height, not `h_full`: the right-click wrapper lays out at its
                // content's height, so a percentage would shrink the tab to its label.
                let tab = h_flex()
                    .id(SharedString::from(format!("tab-{pane_id}-{index}")))
                    .debug_selector(move || format!("tab-{pane_id}-{index}"))
                    .group(group)
                    .h(px(HEADER_HEIGHT))
                    .px(px(studio_ui::ds::space::XS))
                    .gap(px(studio_ui::ds::space::XS))
                    .items_center()
                    .flex_none()
                    .relative()
                    .border_r_1()
                    .border_color(hairline)
                    .when(in_corner && index == 0, |tab| {
                        tab.rounded_tl(px(studio_ui::ds::radii(cx).sheet()))
                    })
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
                                    .h(px(studio_ui::ds::size::HAIRLINE))
                                    .bg(hairline),
                            )
                    })
                    .cursor_pointer()
                    .child(
                        h_flex()
                            .size(px(TAB_START_SLOT))
                            .flex_none()
                            .justify_center()
                            .children(indicator),
                    )
                    .child(
                        h_flex()
                            .gap(px(studio_ui::ds::space::XS))
                            .items_center()
                            .children(item.tab_icon(cx).map(studio_ui::ds::icon::meta))
                            .child(
                                studio_ui::ds::text::body(item.tab_title(cx))
                                    .color(if strong { Color::Default } else { Color::Muted })
                                    .when(preview, |label| label.italic())
                                    .single_line(),
                            ),
                    )
                    .child(
                        h_flex()
                            .size(px(TAB_END_SLOT))
                            .flex_none()
                            .justify_center()
                            .child(end_button),
                    )
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        // The strip's own double click zooms the window; a tab's keeps it.
                        cx.stop_propagation();
                        if event.click_count() == 2
                            && let Some(pane) = this.pane_mut(pane_id)
                        {
                            pane.keep(item_id);
                        }
                        this.activate_in(pane_id, index, true, window, cx)
                    }))
                    // A press on a tab starts a drag of the tab, never a window move.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| this.press_on_tab = true),
                    )
                    // As in Zed, a middle click closes a tab that is not pinned.
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, window, cx| {
                            if !pinned {
                                this.close_in(pane_id, index, window, cx);
                            }
                        }),
                    )
                    .on_drag(
                        DraggedTab {
                            item: item_id,
                            title: item.tab_title(cx),
                            icon: item.tab_icon(cx),
                        },
                        |tab, _, _, cx| cx.new(|_| tab.clone()),
                    )
                    .drag_over::<DraggedTab>(move |style, _, _, _| style.bg(drop_bg))
                    .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                        this.drop_tab(dragged, pane_id, Some(index), window, cx)
                    }));
                let workspace = workspace.clone();
                let focus = item.focus_handle(cx);
                right_click_menu(SharedString::from(format!("tab-menu-{pane_id}-{index}")))
                    .trigger(move |_, _, _| tab)
                    .menu(move |window, cx| {
                        let (workspace, focus) = (workspace.clone(), focus.clone());
                        ContextMenu::build(window, cx, move |menu, _, cx| {
                            tab_menu(menu.context(focus), workspace, item_id, cx)
                        })
                    })
            }))
            // The strip after the tabs: a dropped tab goes to the end, as Zed's.
            .child(
                div()
                    .id(SharedString::from(format!("tabs-end-{pane_id}")))
                    .flex_1()
                    // A strip full of tabs keeps a gutter to drop on.
                    .min_w(px(studio_ui::ds::size::GUTTER))
                    .h_full()
                    .border_b_1()
                    .border_color(hairline)
                    .drag_over::<DraggedTab>(move |style, _, _, _| style.bg(drop_bg))
                    .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                        this.drop_tab(dragged, pane_id, None, window, cx)
                    })),
            )
            .into_any_element()
    }

    /// A pane's header strip, as Zed's tab bar starts: Back and Forward for this pane, then its
    /// tabs. The top-left pane makes room for the traffic lights when the sidebar is hidden.
    fn render_pane_header(&self, pane_id: PaneId, cx: &mut Context<Self>) -> impl IntoElement {
        let header_bg = studio_ui::ds::surface::header_strip(cx);
        let hairline = studio_ui::ds::surface::rule(cx);
        let top_left = self.center.top_left() == pane_id;
        let zoomed = self.zoomed == Some(Zoomed::Pane(pane_id));
        let (can_back, can_forward) = self.pane(pane_id).map_or((false, false), |pane| {
            (pane.can_step(true), pane.can_step(false))
        });
        let nav_button = |back: bool, cx: &mut Context<Self>| {
            let (icon, label, enabled): (_, &'static str, _) = if back {
                (IconName::ArrowLeft, "Go back", can_back)
            } else {
                (IconName::ArrowRight, "Go forward", can_forward)
            };
            IconButton::new(
                SharedString::from(format!(
                    "{}-{pane_id}",
                    if back { "go-back" } else { "go-forward" }
                )),
                icon,
            )
            .disabled(!enabled)
            .tooltip(move |_, cx| {
                if back {
                    Tooltip::for_action(label, &GoBack, cx)
                } else {
                    Tooltip::for_action(label, &GoForward, cx)
                }
            })
            .on_click(
                cx.listener(move |this, _, window, cx| this.navigate(pane_id, back, window, cx)),
            )
        };
        let nav = h_flex()
            .h_full()
            .flex_none()
            .px(px(studio_ui::ds::space::SM))
            .gap(px(studio_ui::ds::space::XS))
            .items_center()
            .border_b_1()
            .border_r_1()
            .border_color(hairline)
            .child(nav_button(true, cx))
            .child(nav_button(false, cx));
        self.strip(
            SharedString::from(format!("pane-header-{pane_id}")),
            HEADER_HEIGHT,
            cx,
        )
        .debug_selector(move || format!("pane-header-{pane_id}"))
        .bg(header_bg)
        .when_some(
            (zoomed || (top_left && (!self.left.visible() || self.session.is_none())))
                .then(|| self.lights_room(zoomed, cx))
                .flatten(),
            |strip, room| {
                strip.child(
                    div()
                        .w(px(room))
                        .h_full()
                        .flex_none()
                        .border_b_1()
                        .border_color(hairline),
                )
            },
        )
        .child(nav)
        .child(self.render_tabs(pane_id, cx))
        .children(
            (pane_id == self.active_pane && self.session.is_some())
                .then(|| self.render_pane_buttons(pane_id, cx)),
        )
    }

    /// The end of the active pane's tab strip, as Zed's tab bar ends: a new-tab menu, a split
    /// menu and zoom.
    fn render_pane_buttons(&self, pane_id: PaneId, cx: &mut Context<Self>) -> impl IntoElement {
        let hairline = studio_ui::ds::surface::rule(cx);
        let zoomed = self.zoomed == Some(Zoomed::Pane(pane_id));
        let menu_button = |id: &'static str, icon: IconName, tooltip: &'static str| {
            IconButton::new(SharedString::from(format!("{id}-{pane_id}")), icon)
                .tooltip(Tooltip::text(tooltip))
        };
        h_flex()
            .h_full()
            .flex_none()
            .px(px(studio_ui::ds::space::SM))
            .gap(px(studio_ui::ds::space::XS))
            .items_center()
            .border_b_1()
            .border_l_1()
            .border_color(hairline)
            .child(
                PopoverMenu::new(SharedString::from(format!("new-tab-menu-{pane_id}")))
                    .anchor(gpui::Anchor::TopRight)
                    .trigger(menu_button("new-tab", IconName::Plus, "New…"))
                    .menu(|window, cx| {
                        Some(ContextMenu::build(window, cx, |menu, _, _| {
                            menu.action("New Query", Box::new(studio_actions::graph::NewQuery))
                                .action(
                                    "Open Query File…",
                                    Box::new(studio_actions::graph::OpenQueryFile::default()),
                                )
                                .separator()
                                .action(
                                    "Go to Type or Node…",
                                    Box::new(studio_actions::workspace::GoTo),
                                )
                                .action("History", Box::new(studio_actions::graph::OpenHistory))
                        }))
                    }),
            )
            .child(
                PopoverMenu::new(SharedString::from(format!("split-menu-{pane_id}")))
                    .anchor(gpui::Anchor::TopRight)
                    .trigger(menu_button("split", IconName::Split, "Split pane"))
                    .menu(|window, cx| {
                        Some(ContextMenu::build(window, cx, |menu, _, _| {
                            menu.action("Split Right", Box::new(SplitRight))
                                .action("Split Left", Box::new(SplitLeft))
                                .action("Split Up", Box::new(SplitUp))
                                .action("Split Down", Box::new(SplitDown))
                        }))
                    }),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("zoom-pane-{pane_id}")),
                    IconName::Maximize,
                )
                .toggle_state(zoomed)
                .selected_icon(IconName::Minimize)
                .tooltip(move |_, cx| {
                    Tooltip::for_action(
                        if zoomed { "Zoom out" } else { "Zoom in" },
                        &ToggleZoom,
                        cx,
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.zoomed = if zoomed {
                        None
                    } else {
                        Some(Zoomed::Pane(pane_id))
                    };
                    cx.notify();
                })),
            )
    }

    /// One pane: its header and its active tab. Pressing anywhere in it makes it the active
    /// pane.
    fn render_pane(
        &self,
        pane_id: PaneId,
        floor: SheetFloor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_item = self
            .pane(pane_id)
            .and_then(|pane| pane.active_item())
            .is_some();
        let body = match self.pane(pane_id).and_then(|pane| pane.active_item()) {
            // Cached, as Zed caches each pane: the tab's view is drawn again only when it
            // notifies, so a hover or a toast elsewhere replays it from the last frame. A tab
            // that caches its own costly part is drawn uncached, or that part would be drawn
            // again with it.
            Some(item) => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(if item.caches_its_parts(cx) {
                    item.to_any().into_any_element()
                } else {
                    item.to_any().cached(cached_fill()).into_any_element()
                })
                .into_any_element(),
            None if self.panes.len() == 1 => self.render_empty_center(cx).into_any_element(),
            None => studio_ui::ds::empty_state(IconName::Sparkle, "No tab open").into_any_element(),
        };
        // A tab dragged over the content: near an edge it splits the pane that way, as Zed's
        // drop targets (a fifth of the shorter side); elsewhere it moves into this pane.
        let drop_bg = studio_ui::ds::surface::drop_target(cx);
        let overlay = self
            .drop_target
            .filter(|(target, _)| *target == pane_id && cx.has_active_drag())
            .map(|(_, direction)| {
                let area = div().absolute().bg(drop_bg);
                match direction {
                    None => area.inset_0(),
                    Some(SplitDirection::Left) => area.top_0().bottom_0().left_0().w_1_2(),
                    Some(SplitDirection::Right) => area.top_0().bottom_0().right_0().w_1_2(),
                    Some(SplitDirection::Up) => area.left_0().right_0().top_0().h_1_2(),
                    Some(SplitDirection::Down) => area.left_0().right_0().bottom_0().h_1_2(),
                }
            });
        // Every tab is a document on the document's fill, which the pane paints, not the view.
        let corner = px(studio_ui::ds::radii(cx).sheet());
        let body = div()
            .id(SharedString::from(format!("pane-body-{pane_id}")))
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .relative()
            .when(has_item, |body| {
                body.bg(studio_ui::ds::surface::document(cx))
                    .when(floor.bottom, |body| body.pb(corner))
                    .when(floor.bottom && floor.left, |body| body.rounded_bl(corner))
                    .when(floor.bottom && floor.right, |body| body.rounded_br(corner))
            })
            .child(body)
            .children(overlay)
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedTab>, _, cx| {
                    let inside = event.bounds.contains(&event.event.position);
                    let target = if inside {
                        Some((pane_id, drop_edge(event.bounds, event.event.position)))
                    } else if this
                        .drop_target
                        .is_some_and(|(target, _)| target == pane_id)
                    {
                        None
                    } else {
                        return;
                    };
                    if this.drop_target != target {
                        this.drop_target = target;
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                let direction = this.drop_target.take().and_then(|(_, direction)| direction);
                match direction {
                    Some(direction) => this.drop_split(dragged, pane_id, direction, window, cx),
                    None => this.drop_tab(dragged, pane_id, None, window, cx),
                }
            }));
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
        floor: SheetFloor,
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
            // A zoomed pane is drawn over the document area instead (`render_zoomed`).
            if self.zoomed == Some(Zoomed::Pane(*id)) {
                return div().size_full().into_any_element();
            }
            return self.render_pane(*id, floor, cx);
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
        let count = members.len();
        for (index, (child, flex)) in members.iter().zip(flexes).enumerate() {
            let mut child_path = path.clone();
            child_path.push(index);
            let child_floor = floor.member(axis, index, count);
            let mut cell = div()
                .relative()
                .flex()
                .flex_col()
                .min_w_0()
                .min_h_0()
                .flex_basis(px(0.))
                .flex_grow(*flex)
                .flex_shrink(1.)
                .child(self.render_member(child, child_path, child_floor, cx));
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
        let path_for_drag = path;
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

    /// The window's status bar, after Zed's: the left dock's panel buttons, then where the open
    /// tab reads and writes (the server, the graph, the branch, a menu each) and the tab's own
    /// facts; at the right, what is running, the tab's controls, Studio's memory and CPU, then
    /// the bottom and right docks' panel buttons.
    fn render_status_bar(
        &self,
        footer: Option<ItemFooter>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_background = studio_ui::ds::surface::status_bar(cx);
        let glass = studio_ui::ds::is_glass(cx);
        let hairline = studio_ui::ds::surface::rule(cx);
        let mut footer = footer.unwrap_or_default();
        let keys = std::mem::take(&mut footer.keys);
        let has_start = !footer.start.is_empty();
        let shown = self.session.is_some();
        let running = self.session.as_ref().and_then(|session| {
            let s = session.read(cx);
            match (s.reads_in_flight, s.writes_in_flight) {
                (0, 0) => None,
                (reads, 0) => Some(format!(
                    "{reads} quer{} running",
                    if reads == 1 { "y" } else { "ies" }
                )),
                (_, writes) => Some(format!("Writing ({writes})…")),
            }
        });
        let left = h_flex()
            .min_w_0()
            .gap(px(studio_ui::ds::space::XS))
            .items_center()
            .overflow_hidden()
            .when(shown && !self.left.panels.is_empty(), |bar| {
                bar.child(self.render_sidebar_toggle())
                    .child(studio_ui::ds::divider_v(cx))
                    .child(self.render_panel_buttons(PanelPosition::Left, cx))
                    .child(studio_ui::ds::divider_v(cx))
            })
            .child(self.render_server_switcher(cx))
            .children(self.render_graph_and_branch(cx))
            .when(has_start, |bar| bar.child(studio_ui::ds::divider_v(cx)))
            .children(footer.start);
        let docks_right = shown && !(self.bottom.panels.is_empty() && self.right.panels.is_empty());
        let has_end = !footer.end.is_empty();
        let performance = self.render_performance();
        // Zed's 4 px between the left side's items, which are chips with padding of their own;
        // the right side's are bare text, so they keep 8 px, and a rule sets the window's
        // readout apart from the tab's facts.
        let right = h_flex()
            .flex_none()
            .gap(px(studio_ui::ds::space::MD))
            .items_center()
            .children(running.map(|text| {
                h_flex()
                    .gap(px(studio_ui::ds::space::XS))
                    .items_center()
                    .flex_none()
                    .child(studio_ui::ds::spinner())
                    .child(studio_ui::ds::text::meta(text).single_line())
            }))
            .children(footer.end)
            // The tab's keys, behind one button: the bar names facts, not shortcuts.
            .when(!keys.is_empty(), |bar| {
                bar.child(
                    PopoverMenu::new("keys-panel")
                        .anchor(gpui::Anchor::BottomRight)
                        .attach(gpui::Anchor::TopRight)
                        .trigger_with_tooltip(
                            IconButton::new("keys", IconName::Keyboard),
                            Tooltip::text("Keys for this tab"),
                        )
                        // Zed's context menu: each key is an entry that runs it, its binding
                        // read in the tab's own context.
                        .menu(move |window, cx| {
                            let keys = keys.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                if let Some(first) = keys.first() {
                                    menu = menu.context(first.focus.clone());
                                }
                                menu = menu.header("Keys");
                                for key in &keys {
                                    menu = menu.action(key.label.clone(), key.action.boxed_clone());
                                }
                                menu
                            }))
                        }),
                )
            })
            .when(has_end && performance.is_some(), |bar| {
                bar.child(studio_ui::ds::divider_v(cx))
            })
            .children(performance)
            .when(docks_right, |bar| {
                bar.child(studio_ui::ds::divider_v(cx))
                    .child(self.render_panel_buttons(PanelPosition::Bottom, cx))
                    .child(self.render_panel_buttons(PanelPosition::Right, cx))
            });
        h_flex()
            .id("status-bar")
            .debug_selector(|| "status-bar".into())
            .h(px(STATUS_BAR_HEIGHT))
            .w_full()
            .flex_none()
            .px(px(if glass {
                studio_ui::ds::size::SHEET_GAP
            } else {
                studio_ui::ds::space::XS
            }))
            .gap(px(studio_ui::ds::space::MD))
            .items_center()
            .justify_between()
            .when(!glass, |bar| {
                bar.bg(status_background)
                    .border_t_1()
                    .border_color(hairline)
            })
            .child(left)
            .child(right)
    }

    /// The graph the open tab reads, and its branch: a chip that opens another graph, then the
    /// branch menu.
    fn render_graph_and_branch(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let session = self.session.as_ref()?;
        let s = session.read(cx);
        let graph = s.graph_id.clone();
        let branch = s.branch.clone();
        let unavailable = s.unavailable.clone();
        let session_for_menu = session.clone();
        Some(
            h_flex()
                .gap(px(studio_ui::ds::space::XS))
                .items_center()
                .flex_none()
                .child(
                    h_flex()
                        .id("footer-graph")
                        .h(px(studio_ui::ds::size::BAR_BUTTON))
                        // A 22 px button's inset, so it reads as the branch button beside it.
                        .px(px(studio_ui::ds::space::MD))
                        .gap(px(studio_ui::ds::space::XS))
                        .items_center()
                        .rounded(px(studio_ui::ds::radii(cx).sm))
                        .cursor_pointer()
                        .hover(|style| style.bg(studio_ui::ds::surface::highlight(cx)))
                        .map(|button| match unavailable.clone() {
                            Some(reason) => button.tooltip(Tooltip::text(format!(
                                "This graph is unavailable: {reason}. Reads resume when it is \
                                 ready."
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
                            // As a button's icon: 80 % of the label's ink.
                            studio_ui::ds::icon::meta(IconName::DatabaseZap)
                                .color(studio_ui::ds::text::icon_ink(cx)),
                        )
                        .when(unavailable.is_some(), |button| {
                            button.child(Indicator::dot().color(Color::Warning))
                        })
                        .child(studio_ui::ds::text::body(graph).single_line())
                        .on_click(|_, window, cx| {
                            window
                                .dispatch_action(Box::new(studio_actions::workspace::OpenGraph), cx)
                        }),
                )
                .child(studio_ui::ds::text::body("/").color(Color::Disabled))
                .child(
                    PopoverMenu::new("branch-menu")
                        .anchor(gpui::Anchor::BottomLeft)
                        .trigger(
                            studio_ui::ds::bar_button("branch", branch.clone())
                                .style(ButtonStyle::Subtle)
                                .tooltip(Tooltip::text(format!(
                                    "Reads and edits in this window use {branch}. Switch branch."
                                )))
                                .start_icon(Icon::new(IconName::GitBranch))
                                .end_icon(Icon::new(IconName::ChevronUpDown)),
                        )
                        .menu(move |window, cx| {
                            let session = session_for_menu.clone();
                            // Read when the menu opens, not copied at every frame drawn.
                            let branches = session
                                .read(cx)
                                .branches
                                .loaded()
                                .cloned()
                                .unwrap_or_default();
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
                ),
        )
    }

    /// The document column: its panes, and the bottom dock when it sits under the document
    /// alone (`studio.bottom_dock_layout` "contained").
    fn render_center(
        &self,
        bottom: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let editor_background = studio_ui::ds::surface::document(cx);
        let center = self.center.clone();
        let glass = studio_ui::ds::is_glass(cx);
        // On glass the panes along the sheet's bottom meet its rounded corners, unless the
        // bottom dock sits under them inside the sheet.
        let floor = SheetFloor {
            bottom: glass && bottom.is_none(),
            left: true,
            right: true,
        };
        let body = div().flex_1().min_h_0().flex().child(self.render_member(
            &center,
            Vec::new(),
            floor,
            cx,
        ));
        let column = if glass {
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
            .children(bottom)
    }

    /// The bottom dock: its panel's header, then the panel. `alone` when it is not inside the
    /// document column, so on glass it is a sheet of its own.
    fn render_bottom_dock(&self, alone: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.bottom.visible() || self.zoomed == Some(Zoomed::Dock(PanelPosition::Bottom)) {
            return None;
        }
        let panel = self.bottom.active_panel()?;
        let glass = studio_ui::ds::is_glass(cx);
        let hairline = studio_ui::ds::surface::rule(cx);
        let panel_background = studio_ui::ds::surface::chrome(cx);
        let column = if glass && alone {
            studio_ui::ds::sheet(cx)
        } else {
            div()
        };
        Some(
            column
                .id("bottom-dock")
                .debug_selector(|| "bottom-dock".into())
                .flex()
                .flex_col()
                .h(px(self.bottom.size))
                .w_full()
                .relative()
                .flex_none()
                .overflow_hidden()
                .when(!glass, |d| d.bg(panel_background))
                .when(!(glass && alone), |d| d.border_t_1().border_color(hairline))
                .child(self.render_dock_header(PanelPosition::Bottom, cx))
                .child(div().flex_1().min_h_0().child(self.panel_body(panel)))
                .child(resize_handle(PanelPosition::Bottom))
                .into_any_element(),
        )
    }

    /// A right or bottom dock's header strip: its panel's title and buttons, then zoom and
    /// hide.
    fn render_dock_header(&self, position: PanelPosition, cx: &mut Context<Self>) -> Stateful<Div> {
        let hairline = studio_ui::ds::surface::rule(cx);
        let dock = match position {
            PanelPosition::Left => &self.left,
            PanelPosition::Right => &self.right,
            PanelPosition::Bottom => &self.bottom,
        };
        let zoomed = self.zoomed == Some(Zoomed::Dock(position));
        let hide_label = match position {
            PanelPosition::Left => "Hide sidebar",
            PanelPosition::Right => "Hide inspector",
            PanelPosition::Bottom => "Hide bottom panel",
        };
        self.strip(
            SharedString::from(format!("dock-header-{position:?}")),
            HEADER_HEIGHT,
            cx,
        )
        // The title on the edge the panel's own toolbar and rows start at.
        .pl(px(studio_ui::ds::space::EDGE))
        .pr(px(studio_ui::ds::space::SM))
        .gap(px(studio_ui::ds::space::XS))
        .border_b_1()
        .border_color(hairline)
        .when_some(
            zoomed.then(|| self.lights_room(true, cx)).flatten(),
            |strip, room| strip.child(div().w(px(room)).flex_none()),
        )
        .children(dock.active_panel().map(dock_title))
        .child(
            IconButton::new(
                SharedString::from(format!("zoom-dock-{position:?}")),
                if zoomed {
                    IconName::Minimize
                } else {
                    IconName::Maximize
                },
            )
            // ⇧⎋ ends any zoom but zooms the active pane, not this dock, so only "Zoom out"
            // shows it.
            .tooltip(move |_, cx| {
                if zoomed {
                    Tooltip::for_action("Zoom out", &ToggleZoom, cx)
                } else {
                    Tooltip::simple("Zoom in", cx)
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.zoomed = if zoomed {
                    None
                } else {
                    Some(Zoomed::Dock(position))
                };
                cx.notify();
            })),
        )
        .child(
            IconButton::new(
                SharedString::from(format!("hide-dock-{position:?}")),
                IconName::Close,
            )
            .tooltip(move |_, cx| {
                Tooltip::for_action(hide_label, toggle_action(position).as_ref(), cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.set_dock_open(position, false, window, cx)
            })),
        )
    }

    /// A panel's content, drawn as it was made for: a side panel in its scrolling host, a
    /// bottom panel cached at the dock's size (it scrolls itself), whichever dock it is in now.
    /// A sidebar panel's list starts below its header by the inset its selected row has from
    /// the column's sides.
    fn panel_body(&self, panel: &PanelEntry) -> AnyElement {
        match panel.home {
            PanelPosition::Bottom => panel.view.clone().cached(cached_fill()).into_any_element(),
            PanelPosition::Left => div()
                .size_full()
                .pt(px(studio_ui::ds::space::SM))
                .child(self.hosted(&panel.view))
                .into_any_element(),
            PanelPosition::Right => self.hosted(&panel.view),
        }
    }

    /// What is zoomed, over the document area with Zed's inset: a pane with its tabs, or a
    /// dock's panel with its header.
    fn render_zoomed(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let content = match self.zoomed? {
            // The zoomed card is the document's fill itself, so nothing shows at its corners.
            Zoomed::Pane(id) => self.render_pane(id, SheetFloor::default(), cx),
            Zoomed::Dock(position) => {
                let dock = match position {
                    PanelPosition::Left => &self.left,
                    PanelPosition::Right => &self.right,
                    PanelPosition::Bottom => &self.bottom,
                };
                let panel = dock.active_panel()?;
                v_flex()
                    .size_full()
                    .child(self.render_dock_header(position, cx))
                    .child(div().flex_1().min_h_0().child(self.panel_body(panel)))
                    .into_any_element()
            }
        };
        Some(
            v_flex()
                .id("zoomed")
                .debug_selector(|| "zoomed".into())
                .absolute()
                .inset(px(ZOOM_INSET))
                .bg(studio_ui::ds::surface::document(cx))
                .border_1()
                .border_color(studio_ui::ds::surface::floating_border(cx))
                .rounded(px(studio_ui::ds::radii(cx).sheet()))
                .shadow(studio_ui::ds::elevation::float(cx))
                .overflow_hidden()
                .occlude()
                .child(content)
                .into_any_element(),
        )
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
                .start_icon(Icon::new(icon))
                .key_binding(binding)
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        };
        if self.session.is_none() {
            return studio_ui::ds::empty_state(IconName::Server, "No graph open")
                .child(
                    div()
                        .w(px(studio_ui::ds::size::STATE_ACTIONS))
                        .child(action_button(
                            "open-graph",
                            "Open a graph",
                            IconName::FolderOpen,
                            Box::new(studio_actions::workspace::OpenGraph),
                            cx,
                        )),
                )
                .into_any_element();
        }
        studio_ui::ds::empty_state(IconName::Sparkle, "No tab open")
            .child(
                v_flex()
                    .w(px(studio_ui::ds::size::STATE_ACTIONS))
                    .gap(px(studio_ui::ds::space::XS))
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
    /// The right dock: its panel's header, then the panel.
    fn render_right_dock(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.right.visible() || self.zoomed == Some(Zoomed::Dock(PanelPosition::Right)) {
            return None;
        }
        let panel = self.panel_body(self.right.active_panel()?);
        let editor_background = studio_ui::ds::surface::document(cx);
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
                .child(self.render_dock_header(PanelPosition::Right, cx))
                .child(div().flex_1().min_h_0().child(panel)),
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
            PanelPosition::Bottom => self
                .bottom
                .resize(height - gap - STATUS_BAR_HEIGHT - y, height),
        }
        cx.notify();
    }

    fn render_toasts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Under the header strips, so a toast never covers a document's footer or composer.
        v_flex()
            .absolute()
            .top(px(HEADER_HEIGHT + studio_ui::ds::space::MD))
            .right(px(studio_ui::ds::space::LG))
            .gap(px(studio_ui::ds::space::MD))
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
                    .gap(px(studio_ui::ds::space::MD))
                    .px(px(studio_ui::ds::space::LG))
                    .py(px(studio_ui::ds::space::MD))
                    .max_w(px(studio_ui::ds::size::POPOVER_MAX))
                    .rounded(px(studio_ui::ds::radii(cx).md))
                    .map(|toast| studio_ui::ds::toast_surface(toast, cx))
                    .child(Indicator::dot().color(color))
                    // `min_w_0` lets the text wrap inside the toast instead of widening it.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(studio_ui::ds::text::body(toast.message.clone())),
                    )
                    .children(toast.action.as_ref().map(|(label, action)| {
                        let action = action.boxed_clone();
                        studio_ui::ds::button(("toast-action", id), label.clone())
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.dismiss_toast(id, cx);
                                window.dispatch_action(action.boxed_clone(), cx);
                            }))
                    }))
                    .child(
                        studio_ui::ds::row_icon_button(("dismiss-toast", id), IconName::Close)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.dismiss_toast(id, cx)),
                            ),
                    )
            }))
    }
}

/// The server and graph a session's saved state is kept under.
fn graph_key_of(session: &Entity<GraphSession>, cx: &App) -> (String, String) {
    let session = session.read(cx);
    let url = session.connection.read(cx).server.url.clone();
    (url, session.graph_id.clone())
}

/// The button on a committed write's toast: Undo for a delete Studio can put back (History
/// still holds the commit), else the commit.
fn committed_follow_up(
    entry: &graph_store::OutcomeEntry,
    commit: &str,
) -> (&'static str, Box<dyn gpui::Action>) {
    match entry.follow_up {
        Some(graph_store::FollowUp::UndoDelete) => (
            "Undo",
            Box::new(studio_actions::graph::UndoDelete {
                commit: commit.to_owned(),
                branch: entry.branch.clone(),
                what: entry.description.clone(),
            }),
        ),
        None => (
            "View commit",
            Box::new(OpenCommit {
                commit_id: commit.to_owned(),
            }),
        ),
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
        // Every change to tabs or panels re-renders the workspace, so this sees all of them. The
        // workspace also renders whenever anything inside it notifies (a streamed reply, a
        // hover), so the state is compared at most every `SAVE_CHECK`, not on every frame.
        if self._save_check.is_none() {
            self._save_check = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SAVE_CHECK).await;
                this.update(cx, |this, cx| {
                    this._save_check = None;
                    this.save_if_changed(cx);
                })
                .ok();
            }));
        }
        // As Zed's workspace does: the window's rem size is the UI font size, which every `ui`
        // size is relative to, and the root carries the UI font.
        let ui_font = theme_settings::setup_ui_font(window, cx);
        let mut root = v_flex()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .font(ui_font)
            .text_size(studio_ui::ds::text::plain_size(cx))
            .map(|root| {
                if studio_ui::ds::is_glass(cx) {
                    // The page, then the wash painted once behind everything.
                    root.bg(studio_ui::ds::surface::page(cx))
                        .child(studio_ui::ds::wash())
                } else {
                    root.bg(studio_ui::ds::surface::window(cx))
                }
            })
            .text_color(Color::Default.color(cx))
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
            .on_action(cx.listener(|this, _: &SplitLeft, window, cx| {
                this.split(SplitDirection::Left, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SplitUp, window, cx| {
                this.split(SplitDirection::Up, window, cx)
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
                |this, action: &studio_actions::workspace::ActivatePanel, window, cx| {
                    this.show_panel(&action.id, window, cx)
                },
            ))
            .on_action(cx.listener(|this, _: &GoBack, window, cx| {
                this.navigate(this.active_pane, true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoForward, window, cx| {
                this.navigate(this.active_pane, false, window, cx)
            }))
            .on_drag_move(cx.listener(Self::resize_dock))
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| {
                this.set_dock_open(PanelPosition::Left, !this.left.open, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleInspector, window, cx| {
                this.set_dock_open(PanelPosition::Right, !this.right.open, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleActivity, window, cx| {
                this.set_dock_open(PanelPosition::Bottom, !this.bottom.open, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleZoom, window, cx| this.toggle_zoom(window, cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                if let Some(session) = &this.session {
                    session.update(cx, |s, cx| s.reload(cx));
                }
            }));
        for registration in self.actions.clone() {
            root = registration(root, window, cx);
        }
        // The modal over a dim (a dialog) or a light veil (the palette); a click on either
        // closes it. A dialog sits a quarter of the way down the space it leaves free, as
        // Tower's `1fr auto 3fr` rows place it; the palette a tenth of the window down.
        let viewport = window.viewport_size();
        let modal = self.modal.as_ref().map(|modal| {
            let palette = modal.placement == ModalPlacement::Palette;
            let card = div()
                .flex_none()
                .min_h_0()
                .max_w_full()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(modal.view.clone());
            deferred(
                v_flex()
                    .absolute()
                    .size_full()
                    .top_0()
                    .left_0()
                    .items_center()
                    .px(px(studio_ui::ds::space::EDGE))
                    .py(px(studio_ui::ds::space::EDGE))
                    .bg(if palette {
                        studio_ui::ds::surface::veil(cx)
                    } else {
                        studio_ui::ds::surface::backdrop(cx)
                    })
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.hide_modal(window, cx);
                        }),
                    )
                    .map(|layer| {
                        if palette {
                            layer.pt(viewport.height * 0.1).child(card)
                        } else {
                            let mut below = div().flex_1();
                            below.style().flex_grow = Some(3.);
                            layer.child(div().flex_1()).child(card).child(below)
                        }
                    }),
            )
            .with_priority(1)
        });
        // Columns, each with its own header strip, over one status bar across the window, as
        // Zed lays out its docks over its status bar. Glass: the sidebar, the document and the
        // inspector are three sheets with one gap around and between them, over the bar on the
        // wash. Flat: columns split by hairlines.
        let glass = studio_ui::ds::is_glass(cx);
        let sidebar_shown = self.left.visible()
            && self.session.is_some()
            && self.zoomed != Some(Zoomed::Dock(PanelPosition::Left));
        let gap = px(studio_ui::ds::size::SHEET_GAP);
        self.lights_shown =
            cfg!(target_os = "macos") && !window.is_fullscreen() && !window.is_simple_fullscreen();
        let lights = traffic_light_position(glass);
        if self.traffic_lights != Some(lights) {
            self.traffic_lights = Some(lights);
            // Only macOS draws traffic lights; GPUI has the call on macOS alone.
            #[cfg(target_os = "macos")]
            window.set_traffic_light_position(lights);
        }
        let footer = self
            .pane(self.active_pane)
            .and_then(|pane| pane.active_item())
            .and_then(|item| item.footer(window, cx));
        // Where the bottom dock spans, as Zed's `bottom_dock_layout`.
        let layout = cx
            .try_global::<StudioSettings>()
            .map(|settings| settings.studio.bottom_dock_layout)
            .unwrap_or_default();
        let contained = layout == BottomDockLayout::Contained;
        let bottom = self.render_bottom_dock(!contained, cx);
        let sidebar = sidebar_shown.then(|| self.render_sidebar(cx).into_any_element());
        let right = self
            .render_right_dock(cx)
            .map(IntoElement::into_any_element);
        let row = || {
            h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .when(glass, |d| d.gap(gap))
        };
        let column = || {
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .when(glass, |d| d.gap(gap))
        };
        let docks = match layout {
            BottomDockLayout::Contained => row()
                .children(sidebar)
                .child(self.render_center(bottom, cx))
                .children(right),
            BottomDockLayout::Full => row().child(
                column()
                    .child(
                        row()
                            .children(sidebar)
                            .child(self.render_center(None, cx))
                            .children(right),
                    )
                    .children(bottom),
            ),
            BottomDockLayout::LeftAligned => row()
                .child(
                    column()
                        .child(row().children(sidebar).child(self.render_center(None, cx)))
                        .children(bottom),
                )
                .children(right),
            BottomDockLayout::RightAligned => row().children(sidebar).child(
                column()
                    .child(row().child(self.render_center(None, cx)).children(right))
                    .children(bottom),
            ),
        };
        root.child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .relative()
                .when(glass, |d| d.p(gap).pb_0())
                .child(docks)
                .children(self.render_zoomed(cx)),
        )
        .child(self.render_status_bar(footer, cx))
        .child(self.render_toasts(cx))
        .children(modal)
    }
}

/// A dock's header for its active panel, as a Zed panel draws its own: the panel's title, then
/// its buttons (search, new).
fn dock_title(panel: &PanelEntry) -> impl IntoElement {
    h_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .gap(px(studio_ui::ds::space::SM))
        .items_center()
        .child(
            studio_ui::ds::text::body(panel.title.clone())
                .single_line()
                .truncate(),
        )
        .child(div().flex_1())
        .children(panel.actions.clone())
}

/// Room for the traffic lights at the start of a header strip at the window's top left; a
/// zoomed one starts inside the zoom's inset, the lights where the window style puts them.
/// `None` without lights, as Zed's title bar: off macOS, and in full screen.
fn lights_room(shown: bool, zoomed: bool, cx: &App) -> Option<f32> {
    if !shown {
        return None;
    }
    if !zoomed {
        return Some(TRAFFIC_LIGHTS_WIDTH);
    }
    let gap = if studio_ui::ds::is_glass(cx) {
        studio_ui::ds::size::SHEET_GAP
    } else {
        0.
    };
    Some(TRAFFIC_LIGHTS_WIDTH + gap - ZOOM_INSET)
}

/// The dock a panel goes in: where the settings place it, else its own.
fn placed(id: &str, home: PanelPosition, cx: &App) -> PanelPosition {
    cx.try_global::<StudioSettings>()
        .and_then(|settings| settings.panels.get(id))
        .and_then(|placement| placement.dock)
        .map_or(home, PanelPosition::from_side)
}

/// The action that shows or hides a dock.
fn toggle_action(position: PanelPosition) -> Box<dyn Action> {
    match position {
        PanelPosition::Left => Box::new(ToggleSidebar),
        PanelPosition::Right => Box::new(ToggleInspector),
        PanelPosition::Bottom => Box::new(ToggleActivity),
    }
}

/// Where a pane meets the bottom of the document sheet on glass. GPUI clips a child to the
/// sheet's rectangle, not its rounded corners, so the pane rounds its document fill into the
/// corners it reaches and keeps its tab's content a corner's radius above the sheet's edge,
/// where a view's own fill (an editor's, a table's) cannot square the corner.
#[derive(Clone, Copy, Default)]
struct SheetFloor {
    bottom: bool,
    left: bool,
    right: bool,
}

impl SheetFloor {
    /// The floor of the `index`th of `count` members split along `axis`.
    fn member(self, axis: SplitAxis, index: usize, count: usize) -> Self {
        let last = index + 1 == count;
        match axis {
            SplitAxis::Horizontal => Self {
                bottom: self.bottom,
                left: self.left && index == 0,
                right: self.right && last,
            },
            SplitAxis::Vertical => Self {
                bottom: self.bottom && last,
                ..self
            },
        }
    }
}

/// Whether the first tab of a pane sits in the document sheet's rounded corner: on glass, in
/// the top-left pane, unless the traffic lights' space comes first (`render_pane_header`: the
/// sidebar hidden, or no graph open). GPUI clips a sheet's children to its rectangle, not its
/// corners, so that tab takes the sheet's radius or its shade shows outside the curve.
fn first_tab_in_sheet_corner(
    glass: bool,
    top_left_pane: bool,
    sidebar_shown: bool,
    graph_open: bool,
) -> bool {
    glass && top_left_pane && sidebar_shown && graph_open
}

/// How often the footer's memory and CPU are read.
const PERFORMANCE_INTERVAL: Duration = Duration::from_secs(2);

/// The last reading of Studio's memory and CPU, and the CPU share since the one before.
struct Performance {
    sample: process_stats::Sample,
    cpu: Option<f32>,
    /// The window went to the background after this reading, so the next one does not measure
    /// from it.
    paused: bool,
}

impl Performance {
    /// The reading `sample` makes after `last`: CPU since `last`, or, after a pause, the figure
    /// shown before it, so no reading averages over time in the background.
    fn next(last: Option<&Performance>, sample: process_stats::Sample) -> Self {
        let cpu = match last {
            Some(last) if last.paused => last.cpu,
            Some(last) => sample.cpu_since(&last.sample),
            None => None,
        };
        Self {
            sample,
            cpu,
            paused: false,
        }
    }
}

/// A tab being dragged: to another place in its strip, another pane, or a pane's edge.
#[derive(Clone)]
struct DraggedTab {
    item: EntityId,
    title: SharedString,
    icon: Option<IconName>,
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        studio_ui::ds::drag_preview(cx)
            .children(self.icon.map(studio_ui::ds::icon::meta))
            .child(studio_ui::ds::text::body(self.title.clone()))
    }
}

/// The edge a tab dropped at `point` splits toward: one within a fifth of the shorter side of
/// `bounds`, the nearest; none in the middle, where the tab moves into the pane.
fn drop_edge(bounds: gpui::Bounds<Pixels>, point: Point<Pixels>) -> Option<SplitDirection> {
    let reach = f32::from(bounds.size.width.min(bounds.size.height)) * 0.2;
    let left = f32::from(point.x - bounds.origin.x);
    let top = f32::from(point.y - bounds.origin.y);
    let right = f32::from(bounds.size.width) - left;
    let bottom = f32::from(bounds.size.height) - top;
    [
        (left, SplitDirection::Left),
        (right, SplitDirection::Right),
        (top, SplitDirection::Up),
        (bottom, SplitDirection::Down),
    ]
    .into_iter()
    .filter(|(distance, _)| *distance < reach)
    .min_by(|a, b| a.0.total_cmp(&b.0))
    .map(|(_, direction)| direction)
}

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
            .left(px(-studio_ui::ds::size::RESIZE_HANDLE / 2.))
            .h_full()
            .w(px(studio_ui::ds::size::RESIZE_HANDLE))
            .cursor_col_resize(),
        SplitAxis::Vertical => handle
            .left_0()
            .top(px(-studio_ui::ds::size::RESIZE_HANDLE / 2.))
            .w_full()
            .h(px(studio_ui::ds::size::RESIZE_HANDLE))
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

/// A tab's right-click menu, after Zed's: close variants, pin, then splits. Each entry names the
/// action its key runs, for the hint, and acts on the clicked tab; a close variant with nothing
/// to close is disabled.
fn tab_menu(
    menu: ContextMenu,
    workspace: WeakEntity<Workspace>,
    item: EntityId,
    cx: &App,
) -> ContextMenu {
    let targets = workspace
        .upgrade()
        .and_then(|workspace| {
            let workspace = workspace.read(cx);
            let (pane, index) = workspace.find_item(item)?;
            let pane = workspace.pane(pane)?;
            Some(TabTargets {
                pinned: pane.is_pinned(index),
                others: !pane.others(index).is_empty(),
                right: !pane.to_the_right(index).is_empty(),
                all: !pane.all_unpinned().is_empty(),
            })
        })
        .unwrap_or_default();
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
    let entry = |label: &'static str, action: Option<Box<dyn Action>>, enabled: bool| {
        let entry = ContextMenuEntry::new(label).disabled(!enabled);
        match action {
            Some(action) => entry.action(action),
            None => entry,
        }
    };
    menu.item(ContextMenuItem::Entry(
        entry("Close", Some(Box::new(CloseActiveItem)), true).handler(on(
            |w, pane, index, window, cx| w.close_in(pane, index, window, cx),
        )),
    ))
    .item(ContextMenuItem::Entry(
        entry(
            "Close Others",
            Some(Box::new(CloseOtherItems)),
            targets.others,
        )
        .handler(on(|w, pane, index, window, cx| {
            let ids = w.pane(pane).map(|p| p.others(index)).unwrap_or_default();
            w.close_clean(ids, window, cx);
        })),
    ))
    .item(ContextMenuItem::Entry(
        entry("Close to the Right", None, targets.right).handler(on(
            |w, pane, index, window, cx| {
                let ids = w
                    .pane(pane)
                    .map(|p| p.to_the_right(index))
                    .unwrap_or_default();
                w.close_clean(ids, window, cx);
            },
        )),
    ))
    .item(ContextMenuItem::Entry(
        entry("Close All", Some(Box::new(CloseAllItems)), targets.all).handler(on(
            |w, pane, _, window, cx| {
                let ids = w.pane(pane).map(|p| p.all_unpinned()).unwrap_or_default();
                w.close_clean(ids, window, cx);
            },
        )),
    ))
    .separator()
    .entry(
        if targets.pinned {
            "Unpin Tab"
        } else {
            "Pin Tab"
        },
        Some(Box::new(TogglePinTab)),
        on(|w, pane, index, _, cx| w.toggle_pin(pane, index, cx)),
    )
    .separator()
    .entry(
        "Split Right",
        Some(Box::new(SplitRight)),
        on(|w, pane, index, window, cx| {
            w.activate_in(pane, index, true, window, cx);
            w.split(SplitDirection::Right, window, cx)
        }),
    )
    .entry(
        "Split Down",
        Some(Box::new(SplitDown)),
        on(|w, pane, index, window, cx| {
            w.activate_in(pane, index, true, window, cx);
            w.split(SplitDirection::Down, window, cx)
        }),
    )
}

/// What a tab's menu can act on: whether the tab is pinned, and whether each close variant has
/// a tab to close.
#[derive(Default)]
struct TabTargets {
    pinned: bool,
    others: bool,
    right: bool,
    all: bool,
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
            .w(px(studio_ui::ds::size::RESIZE_HANDLE))
            .cursor_col_resize(),
        PanelPosition::Right => handle
            .top_0()
            .left_0()
            .h_full()
            .w(px(studio_ui::ds::size::RESIZE_HANDLE))
            .cursor_col_resize(),
        PanelPosition::Bottom => handle
            .top_0()
            .left_0()
            .w_full()
            .h(px(studio_ui::ds::size::RESIZE_HANDLE))
            .cursor_row_resize(),
    };
    handle.on_drag(DraggedDock(position), |dock, _, _, cx| {
        cx.stop_propagation();
        cx.new(|_| *dock)
    })
}

/// What a close asked about closes: a tab, the shown graph, the window, or Studio.
#[derive(Debug, Clone, Copy)]
enum Closing {
    Tab(EntityId),
    Graph,
    Window,
    Quit,
}

impl Closing {
    /// How much it closes: each holds the ones before it.
    fn reach(self) -> u8 {
        match self {
            Closing::Tab(_) => 0,
            Closing::Graph => 1,
            Closing::Window => 2,
            Closing::Quit => 3,
        }
    }
}

/// Asks before a close drops unsaved work (constitution IV), naming the tabs that hold it.
struct ConfirmClose {
    workspace: WeakEntity<Workspace>,
    closing: Closing,
    /// The tabs with unsaved work: a tab's own close names the tab.
    unsaved: Vec<SharedString>,
    focus_handle: FocusHandle,
}

/// Tabs a close dialog names before "and N more".
const UNSAVED_NAMED: usize = 8;

impl EventEmitter<DismissEvent> for ConfirmClose {}

impl ModalView for ConfirmClose {}

impl Focusable for ConfirmClose {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ConfirmClose {
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let closing = self.closing;
        cx.emit(DismissEvent);
        self.workspace
            .update(cx, |workspace, cx| match closing {
                Closing::Tab(item_id) => {
                    if let Some((pane, index)) = workspace.find_item(item_id) {
                        // The focus had been the closed tab's: the tab shown next takes it, not
                        // a handle of the closed one the modal would hand it back to.
                        if let Some(modal) = &mut workspace.modal {
                            modal.previous_focus = None;
                        }
                        workspace.force_close(pane, index, window, cx);
                    }
                }
                Closing::Graph => {
                    if let Some(modal) = &mut workspace.modal {
                        modal.previous_focus = None;
                    }
                    workspace.close_graph_asking(false, window, cx);
                }
                // Asked again for a write started since; the unsaved tabs were.
                Closing::Window => match workspace.running_write(cx) {
                    Some(graph) => workspace.show_toast(
                        ToastKind::Error,
                        format!(
                            "A write to {graph} is still running. Close the window when it finishes."
                        ),
                        cx,
                    ),
                    None => {
                        workspace.flush_saves(cx);
                        window.remove_window();
                    }
                },
                Closing::Quit => cx.defer(|cx| quit(true, cx)),
            })
            .ok();
    }

    fn header(&self) -> (SharedString, &'static str) {
        let lost = "Their unsaved changes are lost. A write in flight still finishes; the \
                    activity log keeps its outcome.";
        match self.closing {
            Closing::Tab(_) => (
                format!(
                    "Close {}?",
                    self.unsaved.first().cloned().unwrap_or_default()
                )
                .into(),
                "It has unsaved changes or a write still running. A write in flight still \
                 finishes; the activity log keeps its outcome.",
            ),
            Closing::Graph => ("Close the graph?".into(), lost),
            Closing::Window => ("Close this window?".into(), lost),
            Closing::Quit => ("Quit OmniGraph Studio?".into(), lost),
        }
    }
}

/// Quits Studio, unless a window holds a write still running: quitting would lose its outcome
/// (spec 002), so that window comes forward with a toast saying so. Unless `confirmed`, tabs
/// with unsaved work in any window are named first, in a dialog that quits once confirmed.
pub fn quit(confirmed: bool, cx: &mut App) {
    let windows: Vec<WindowHandle<Workspace>> = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<Workspace>())
        .collect();
    for handle in &windows {
        let running = handle
            .update(cx, |workspace, window, cx| {
                let graph = workspace.running_write(cx)?;
                workspace.show_toast(
                    ToastKind::Error,
                    format!("A write to {graph} is still running. Quit when it finishes."),
                    cx,
                );
                window.activate_window();
                Some(())
            })
            .ok()
            .flatten();
        if running.is_some() {
            return;
        }
    }
    if !confirmed {
        let unsaved: Vec<SharedString> = windows
            .iter()
            .filter_map(|handle| handle.read(cx).ok())
            .flat_map(|workspace| workspace.unsaved_tabs(cx))
            .collect();
        // Asked in the window in front.
        let ask = cx
            .active_window()
            .and_then(|window| window.downcast::<Workspace>())
            .or_else(|| windows.first().copied());
        if !unsaved.is_empty()
            && let Some(handle) = ask
        {
            handle
                .update(cx, |workspace, window, cx| {
                    workspace.confirm_close(Closing::Quit, unsaved, window, cx);
                    window.activate_window();
                })
                .ok();
            return;
        }
    }
    cx.quit();
}

impl Render for ConfirmClose {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        studio_ui::ds::modal(cx)
            .key_context("ConfirmClose")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child({
                let (title, description) = self.header();
                studio_ui::ds::ModalHeader::new(title)
                    .description(SharedString::from(description))
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            })
            .when(!matches!(self.closing, Closing::Tab(_)), |modal| {
                let more = self.unsaved.len().saturating_sub(UNSAVED_NAMED);
                modal.child(
                    studio_ui::ds::modal_body()
                        .children(
                            self.unsaved
                                .iter()
                                .take(UNSAVED_NAMED)
                                .map(|title| studio_ui::ds::text::body(title.clone())),
                        )
                        .when(more > 0, |body| {
                            body.child(studio_ui::ds::text::muted(format!("and {more} more")))
                        }),
                )
            })
            .child(
                studio_ui::ds::modal_footer(cx)
                    .child(
                        studio_ui::ds::Button::new("keep-open", "Keep open")
                            .variant(studio_ui::ds::Variant::Outline)
                            .key_binding(ui::KeyBinding::for_action_in(&menu::Cancel, &focus, cx))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        studio_ui::ds::action_button(
                            "close-anyway",
                            match self.closing {
                                Closing::Quit => "Quit",
                                _ => "Close",
                            },
                        )
                        .key_binding(ui::KeyBinding::for_action_in(&menu::Confirm, &focus, cx))
                        .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                    ),
            )
    }
}

/// The colour of a connection's status dot: connected, on the way, a server Studio cannot use,
/// or one it cannot reach or that refused the token.
pub fn connection_color(state: &ConnectionState) -> Color {
    match state {
        ConnectionState::Ready => Color::Success,
        ConnectionState::Connecting | ConnectionState::Idle => Color::Muted,
        ConnectionState::ContractMismatch { .. } | ConnectionState::TooOld { .. } => Color::Warning,
        ConnectionState::Unauthorized
        | ConnectionState::Unreachable(_)
        | ConnectionState::TokenUnreadable(_) => Color::Error,
    }
}

/// The workspace that owns a window, if any.
pub fn workspace_for(window: &Window) -> Option<Entity<Workspace>> {
    window.root::<Workspace>().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A modal standing in for a dialog whose write may be running.
    struct Busy {
        busy: bool,
        focus_handle: FocusHandle,
    }

    impl EventEmitter<DismissEvent> for Busy {}

    impl Focusable for Busy {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for Busy {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.focus_handle)
        }
    }

    impl ModalView for Busy {
        fn on_before_dismiss(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
            !self.busy
        }
    }

    /// Another modal, which may close whenever.
    struct Other {
        focus_handle: FocusHandle,
    }

    impl EventEmitter<DismissEvent> for Other {}

    impl Focusable for Other {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for Other {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.focus_handle)
        }
    }

    impl ModalView for Other {}

    #[gpui::test]
    fn a_busy_modal_stays_open_until_it_agrees(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let window = cx.add_window(Workspace::new);
        let open = |busy: bool, cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, |_, cx| Busy {
                        busy,
                        focus_handle: cx.focus_handle(),
                    });
                })
                .unwrap();
        };
        let hide = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |workspace, window, cx| workspace.hide_modal(window, cx))
                .unwrap()
        };
        let open_now = |cx: &mut gpui::TestAppContext| {
            window
                .read_with(cx, |workspace, _| workspace.has_modal())
                .unwrap()
        };

        open(true, cx);
        assert!(!hide(cx), "a busy modal refuses to close");
        assert!(open_now(cx));
        // The same command again asks too.
        open(true, cx);
        assert!(open_now(cx));
        // So does another modal, which would replace it: it is not shown, as in Zed.
        window
            .update(cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |_, cx| Other {
                    focus_handle: cx.focus_handle(),
                });
            })
            .unwrap();
        window
            .read_with(cx, |workspace, _| {
                let modal = workspace.modal.as_ref().unwrap().view.clone();
                assert!(modal.downcast::<Busy>().is_ok(), "the busy modal stays");
            })
            .unwrap();
        // Once the write is over it closes.
        window
            .update(cx, |workspace, _, cx| {
                let modal = workspace.modal.as_ref().unwrap().view.clone();
                modal
                    .downcast::<Busy>()
                    .unwrap()
                    .update(cx, |busy, _| busy.busy = false);
            })
            .unwrap();
        assert!(hide(cx));
        assert!(!open_now(cx));
    }

    /// A tab's costly part, drawn cached, counting its draws.
    struct Part {
        renders: usize,
    }

    impl Render for Part {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.renders += 1;
            div().size_full()
        }
    }

    /// Stands in for a field: notifying it is a keystroke.
    struct Field;

    impl Render for Field {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// A tab holding unsaved work.
    struct Unsaved {
        focus_handle: FocusHandle,
    }

    impl EventEmitter<ItemEvent> for Unsaved {}

    impl Focusable for Unsaved {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for Unsaved {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl Item for Unsaved {
        fn tab_title(&self, _: &App) -> SharedString {
            "Draft".into()
        }

        fn is_dirty(&self, _: &App) -> bool {
            true
        }
    }

    /// Close Graph with a tab holding unsaved work names it and waits: the graph closes once
    /// that is confirmed, not before. The window and Quit ask the same, of every graph's tabs.
    /// A second close asked meanwhile asks the wider question, never takes the dialog away.
    #[gpui::test]
    fn closing_a_graph_with_unsaved_work_asks_first(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let http = http_client::FakeHttpClient::create(|_| async {
            Ok(http_client::http::Response::builder()
                .status(404)
                .body(http_client::AsyncBody::default())
                .unwrap())
        });
        let server = graph_store::ServerEntry {
            name: "test".into(),
            url: "http://studio.test".into(),
            source: graph_store::ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| graph_store::Connection::new(server, None, http).unwrap());
        let session = cx.new(|cx| GraphSession::new(connection, "notes".into(), "main".into(), cx));
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let asked = |workspace: &Workspace, cx: &App| {
            workspace
                .modal
                .as_ref()
                .and_then(|modal| modal.view.clone().downcast::<ConfirmClose>().ok())
                .map(|dialog| dialog.read(cx).closing)
        };
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.graph_order.push(session.entity_id());
            workspace.subscribe_session(session.clone(), cx);
            workspace.open_item("draft".to_owned(), window, cx, |_, cx| Unsaved {
                focus_handle: cx.focus_handle(),
            });
            assert_eq!(workspace.unsaved_tabs(cx), [SharedString::from("Draft")]);
            // The tab's own close asks about the tab; Close Graph then asks about the graph.
            workspace.close_item(0, window, cx);
            assert!(matches!(asked(workspace, cx), Some(Closing::Tab(_))));
            workspace.close_graph(window, cx);
            assert!(
                matches!(asked(workspace, cx), Some(Closing::Graph)),
                "asked"
            );
            // Asked again, or for the tab alone, the graph's question stays.
            workspace.close_graph(window, cx);
            workspace.close_item(0, window, cx);
            assert!(
                matches!(asked(workspace, cx), Some(Closing::Graph)),
                "still asked"
            );
            assert!(workspace.session().is_some(), "not closed yet");
        });
        let dialog = workspace.read_with(cx, |workspace, _| {
            workspace
                .modal
                .as_ref()
                .and_then(|modal| modal.view.clone().downcast::<ConfirmClose>().ok())
                .expect("the close dialog")
        });
        dialog.update_in(cx, |dialog, window, cx| dialog.close(window, cx));
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.session().is_none(), "closed once confirmed");
        });
    }

    struct Split {
        field: Entity<Field>,
        part: Entity<Part>,
        caches: bool,
        focus_handle: FocusHandle,
    }

    impl EventEmitter<ItemEvent> for Split {}

    impl Focusable for Split {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for Split {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(self.field.clone())
                .child(AnyView::from(self.part.clone()).cached(cached_fill()))
        }
    }

    impl Item for Split {
        fn tab_title(&self, _: &App) -> SharedString {
            "Split".into()
        }

        fn caches_its_parts(&self) -> bool {
            self.caches
        }
    }

    /// The tab strip and its tabs are the header strip's height, under which every column's
    /// header rule lines up; the status bar has its own height.
    #[gpui::test]
    fn tabs_fill_the_header_strip(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_item("split", window, cx, |_, cx| Split {
                field: cx.new(|_| Field),
                part: cx.new(|_| Part { renders: 0 }),
                caches: false,
                focus_handle: cx.focus_handle(),
            });
        });
        cx.run_until_parked();
        let height = |cx: &mut gpui::VisualTestContext, selector: &'static str| {
            f32::from(
                cx.debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector} was not drawn"))
                    .size
                    .height,
            )
        };
        assert_eq!(
            height(cx, "pane-header-0"),
            studio_ui::ds::size::HEADER_STRIP
        );
        assert_eq!(height(cx, "tab-0-0"), studio_ui::ds::size::HEADER_STRIP);
        assert_eq!(height(cx, "status-bar"), studio_ui::ds::size::STATUS_BAR);
    }

    /// Selection fills the inspector without reopening a dock the person hid; a peek asked for
    /// (Space, a click) shows it. An inspector replaced or taken away goes with its host, which
    /// would otherwise keep the view, and the graph session it holds, alive.
    #[gpui::test]
    fn an_asked_for_peek_shows_the_inspector_that_selection_leaves_hidden(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            let hosted = |workspace: &Workspace, view: &Entity<Field>| {
                workspace.dock_hosts.contains_key(&view.entity_id())
            };
            workspace.set_dock_open(PanelPosition::Right, false, window, cx);
            let view = cx.new(|_| Field);
            workspace.set_inspector(Some(view.clone().into()), cx);
            assert!(!workspace.right.open, "selection leaves the dock hidden");
            assert_eq!(
                workspace.right.active_id().as_deref(),
                Some(INSPECTOR_PANEL)
            );
            workspace.reveal_inspector(cx);
            assert!(workspace.right.open, "an asked-for peek shows it");
            assert!(hosted(workspace, &view));
            let next = cx.new(|_| Field);
            workspace.set_inspector(Some(next.clone().into()), cx);
            assert!(!hosted(workspace, &view), "the replaced one's host goes");
            assert!(hosted(workspace, &next));
            workspace.set_inspector(None, cx);
            assert!(!hosted(workspace, &next), "and the removed one's");
        });
    }

    /// As Zed's panel buttons: a button shows its panel and opens the dock; pressed again while
    /// its panel shows, it hides the dock, which keeps that panel for the next time.
    #[gpui::test]
    fn panel_buttons_show_their_panel_and_hide_the_dock_it_shows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            for id in ["types", "explore"] {
                let view = cx.new(|_| Field);
                workspace.add_panel(
                    PanelPosition::Left,
                    id,
                    id,
                    IconName::Table,
                    view.into(),
                    cx,
                );
            }
            // Explore has keys of its own: showing it from its button gives it the keyboard,
            // as Zed focuses a dock's panel.
            let explore_focus = cx.focus_handle();
            let provided = explore_focus.clone();
            workspace.set_panel_focus("explore", move |_| Some(provided.clone()), cx);
            let left = |w: &Workspace| (w.left.open, w.left.active_id());
            assert_eq!(left(workspace), (true, Some("types".into())));

            workspace.press_panel_button(PanelPosition::Left, "explore", window, cx);
            assert_eq!(
                left(workspace),
                (true, Some("explore".into())),
                "shows explore"
            );
            assert!(
                explore_focus.is_focused(window),
                "and gives it the keyboard"
            );
            workspace.press_panel_button(PanelPosition::Left, "explore", window, cx);
            assert_eq!(
                left(workspace),
                (false, Some("explore".into())),
                "hides the dock"
            );
            workspace.press_panel_button(PanelPosition::Left, "explore", window, cx);
            assert_eq!(
                left(workspace),
                (true, Some("explore".into())),
                "opens it again"
            );
            workspace.press_panel_button(PanelPosition::Left, "types", window, cx);
            assert_eq!(left(workspace), (true, Some("types".into())));
        });
    }

    fn add_panels(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        for (position, id) in [
            (PanelPosition::Left, "types"),
            (PanelPosition::Left, "explore"),
            (PanelPosition::Bottom, "history"),
            (PanelPosition::Right, "inspector"),
        ] {
            let view = cx.new(|_| Field);
            workspace.add_panel(position, id, id, IconName::Table, view.into(), cx);
        }
    }

    fn placed_in(workspace: &Workspace, id: &str) -> Option<PanelPosition> {
        workspace.dock_of(id)
    }

    /// A panel goes where the settings place it, moves when they change, and goes back to its
    /// own dock when they no longer name one; one that showed keeps showing where it lands. The
    /// menu on its button moves it at once.
    #[gpui::test]
    fn a_panel_goes_where_the_settings_place_it(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
            let mut settings = StudioSettings::default();
            settings.panels.insert(
                "explore".into(),
                studio_settings::PanelPlacement {
                    dock: Some(studio_settings::DockSide::Bottom),
                },
            );
            cx.set_global(settings);
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, _, cx| {
            add_panels(workspace, cx);
            assert_eq!(placed_in(workspace, "explore"), Some(PanelPosition::Bottom));
            assert_eq!(placed_in(workspace, "types"), Some(PanelPosition::Left));
            workspace.activate_panel("explore", cx);
            assert!(workspace.bottom.open);
        });
        // The setting goes: explore goes home, still showing.
        cx.update(|_, cx| cx.set_global(StudioSettings::default()));
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, _, cx| {
            assert_eq!(placed_in(workspace, "explore"), Some(PanelPosition::Left));
            assert_eq!(workspace.left.active_id().as_deref(), Some("explore"));
            assert!(workspace.left.open);
            // The menu: the inspector to the left dock, beside the others.
            workspace.dock_panel("inspector".into(), PanelPosition::Left, cx);
            assert_eq!(placed_in(workspace, "inspector"), Some(PanelPosition::Left));
            assert!(workspace.right.panels.is_empty());
        });
    }

    /// ⇧⎋ zooms the active pane over the document area and back; a zoomed dock ends its zoom
    /// when it is hidden.
    #[gpui::test]
    fn zoom_covers_the_document_area_until_toggled_or_hidden(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            add_panels(workspace, cx);
            workspace.open_item("a", window, cx, |_, cx| Split {
                field: cx.new(|_| Field),
                part: cx.new(|_| Part { renders: 0 }),
                caches: false,
                focus_handle: cx.focus_handle(),
            });
            workspace.toggle_zoom(window, cx);
            assert_eq!(workspace.zoomed, Some(Zoomed::Pane(workspace.active_pane)));
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("zoomed").is_some(), "drawn over the panes");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_zoom(window, cx);
            assert_eq!(workspace.zoomed, None);
            workspace.activate_panel("history", cx);
            workspace.zoomed = Some(Zoomed::Dock(PanelPosition::Bottom));
            workspace.set_dock_open(PanelPosition::Bottom, false, window, cx);
            assert_eq!(
                workspace.zoomed, None,
                "hiding the zoomed dock ends the zoom"
            );
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("zoomed").is_none());
    }

    /// As Zed's `bottom_dock_layout`: contained, the bottom panel spans the document only;
    /// full, the whole window, under the inspector too.
    #[gpui::test]
    fn the_bottom_panel_spans_as_its_layout_says(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, _, cx| {
            add_panels(workspace, cx);
            workspace.activate_panel("history", cx);
            workspace.activate_panel("inspector", cx);
        });
        cx.run_until_parked();
        let width = |cx: &mut gpui::VisualTestContext, selector: &'static str| {
            f32::from(cx.debug_bounds(selector).expect(selector).size.width)
        };
        let window = width(cx, "status-bar");
        let contained = width(cx, "bottom-dock");
        let inspector = workspace.read_with(cx, |workspace, _| workspace.right.size);
        assert_eq!(contained, window - inspector, "beside the inspector");

        cx.update(|_, cx| {
            let mut settings = StudioSettings::default();
            settings.studio.bottom_dock_layout = BottomDockLayout::Full;
            cx.set_global(settings);
        });
        cx.run_until_parked();
        assert_eq!(width(cx, "bottom-dock"), window, "across the window");
    }

    /// A graph's saved docks come back with it, so a graph shown again (parked, then shown)
    /// has its own docks, not the docks of the graph shown before.
    #[gpui::test]
    fn a_graphs_docks_come_back_with_it(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, _, cx| {
            add_panels(workspace, cx);
            let saved = SavedWorkspace {
                sidebar_open: false,
                inspector_open: false,
                activity_open: true,
                bottom_panel: Some("history".into()),
                bottom_size: Some(240.),
                ..workspace.snapshot(cx)
            };
            workspace.zoomed = Some(Zoomed::Pane(workspace.active_pane));
            workspace.apply_docks(&saved, gpui::size(px(1440.), px(900.)));
            assert!(!workspace.left.open && !workspace.right.open && workspace.bottom.open);
            assert_eq!(workspace.bottom.active_id().as_deref(), Some("history"));
            assert_eq!(workspace.bottom.size, 240.);
            assert_eq!(workspace.zoomed, None);
        });
    }

    /// A graph reopened behind the shown one opens none of its tabs, so a start reads nothing
    /// for it, until it is first shown; then its saved tabs open, once.
    #[gpui::test]
    fn a_graph_reopened_behind_opens_its_tabs_when_first_shown(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
            persistence::register_item_kind(cx, "split", |data, workspace, window, cx| {
                open_tab(workspace, data.as_str().unwrap_or_default(), window, cx);
            });
        });
        let http = http_client::FakeHttpClient::create(|_| async {
            Ok(http_client::http::Response::builder()
                .status(404)
                .body(http_client::AsyncBody::default())
                .unwrap())
        });
        let server = graph_store::ServerEntry {
            name: "test".into(),
            url: "http://studio.test".into(),
            source: graph_store::ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        // The CLI can give one server two names, each its own connection.
        let renamed = graph_store::ServerEntry {
            name: "test-admin".into(),
            ..server.clone()
        };
        let other = cx.new(|_| graph_store::Connection::new(renamed, None, http.clone()).unwrap());
        let connection = cx.new(|_| graph_store::Connection::new(server, None, http).unwrap());
        let session = |graph: &str, cx: &mut gpui::TestAppContext| {
            let connection = connection.clone();
            let graph = graph.to_owned();
            cx.new(|cx| GraphSession::new(connection, graph, "main".into(), cx))
        };
        let (shown, behind) = (session("notes", cx), session("crm", cx));
        let behind_again = cx.new(|cx| GraphSession::new(other, "crm".into(), "main".into(), cx));
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            let tabs = |workspace: &Workspace| -> usize {
                workspace.panes.iter().map(|pane| pane.items.len()).sum()
            };
            workspace.graph_order.push(shown.entity_id());
            workspace.subscribe_session(shown.clone(), cx);
            open_tab(workspace, "notes", window, cx);
            let item = |key: &str| SavedItem {
                kind: "split".into(),
                data: serde_json::json!(key),
            };
            let saved = SavedWorkspace {
                panes: vec![SavedPane {
                    items: vec![item("a"), item("b")],
                    active: 1,
                    pinned: 0,
                }],
                ..workspace.snapshot(cx)
            };
            workspace.add_behind(behind.clone(), Some(saved), cx);
            assert_eq!(tabs(workspace), 1, "the shown graph's tab alone");
            assert!(
                workspace
                    .parked
                    .iter()
                    .all(|parked| { parked.panes.iter().all(|pane| pane.items.is_empty()) })
            );
            assert_eq!(workspace.open_graphs().len(), 2);
            // The graph under the server's other name is the one open: found by its address,
            // and not opened a second time.
            let found = workspace.open_session_for("http://studio.test", "crm", cx);
            assert_eq!(found.map(|s| s.entity_id()), Some(behind.entity_id()));
            workspace.add_behind(behind_again.clone(), None, cx);
            assert_eq!(workspace.open_graphs().len(), 2);

            // The shown graph's inspector goes with it, host and all: the host would keep the
            // inspector, and the session it holds polling, after the graph is put away.
            let inspector = cx.new(|_| Field);
            workspace.set_inspector(Some(inspector.clone().into()), cx);
            workspace.show_graph(behind.entity_id(), window, cx);
            assert_eq!(workspace.dock_of(INSPECTOR_PANEL), None);
            assert!(!workspace.dock_hosts.contains_key(&inspector.entity_id()));
            let pane = workspace.pane(workspace.active_pane).unwrap();
            assert_eq!((pane.items.len(), pane.active), (2, 1));

            // Shown again after the other graph: its tabs as they were, not opened twice.
            workspace.show_graph(shown.entity_id(), window, cx);
            assert_eq!(tabs(workspace), 1);
            workspace.show_graph(behind.entity_id(), window, cx);
            assert_eq!(tabs(workspace), 2);
        });
    }

    /// A saved tab that declines to open (a type the schema dropped) takes neither its pin nor
    /// the active place with it to the tab after it.
    #[gpui::test]
    fn a_saved_tab_that_does_not_open_leaves_the_pins_and_the_active_tab_in_place(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
            persistence::register_item_kind(cx, "maybe", |data, workspace, window, cx| {
                let key = data.as_str().unwrap_or_default();
                if key != "gone" {
                    open_tab(workspace, key, window, cx);
                }
            });
            // As a conversation another pane holds: shown there, opening nothing here.
            persistence::register_item_kind(cx, "elsewhere", |_, workspace, window, cx| {
                let shown = workspace.all_items().next().map(|item| item.item_id());
                if let Some(id) = shown {
                    workspace.activate_item_by_id(id, window, cx);
                }
            });
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let item = |key: &str| SavedItem {
            kind: "maybe".into(),
            data: serde_json::json!(key),
        };
        let restored = |workspace: &mut Workspace,
                        saved: SavedPane,
                        window: &mut Window,
                        cx: &mut Context<Workspace>| {
            let saved = SavedWorkspace {
                panes: vec![saved],
                ..workspace.snapshot(cx)
            };
            workspace.restore_saved(saved, window, cx);
            let pane = workspace.pane(workspace.active_pane).unwrap();
            (pane.items.len(), pane.pinned, pane.active)
        };
        workspace.update_in(cx, |workspace, window, cx| {
            // The pinned first tab is gone: b is not pinned in its place, and b stays active.
            let pane = SavedPane {
                items: vec![item("gone"), item("b"), item("c")],
                active: 1,
                pinned: 1,
            };
            assert_eq!(restored(workspace, pane, window, cx), (2, 0, 0));
            // The active tab is gone: the one before it is active, not the one after.
            let pane = SavedPane {
                items: vec![item("a"), item("gone"), item("c")],
                active: 1,
                pinned: 0,
            };
            assert_eq!(restored(workspace, pane, window, cx), (2, 0, 0));
            // A tab shown in another pane instead (a conversation has one tab) leaves the tabs
            // after it in their own pane.
            let mut layout = Member::Pane(0);
            layout.split(0, 1, SplitDirection::Right);
            let elsewhere = SavedItem {
                kind: "elsewhere".into(),
                data: serde_json::Value::Null,
            };
            let saved = SavedWorkspace {
                panes: vec![
                    SavedPane {
                        items: vec![item("a")],
                        active: 0,
                        pinned: 0,
                    },
                    SavedPane {
                        items: vec![elsewhere, item("b")],
                        active: 1,
                        pinned: 0,
                    },
                ],
                layout: Some(layout),
                ..workspace.snapshot(cx)
            };
            workspace.restore_saved(saved, window, cx);
            let tabs: Vec<usize> = workspace
                .panes
                .iter()
                .map(|pane| pane.items.len())
                .collect();
            assert_eq!(tabs, [1, 1]);
        });
    }

    fn open_tab(
        workspace: &mut Workspace,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> EntityId {
        workspace
            .open_item(key.to_owned(), window, cx, |_, cx| Split {
                field: cx.new(|_| Field),
                part: cx.new(|_| Part { renders: 0 }),
                caches: false,
                focus_handle: cx.focus_handle(),
            })
            .map(|item| item.entity_id())
            .expect("opened")
    }

    fn dragged(item: EntityId) -> DraggedTab {
        DraggedTab {
            item,
            title: "tab".into(),
            icon: None,
        }
    }

    /// As Zed's tab drops: on a tab of its pane a tab takes that place; on another pane's strip
    /// it moves there and shows; a pane left with no tab closes; near a pane's edge it splits
    /// a new pane off with the tab.
    #[gpui::test]
    fn dropped_tabs_move_within_and_between_panes_and_split_at_an_edge(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            let a = open_tab(workspace, "a", window, cx);
            let b = open_tab(workspace, "b", window, cx);
            let c = open_tab(workspace, "c", window, cx);
            let first = workspace.active_pane;
            let order = |w: &Workspace, pane: PaneId| -> Vec<EntityId> {
                w.pane(pane)
                    .unwrap()
                    .items
                    .iter()
                    .map(|i| i.item_id())
                    .collect()
            };
            workspace.drop_tab(&dragged(c), first, Some(0), window, cx);
            assert_eq!(order(workspace, first), [c, a, b], "c takes a's place");

            // A tab dropped at an edge splits a pane off with it.
            workspace.drop_split(&dragged(b), first, SplitDirection::Right, window, cx);
            let second = workspace.active_pane;
            assert_ne!(second, first);
            assert_eq!(order(workspace, second), [b]);
            assert_eq!(order(workspace, first), [c, a]);

            // Onto the other pane's strip: it moves there; the emptied pane closes.
            workspace.drop_tab(&dragged(b), first, None, window, cx);
            assert_eq!(order(workspace, first), [c, a, b]);
            assert!(workspace.pane(second).is_none(), "an empty pane closes");
            assert_eq!(workspace.panes.len(), 1);

            // The only tab of a pane dropped at that pane's own edge stays where it is.
            workspace.drop_split(&dragged(b), first, SplitDirection::Down, window, cx);
            let lone = workspace.active_pane;
            assert_eq!(order(workspace, lone), [b]);
            workspace.drop_split(&dragged(b), lone, SplitDirection::Right, window, cx);
            assert_eq!(workspace.panes.len(), 2, "no pane split off");
            assert_eq!(order(workspace, lone), [b]);
        });
    }

    #[test]
    fn a_drop_near_an_edge_splits_toward_it() {
        let bounds = gpui::Bounds::new(point(px(0.), px(0.)), gpui::size(px(1000.), px(500.)));
        let at = |x: f32, y: f32| drop_edge(bounds, point(px(x), px(y)));
        // A fifth of the shorter side: 100 px.
        assert_eq!(at(50., 250.), Some(SplitDirection::Left));
        assert_eq!(at(950., 250.), Some(SplitDirection::Right));
        assert_eq!(at(500., 20.), Some(SplitDirection::Up));
        assert_eq!(at(500., 480.), Some(SplitDirection::Down));
        assert_eq!(
            at(500., 250.),
            None,
            "the middle moves the tab into the pane"
        );
        assert_eq!(
            at(30., 20.),
            Some(SplitDirection::Up),
            "the nearest edge wins"
        );
    }

    /// Back and Forward walk the tabs the active pane showed, as Zed's per-pane history: a new
    /// pane starts with none, and stepping in it leaves the other pane's trail alone.
    #[gpui::test]
    fn back_and_forward_walk_the_active_panes_own_tabs(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            for key in ["a", "b"] {
                workspace.open_item(key, window, cx, |_, cx| Split {
                    field: cx.new(|_| Field),
                    part: cx.new(|_| Part { renders: 0 }),
                    caches: false,
                    focus_handle: cx.focus_handle(),
                });
            }
            let first = workspace.active_pane;
            let active = |w: &Workspace| w.pane(w.active_pane).map(|p| p.active);
            assert_eq!(active(workspace), Some(1), "b shows");
            workspace.navigate(first, true, window, cx);
            assert_eq!(active(workspace), Some(0), "back to a");
            workspace.navigate(first, false, window, cx);
            assert_eq!(active(workspace), Some(1), "forward to b");

            workspace.split(SplitDirection::Right, window, cx);
            let second = workspace.active_pane;
            assert_ne!(second, first);
            assert!(
                !workspace.pane(second).unwrap().can_step(true),
                "a new pane has no trail"
            );
            workspace.navigate(second, true, window, cx);
            assert!(
                workspace.pane(first).unwrap().can_step(true),
                "the first pane keeps its trail"
            );
        });
    }

    /// A CPU reading measures from the one before it, never across a pause in the background:
    /// after one, the shown figure stays until a reading made after the pause.
    #[test]
    fn a_cpu_reading_never_spans_a_pause() {
        use process_stats::Sample;
        use std::time::{Duration, Instant};
        let at = Instant::now();
        let sample = |secs: u64, cpu_ms: u64| Sample {
            at: at + Duration::from_secs(secs),
            memory: None,
            cpu_time: Some(Duration::from_millis(cpu_ms)),
        };
        let first = Performance::next(None, sample(0, 0));
        assert_eq!(first.cpu, None, "nothing to measure from yet");
        // Half a core over two seconds.
        let second = Performance::next(Some(&first), sample(2, 1000));
        assert_eq!(second.cpu, Some(0.5));
        // Then a minute in the background, with almost no CPU.
        let paused = Performance {
            paused: true,
            ..second
        };
        let back = Performance::next(Some(&paused), sample(62, 1010));
        assert_eq!(
            back.cpu,
            Some(0.5),
            "the figure from before the pause, not 0%"
        );
        let after = Performance::next(Some(&back), sample(64, 1410));
        assert_eq!(
            after.cpu,
            Some(0.2),
            "measured from the reading after the pause"
        );
    }

    /// The window leaving marks the last reading at once, so a return before the next tick
    /// does not measure across the time away.
    /// A delete Studio can put back offers Undo on its toast, bound to its commit and branch;
    /// any other commit offers itself.
    #[test]
    fn a_delete_that_can_be_put_back_offers_undo() {
        let entry = |follow_up| graph_store::OutcomeEntry {
            at: std::time::SystemTime::now(),
            branch: "main".into(),
            description: "Delete Company Acme".into(),
            outcome: graph_store::OutcomeKind::Committed {
                commit: Some("c2".into()),
            },
            follow_up,
        };
        let (label, action) =
            committed_follow_up(&entry(Some(graph_store::FollowUp::UndoDelete)), "c2");
        assert_eq!(label, "Undo");
        assert!(action.partial_eq(&studio_actions::graph::UndoDelete {
            commit: "c2".into(),
            branch: "main".into(),
            what: "Delete Company Acme".into(),
        }));
        let (label, action) = committed_follow_up(&entry(None), "c2");
        assert_eq!(label, "View commit");
        assert!(action.partial_eq(&OpenCommit {
            commit_id: "c2".into()
        }));
    }

    #[gpui::test]
    fn leaving_the_window_pauses_the_cpu_reading(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let window = cx.add_window(Workspace::new);
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |workspace, _, _| {
                workspace.performance = Some(Performance::next(None, process_stats::Sample::now()));
            })
            .unwrap();
        let paused = |cx: &mut gpui::TestAppContext| {
            window
                .read_with(cx, |workspace, _| {
                    workspace.performance.as_ref().map(|reading| reading.paused)
                })
                .unwrap()
        };
        assert_eq!(paused(cx), Some(false));
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.deactivate_window();
        assert_eq!(paused(&mut cx), Some(true));
    }

    /// The top-left strips make room for traffic lights only where the window shows them: off
    /// macOS and in full screen they start at their own edge, as Zed's title bar does.
    #[gpui::test]
    fn strips_make_room_for_traffic_lights_only_when_they_show(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            assert_eq!(lights_room(false, false, cx), None, "no lights");
            assert_eq!(lights_room(false, true, cx), None, "no lights, zoomed");
            assert_eq!(lights_room(true, false, cx), Some(TRAFFIC_LIGHTS_WIDTH));
            assert_eq!(
                lights_room(true, true, cx),
                Some(TRAFFIC_LIGHTS_WIDTH - ZOOM_INSET),
                "flat, inside the zoom's inset"
            );
        });
    }

    /// Only the top-left pane's first tab on glass, with nothing before it, meets the sheet's
    /// corner.
    #[test]
    fn the_first_tab_meets_the_sheet_corner_only_when_nothing_comes_before_it() {
        assert!(first_tab_in_sheet_corner(true, true, true, true));
        assert!(!first_tab_in_sheet_corner(false, true, true, true), "flat");
        assert!(
            !first_tab_in_sheet_corner(true, false, true, true),
            "another pane"
        );
        assert!(
            !first_tab_in_sheet_corner(true, true, false, true),
            "traffic lights first"
        );
        assert!(
            !first_tab_in_sheet_corner(true, true, true, false),
            "traffic lights first"
        );
    }

    /// A keystroke in a tab's field draws its cached part again only when the pane caches the
    /// whole tab: GPUI draws every cached view inside a cached view that draws again. If GPUI
    /// stops doing that, the second assertion fails and `caches_its_parts` can go.
    #[gpui::test]
    fn a_tab_that_caches_its_parts_keeps_them_while_a_field_changes(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let window = cx.add_window(Workspace::new);
        for caches in [true, false] {
            let split = window
                .update(cx, |workspace, window, cx| {
                    workspace.open_item(format!("split-{caches}"), window, cx, |_, cx| Split {
                        field: cx.new(|_| Field),
                        part: cx.new(|_| Part { renders: 0 }),
                        caches,
                        focus_handle: cx.focus_handle(),
                    })
                })
                .unwrap()
                .unwrap();
            cx.run_until_parked();
            let renders = |cx: &mut gpui::TestAppContext| {
                split.read_with(cx, |split, cx| split.part.read(cx).renders)
            };
            let before = renders(cx);
            assert!(before > 0, "the part was drawn");
            split.update(cx, |split, cx| split.field.update(cx, |_, cx| cx.notify()));
            cx.run_until_parked();
            if caches {
                assert_eq!(renders(cx), before, "the part was drawn again");
            } else {
                assert_eq!(renders(cx), before + 1);
            }
        }
    }
}
