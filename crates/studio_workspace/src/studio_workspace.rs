//! The window frame (spec 002): columns with header strips (a sidebar dock on the left, panes
//! of tabs in the center over a bottom dock, an inspector dock on the right) over one status
//! bar that holds every panel's button, the server, graph and branch, and the tab's facts, as
//! Zed lays out its docks and status bar; a modal layer and toasts.
//!
//! Layer 4 (`docs/architecture.md`). A small version of Zed's `workspace` crate, written for
//! Studio rather than forked: Studio has no projects, collaboration or Zed database. Feature
//! crates attach to it with `cx.observe_new::<Workspace>` and `Workspace::register_action`.

mod item;
pub mod models;
mod pane;
mod panels;
mod persistence;
mod process_stats;
mod sign_in_prompt;

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
use graph_store::{
    AccessAction, AccessScope, ConnectionRegistry, ConnectionState, GraphSession, Loadable,
    OutcomeKind, SessionEvent,
};
use studio_actions::graph::{
    OpenAsCanvas, OpenAsHistory, OpenAsMap, OpenAsPage, OpenAsReview, OpenAsSource, OpenAsTable,
    OpenAsTree, OpenCommit, OpenHistory,
};
use studio_actions::workspace::OpenViewAt;
use studio_actions::workspace::{
    ActivateItem, ActivateLastItem, ActivateNextGraph, ActivateNextItem, ActivateNextPane,
    ActivatePreviousGraph, ActivatePreviousItem, ActivatePreviousPane, CloseActiveItem,
    CloseAllItems, CloseGraph, CloseOtherItems, GoBack, GoForward, KeepPreviewTab, Refresh,
    SplitDown, SplitLeft, SplitRight, SplitUp, ToggleActivity, ToggleInspector, TogglePinTab,
    ToggleSidebar, ToggleZoom,
};
use studio_actions::{ViewKind, ViewLink};
use studio_settings::{BottomDockLayout, StudioSettings};
use studio_ui::ds::IconButton;
use studio_ui::menu::{ContextMenu, ContextMenuEntry, ContextMenuItem};
use ui::prelude::*;
use ui::{ButtonLike, PopoverMenu, Tooltip, right_click_menu};

use item::ItemHandle;
pub use item::{Item, ItemEvent, ItemStatus, KeyHint};
use pane::{Member, Pane, PaneId, SplitAxis, SplitDirection, Tab};
pub use panels::PanelPosition;
use panels::{Dock, PanelEntry};
pub use persistence::{
    SavedGraph, SavedItem, SavedPane, SavedSession, SavedWindow, SavedWorkspace,
    keep_saved_windows, load_windows, register_item_kind, saved_window_bounds,
};
pub use sign_in_prompt::sign_in_prompt;

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

    /// Whether the modal asks about the shown graph or one of its tabs. Such a modal goes,
    /// unasked, when that graph leaves the window: answered over another graph, it would act
    /// on one no longer shown.
    fn goes_with_graph(&self) -> bool {
        false
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
    /// [`ModalView::goes_with_graph`], asked as it opens.
    goes_with_graph: bool,
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
    action: Option<FollowUp>,
    /// Time left on screen; `None` stays until dismissed (errors, spec 002).
    remaining: Option<Duration>,
    hovered: bool,
    /// How many errors this toast stands for, folded into it off a full stack
    /// ([`Workspace::make_room`]); 0 for a toast that stands for itself.
    folded: usize,
    /// The graph the toast is about (a write's outcome, a fold of a graph's errors); `None` for
    /// one about no graph (a notice about the servers, a file not written). Errors fold with
    /// errors about the same graph, and across graphs only when no graph has two to fold
    /// ([`Self::across`]).
    graph: Option<GraphAt>,
    /// A fold of errors about several graphs, or about graphs and the servers
    /// ([`Workspace::fold_oldest_error`]).
    across: bool,
}

impl Toast {
    /// Whether the toast is not simply taken away when the stack is full
    /// ([`Workspace::make_room`]): an error, which stays until dismissed and is folded at
    /// most, and an Undo, which only this toast offers.
    fn kept(&self) -> bool {
        self.kind == ToastKind::Error || self.action.as_ref().is_some_and(|f| f.undoes)
    }

    /// The toast that stands for `count` errors about `graph` folded off a full stack. It stays
    /// until dismissed, as they would have. A graph's fold opens that graph's activity log,
    /// showing the graph first whatever the window shows then: the log lists each of its
    /// writes' refusals and unknown outcomes with their reasons, and an unknown outcome's
    /// branch history is checked from there. Errors about no graph are in no activity log, so
    /// their fold offers nothing to open.
    fn folded_errors(id: usize, count: usize, graph: Option<GraphAt>) -> Self {
        let action = graph.as_ref().map(|at| FollowUp {
            label: "Open activity".into(),
            action: Box::new(studio_actions::workspace::ActivatePanel {
                id: ACTIVITY_PANEL.into(),
            }),
            on: Some(WrittenTo {
                server_url: at.server_url.clone(),
                graph: at.graph.clone(),
                branch: None,
            }),
            undoes: false,
        });
        Self {
            id,
            kind: ToastKind::Error,
            message: folded_errors_text(count, graph.as_ref()),
            action,
            remaining: None,
            hovered: false,
            folded: count,
            graph,
            across: false,
        }
    }

    /// The toast that stands for `count` errors folded off a full stack whatever they were
    /// about, when no graph had two errors to fold. It stays until dismissed, as they would have.
    /// Its Open activity shows the activity log of the graph the window shows when it is
    /// pressed, which lists that graph's refusals and unknown outcomes; each other graph's are
    /// in its own log, shown with it.
    fn folded_across(id: usize, count: usize) -> Self {
        Self {
            id,
            kind: ToastKind::Error,
            message: folded_errors_text(count, None),
            action: Some(FollowUp {
                label: "Open activity".into(),
                action: Box::new(studio_actions::workspace::ActivatePanel {
                    id: ACTIVITY_PANEL.into(),
                }),
                on: None,
                undoes: false,
            }),
            remaining: None,
            hovered: false,
            folded: count,
            graph: None,
            across: true,
        }
    }
}

/// The text of the toast that stands for `count` errors about `graph` folded off the stack,
/// which takes the place of the oldest of them: so they are earlier than every error about it
/// still shown. It names its graph, as a parked graph's toast does, since each graph's errors
/// fold apart. A fold starts at two, the fewest that makes room.
fn folded_errors_text(count: usize, graph: Option<&GraphAt>) -> SharedString {
    match graph {
        Some(at) => format!("{}: {count} earlier errors", at.graph).into(),
        None => format!("{count} earlier errors").into(),
    }
}

struct FollowUp {
    label: SharedString,
    action: Box<dyn Action>,
    /// The write's graph and branch, which the action acts on whatever the window shows when
    /// it is run; `None` for one that acts where the window is ("Open settings").
    on: Option<WrittenTo>,
    /// It puts back what a write took (Undo), which nothing but this toast offers.
    undoes: bool,
}

/// Where a write went: its graph, by its server's address and id as the window finds an open
/// graph ([`Workspace::open_session_for`]), and its branch; `None` for a follow-up about the
/// graph as a whole (a fold of its errors), which leaves the graph on its branch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WrittenTo {
    server_url: String,
    graph: String,
    branch: Option<String>,
}

impl WrittenTo {
    fn graph_at(&self) -> GraphAt {
        GraphAt {
            server_url: self.server_url.clone(),
            graph: self.graph.clone(),
        }
    }
}

/// A graph a toast is about, as the window finds an open graph: its server's address and its
/// id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphAt {
    server_url: String,
    graph: String,
}

/// How long a success or info toast stays (spec 002); hovering pauses it.
const TOAST_DURATION: Duration = Duration::from_secs(10);
const TOAST_TICK: Duration = Duration::from_millis(250);
/// Toasts shown at once.
const TOASTS_SHOWN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceEvent {
    SessionChanged,
    /// Another tab is active, or none is: what follows the active tab follows it (spec 020).
    ActiveItemChanged,
}

/// An action handler a feature crate registered on the window, and when it is registered:
/// always, or while its condition holds, so the palette lists it only then.
#[derive(Clone)]
struct ActionRegistration {
    when: Option<ActionCondition>,
    register: ActionHandler,
}

/// Whether a registered action is available where the window is now.
type ActionCondition = Rc<dyn Fn(&Workspace, &App) -> bool>;

/// Puts a registered action's handler on the window's root.
type ActionHandler = Rc<dyn Fn(Div, &mut Window, &mut Context<Workspace>) -> Div>;

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

/// The view at the right of the status bar ([`Workspace::set_status_item`]), and whether it
/// has anything to say now.
struct StatusItem {
    view: AnyView,
    shown: Box<dyn Fn(&App) -> bool>,
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
    /// Each tab's inspector, by the tab's entity: the finished view, with its node or edge, its
    /// values, its commit and its saving state, which come back with the tab (spec 020).
    inspectors: HashMap<EntityId, AnyView>,
    /// The tab what follows the active tab last followed.
    followed_item: Option<EntityId>,
    /// The next new tab goes in a pane to the right ([`Self::open_view`]).
    open_beside: bool,
    _follow_subscription: Subscription,
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
    /// The registry's notices about the servers ([`ConnectionRegistry::notices`]), as last
    /// shown.
    server_notices: Vec<String>,
    _settings_subscription: Subscription,
    /// Studio's memory and CPU for the status bar while `studio.show_performance` is on (the
    /// default), read every two seconds while the window is in front.
    performance: Option<Performance>,
    /// A view of the window's own at the right of the status bar: the updater's (spec 017).
    status_item: Option<StatusItem>,
    /// Whether this window is in front, which the memory and CPU reading waits for.
    window_active: bool,
    _performance: Option<Task<()>>,
    _performance_subscription: Subscription,
    /// What was last written for this graph, so unchanged state is not rewritten.
    saved: Option<SavedWorkspace>,
    _save: Option<Task<()>>,
    /// The next comparison of the state with what was last written.
    _save_check: Option<Task<()>>,
    /// The status bar's connection dot's beats, counted every `motion::PULSE` by `_beats` while
    /// its server is ready and the window in front ([`Workspace::count_beats`]).
    beat: u64,
    _beats: Option<Task<()>>,
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

/// Where the traffic lights sit: centered in the top bar. A window opens with them there, so they
/// do not move on its first draw.
pub fn traffic_light_position(_glass: bool) -> Point<Pixels> {
    point(px(12.), px(TRAFFIC_LIGHTS_TOP))
}
/// A tab's view strip (spec 020): links to the other views of what it shows, the first thing in
/// its toolbar. Nothing to choose between draws nothing. A view that cannot open stays, with why,
/// so the strip does not shift.
pub fn view_strip(views: Vec<ViewLink>, cx: &App) -> Option<AnyElement> {
    if views.len() < 2 {
        return None;
    }
    Some(
        studio_ui::ds::segmented(cx)
            .children(views.into_iter().enumerate().map(|(index, link)| {
                let segment =
                    studio_ui::ds::segment(("view", index), link.kind.label(), link.current, cx);
                match link.disabled {
                    Some(reason) => segment
                        .opacity(studio_ui::ds::opacity::DIMMED)
                        .cursor_default()
                        .tooltip(Tooltip::text(reason))
                        .into_any_element(),
                    None if link.current => segment.into_any_element(),
                    None => segment
                        .on_click(move |event: &ClickEvent, window, cx| {
                            window.dispatch_action(
                                Box::new(OpenViewAt {
                                    index,
                                    beside: event.modifiers().alt,
                                }),
                                cx,
                            )
                        })
                        .into_any_element(),
                }
            }))
            .into_any_element(),
    )
}

/// The inspector while the active tab has nothing selected.
struct NothingSelected;

impl Render for NothingSelected {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        studio_ui::ds::sidebar_note("Nothing selected")
    }
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
    /// Held by the panel's whole body, in whichever dock it is drawn: the keyboard in any of
    /// the panel's elements is in its dock ([`Workspace::dock_has_focus`]).
    focus_handle: FocusHandle,
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
        // So is each notice about the servers: the CLI's targets that cannot be listed or used
        // (servers read before stand meanwhile), the environment's server not added.
        let registry = ConnectionRegistry::try_global(cx);
        let registry_subscription = registry.as_ref().map(|registry| {
            cx.observe(registry, |this, registry, cx| {
                this.show_server_notices(&registry, cx)
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
            inspectors: HashMap::new(),
            followed_item: None,
            open_beside: false,
            _follow_subscription: cx.observe_self(|this, cx| this.follow_active_item(cx)),
            toasts: Vec::new(),
            next_toast: 0,
            actions: Vec::new(),
            focus_handle: cx.focus_handle(),
            drag_pending: false,
            traffic_lights: None,
            lights_shown: cfg!(target_os = "macos"),
            settings_error: None,
            server_notices: Vec::new(),
            _settings_subscription: settings_subscription,
            performance: None,
            status_item: None,
            window_active: window.is_window_active(),
            _performance: None,
            _performance_subscription: performance_subscription,
            saved: None,
            _save: None,
            _save_check: None,
            beat: 0,
            _beats: None,
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
            this.show_server_notices(&registry, cx);
        }
        this.watch_performance(cx);
        this.register_view_action::<OpenAsTable>(ViewKind::Table);
        this.register_view_action::<OpenAsPage>(ViewKind::Page);
        this.register_view_action::<OpenAsTree>(ViewKind::Tree);
        this.register_view_action::<OpenAsCanvas>(ViewKind::Canvas);
        this.register_view_action::<OpenAsMap>(ViewKind::Map);
        this.register_view_action::<OpenAsSource>(ViewKind::Source);
        this.register_view_action::<OpenAsReview>(ViewKind::Review);
        this.register_view_action::<OpenAsHistory>(ViewKind::History);
        this.register_action(|workspace, action: &OpenViewAt, window, cx| {
            let kind = workspace
                .active_item()
                .and_then(|item| item.views(cx).get(action.index).map(|link| link.kind));
            if let Some(kind) = kind {
                workspace.open_view(kind, action.beside, window, cx);
            }
        });
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

    /// Studio's memory and CPU, muted at the end of the status bar; in the warning colour past
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
                .id("status-performance")
                .flex_none()
                .tooltip(Tooltip::text(tooltip))
                .child(
                    studio_ui::ds::text::meta(parts.join(" · "))
                        .color(if busy { Color::Warning } else { Color::Muted })
                        .single_line(),
                ),
        )
    }

    /// The key the shown graph's tabs and drafts are saved under ([`saved_key_of`]).
    fn saved_key(&self, cx: &App) -> Option<String> {
        self.session
            .as_ref()
            .map(|session| saved_key_of(session, cx))
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
            canvas_positions: self
                .session
                .as_ref()
                .map(|session| session.read(cx).canvas_positions.clone())
                .unwrap_or_default(),
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
                    let server = &session.connection.read(cx).server;
                    SavedGraph {
                        target: server.name.clone(),
                        url: server.url.clone(),
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
        let Some(key) = self.saved_key(cx) else {
            return;
        };
        let snapshot = self.snapshot(cx);
        if self.saved.as_ref() == Some(&snapshot) {
            return;
        }
        self.saved = Some(snapshot.clone());
        self._save = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            persistence::save(&key, &snapshot);
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
        let Some(key) = self.saved_key(cx) else {
            return;
        };
        if let Some(saved) = persistence::load(&key) {
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
        if let Some(session) = &self.session {
            let positions = saved.canvas_positions.clone();
            session.update(cx, |session, _| session.canvas_positions = positions);
        }
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
        let (url, graph) = address_of(&session, cx);
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
            .then(|| persistence::load(&saved_key_of(&session, cx)))
            .flatten();
        self.add_behind(session, saved, cx);
    }

    fn add_behind(
        &mut self,
        session: Entity<GraphSession>,
        saved: Option<SavedWorkspace>,
        cx: &mut Context<Self>,
    ) {
        let (url, graph) = address_of(&session, cx);
        if self.open_session_for(&url, &graph, cx).is_some() {
            return;
        }
        self.graph_order.push(session.entity_id());
        self.watch_graph(&session, cx);
        // Parked: it polls nothing until shown.
        session.update(cx, |session, cx| session.set_shown(false, cx));
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

    /// Shows `session`'s graph as the window's: it asks after its heads and its server's health
    /// again, which it stops while parked ([`GraphSession::set_shown`]).
    fn subscribe_session(&mut self, session: Entity<GraphSession>, cx: &mut Context<Self>) {
        self.watch_graph(&session, cx);
        session.update(cx, |session, cx| session.set_shown(true, cx));
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
        let Some(key) = self.saved_key(cx) else {
            return;
        };
        let pending = self._save.take().is_some();
        let snapshot = self.snapshot(cx);
        if pending || self.saved.as_ref() != Some(&snapshot) {
            persistence::save(&key, &snapshot);
            self.saved = Some(snapshot);
        }
    }

    /// Takes the shown graph out of the window, saved, leaving no graph shown. Parked, it asks
    /// after no head and no branch list, and holds no watch on its server's health.
    fn park(&mut self, cx: &mut Context<Self>) -> Option<ParkedGraph> {
        self.save_now(cx);
        let session = self.session.take()?;
        session.update(cx, |session, cx| session.set_shown(false, cx));
        // The inspector shows the graph's rows, and holds its session.
        self.inspectors.clear();
        self.remove_panel(INSPECTOR_PANEL);
        self.zoomed = None;
        // A modal about this graph goes with it: a close asked about the graph, confirmed over
        // the next one shown, would close that one and drop its unsaved tabs unasked. Another
        // modal stays, and must not hand the focus back to a tab of this graph. The callers
        // focus the graph shown next.
        if self
            .modal
            .as_ref()
            .is_some_and(|modal| modal.goes_with_graph)
        {
            self.modal = None;
        }
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

    /// The session this window has open for `graph` on the server at `url`. A graph open is
    /// matched by its server's address, as a link and the Open dialog name it: an address is
    /// one connection, which a target renamed on it keeps, so the graph stays the same one.
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

    /// Toasts the outcomes `session` logged since the last asked, each once, by its place in
    /// the log ([`GraphSession::outcomes_logged`]), never by its text: two deletes described
    /// alike are two toasts, each with its own Undo. Every refusal, every outcome nobody knows
    /// and every commit is toasted. None waits for the graph's other writes, which may be
    /// another view's or the agent's, so a delete's Undo, which only its toast offers, is never
    /// passed over. Nothing is folded: the session logs each write's outcome in an update of
    /// its own, which the window hears before the next is logged, and an outcome names no run
    /// of writes it belongs to. A graph not shown names itself.
    fn toast_outcomes(&mut self, session: &Entity<GraphSession>, cx: &mut Context<Self>) {
        let id = session.entity_id();
        let (server_url, _) = address_of(session, cx);
        let read = session.read(cx);
        let logged = read.outcomes_logged;
        let seen = self.outcomes_toasted.insert(id, logged).unwrap_or(logged);
        let new = (logged.saturating_sub(seen) as usize).min(read.outcomes.len());
        let fresh = read.outcomes[read.outcomes.len() - new..].to_vec();
        let shown = self.session.as_ref() == Some(session);
        let graph = read.graph_id.clone();
        for entry in &fresh {
            let written_to = WrittenTo {
                server_url: server_url.clone(),
                graph: graph.clone(),
                branch: Some(entry.branch.clone()),
            };
            self.toast_outcome(entry, written_to, !shown, cx);
        }
    }

    /// Toasts one outcome, its follow-up bound to the graph and branch written to. `named`
    /// starts the text with the graph's name, for a graph the window does not show.
    fn toast_outcome(
        &mut self,
        latest: &graph_store::OutcomeEntry,
        written_to: WrittenTo,
        named: bool,
        cx: &mut Context<Self>,
    ) {
        let text = if named {
            format!("{}: {}", written_to.graph, toast_text(latest))
        } else {
            toast_text(latest)
        };
        let (kind, label, action): (_, SharedString, Box<dyn Action>) = match &latest.outcome {
            // No publish, no toast (constitution VII); the surface that wrote explains it, and
            // the activity log says where a held commit is.
            OutcomeKind::Committed { commit: None } | OutcomeKind::Held { .. } => return,
            OutcomeKind::Committed {
                commit: Some(commit),
            } => {
                let (label, action) = committed_follow_up(latest, commit);
                (ToastKind::Success, label.into(), action)
            }
            // The write may or may not have landed: the branch's history says which.
            OutcomeKind::Unknown { .. } => (
                ToastKind::Error,
                "Open history".into(),
                Box::new(OpenHistory),
            ),
            OutcomeKind::Refused { .. } => {
                let graph = Some(written_to.graph_at());
                return self.push_toast(ToastKind::Error, text.into(), None, graph, cx);
            }
        };
        let undoes = matches!(latest.outcome, OutcomeKind::Committed { .. })
            && latest.follow_up == Some(graph_store::FollowUp::UndoDelete);
        let graph = Some(written_to.graph_at());
        let follow_up = FollowUp {
            label,
            action,
            on: Some(written_to),
            undoes,
        };
        self.push_toast(kind, text.into(), Some(follow_up), graph, cx);
    }

    /// Runs a toast's follow-up. One about a write acts on the graph and branch written to,
    /// shown first (the graph brought back if parked), so "Open history" never opens another
    /// graph's or branch's history, nor Undo puts rows back elsewhere. A graph closed since is
    /// named, and nothing runs.
    fn run_follow_up(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((action, on)) = self
            .toasts
            .iter()
            .find(|toast| toast.id == id)
            .and_then(|toast| toast.action.as_ref())
            .map(|follow_up| (follow_up.action.boxed_clone(), follow_up.on.clone()))
        else {
            return;
        };
        self.dismiss_toast(id, cx);
        let mut switched = false;
        if let Some(on) = on {
            let Some(session) = self.open_session_for(&on.server_url, &on.graph, cx) else {
                self.show_toast(
                    ToastKind::Error,
                    format!(
                        "Nothing was done: {} is no longer open in this window. Open it, then \
                         check its History.",
                        on.graph
                    ),
                    cx,
                );
                return;
            };
            switched = self.session.as_ref() != Some(&session);
            if let Some(branch) = on.branch {
                session.update(cx, |session, cx| session.set_branch(branch, cx));
            }
            self.show_graph(session.entity_id(), window, cx);
            // Actions reach the window's handlers along the focus path of the frame drawn last,
            // which has no tab of the graph just shown: dispatched from the window, they do.
            if switched {
                window.focus(&self.focus_handle, cx);
            }
        }
        window.dispatch_action(action, cx);
        if switched {
            // One that opened no tab (Undo) leaves the keyboard on the window: the graph's tab
            // takes it.
            cx.defer_in(window, |this, window, cx| {
                if this.focus_handle.is_focused(window) {
                    this.focus_active_tab(window, cx);
                }
            });
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
                // The item asked to close; it has already decided. A tab of a parked graph
                // (a conversation deleted in another window) goes from that graph's panes, so
                // the graph shows without it when shown again.
                ItemEvent::Close => match this.find_item(item.entity_id()) {
                    Some((pane, index)) => this.force_close(pane, index, window, cx),
                    None => this.close_parked(item.entity_id()),
                },
            },
        );
        self._item_subscriptions
            .push((item.entity_id(), subscription));
        if std::mem::take(&mut self.open_beside) {
            let at = self.active_pane;
            let new = self.next_pane;
            if self.center.split(at, new, SplitDirection::Right) {
                self.next_pane += 1;
                self.panes.push(Pane::new(new));
                self.active_pane = new;
            }
        }
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

    /// Keeps the active tab if it is the pane's preview: a view opened from it must not take its
    /// place (spec 020).
    pub fn keep_active_tab(&mut self, _cx: &mut Context<Self>) {
        let pane_id = self.active_pane;
        if let Some(pane) = self.pane_mut(pane_id)
            && let Some(active) = pane.active_item().map(|item| item.item_id())
        {
            pane.keep(active);
        }
    }

    /// Opens a view of what the active tab shows (spec 020). The tab it opens from stays, as a
    /// filtered table in the preview slot must, and `beside` puts a new tab in a pane to the
    /// right. A view already open is activated where it is.
    pub fn open_view(
        &mut self,
        kind: ViewKind,
        beside: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(action) = self.active_item().and_then(|item| {
            let link = item.views(cx).into_iter().find(|link| link.kind == kind)?;
            (!link.current && link.disabled.is_none())
                .then(|| item.view_action(kind, cx))
                .flatten()
        }) else {
            return;
        };
        self.keep_active_tab(cx);
        self.open_beside = beside;
        window.dispatch_action(action, cx);
        // After the action, which the dispatch defers too: a tab it did not add takes no pane.
        cx.defer_in(window, |this, _, _| this.open_beside = false);
    }

    /// Whether the active tab offers a view that can open now.
    fn offers_view(&self, kind: ViewKind, cx: &App) -> bool {
        self.active_item().is_some_and(|item| {
            item.views(cx)
                .iter()
                .any(|link| link.kind == kind && !link.current && link.disabled.is_none())
        })
    }

    fn register_view_action<A: Action>(&mut self, kind: ViewKind) {
        self.register_action_when(
            move |workspace, cx| workspace.offers_view(kind, cx),
            move |workspace, _: &A, window, cx| workspace.open_view(kind, false, window, cx),
        );
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
            // An empty pane takes the focus itself. Past the last tab of a pane that has some
            // (⌘5 with three open), nothing moves: the tab shown keeps the keyboard.
            if self.pane(pane_id).is_some_and(|pane| pane.items.is_empty()) {
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
            // ⌘W in an empty pane (a split of a tab that opens once in a window) closes the
            // pane, as closing its last tab would, unless it is the only one.
            if self.pane(pane).is_some_and(|p| p.items.is_empty()) && self.panes.len() > 1 {
                self.remove_pane(pane);
                let active = self.active_pane;
                let index = self.pane(active).map_or(0, |p| p.active);
                self.activate_in(active, index, false, window, cx);
                cx.notify();
            }
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
        // The keyboard moves to the tab shown now only when the one closed held it: a tab
        // closed from elsewhere (a conversation deleted from the sidebar) leaves it where it is.
        let held_focus = removed.focus_handle(cx).contains_focused(window, cx);
        self._item_subscriptions
            .retain(|(id, _)| *id != removed.item_id());
        let emptied = self.pane(pane_id).is_some_and(|p| p.items.is_empty());
        if emptied && self.panes.len() > 1 {
            self.remove_pane(pane_id);
        }
        if held_focus && (pane_id == self.active_pane || !self.center.contains(pane_id)) {
            let active = self.active_pane;
            let index = self.pane(active).map_or(0, |p| p.active);
            self.activate_in(active, index, false, window, cx);
        }
        cx.notify();
    }

    /// Closes the tab `id` of a parked graph without asking, as [`Self::force_close`] does a
    /// shown one's. A pane left empty closes too, unless it is the graph's only one.
    fn close_parked(&mut self, id: EntityId) {
        let Some(parked) = self
            .parked
            .iter_mut()
            .find(|parked| parked.panes.iter().any(|pane| pane.position(id).is_some()))
        else {
            return;
        };
        let Some(pane) = parked
            .panes
            .iter_mut()
            .find(|pane| pane.position(id).is_some())
        else {
            return;
        };
        let pane_id = pane.id;
        if let Some(index) = pane.position(id) {
            pane.remove(index);
        }
        if pane.items.is_empty() && parked.panes.len() > 1 {
            take_pane(
                &mut parked.center,
                &mut parked.panes,
                &mut parked.active_pane,
                pane_id,
            );
        }
        self._item_subscriptions.retain(|(item, _)| *item != id);
    }

    /// Takes a pane out of the split tree; its neighbour in reading order becomes active.
    fn remove_pane(&mut self, id: PaneId) {
        if !take_pane(&mut self.center, &mut self.panes, &mut self.active_pane, id) {
            return;
        }
        if self.zoomed == Some(Zoomed::Pane(id)) {
            self.zoomed = None;
        }
        self.tab_scrolls.borrow_mut().remove(&id);
        self.tabs_scrolled_to.borrow_mut().remove(&id);
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
        // A pane gone by the time the drop lands takes nothing: the tab stays in its own.
        if self.pane(to_pane).is_none() {
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
            cx.new(|cx| DockHost {
                view,
                scrolls_itself: false,
                focus_handle: cx.focus_handle(),
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
            .chain(self.inspectors.values().map(|view| view.entity_id()))
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

    /// The inspector: the right dock's panel for what the active tab has selected. `None` removes
    /// it. It stays that tab's, and comes back with it.
    pub fn set_inspector(&mut self, view: Option<AnyView>, cx: &mut Context<Self>) {
        match self.active_item().map(|item| item.item_id()) {
            Some(tab) => self.set_inspector_of(tab, view, cx),
            None => self.show_inspector(view, cx),
        }
    }

    /// [`Self::set_inspector`] for one tab, which may have stopped being the active one while
    /// its selection was read: another tab's inspector stays shown.
    pub fn set_inspector_of(
        &mut self,
        tab: EntityId,
        view: Option<AnyView>,
        cx: &mut Context<Self>,
    ) {
        match &view {
            Some(view) => {
                self.host_for(view, cx);
                self.inspectors.insert(tab, view.clone());
            }
            None => {
                self.inspectors.remove(&tab);
            }
        }
        if self.active_item().map(|item| item.item_id()) == Some(tab) {
            self.show_inspector(view, cx);
        } else {
            self.drop_stale_hosts();
        }
    }

    /// Follows the active tab (spec 020): shows its inspector, or says nothing is selected where
    /// an inspector was showing, and tells whoever else follows. Every change of the active tab
    /// notifies the workspace, so this one place sees them all.
    fn follow_active_item(&mut self, cx: &mut Context<Self>) {
        let active = self.active_item().map(|item| item.item_id());
        if active == self.followed_item {
            return;
        }
        self.followed_item = active;
        let open: HashSet<EntityId> = self
            .panes
            .iter()
            .flat_map(|pane| pane.items.iter().map(|item| item.item_id()))
            .collect();
        self.inspectors.retain(|tab, _| open.contains(tab));
        match active.and_then(|tab| self.inspectors.get(&tab).cloned()) {
            Some(view) => self.show_inspector(Some(view), cx),
            None if self.dock_of(INSPECTOR_PANEL).is_some() => {
                self.show_inspector(Some(cx.new(|_| NothingSelected).into()), cx)
            }
            None => self.drop_stale_hosts(),
        }
        cx.emit(WorkspaceEvent::ActiveItemChanged);
    }

    fn show_inspector(&mut self, view: Option<AnyView>, cx: &mut Context<Self>) {
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

    /// The view drawn at the right of the status bar in every state, with the memory and CPU
    /// readout on the window's side of its rule: the updater's (spec 017). `shown` says whether
    /// it has anything to say now; while it has not, it and its rule take no room.
    pub fn set_status_item(
        &mut self,
        view: AnyView,
        shown: impl Fn(&App) -> bool + 'static,
        cx: &mut Context<Self>,
    ) {
        self.status_item = Some(StatusItem {
            view,
            shown: Box::new(shown),
        });
        cx.notify();
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
        let goes_with_graph = view.read(cx).goes_with_graph();
        let asked = view.clone();
        let before_dismiss = Box::new(move |window: &mut Window, cx: &mut App| {
            asked.update(cx, |modal, cx| modal.on_before_dismiss(window, cx))
        });
        self.modal = Some(ActiveModal {
            view: view.into(),
            placement,
            goes_with_graph,
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

    /// Whether a modal is open: a window waiting to ask for a graph does not ask over one.
    pub fn has_modal(&self) -> bool {
        self.modal.is_some()
    }

    pub fn show_toast(
        &mut self,
        kind: ToastKind,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.push_toast(kind, message.into(), None, None, cx);
    }

    /// A toast with one follow-up button, such as "Open settings" (spec 002), run where the
    /// window is when it is pressed.
    pub fn show_toast_with_action(
        &mut self,
        kind: ToastKind,
        message: impl Into<SharedString>,
        label: impl Into<SharedString>,
        action: Box<dyn Action>,
        cx: &mut Context<Self>,
    ) {
        let follow_up = FollowUp {
            label: label.into(),
            action,
            on: None,
            undoes: false,
        };
        self.push_toast(kind, message.into(), Some(follow_up), None, cx);
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

    /// Names each new notice about the servers ([`ConnectionRegistry::notices`]) in a toast
    /// that stays until dismissed, in its own words.
    fn show_server_notices(
        &mut self,
        registry: &Entity<ConnectionRegistry>,
        cx: &mut Context<Self>,
    ) {
        let notices = registry.read(cx).notices().to_vec();
        for notice in &notices {
            if !self.server_notices.contains(notice) {
                self.show_toast(ToastKind::Error, notice.clone(), cx);
            }
        }
        self.server_notices = notices;
    }

    /// Shows a toast, about `graph` when it is about one.
    fn push_toast(
        &mut self,
        kind: ToastKind,
        message: SharedString,
        action: Option<FollowUp>,
        graph: Option<GraphAt>,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_toast;
        self.next_toast += 1;
        self.make_room();
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
            folded: 0,
            graph,
            across: false,
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

    /// Makes room for one more toast within [`TOASTS_SHOWN`], past which they would run below
    /// the window's edge, the newest last: the oldest toast that is not kept ([`Toast::kept`])
    /// goes, one with no follow-up before one with. While every toast shown is kept, errors
    /// fold instead ([`Self::fold_oldest_error`]), so a run of them, such as an agent's
    /// refused writes, takes the place of one, whatever graphs they are about. An Undo, which
    /// only its toast offers, is never taken away nor folded: the stack passes the limit only
    /// when Undos fill it, with at most one error and the toast coming. Undos come only from
    /// deletes the person confirmed, and each goes after [`TOAST_DURATION`].
    fn make_room(&mut self) {
        if self.toasts.len() < TOASTS_SHOWN {
            return;
        }
        let gone = self
            .toasts
            .iter()
            .position(|toast| !toast.kept() && toast.action.is_none())
            .or_else(|| self.toasts.iter().position(|toast| !toast.kept()));
        match gone {
            Some(gone) => {
                self.toasts.remove(gone);
            }
            None => self.fold_oldest_error(),
        }
    }

    /// One toast fewer by folding errors, by graph when one graph can: the oldest error shown on
    /// its own that can fold joins the toast that counts its graph's errors folded, or, with
    /// none up, it and the next of its graph's errors shown alone become that toast, in the
    /// older's place. A fold that stands for one graph opens that graph's log, where each of the
    /// errors folded is listed. With no two errors of one graph up (three graphs', say), the
    /// errors fold across graphs instead ([`Self::fold_across`]), so the stack still makes room.
    /// Errors about a graph fold first, since its log keeps each of them: one about no graph (a
    /// notice about the servers) is in its toast alone, and folds, with the others alike, only
    /// when nothing else can.
    fn fold_oldest_error(&mut self) {
        for graphs_only in [true, false] {
            if self.fold_by_graph(graphs_only) || self.fold_across(graphs_only) {
                return;
            }
        }
    }

    /// Folds the oldest error that can join its graph's fold, or make one with the next of its
    /// graph's errors; with `graphs_only`, only an error about a graph. Whether it folded one.
    fn fold_by_graph(&mut self, graphs_only: bool) -> bool {
        let alone = |toast: &Toast| {
            toast.kind == ToastKind::Error
                && toast.folded == 0
                && (toast.graph.is_some() || !graphs_only)
        };
        for oldest in 0..self.toasts.len() {
            if !alone(&self.toasts[oldest]) {
                continue;
            }
            let graph = self.toasts[oldest].graph.clone();
            if let Some(fold) = self
                .toasts
                .iter_mut()
                .find(|toast| toast.folded > 0 && !toast.across && toast.graph == graph)
            {
                fold.folded += 1;
                fold.message = folded_errors_text(fold.folded, graph.as_ref());
                self.toasts.remove(oldest);
                return true;
            }
            let next = self.toasts[oldest + 1..]
                .iter()
                .position(|toast| alone(toast) && toast.graph == graph);
            if let Some(next) = next {
                self.toasts.remove(oldest + 1 + next);
                let id = self.next_toast;
                self.next_toast += 1;
                self.toasts[oldest] = Toast::folded_errors(id, 2, graph);
                return true;
            }
        }
        false
    }

    /// One toast fewer when no graph's errors fold: the oldest error shown joins the fold made
    /// across graphs before, or, with none up, the two oldest errors shown, alone or folded,
    /// become one, in the older's place, counting every error they stood for
    /// ([`Toast::folded_across`]); with `graphs_only`, only errors about a graph. With fewer
    /// than two errors up, nothing folds: the rest of the stack is Undos. Whether it folded.
    fn fold_across(&mut self, graphs_only: bool) -> bool {
        let errors: Vec<usize> = (0..self.toasts.len())
            .filter(|&at| {
                let toast = &self.toasts[at];
                toast.kind == ToastKind::Error
                    && !toast.across
                    && (toast.graph.is_some() || !graphs_only)
            })
            .collect();
        let stands_for = |toast: &Toast| toast.folded.max(1);
        if let Some(fold) = self.toasts.iter().position(|toast| toast.across) {
            let Some(&oldest) = errors.first() else {
                return false;
            };
            let count = self.toasts[fold].folded + stands_for(&self.toasts[oldest]);
            self.toasts[fold].folded = count;
            self.toasts[fold].message = folded_errors_text(count, None);
            self.toasts.remove(oldest);
            return true;
        }
        let [older, newer, ..] = errors[..] else {
            return false;
        };
        let count = stands_for(&self.toasts[older]) + stands_for(&self.toasts[newer]);
        self.toasts.remove(newer);
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts[older] = Toast::folded_across(id, count);
        true
    }

    pub fn dismiss_toast(&mut self, id: usize, cx: &mut Context<Self>) {
        self.toasts.retain(|t| t.id != id);
        cx.notify();
    }

    /// The text of each toast shown, oldest first: for the tests of crates that show toasts.
    #[cfg(any(test, feature = "test-support"))]
    pub fn toast_messages(&self) -> Vec<SharedString> {
        self.toasts
            .iter()
            .map(|toast| toast.message.clone())
            .collect()
    }

    // ---- actions ----

    /// Registers a handler for an action anywhere in the window. Feature crates call this from
    /// `cx.observe_new::<Workspace>`.
    pub fn register_action<A: Action>(
        &mut self,
        handler: impl Fn(&mut Workspace, &A, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        self.register(None, handler);
    }

    /// As [`Self::register_action`], registered only while `when` holds: an action the window
    /// cannot take now is not offered by the palette or the keys (constitution VIII). It is
    /// asked at every draw, so it reads what it needs cheaply.
    pub fn register_action_when<A: Action>(
        &mut self,
        when: impl Fn(&Workspace, &App) -> bool + 'static,
        handler: impl Fn(&mut Workspace, &A, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        self.register(Some(Rc::new(when)), handler);
    }

    fn register<A: Action>(
        &mut self,
        when: Option<ActionCondition>,
        handler: impl Fn(&mut Workspace, &A, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let handler = Rc::new(handler);
        self.actions.push(ActionRegistration {
            when,
            register: Rc::new(move |div, _window, cx| {
                let handler = handler.clone();
                div.on_action(cx.listener(move |workspace, action: &A, window, cx| {
                    handler(workspace, action, window, cx)
                }))
            }),
        });
    }

    // ---- rendering ----

    /// A column's header strip (spec 002, after Delta): fixed height, and a place
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

    /// The sidebar, drawn `width` wide ([`panels::side_widths`]).
    fn render_sidebar(&self, width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = studio_ui::ds::surface::chrome(cx);
        let hairline = studio_ui::ds::surface::rule(cx);
        // What can be listed, in words, one row each (spec 020). The active one's own buttons
        // (new, filter) sit at its end.
        let nav = v_flex()
            .id("sidebar-nav")
            .flex_none()
            .py(px(studio_ui::ds::space::XS))
            .when(!studio_ui::ds::is_glass(cx), |d| {
                d.border_b_1().border_color(hairline)
            })
            .children(self.left.panels.iter().enumerate().map(|(index, panel)| {
                let lit = index == self.left.active;
                let id = panel.id.to_string();
                let tooltip_id = id.clone();
                let title = panel.title.clone();
                studio_ui::ds::SidebarRow::new(("sidebar-nav", index), panel.title.clone())
                    .icon(panel.icon)
                    .selected(lit)
                    .when_some(panel.actions.clone().filter(|_| lit), |row, actions| {
                        row.end(actions)
                    })
                    .tooltip_fn(move |_, cx| {
                        Tooltip::for_action(
                            title.clone(),
                            &studio_actions::workspace::ActivatePanel {
                                id: tooltip_id.clone(),
                            },
                            cx,
                        )
                    })
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(studio_actions::workspace::ActivatePanel { id: id.clone() }),
                            cx,
                        )
                    })
            }));
        // The content scrolls in its cached host.
        let body = v_flex().id("sidebar-panel").flex_1().min_h_0().children(
            self.left
                .active_panel()
                .map(|panel| div().size_full().child(self.panel_body(panel, cx))),
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
            .w(px(width))
            .h_full()
            .flex_none()
            .child(nav)
            .child(body)
            .child(resize_handle(PanelPosition::Left))
    }

    /// The top bar (spec 020): which server, graph and branch the window reads, always in the
    /// same place, with what shows and hides the frame's docks. It takes the window's title bar
    /// band, so the traffic lights sit in it.
    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shown = self.session.is_some();
        let hairline = studio_ui::ds::surface::rule(cx);
        let docks_right = shown && !(self.bottom.panels.is_empty() && self.right.panels.is_empty());
        self.strip("top-bar", HEADER_HEIGHT, cx)
            .debug_selector(|| "top-bar".into())
            .pr(px(studio_ui::ds::space::SM))
            .gap(px(studio_ui::ds::space::XS))
            .bg(studio_ui::ds::surface::status_bar(cx))
            .border_b_1()
            .border_color(hairline)
            .map(|strip| match lights_room(self.lights_shown) {
                Some(room) => strip.child(div().w(px(room)).flex_none()),
                None => strip.pl(px(studio_ui::ds::space::SM)),
            })
            .when(shown && !self.left.panels.is_empty(), |bar| {
                bar.child(self.render_sidebar_toggle())
                    .child(studio_ui::ds::divider_v(cx))
            })
            .child(self.render_graph_switcher(cx))
            .children(self.render_branch(cx))
            .child(div().flex_1())
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
            )
            .when(docks_right, |bar| {
                bar.child(studio_ui::ds::divider_v(cx))
                    .child(self.render_panel_buttons(PanelPosition::Bottom, cx))
                    .child(self.render_panel_buttons(PanelPosition::Right, cx))
            })
    }

    /// The top bar's first button, as Zed's sidebar toggle: shows or hides the sidebar, its icon
    /// drawn as the sidebar is.
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
                // In the top bar: its menu opens below.
                let (anchor, attach) = (gpui::Anchor::TopRight, gpui::Anchor::BottomRight);
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
        self.toggle_zoom_of(Zoomed::Pane(self.active_pane), window, cx);
    }

    /// Zooms `target` over the document area, or ends any zoom: ⇧⎋ zooms the active pane, a
    /// pane's or a dock's header button its own. The keyboard leaves what the zoom covers, or
    /// it would go on driving a panel or a tab nobody sees: a zoomed pane takes it from the
    /// docks and the other panes, a zoomed dock from the panes and the other docks.
    fn toggle_zoom_of(&mut self, target: Zoomed, window: &mut Window, cx: &mut Context<Self>) {
        self.zoomed = match self.zoomed {
            Some(_) => None,
            None => Some(target),
        };
        let docks = [
            PanelPosition::Left,
            PanelPosition::Right,
            PanelPosition::Bottom,
        ];
        let in_pane = |pane: PaneId| self.pane_has_focus(pane, window, cx);
        match self.zoomed {
            Some(Zoomed::Pane(zoomed)) => {
                let covered = docks
                    .into_iter()
                    .any(|dock| self.dock_has_focus(dock, window, cx))
                    || self.panes.iter().any(|p| p.id != zoomed && in_pane(p.id));
                if covered {
                    let index = self.pane(zoomed).map_or(0, |p| p.active);
                    self.activate_in(zoomed, index, false, window, cx);
                }
            }
            Some(Zoomed::Dock(zoomed)) => {
                let covered = docks
                    .into_iter()
                    .any(|dock| dock != zoomed && self.dock_has_focus(dock, window, cx))
                    || self.panes.iter().any(|p| in_pane(p.id));
                if covered {
                    self.focus_dock_panel(zoomed, window, cx);
                }
            }
            None => {}
        }
        cx.notify();
    }

    /// Whether the keyboard is in the tab `pane` shows.
    fn pane_has_focus(&self, pane: PaneId, window: &Window, cx: &App) -> bool {
        self.pane(pane)
            .and_then(|pane| pane.active_item())
            .is_some_and(|item| item.focus_handle(cx).contains_focused(window, cx))
    }

    /// Gives the keyboard to the active panel of the dock at `position`: the focus the panel
    /// names for itself, else its body.
    fn focus_dock_panel(&self, position: PanelPosition, window: &mut Window, cx: &mut App) {
        let dock = match position {
            PanelPosition::Left => &self.left,
            PanelPosition::Right => &self.right,
            PanelPosition::Bottom => &self.bottom,
        };
        let Some(panel) = dock.active_panel() else {
            return;
        };
        let focus = panel
            .focus
            .as_ref()
            .and_then(|focus| focus(cx))
            .or_else(|| {
                self.dock_hosts
                    .get(&panel.view.entity_id())
                    .map(|host| host.read(cx).focus_handle.clone())
            });
        if let Some(focus) = focus {
            window.focus(&focus, cx);
        }
    }

    /// Whether the keyboard is in a panel of the dock at `position`: in any element of its body
    /// (a filter or rename field included), or in the focus the panel names for itself.
    fn dock_has_focus(&self, position: PanelPosition, window: &Window, cx: &App) -> bool {
        let dock = match position {
            PanelPosition::Left => &self.left,
            PanelPosition::Right => &self.right,
            PanelPosition::Bottom => &self.bottom,
        };
        dock.panels.iter().any(|panel| {
            let in_body = self
                .dock_hosts
                .get(&panel.view.entity_id())
                .is_some_and(|host| host.read(cx).focus_handle.contains_focused(window, cx));
            in_body
                || panel
                    .focus
                    .as_ref()
                    .and_then(|focus| focus(cx))
                    .is_some_and(|focus| focus.contains_focused(window, cx))
        })
    }

    /// Gives the keyboard to the active tab: the dock that had it is hidden or covered, and the
    /// keys would go on driving a panel nobody sees.
    fn focus_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.pane(self.active_pane).map_or(0, |p| p.active);
        self.activate_in(self.active_pane, index, false, window, cx);
    }

    /// Where the window reads, the first part of the bar's breadcrumb: the connection's status
    /// dot, the server and the graph. It opens the graph switcher: the graphs open in this
    /// window, another graph, closing this one. The connection's state shows beside it only
    /// when it is not ready, and always in its tooltip, with the round trip and the actor.
    /// Whether the window's server answers: its connection is ready.
    fn server_ready(&self, cx: &App) -> bool {
        self.session.as_ref().is_some_and(|session| {
            session.read(cx).connection.read(cx).state == ConnectionState::Ready
        })
    }

    /// Counts the connection dot's beats, one every `motion::PULSE`, while it is `live`.
    /// Not live, the count stops (its task dropped), so a dot nobody sees asks for no frames.
    fn count_beats(&mut self, live: bool, cx: &mut Context<Self>) {
        if !live {
            self._beats = None;
            return;
        }
        if self._beats.is_some() {
            return;
        }
        self._beats = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(studio_ui::motion::PULSE)
                    .await;
                let counted = this.update(cx, |this, cx| {
                    this.beat += 1;
                    cx.notify();
                });
                if counted.is_err() {
                    break;
                }
            }
        }));
    }

    fn render_graph_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (color, server, url, state, ready, actor, graph, unavailable) = match &self.session {
            Some(session) => {
                let s = session.read(cx);
                let actor = s.actor().map(str::to_owned);
                let connection = s.connection.read(cx);
                let ready = connection.state == ConnectionState::Ready;
                // The server's round trip from the last health check, beside its state.
                let state = match (&connection.state, connection.latency) {
                    (ConnectionState::Ready, Some(latency)) => {
                        format!(
                            "{} · {}",
                            connection.label(),
                            studio_ui::format_elapsed(latency)
                        )
                    }
                    _ => connection.label(),
                };
                (
                    connection_color(&connection.state),
                    connection.server.name.clone(),
                    connection.server.url.clone(),
                    state,
                    ready,
                    actor,
                    Some(s.graph_id.clone()),
                    s.unavailable.clone(),
                )
            }
            None => (
                Color::Muted,
                "No server".to_owned(),
                String::new(),
                "Not connected".to_owned(),
                false,
                None,
                None,
                None,
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
        let mut tooltip = connection_tooltip(&url, &state, actor.as_deref());
        if let Some(reason) = &unavailable {
            tooltip = format!(
                "This graph is unavailable: {}. Reads resume when it is ready. · {tooltip}",
                studio_ui::clause(reason)
            );
        }
        let trigger = ButtonLike::new("graph-switcher")
            .tooltip(Tooltip::text(tooltip))
            .child(
                h_flex()
                    .px(px(studio_ui::ds::space::XS))
                    .gap(px(studio_ui::ds::space::SM))
                    .items_center()
                    // A ready server's dot beats while the window is in front.
                    .child(studio_ui::ds::live_dot(
                        "connection-dot",
                        color.color(cx),
                        self._beats.is_some().then_some(self.beat),
                        cx,
                    ))
                    .child(studio_ui::ds::text::muted(server).single_line())
                    .when_some(graph, |row, graph| {
                        row.child(studio_ui::ds::text::body("/").color(Color::Disabled))
                            .child(studio_ui::ds::text::body(graph).single_line())
                    })
                    .when(unavailable.is_some(), |row| {
                        row.child(studio_ui::ds::dot(Color::Warning))
                    })
                    // Ready, the dot says so; anything else is spelled out.
                    .when(!ready, |row| {
                        row.child(studio_ui::ds::text::muted(state).single_line())
                    })
                    .when(others > 0, |row| {
                        row.child(studio_ui::ds::sidebar_count(format!(
                            "+{}",
                            studio_ui::plural(others, "graph", "graphs")
                        )))
                    })
                    .child(studio_ui::ds::icon::meta(IconName::ChevronUpDown)),
            );
        PopoverMenu::new("graph-switcher-menu")
            .anchor(gpui::Anchor::TopLeft)
            .attach(gpui::Anchor::BottomLeft)
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
                    Some(studio_ui::ds::dot(Color::Warning))
                } else if stale {
                    Some(studio_ui::ds::dot(Color::Muted))
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
    /// tabs.
    fn render_pane_header(&self, pane_id: PaneId, cx: &mut Context<Self>) -> impl IntoElement {
        let header_bg = studio_ui::ds::surface::header_strip(cx);
        let hairline = studio_ui::ds::surface::rule(cx);
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
                                .action("Go to Node…", Box::new(studio_actions::workspace::GoTo))
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
                div()
                    .flex_none()
                    .debug_selector(move || format!("zoom-pane-{pane_id}"))
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
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.toggle_zoom_of(Zoomed::Pane(pane_id), window, cx)
                            },
                        )),
                    ),
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
        let active = self.pane(pane_id).and_then(|pane| pane.active_item());
        // A tab that reads the graph shows the sign-in its server asks for instead of the 401s
        // its reads would get.
        let prompt = active
            .filter(|item| item.reads_graph(cx))
            .and_then(|_| sign_in_prompt(&self.session.as_ref()?.read(cx).connection, cx));
        let body = match active {
            Some(_) if prompt.is_some() => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .children(prompt)
                .into_any_element(),
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
                let own = this
                    .find_item(dragged.item)
                    .is_some_and(|(pane, _)| pane == pane_id);
                match direction {
                    Some(direction) => this.drop_split(dragged, pane_id, direction, window, cx),
                    // Back on its own pane's content (a drag let go): it stays where it was.
                    None if own => cx.notify(),
                    None => this.drop_tab(dragged, pane_id, None, window, cx),
                }
            }));
        v_flex()
            .id(SharedString::from(format!("pane-{pane_id}")))
            .size_full()
            .min_w_0()
            .min_h_0()
            // An empty pane has no tab to take the keyboard: a click in it makes it active and
            // gives the workspace the keyboard ([`Self::activate_in`]), so the next frame, which
            // follows the keyboard, keeps it so. Any other is made active by its tab taking the
            // keyboard (`render`).
            .capture_any_mouse_down(cx.listener(move |this, _, window, cx| {
                if this.active_pane != pane_id
                    && this.pane(pane_id).is_some_and(|pane| pane.items.is_empty())
                {
                    this.activate_in(pane_id, 0, false, window, cx);
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
        let (axis, members, flexes) = match member {
            Member::Pane(id) => {
                // A zoomed pane is drawn over the document area instead (`render_zoomed`).
                if self.zoomed == Some(Zoomed::Pane(*id)) {
                    return div().size_full().into_any_element();
                }
                return self.render_pane(*id, floor, cx);
            }
            Member::Split {
                axis,
                members,
                flexes,
            } => (*axis, members, flexes),
        };
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
        status: Option<ItemStatus>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_background = studio_ui::ds::surface::status_bar(cx);
        let glass = studio_ui::ds::is_glass(cx);
        let hairline = studio_ui::ds::surface::rule(cx);
        let mut status = status.unwrap_or_default();
        let keys = std::mem::take(&mut status.keys);
        let running = self.session.as_ref().and_then(|session| {
            let s = session.read(cx);
            match (s.reads_in_flight, s.writes_in_flight) {
                (0, 0) => None,
                (reads, 0) => Some(format!(
                    "{} running",
                    studio_ui::plural(reads, "query", "queries")
                )),
                (_, writes) => Some(format!("Writing ({writes})…")),
            }
        });
        let left = h_flex()
            .min_w_0()
            .gap(px(studio_ui::ds::space::XS))
            .items_center()
            .overflow_hidden()
            .children(status.start);
        let has_end = !status.end.is_empty();
        let performance = self.render_performance();
        let item = self
            .status_item
            .as_ref()
            .filter(|item| (item.shown)(cx))
            .map(|item| item.view.clone());
        // The window's side of the bar: its own item and the memory and CPU readout.
        let readout = item.is_some() || performance.is_some();
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
            .children(status.end)
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
            .when(has_end && readout, |bar| {
                bar.child(studio_ui::ds::divider_v(cx))
            })
            .children(item)
            .children(performance);
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

    /// The breadcrumb's last part, after the graph switcher: the branch the window reads and
    /// edits, its menu switching it, and "read only" when the credential may not change it.
    fn render_branch(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let session = self.session.as_ref()?;
        let s = session.read(cx);
        let branch = s.branch.clone();
        // The access route refused changes on the window's branch: its edits are disabled, so
        // the bar says so beside the name (spec 002).
        let read_only = s.refusal(AccessAction::Change, &AccessScope::branch(&branch));
        let branch_tooltip = if read_only.is_some() {
            format!(
                "Reads in this window use {branch}, which your credential may not change. Switch \
                 branch."
            )
        } else {
            format!("Reads and edits in this window use {branch}. Switch branch.")
        };
        let session_for_menu = session.clone();
        Some(
            h_flex()
                .gap(px(studio_ui::ds::space::XS))
                .items_center()
                .flex_none()
                .child(studio_ui::ds::text::body("/").color(Color::Disabled))
                .child(
                    PopoverMenu::new("branch-menu")
                        .anchor(gpui::Anchor::TopLeft)
                        .attach(gpui::Anchor::BottomLeft)
                        .trigger(
                            studio_ui::ds::bar_button("branch", branch.clone())
                                .style(ButtonStyle::Subtle)
                                .tooltip(Tooltip::text(branch_tooltip))
                                .start_icon(Icon::new(IconName::GitBranch))
                                .end_icon(Icon::new(IconName::ChevronUpDown)),
                        )
                        .menu(move |window, cx| {
                            let session = session_for_menu.clone();
                            // Read when the menu opens, not copied at every frame drawn.
                            let (current, branches) = {
                                let s = session.read(cx);
                                (s.branch.clone(), s.branches.clone())
                            };
                            // A list not read, or that could not be, is read for the next look.
                            if matches!(branches, Loadable::NotLoaded | Loadable::Failed(_)) {
                                session.update(cx, |s, cx| s.reload_branches(cx));
                            }
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                menu = menu.header("Branches");
                                let note =
                                    |text: String| ContextMenuEntry::new(text).disabled(true);
                                match &branches {
                                    Loadable::Loaded(list) => {
                                        for branch in list {
                                            let session = session.clone();
                                            let name = branch.clone();
                                            menu = menu.toggleable_entry(
                                                branch.clone(),
                                                *branch == current,
                                                IconPosition::Start,
                                                None,
                                                move |_, cx| {
                                                    session.update(cx, |s, cx| {
                                                        s.set_branch(name.clone(), cx)
                                                    });
                                                },
                                            );
                                        }
                                    }
                                    Loadable::Failed(why) => {
                                        menu = menu.item(note(format!(
                                            "The branches could not be read: {why}"
                                        )));
                                    }
                                    Loadable::NotLoaded | Loadable::Loading => {
                                        menu = menu.item(note("Reading the branches…".into()));
                                    }
                                }
                                menu
                            }))
                        }),
                )
                .when_some(read_only, |row, reason| {
                    row.child(
                        div()
                            .id("branch-read-only")
                            .debug_selector(|| "branch-read-only".into())
                            .flex_none()
                            .tooltip(move |_, cx| {
                                Tooltip::with_meta(
                                    reason.clone(),
                                    None,
                                    "Edits here are off; switch to a branch it may change",
                                    cx,
                                )
                            })
                            .child(studio_ui::ds::text::muted("read only").single_line()),
                    )
                }),
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

    /// The bottom dock: its panel's header, then the panel, drawn `height` tall
    /// ([`panels::bottom_height`]). `alone` when it is not inside the document column, so on
    /// glass it is a sheet of its own.
    fn render_bottom_dock(
        &self,
        alone: bool,
        height: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
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
                .h(px(height))
                .w_full()
                .relative()
                .flex_none()
                .overflow_hidden()
                .when(!glass, |d| d.bg(panel_background))
                .when(!(glass && alone), |d| d.border_t_1().border_color(hairline))
                .child(self.render_dock_header(PanelPosition::Bottom, cx))
                .child(div().flex_1().min_h_0().child(self.panel_body(panel, cx)))
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
        .children(dock.active_panel().map(dock_title))
        .child(
            div()
                .flex_none()
                .debug_selector(move || format!("zoom-dock-{position:?}"))
                .child(
                    IconButton::new(
                        SharedString::from(format!("zoom-dock-{position:?}")),
                        if zoomed {
                            IconName::Minimize
                        } else {
                            IconName::Maximize
                        },
                    )
                    // ⇧⎋ ends any zoom but zooms the active pane, not this dock, so only "Zoom
                    // out" shows it.
                    .tooltip(move |_, cx| {
                        if zoomed {
                            Tooltip::for_action("Zoom out", &ToggleZoom, cx)
                        } else {
                            Tooltip::simple("Zoom in", cx)
                        }
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_zoom_of(Zoomed::Dock(position), window, cx)
                    })),
                ),
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
    /// the column's sides. The body holds its host's focus handle, so the keyboard anywhere in
    /// it is the dock's ([`Self::dock_has_focus`]).
    fn panel_body(&self, panel: &PanelEntry, cx: &App) -> AnyElement {
        let body = match panel.home {
            PanelPosition::Bottom => panel.view.clone().cached(cached_fill()).into_any_element(),
            PanelPosition::Left => div()
                .size_full()
                .pt(px(studio_ui::ds::space::SM))
                .child(self.hosted(&panel.view))
                .into_any_element(),
            PanelPosition::Right => self.hosted(&panel.view),
        };
        match self.dock_hosts.get(&panel.view.entity_id()) {
            Some(host) => div()
                .size_full()
                .track_focus(&host.read(cx).focus_handle)
                .child(body)
                .into_any_element(),
            None => body,
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
                    .child(div().flex_1().min_h_0().child(self.panel_body(panel, cx)))
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
    /// The right dock: its panel's header, then the panel, drawn `width` wide
    /// ([`panels::side_widths`]).
    fn render_right_dock(&self, width: f32, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.right.visible() || self.zoomed == Some(Zoomed::Dock(PanelPosition::Right)) {
            return None;
        }
        let panel = self.panel_body(self.right.active_panel()?, cx);
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
                .w(px(width))
                .h_full()
                .flex_none()
                .child(resize_handle(PanelPosition::Right))
                .child(self.render_dock_header(PanelPosition::Right, cx))
                .child(div().flex_1().min_h_0().child(panel)),
        )
    }

    /// Follows a dock edge being dragged. Sizes are measured from the window's edges, inside the
    /// glass style's gap and above the status bar.
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
        // Under the top bar and a pane's tabs, so a toast never covers the scope or a tab.
        v_flex()
            .absolute()
            .top(px(HEADER_HEIGHT * 2. + studio_ui::ds::space::MD))
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
                    .child(studio_ui::ds::dot(color))
                    // `min_w_0` lets the text wrap inside the toast instead of widening it.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(studio_ui::ds::text::body(toast.message.clone())),
                    )
                    .children(toast.action.as_ref().map(|follow_up| {
                        studio_ui::ds::button(("toast-action", id), follow_up.label.clone())
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.run_follow_up(id, window, cx)
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

/// Takes pane `id` out of a graph's split tree and panes, shown or parked; when it was
/// `active`, its neighbour in reading order becomes active. False for the graph's last pane,
/// which stays.
fn take_pane(
    center: &mut Member,
    panes: &mut Vec<Pane<Box<dyn ItemHandle>>>,
    active: &mut PaneId,
    id: PaneId,
) -> bool {
    let order = center.panes();
    if !center.remove(id) {
        return false;
    }
    panes.retain(|pane| pane.id != id);
    if *active == id {
        let position = order.iter().position(|p| *p == id).unwrap_or_default();
        *active = order
            .get(position.wrapping_sub(1))
            .or_else(|| order.get(position + 1))
            .copied()
            .filter(|p| *p != id)
            .unwrap_or_else(|| center.top_left());
    }
    true
}

/// The server address and graph id an open graph is found by in memory
/// ([`Workspace::open_session_for`]): what a link, the Open dialog and a toast's follow-up name.
/// Nothing is saved under it.
fn address_of(session: &Entity<GraphSession>, cx: &App) -> (String, String) {
    let session = session.read(cx);
    let url = session.connection.read(cx).server.url.clone();
    (url, session.graph_id.clone())
}

/// The key a graph's tabs and drafts are saved under: the graph's one key, its CLI target's
/// name and address and its id (`GraphSession::overrides_key`), as the saved windows name the
/// graph and its settings, saved queries and conversations are filed (spec 002).
fn saved_key_of(session: &Entity<GraphSession>, cx: &App) -> String {
    session.read(cx).overrides_key()
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
                preview: false,
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
        OutcomeKind::Held {
            commit,
            held: graph_store::HeldAs::Proposal,
        } => format!(
            "{} · held as commit {}, on no branch until published to {}",
            entry.description,
            studio_ui::short_commit_id(commit),
            entry.branch
        ),
        OutcomeKind::Held {
            commit,
            held: graph_store::HeldAs::Snapshot,
        } => format!(
            "{} · saved as snapshot {}, on no branch",
            entry.description,
            studio_ui::short_commit_id(commit)
        ),
        OutcomeKind::Refused { message } => format!(
            "{} · refused: {}. Nothing was written.",
            entry.description,
            studio_ui::clause(message)
        ),
        // Its history is the branch's, which Open history shows.
        OutcomeKind::Unknown { .. } => format!(
            "{} · outcome unknown on {}. Check its history before trying again.",
            entry.description, entry.branch
        ),
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Every change to tabs or panels re-renders the workspace, so this sees all of them. The
        // workspace also renders whenever anything inside it notifies (a streamed reply, a
        // hover), so the state is compared at most every `SAVE_CHECK`, not on every frame.
        // The connection dot beats while its server is ready and someone can see it; render
        // runs when either changes (a connection's state, the window's activation).
        let live =
            self.server_ready(cx) && window.is_window_active() && studio_ui::motion::enabled(cx);
        self.count_beats(live, cx);
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
        // The active pane is the one whose tab has the keyboard, as Zed follows focus: a press
        // that moves none (a drag by a pane's header, its empty gutter) changes nothing.
        let focused_pane = self
            .panes
            .iter()
            .find(|pane| {
                pane.active_item()
                    .is_some_and(|item| item.focus_handle(cx).contains_focused(window, cx))
            })
            .map(|pane| pane.id);
        if let Some(pane) = focused_pane {
            self.active_pane = pane;
        }
        // A focused view gone (an inspector replaced, a panel removed, while it held the
        // keyboard) leaves keys dispatched from the window's root, where no tab hears them: the
        // active tab takes them once the frame is out.
        if window.focused(cx).is_none() {
            cx.defer_in(window, |this, window, cx| {
                if window.focused(cx).is_none() {
                    this.focus_active_tab(window, cx);
                }
            });
        }
        // As Zed's workspace does: the window's rem size is the UI font size, which every `ui`
        // size is relative to, and the root carries the UI font.
        let ui_font = theme_settings::setup_ui_font(window, cx);
        // What the window can do now: an action it cannot is not offered (constitution VIII).
        let has_graph = self.session.is_some();
        let other_graphs = self.graph_order.len() > 1;
        let pane = self.pane(self.active_pane);
        let (can_back, can_forward) =
            pane.map_or((false, false), |p| (p.can_step(true), p.can_step(false)));
        let (closes_others, closes_all) = pane.map_or((false, false), |p| {
            (!p.others(p.active).is_empty(), !p.all_unpinned().is_empty())
        });
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
            .when(other_graphs, |root| {
                root.on_action(cx.listener(|this, _: &ActivateNextGraph, window, cx| {
                    this.show_graph_step(true, window, cx)
                }))
                .on_action(cx.listener(
                    |this, _: &ActivatePreviousGraph, window, cx| {
                        this.show_graph_step(false, window, cx)
                    },
                ))
            })
            .when(has_graph, |root| {
                root.on_action(
                    cx.listener(|this, _: &CloseGraph, window, cx| this.close_graph(window, cx)),
                )
            })
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
            .when(closes_others, |root| {
                root.on_action(cx.listener(|this, _: &CloseOtherItems, window, cx| {
                    let ids = this
                        .pane(this.active_pane)
                        .map(|p| p.others(p.active))
                        .unwrap_or_default();
                    this.close_clean(ids, window, cx);
                }))
            })
            .when(closes_all, |root| {
                root.on_action(cx.listener(|this, _: &CloseAllItems, window, cx| {
                    let ids = this
                        .pane(this.active_pane)
                        .map(|p| p.all_unpinned())
                        .unwrap_or_default();
                    this.close_clean(ids, window, cx);
                }))
            })
            .on_action(cx.listener(
                |this, action: &studio_actions::workspace::ActivatePanel, window, cx| {
                    this.show_panel(&action.id, window, cx)
                },
            ))
            .when(can_back, |root| {
                root.on_action(cx.listener(|this, _: &GoBack, window, cx| {
                    this.navigate(this.active_pane, true, window, cx)
                }))
            })
            .when(can_forward, |root| {
                root.on_action(cx.listener(|this, _: &GoForward, window, cx| {
                    this.navigate(this.active_pane, false, window, cx)
                }))
            })
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
            .when(has_graph, |root| {
                root.on_action(cx.listener(|this, _: &Refresh, _, cx| {
                    if let Some(session) = &this.session {
                        session.update(cx, |s, cx| s.refresh(cx));
                    }
                }))
            });
        for registration in self.actions.clone() {
            if registration.when.as_ref().is_none_or(|when| when(self, cx)) {
                root = (registration.register)(root, window, cx);
            }
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
        let status = self
            .pane(self.active_pane)
            .and_then(|pane| pane.active_item())
            .and_then(|item| item.status(window, cx));
        // Where the bottom dock spans, as Zed's `bottom_dock_layout`.
        let layout = cx
            .try_global::<StudioSettings>()
            .map(|settings| settings.studio.bottom_dock_layout)
            .unwrap_or_default();
        let contained = layout == BottomDockLayout::Contained;
        // The docks give way to a window made smaller than they are, the sizes held kept.
        let viewport = window.viewport_size();
        let right_shown =
            self.right.visible() && self.zoomed != Some(Zoomed::Dock(PanelPosition::Right));
        let (left_width, right_width) = panels::side_widths(
            if sidebar_shown { self.left.size } else { 0. },
            if right_shown { self.right.size } else { 0. },
            f32::from(viewport.width),
        );
        let bottom_height = panels::bottom_height(self.bottom.size, f32::from(viewport.height));
        let bottom = self.render_bottom_dock(!contained, bottom_height, cx);
        let sidebar = sidebar_shown.then(|| self.render_sidebar(left_width, cx).into_any_element());
        let right = self
            .render_right_dock(right_width, cx)
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
        root.child(self.render_top_bar(cx))
            .child(
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
            .child(self.render_status_bar(status, cx))
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

/// Room for the traffic lights at the start of the top bar. `None` without lights, as Zed's
/// title bar: off macOS, and in full screen.
fn lights_room(shown: bool) -> Option<f32> {
    shown.then_some(TRAFFIC_LIGHTS_WIDTH)
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

/// How often the status bar's memory and CPU are read.
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

/// What a close asked about closes: a tab, the shown graph, the window, or Studio, quitting
/// or restarting into an update.
#[derive(Debug, Clone, Copy)]
enum Closing {
    Tab(EntityId),
    Graph,
    Window,
    Quit,
    Restart,
}

impl Closing {
    /// The confirm button's label: what pressing it does.
    fn confirm(self) -> &'static str {
        match self {
            Closing::Quit => "Quit",
            Closing::Restart => "Restart",
            Closing::Tab(_) | Closing::Graph | Closing::Window => "Close",
        }
    }

    /// How much it closes: each holds the ones before it.
    fn reach(self) -> u8 {
        match self {
            Closing::Tab(_) => 0,
            Closing::Graph => 1,
            Closing::Window => 2,
            Closing::Quit | Closing::Restart => 3,
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

impl ModalView for ConfirmClose {
    /// A tab's or the graph's question is about the graph shown; the window's and Quit's are
    /// about every graph.
    fn goes_with_graph(&self) -> bool {
        matches!(self.closing, Closing::Tab(_) | Closing::Graph)
    }
}

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
                Closing::Restart => cx.defer(|cx| restart(true, cx)),
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
            Closing::Quit => ("Quit Omnigraph Studio?".into(), lost),
            Closing::Restart => ("Restart to update Omnigraph Studio?".into(), lost),
        }
    }
}

/// Quits Studio, unless a window holds a write still running: quitting would lose its outcome
/// (spec 002), so that window comes forward with a toast saying so. Unless `confirmed`, tabs
/// with unsaved work in any window are named first, in a dialog that quits once confirmed.
pub fn quit(confirmed: bool, cx: &mut App) {
    leave(Closing::Quit, confirmed, cx);
}

/// Restarts Studio into the update that is ready (spec 017), asked about as Quit is.
pub fn restart(confirmed: bool, cx: &mut App) {
    leave(Closing::Restart, confirmed, cx);
}

fn leave(closing: Closing, confirmed: bool, cx: &mut App) {
    let verb = match closing {
        Closing::Restart => "Restart",
        _ => "Quit",
    };
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
                    format!("A write to {graph} is still running. {verb} when it finishes."),
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
                    workspace.confirm_close(closing, unsaved, window, cx);
                    window.activate_window();
                })
                .ok();
            return;
        }
    }
    match closing {
        Closing::Restart => cx.restart(),
        _ => cx.quit(),
    }
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
                        studio_ui::ds::action_button("close-anyway", self.closing.confirm())
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
        ConnectionState::ContractMismatch { .. } => Color::Warning,
        ConnectionState::Unauthorized | ConnectionState::Unreachable(_) => Color::Error,
    }
}

/// What the connection chip's tooltip says: the server, its state, and the actor the
/// credential resolves to once the access route has named it.
fn connection_tooltip(url: &str, state: &str, actor: Option<&str>) -> String {
    match actor {
        Some(actor) => format!("{url} · {state} · as {actor} · Switch graph"),
        None => format!("{url} · {state} · Switch graph"),
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
            // Holding its focus, as a tab does: an action dispatched from it reaches the
            // window's handlers once it is drawn.
            div().size_full().track_focus(&self.focus_handle)
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

    /// A tab that draws `name`, reading the window's graph or not.
    struct TabProbe {
        focus_handle: FocusHandle,
        name: &'static str,
        reads: bool,
    }

    impl EventEmitter<ItemEvent> for TabProbe {}

    impl Focusable for TabProbe {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for TabProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let name = self.name;
            div()
                .size_full()
                .track_focus(&self.focus_handle)
                .debug_selector(move || name.into())
        }
    }

    impl Item for TabProbe {
        fn tab_title(&self, _: &App) -> SharedString {
            self.name.into()
        }

        fn reads_graph(&self) -> bool {
            self.reads
        }
    }

    /// While the graph's server asks for a sign-in nobody holds, a tab that reads the graph says
    /// who asks and where, and offers Sign in, in place of its refused reads; a tab that reads
    /// nothing from the server is drawn as it is. Once the server stops asking, the tab is drawn
    /// again: the pane read the connection when it drew, so the connection's change redraws it.
    #[gpui::test]
    fn a_tab_that_reads_the_graph_shows_the_sign_in_its_server_asks_for(
        cx: &mut gpui::TestAppContext,
    ) {
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
            name: "until-labs".into(),
            url: "http://studio.test".into(),
            source: graph_store::ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| graph_store::Connection::new(server, None, http).unwrap());
        let session = cx
            .new(|cx| GraphSession::new(connection.clone(), "strategy".into(), "main".into(), cx));
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.graph_order.push(session.entity_id());
            workspace.subscribe_session(session.clone(), cx);
            workspace.open_item("local".to_owned(), window, cx, |_, cx| TabProbe {
                focus_handle: cx.focus_handle(),
                name: "local-probe",
                reads: false,
            });
            workspace.open_item("table".to_owned(), window, cx, |_, cx| TabProbe {
                focus_handle: cx.focus_handle(),
                name: "graph-probe",
                reads: true,
            });
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("graph-probe").is_some());

        let asks = |asks: bool, cx: &mut gpui::VisualTestContext| {
            connection.update(cx, |c, cx| {
                if asks {
                    c.state = ConnectionState::Unauthorized;
                    c.sign_in = graph_store::sign_in::SignIn::Offered {
                        service: "id.test".into(),
                        why: None,
                    };
                } else {
                    c.state = ConnectionState::Ready;
                    c.sign_in = graph_store::sign_in::SignIn::NotOffered;
                }
                cx.notify();
            });
            cx.run_until_parked();
        };
        asks(true, cx);
        assert!(cx.debug_bounds("sign-in-prompt").is_some());
        assert!(cx.debug_bounds("graph-probe").is_none());

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(0, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("local-probe").is_some());
        assert!(cx.debug_bounds("sign-in-prompt").is_none());

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(1, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("sign-in-prompt").is_some());
        asks(false, cx);
        assert!(cx.debug_bounds("sign-in-prompt").is_none());
        assert!(cx.debug_bounds("graph-probe").is_some());
    }

    struct StatusProbe;

    impl Render for StatusProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .debug_selector(|| "status-probe".into())
                .child("probe")
        }
    }

    /// The window's own status item (the updater's, spec 017) is drawn in the status bar,
    /// with no graph open as with one, and only while it has something to say.
    #[gpui::test]
    fn the_status_item_is_drawn_in_the_status_bar(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let says = Rc::new(std::cell::Cell::new(true));
        workspace.update(cx, |workspace, cx| {
            let probe = cx.new(|_| StatusProbe);
            let shown = says.clone();
            workspace.set_status_item(probe.into(), move |_| shown.get(), cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("status-probe").is_some());
        // With nothing to say, it is not drawn, so it takes no room in the bar.
        says.set(false);
        workspace.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(cx.debug_bounds("status-probe").is_none());
    }

    /// Restart to Update asks as Quit does when a tab holds unsaved work (spec 017), in its
    /// own words; nothing restarts until it is confirmed.
    #[gpui::test]
    fn restarting_to_update_with_unsaved_work_asks_first(cx: &mut gpui::TestAppContext) {
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
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.graph_order.push(session.entity_id());
            workspace.subscribe_session(session.clone(), cx);
            workspace.open_item("draft".to_owned(), window, cx, |_, cx| Unsaved {
                focus_handle: cx.focus_handle(),
            });
        });
        // Deferred, as the action's handler does: the window is held while it dispatches.
        cx.update(|_, cx| cx.defer(|cx| restart(false, cx)));
        cx.run_until_parked();
        let dialog = workspace.read_with(cx, |workspace, _| {
            workspace
                .modal
                .as_ref()
                .and_then(|modal| modal.view.clone().downcast::<ConfirmClose>().ok())
                .expect("the restart dialog")
        });
        dialog.read_with(cx, |dialog, _| {
            assert!(matches!(dialog.closing, Closing::Restart));
            assert_eq!(
                dialog.header().0,
                SharedString::from("Restart to update Omnigraph Studio?")
            );
            assert_eq!(dialog.unsaved, [SharedString::from("Draft")]);
            assert_eq!(dialog.closing.confirm(), "Restart");
        });
    }

    /// A close asked about the shown graph or one of its tabs goes when another graph is shown:
    /// confirmed over that one, it would close the graph on screen and drop its unsaved tabs
    /// unasked.
    #[gpui::test]
    fn a_close_asked_about_a_graph_goes_when_another_is_shown(cx: &mut gpui::TestAppContext) {
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
        let notes =
            cx.new(|cx| GraphSession::new(connection.clone(), "notes".into(), "main".into(), cx));
        let people = cx.new(|cx| GraphSession::new(connection, "people".into(), "main".into(), cx));
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.graph_order.push(notes.entity_id());
            workspace.subscribe_session(notes.clone(), cx);
            workspace.open_item("draft".to_owned(), window, cx, |_, cx| Unsaved {
                focus_handle: cx.focus_handle(),
            });
            workspace.add_behind(people.clone(), None, cx);
            for about_the_tab in [false, true] {
                if about_the_tab {
                    workspace.close_item(0, window, cx);
                } else {
                    workspace.close_graph(window, cx);
                }
                assert!(workspace.has_modal(), "asked");
                workspace.show_graph(people.entity_id(), window, cx);
                assert!(!workspace.has_modal(), "the question went with its graph");
                assert_eq!(
                    workspace.session().map(Entity::entity_id),
                    Some(people.entity_id())
                );
                workspace.show_graph(notes.entity_id(), window, cx);
                assert_eq!(
                    workspace.unsaved_tabs(cx),
                    [SharedString::from("Draft")],
                    "nothing was closed"
                );
            }
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

    gpui::actions!(workspace_test, [Probe]);

    /// An action a feature registers with a condition is available only while it holds, so
    /// the palette and the keys offer it only then.
    #[gpui::test]
    fn a_conditional_action_is_registered_only_while_it_holds(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let holds = Rc::new(std::cell::Cell::new(false));
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            let holds = holds.clone();
            workspace.register_action_when(move |_, _| holds.get(), |_, _: &Probe, _, _| {});
            window.focus(&workspace.focus_handle(cx), cx);
        });
        let available = |cx: &mut gpui::VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| window.is_action_available(&Probe, cx))
        };
        assert!(!available(cx));
        holds.set(true);
        workspace.update(cx, |_, cx| cx.notify());
        assert!(available(cx));
    }

    /// The status bar's branch reads "read only" once the access route refuses changes there,
    /// and not while it is unknown (unread, or not answered) or allowed. The connection chip's
    /// tooltip names the actor once an answer has.
    #[gpui::test]
    fn a_branch_the_credential_may_not_change_reads_read_only(cx: &mut gpui::TestAppContext) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let answers = Arc::new(AtomicBool::new(false));
        let answering = answers.clone();
        let http = http_client::FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let answering = answering.load(Ordering::SeqCst);
            async move {
                // Every branch reads; only fix takes changes. Until `answers`, the route fails.
                let (status, body) = match (path.ends_with("/access"), answering) {
                    (true, false) => (503, r#"{"error":"later"}"#.to_owned()),
                    (true, true) => {
                        let actions = if query.contains("branch=fix") {
                            r#"["read","change"]"#
                        } else {
                            r#"["read"]"#
                        };
                        (
                            200,
                            format!(
                                r#"{{"graph_id":"notes","actor_id":"act-ada","graph_wide":["read"],"actions":{actions}}}"#
                            ),
                        )
                    }
                    _ => (200, r#"{"branches":["main","fix"]}"#.to_owned()),
                };
                Ok(http_client::http::Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(http_client::AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = graph_store::ServerEntry {
            name: "test".into(),
            url: "http://studio.test".into(),
            source: graph_store::ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection =
            cx.new(|_| graph_store::Connection::new(server, Some("t".into()), http).unwrap());
        let session = cx.new(|cx| GraphSession::new(connection, "notes".into(), "main".into(), cx));
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update(cx, |workspace, cx| {
            workspace.graph_order.push(session.entity_id());
            workspace.subscribe_session(session.clone(), cx);
        });
        cx.run_until_parked();
        let read_only = |cx: &mut gpui::VisualTestContext| {
            cx.run_until_parked();
            cx.debug_bounds("branch-read-only").is_some()
        };
        assert!(!read_only(cx), "not known: edits stay offered");
        session.read_with(cx, |session, _| assert_eq!(session.actor(), None));

        // Answered at last: main, read again when the window comes back to it, refuses.
        answers.store(true, Ordering::SeqCst);
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        assert!(!read_only(cx), "fix takes changes");
        session.update(cx, |session, cx| session.set_branch("main".into(), cx));
        assert!(read_only(cx), "main is read only");
        let actor = session.read_with(cx, |session, _| session.actor().map(str::to_owned));
        assert_eq!(
            connection_tooltip("http://studio.test", "ready", actor.as_deref()),
            "http://studio.test · ready · as act-ada · Switch graph"
        );
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

    gpui::actions!(view_test, [OpenTarget]);

    /// A tab with a second view, which [`OpenTarget`] opens.
    struct Viewed {
        focus_handle: FocusHandle,
        key: &'static str,
        disabled: Option<&'static str>,
    }

    impl EventEmitter<ItemEvent> for Viewed {}

    impl Focusable for Viewed {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for Viewed {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.focus_handle)
        }
    }

    impl Item for Viewed {
        fn tab_title(&self, _: &App) -> SharedString {
            "Viewed".into()
        }

        fn dedupe_key(&self, _: &App) -> Option<SharedString> {
            Some(self.key.into())
        }

        fn views(&self, _: &App) -> Vec<ViewLink> {
            vec![
                ViewLink::current(ViewKind::Table),
                match self.disabled {
                    Some(reason) => ViewLink::disabled_because(ViewKind::Canvas, reason),
                    None => ViewLink::to(ViewKind::Canvas),
                },
            ]
        }

        fn view_action(&self, kind: ViewKind, _: &App) -> Option<Box<dyn Action>> {
            (kind == ViewKind::Canvas).then(|| Box::new(OpenTarget) as Box<dyn Action>)
        }
    }

    fn views_workspace<'a>(
        cx: &'a mut gpui::TestAppContext,
        disabled: Option<&'static str>,
    ) -> (Entity<Workspace>, &'a mut gpui::VisualTestContext, EntityId) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let source = workspace.update_in(cx, |workspace, window, cx| {
            workspace.register_action(|workspace, _: &OpenTarget, window, cx| {
                workspace.open_item_with("target".to_owned(), true, window, cx, |_, cx| Viewed {
                    focus_handle: cx.focus_handle(),
                    key: "target",
                    disabled: None,
                });
            });
            workspace
                .open_item_with("viewed".to_owned(), true, window, cx, |_, cx| Viewed {
                    focus_handle: cx.focus_handle(),
                    key: "viewed",
                    disabled,
                })
                .expect("opened")
                .entity_id()
        });
        cx.run_until_parked();
        (workspace, cx, source)
    }

    /// A view opens from a preview tab without taking its place, and Back returns to it; a view
    /// that is open is activated where it is (spec 020).
    #[gpui::test]
    fn opening_a_view_keeps_its_source(cx: &mut gpui::TestAppContext) {
        let (workspace, cx, source) = views_workspace(cx, None);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_view(ViewKind::Canvas, false, window, cx);
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.pane(workspace.active_pane).expect("the pane");
            assert_eq!(pane.items.len(), 2, "the source stayed");
            assert_eq!(workspace.center.panes().len(), 1);
            assert_eq!(pane.items[0].item_id(), source);
            let active = workspace.active_pane;
            workspace.navigate(active, true, window, cx);
            assert_eq!(
                workspace.active_item().map(|item| item.item_id()),
                Some(source),
                "Back returns to it"
            );
        });
    }

    /// Beside opens a new pane to the right, with the source still on screen.
    #[gpui::test]
    fn a_view_opens_beside_its_source(cx: &mut gpui::TestAppContext) {
        let (workspace, cx, source) = views_workspace(cx, None);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_view(ViewKind::Canvas, true, window, cx);
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            assert_eq!(workspace.center.panes().len(), 2);
            let active = workspace.pane(workspace.active_pane).expect("the pane");
            assert_eq!(active.items.len(), 1);
            assert_ne!(active.items[0].item_id(), source);
            assert!(!workspace.open_beside, "only that opening");
            // Reused where it is: asking again adds no pane.
            let first = workspace.center.panes()[0];
            let index = workspace.pane(first).expect("the first").position(source);
            workspace.activate_in(first, index.expect("the source"), true, window, cx);
            workspace.open_view(ViewKind::Canvas, true, window, cx);
        });
        cx.run_until_parked();
        workspace.update(cx, |workspace, _| {
            assert_eq!(workspace.center.panes().len(), 2, "no third pane");
        });
    }

    /// A view that cannot open stays in the strip with why, and asking for it does nothing.
    #[gpui::test]
    fn a_disabled_view_does_not_open(cx: &mut gpui::TestAppContext) {
        let (workspace, cx, _) = views_workspace(cx, Some("No rows to draw"));
        workspace.update_in(cx, |workspace, window, cx| {
            assert!(!workspace.offers_view(ViewKind::Canvas, cx));
            workspace.open_view(ViewKind::Canvas, false, window, cx);
        });
        cx.run_until_parked();
        workspace.update(cx, |workspace, _| {
            let pane = workspace.pane(workspace.active_pane).expect("the pane");
            assert_eq!(pane.items.len(), 1);
            assert!(pane.is_preview(0), "and the source stays a preview");
        });
    }

    /// Each tab keeps its own inspector, and whoever follows the active tab hears of the change.
    #[gpui::test]
    fn each_tab_keeps_its_inspector(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let changes = Rc::new(std::cell::Cell::new(0));
        let heard = changes.clone();
        let _hearing = cx.update(|_, cx| {
            cx.subscribe(&workspace, move |_, event: &WorkspaceEvent, _| {
                if matches!(event, WorkspaceEvent::ActiveItemChanged) {
                    heard.set(heard.get() + 1);
                }
            })
        });
        let shown = |workspace: &Workspace| {
            workspace
                .right
                .panels
                .iter()
                .find(|panel| panel.id == INSPECTOR_PANEL)
                .map(|panel| panel.view.entity_id())
        };
        let (first, second, first_peek, second_peek) =
            workspace.update_in(cx, |workspace, window, cx| {
                let first = workspace.add_item(window, cx, |_, cx| TabProbe {
                    focus_handle: cx.focus_handle(),
                    name: "first",
                    reads: false,
                });
                let first_peek = cx.new(|_| Field);
                workspace.set_inspector(Some(first_peek.clone().into()), cx);
                let second = workspace.add_item(window, cx, |_, cx| TabProbe {
                    focus_handle: cx.focus_handle(),
                    name: "second",
                    reads: false,
                });
                let second_peek = cx.new(|_| Field);
                (first, second, first_peek, second_peek)
            });
        cx.run_until_parked();
        workspace.update(cx, |workspace, _| {
            assert_ne!(
                shown(workspace),
                Some(first_peek.entity_id()),
                "the new tab has nothing selected"
            );
            assert!(shown(workspace).is_some(), "and says so, in the same place");
        });
        let heard_so_far = changes.get();
        assert!(heard_so_far > 0);
        workspace.update_in(cx, |workspace, window, cx| {
            // A selection read in the tab behind arrives while the other is active.
            workspace.set_inspector_of(first.entity_id(), Some(first_peek.clone().into()), cx);
            workspace.set_inspector(Some(second_peek.clone().into()), cx);
            assert_eq!(shown(workspace), Some(second_peek.entity_id()));
            workspace.activate_item_by_id(first.entity_id(), window, cx);
        });
        cx.run_until_parked();
        workspace.update(cx, |workspace, _| {
            assert_eq!(shown(workspace), Some(first_peek.entity_id()));
            assert!(
                workspace.dock_hosts.contains_key(&second_peek.entity_id()),
                "a remembered inspector keeps its host"
            );
        });
        assert!(changes.get() > heard_so_far);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item_by_id(second.entity_id(), window, cx);
        });
        cx.run_until_parked();
        workspace.update(cx, |workspace, _| {
            assert_eq!(shown(workspace), Some(second_peek.entity_id()));
        });
    }

    /// The keyboard in an inspector another replaces is not left with no view to hear it: the
    /// active tab takes it, so the table's arrows and ⎋ work again.
    #[gpui::test]
    fn the_keyboard_in_a_replaced_inspector_goes_to_the_active_tab(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let (tab, inspector) = workspace.update_in(cx, |workspace, window, cx| {
            let tab = cx.focus_handle();
            let focus_handle = tab.clone();
            workspace.open_item("a", window, cx, move |_, cx| Split {
                field: cx.new(|_| Field),
                part: cx.new(|_| Part { renders: 0 }),
                caches: false,
                focus_handle,
            });
            let inspector = cx.new(|_| Field);
            workspace.set_inspector(Some(inspector.clone().into()), cx);
            workspace.set_dock_open(PanelPosition::Right, true, window, cx);
            (tab, inspector)
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            let host = workspace.dock_hosts[&inspector.entity_id()]
                .read(cx)
                .focus_handle
                .clone();
            window.focus(&host, cx);
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            assert!(!tab.is_focused(window), "the inspector has the keyboard");
            let next = cx.new(|_| Field);
            workspace.set_inspector(Some(next.into()), cx);
        });
        cx.run_until_parked();
        cx.update(|window, _| assert!(tab.is_focused(window), "the tab has it"));
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

    /// Hiding a dock, or zooming a pane over it, gives the keyboard back to the tab from
    /// anywhere in the dock's panel: from a field of a panel that names no focus of its own
    /// too, as the Conversations filter, the inspector and the activity log are. A side panel
    /// and a bottom one are drawn differently, so both are asked. The zoom buttons hand it over
    /// as ⇧⎋ does: a pane's from the panel under it, a dock's from the tab under it.
    #[gpui::test]
    fn a_hidden_or_covered_dock_gives_the_keyboard_back_from_any_field_of_its_panel(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        // A graph shown, for the pane's buttons.
        let session = idle_session("http://studio.test", "notes", cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let tab = workspace.update_in(cx, |workspace, window, cx| {
            workspace.graph_order.push(session.entity_id());
            workspace.subscribe_session(session.clone(), cx);
            workspace.open_item("draft".to_owned(), window, cx, |_, cx| Unsaved {
                focus_handle: cx.focus_handle(),
            });
            workspace
                .pane(workspace.active_pane)
                .and_then(|pane| pane.active_item())
                .map(|item| item.focus_handle(cx))
                .expect("the tab")
        });
        cx.run_until_parked();
        let fields = workspace.update_in(cx, |workspace, _, cx| {
            let mut fields = Vec::new();
            for (position, id) in [
                (PanelPosition::Right, "inspector"),
                (PanelPosition::Bottom, "activity"),
            ] {
                let field = cx.focus_handle();
                let view = cx.new(|_| Other {
                    focus_handle: field.clone(),
                });
                workspace.add_panel(position, id, id, IconName::Table, view.into(), cx);
                workspace.activate_panel(id, cx);
                fields.push(field);
            }
            fields
        });
        // Drawn, so the frame knows which panel holds each field.
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            window.focus(&fields[0], cx);
            workspace.set_dock_open(PanelPosition::Right, false, window, cx);
            assert!(
                tab.is_focused(window),
                "hiding the dock gives the tab the keyboard"
            );
            window.focus(&fields[1], cx);
            workspace.toggle_zoom(window, cx);
            assert!(tab.is_focused(window), "so does zooming the pane over it");
        });

        let click = |selector: &'static str, cx: &mut gpui::VisualTestContext| {
            cx.run_until_parked();
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} was drawn"));
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        };
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane);
        let pane_button: &'static str = format!("zoom-pane-{pane}").leak();
        click(pane_button, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            assert_eq!(workspace.zoomed, None, "the button zooms out");
            window.focus(&fields[1], cx);
        });
        click(pane_button, cx);
        workspace.update_in(cx, |workspace, window, _| {
            assert_eq!(workspace.zoomed, Some(Zoomed::Pane(pane)));
            assert!(
                tab.is_focused(window),
                "the pane's button takes the keyboard from the panel under it"
            );
        });
        click(pane_button, cx);
        click("zoom-dock-Bottom", cx);
        workspace.update_in(cx, |workspace, window, cx| {
            assert_eq!(workspace.zoomed, Some(Zoomed::Dock(PanelPosition::Bottom)));
            assert!(
                !tab.is_focused(window)
                    && workspace.dock_has_focus(PanelPosition::Bottom, window, cx),
                "the dock's button takes it from the tab under it"
            );
        });
    }

    /// The connection dot beats, one beat a pulse, only while its server is ready and the
    /// window in front, so a dot nobody sees asks for no frames; a server that stops answering
    /// stops it at once (the workspace reads the connection as it draws, so GPUI draws it again
    /// when the connection changes).
    #[gpui::test]
    fn the_connection_dot_beats_only_while_ready_and_seen(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let session = idle_session("http://localhost:1", "g", cx);
        let connection = session.read_with(cx, |session, _| session.connection.clone());
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_session(session.clone(), window, cx)
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let beating =
            |cx: &mut gpui::VisualTestContext| workspace.read_with(cx, |w, _| w._beats.is_some());
        let beat = |cx: &mut gpui::VisualTestContext| workspace.read_with(cx, |w, _| w.beat);
        let set_state = |state: ConnectionState, cx: &mut gpui::VisualTestContext| {
            connection.update(cx, |connection, cx| {
                connection.state = state;
                cx.notify();
            });
            cx.run_until_parked();
        };
        let pulses = |count: u32, cx: &mut gpui::VisualTestContext| {
            cx.executor()
                .advance_clock(studio_ui::motion::PULSE * count);
            cx.run_until_parked();
        };
        set_state(ConnectionState::Connecting, cx);
        assert!(!beating(cx), "not ready");
        set_state(ConnectionState::Ready, cx);
        assert!(beating(cx), "ready and in front");
        let before = beat(cx);
        pulses(2, cx);
        assert_eq!(beat(cx), before + 2, "one beat a pulse");
        cx.deactivate_window();
        assert!(!beating(cx), "not in front");
        let before = beat(cx);
        pulses(3, cx);
        assert_eq!(beat(cx), before, "no beats behind other windows");
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        assert!(beating(cx), "in front again");
        set_state(ConnectionState::Unreachable("refused".into()), cx);
        assert!(!beating(cx), "stopped answering");
    }

    /// A session for `graph` on the server at `url`, which answers every request with a 404.
    fn idle_session(url: &str, graph: &str, cx: &mut gpui::TestAppContext) -> Entity<GraphSession> {
        let http = http_client::FakeHttpClient::create(|_| async {
            Ok(http_client::http::Response::builder()
                .status(404)
                .body(http_client::AsyncBody::default())
                .unwrap())
        });
        let server = graph_store::ServerEntry {
            name: "test".into(),
            url: url.into(),
            source: graph_store::ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| graph_store::Connection::new(server, None, http).unwrap());
        cx.new(|cx| GraphSession::new(connection, graph.into(), "main".into(), cx))
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
        // One server under two names, each its own connection: one the registry replaced,
        // still held by a window, beside the one it holds now.
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
            // A parked graph polls nothing; the shown one does.
            let shown_now = |cx: &App| (shown.read(cx).is_shown(), behind.read(cx).is_shown());
            assert_eq!(shown_now(cx), (true, false));

            // The shown graph's inspector goes with it, host and all: the host would keep the
            // inspector, and the session it holds polling, after the graph is put away.
            let inspector = cx.new(|_| Field);
            workspace.set_inspector(Some(inspector.clone().into()), cx);
            workspace.show_graph(behind.entity_id(), window, cx);
            assert_eq!(workspace.dock_of(INSPECTOR_PANEL), None);
            assert!(!workspace.dock_hosts.contains_key(&inspector.entity_id()));
            let pane = workspace.pane(workspace.active_pane).unwrap();
            assert_eq!((pane.items.len(), pane.active), (2, 1));
            assert_eq!(shown_now(cx), (false, true));

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
            // ⌘5 with two tabs moves nothing, the keyboard included.
            workspace.activate_item(4, window, cx);
            assert_eq!(active(workspace), Some(1));
            assert!(
                !workspace.focus_handle.is_focused(window),
                "the keyboard stays on the tab"
            );

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
            // Empty (its tab could not be copied), the new pane closes with ⌘W, as it would
            // with its last tab.
            assert!(workspace.pane(second).unwrap().items.is_empty());
            workspace.active_pane = second;
            workspace.close_item(0, window, cx);
            assert!(workspace.pane(second).is_none());
            assert_eq!(workspace.active_pane, first);
        });
    }

    /// A click in an empty pane makes it the active pane, and it stays so: it takes the keyboard
    /// from the other pane's tab, which the frames after follow.
    #[gpui::test]
    fn a_click_in_an_empty_pane_makes_it_active(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let (first, second) = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_item("a", window, cx, |_, cx| Split {
                field: cx.new(|_| Field),
                part: cx.new(|_| Part { renders: 0 }),
                caches: false,
                focus_handle: cx.focus_handle(),
            });
            let first = workspace.active_pane;
            // A split of a tab that is not copied: the new pane is empty.
            workspace.split(SplitDirection::Right, window, cx);
            let second = workspace.active_pane;
            workspace.activate_in(first, 0, true, window, cx);
            (first, second)
        });
        cx.run_until_parked();
        assert_eq!(workspace.read_with(cx, |w, _| w.active_pane), first);
        let selector: &'static str = Box::leak(format!("pane-header-{second}").into_boxed_str());
        let header = cx.debug_bounds(selector).expect("the empty pane is drawn");
        cx.simulate_click(header.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            workspace.read_with(cx, |w, _| w.active_pane),
            second,
            "the click stays"
        );
    }

    /// Closing a tab moves the keyboard only when that tab held it: one closed while the keyboard
    /// is elsewhere (a conversation deleted from the sidebar's list) leaves it there.
    #[gpui::test]
    fn closing_a_tab_moves_the_keyboard_only_from_that_tab(cx: &mut gpui::TestAppContext) {
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
            let elsewhere = cx.focus_handle();
            window.focus(&elsewhere, cx);
            workspace.close_item(1, window, cx);
            assert!(
                elsewhere.is_focused(window),
                "the keyboard stays where it was"
            );
            let pane = workspace.pane(workspace.active_pane).unwrap();
            assert_eq!((pane.items.len(), pane.active), (1, 0), "a shows");
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
            merge: None,
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
            commit_id: "c2".into(),
            preview: false,
        }));
    }

    /// A write's toast acts on the graph and branch written to, whatever the window shows when
    /// its button is pressed: "Open history" after moving to another graph shows the written
    /// graph again, on its branch; after moving to another branch, the branch written to; Undo
    /// from a parked graph undoes there. A graph closed since is named, and nothing runs.
    #[gpui::test]
    fn a_toasts_follow_up_acts_on_the_graph_and_branch_written_to(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let notes = idle_session("http://a.test", "notes", cx);
        let people = idle_session("http://b.test", "people", cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        // Where each follow-up ran: the action, the graph shown and its branch.
        type Ran = Vec<(&'static str, String, String)>;
        let ran: Rc<RefCell<Ran>> = Rc::default();
        workspace.update(cx, |workspace, cx| {
            let shown = |workspace: &Workspace, cx: &App| {
                let session = workspace.session().unwrap().read(cx);
                (session.graph_id.clone(), session.branch.clone())
            };
            let history = ran.clone();
            workspace.register_action(move |workspace, _: &OpenHistory, _, cx| {
                let (graph, branch) = shown(workspace, cx);
                history.borrow_mut().push(("history", graph, branch));
            });
            let undo = ran.clone();
            workspace.register_action(
                move |workspace, action: &studio_actions::graph::UndoDelete, _, cx| {
                    let (graph, branch) = shown(workspace, cx);
                    assert_eq!(action.branch, branch, "undone on the delete's branch");
                    undo.borrow_mut().push(("undo", graph, branch));
                },
            );
            workspace.graph_order.push(notes.entity_id());
            workspace.subscribe_session(notes.clone(), cx);
            workspace.add_behind(people.clone(), None, cx);
        });
        // A tab in each graph: the one shown when a follow-up runs has not been drawn yet.
        let mut tabs = Vec::new();
        for session in [&people, &notes] {
            let tab = workspace.update_in(cx, |workspace, window, cx| {
                workspace.show_graph(session.entity_id(), window, cx);
                let focus_handle = cx.focus_handle();
                let held = focus_handle.clone();
                workspace.open_item(session.entity_id().to_string(), window, cx, |_, _| {
                    Unsaved { focus_handle }
                });
                held
            });
            tabs.push(tab);
        }
        let (people_tab, notes_tab) = (tabs[0].clone(), tabs[1].clone());
        notes.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        let log = |session: &Entity<GraphSession>,
                   description: &str,
                   outcome: OutcomeKind,
                   follow_up: Option<graph_store::FollowUp>,
                   cx: &mut gpui::VisualTestContext| {
            session.update(cx, |session, cx| {
                session.outcomes.push(graph_store::OutcomeEntry {
                    at: std::time::SystemTime::now(),
                    branch: session.branch.clone(),
                    description: description.into(),
                    outcome,
                    follow_up,
                    merge: None,
                });
                session.outcomes_logged += 1;
                cx.emit(SessionEvent::ActivityChanged);
            });
            cx.run_until_parked();
            workspace.read_with(cx, |workspace, _| workspace.toasts.last().unwrap().id)
        };
        let follow = |toast: usize, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.run_follow_up(toast, window, cx)
            });
            cx.run_until_parked();
        };
        let shown = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let session = workspace.session().unwrap().read(cx);
                (session.graph_id.clone(), session.branch.clone())
            })
        };
        let unknown = || OutcomeKind::Unknown {
            reason: "the connection closed".into(),
        };

        // A write to notes on fix whose outcome nobody knows, then people shown.
        let toast = log(&notes, "Edit Person ada", unknown(), None, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.show_graph(people.entity_id(), window, cx)
        });
        follow(toast, cx);
        assert_eq!(shown(cx), ("notes".into(), "fix".into()));
        assert_eq!(
            ran.borrow_mut().pop(),
            Some(("history", "notes".into(), "fix".into()))
        );
        let focused = |tab: &FocusHandle, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, _| tab.is_focused(window))
        };
        assert!(focused(&notes_tab, cx), "the graph's tab has the keyboard");

        // Another, then notes moved to main.
        let toast = log(&notes, "Edit Person grace", unknown(), None, cx);
        notes.update(cx, |session, cx| session.set_branch("main".into(), cx));
        follow(toast, cx);
        assert_eq!(shown(cx), ("notes".into(), "fix".into()), "back on fix");
        assert_eq!(
            ran.borrow_mut().pop(),
            Some(("history", "notes".into(), "fix".into()))
        );

        // A delete in people while notes is shown: its Undo runs in people.
        let toast = log(
            &people,
            "Delete Person ada",
            OutcomeKind::Committed {
                commit: Some("c2".into()),
            },
            Some(graph_store::FollowUp::UndoDelete),
            cx,
        );
        let message = workspace.read_with(cx, |workspace, _| {
            workspace.toasts.last().unwrap().message.clone()
        });
        assert!(message.starts_with("people: "), "{message}");
        follow(toast, cx);
        assert_eq!(shown(cx), ("people".into(), "main".into()));
        assert_eq!(
            ran.borrow_mut().pop(),
            Some(("undo", "people".into(), "main".into()))
        );
        assert!(
            focused(&people_tab, cx),
            "an undo opens no tab: the graph's has it"
        );

        // people closed with its toast still up: nothing runs, and the toast says why.
        let toast = log(&people, "Edit Person lin", unknown(), None, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.close_graph_asking(false, window, cx)
        });
        follow(toast, cx);
        assert_eq!(shown(cx), ("notes".into(), "fix".into()));
        assert!(ran.borrow().is_empty(), "nothing ran");
        let message = workspace.read_with(cx, |workspace, _| {
            workspace.toasts.last().unwrap().message.clone()
        });
        assert!(
            message.contains("people is no longer open in this window"),
            "{message}"
        );
    }

    /// Logs `outcome` on `session`'s branch as the session logs a write's, and lets the window
    /// hear of it.
    fn log_outcome(
        session: &Entity<GraphSession>,
        description: &str,
        outcome: OutcomeKind,
        follow_up: Option<graph_store::FollowUp>,
        cx: &mut gpui::VisualTestContext,
    ) {
        session.update(cx, |session, cx| {
            session.outcomes.push(graph_store::OutcomeEntry {
                at: std::time::SystemTime::now(),
                branch: session.branch.clone(),
                description: description.into(),
                outcome,
                follow_up,
                merge: None,
            });
            session.outcomes_logged += 1;
            cx.emit(SessionEvent::ActivityChanged);
        });
        cx.run_until_parked();
    }

    /// A refusal's toast ends its message once, whether or not the message ends its own
    /// sentence, as a server's and a write never sent ("…, so nothing was sent.") do. Before, those
    /// read "… sent.. Nothing was written."
    #[test]
    fn a_refusal_toast_ends_its_message_once() {
        let entry = |message: &str| graph_store::OutcomeEntry {
            at: std::time::SystemTime::now(),
            branch: "main".into(),
            description: "Revert Person p1".into(),
            outcome: OutcomeKind::Refused {
                message: message.into(),
            },
            follow_up: None,
            merge: None,
        };
        assert_eq!(
            toast_text(&entry("the target is gone, so nothing was sent.")),
            "Revert Person p1 · refused: the target is gone, so nothing was sent. Nothing was \
             written."
        );
        assert_eq!(
            toast_text(&entry("the policy denies it")),
            "Revert Person p1 · refused: the policy denies it. Nothing was written."
        );
        assert_eq!(
            toast_text(&entry("the server is busy...")),
            "Revert Person p1 · refused: the server is busy... Nothing was written."
        );
    }

    /// A delete's Undo, which only its toast offers, is offered though another write of the
    /// graph (a cell's save, the agent's) is still out; so is every other commit.
    #[gpui::test]
    fn a_commit_is_toasted_while_another_write_of_its_graph_is_out(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let people = idle_session("http://b.test", "people", cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update(cx, |workspace, cx| {
            workspace.graph_order.push(people.entity_id());
            workspace.subscribe_session(people.clone(), cx);
        });
        // The agent's write, out the whole time.
        people.update(cx, |session, _| session.writes_in_flight = 1);
        let commit = |id: &str| OutcomeKind::Committed {
            commit: Some(id.into()),
        };
        log_outcome(
            &people,
            "Delete Person ada",
            commit("c2"),
            Some(graph_store::FollowUp::UndoDelete),
            cx,
        );
        log_outcome(&people, "Set name of p1", commit("c3"), None, cx);
        let toasts = workspace.read_with(cx, |workspace, _| {
            workspace
                .toasts
                .iter()
                .map(|toast| {
                    let label = toast
                        .action
                        .as_ref()
                        .map(|follow_up| follow_up.label.clone());
                    (toast.message.clone(), label)
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            toasts,
            [
                (
                    "Delete Person ada · committed to main".into(),
                    Some("Undo".into())
                ),
                (
                    "Set name of p1 · committed to main".into(),
                    Some("View commit".into())
                ),
            ]
        );
    }

    /// A full stack takes away a toast with no follow-up first, then the oldest with one, but
    /// never an Undo or an error: an agent's four commits after a delete leave the delete's
    /// Undo up. A stack of nothing but those makes room by folding its two oldest errors into
    /// one toast that counts them.
    #[gpui::test]
    fn an_undo_and_an_error_stay_when_toasts_overflow(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let people = idle_session("http://b.test", "people", cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update(cx, |workspace, cx| {
            workspace.graph_order.push(people.entity_id());
            workspace.subscribe_session(people.clone(), cx);
        });
        let toast = |kind: ToastKind, text: &str, cx: &mut gpui::VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(kind, text.to_owned(), cx)
            });
        };
        let commit = |n: usize, follow_up, cx: &mut gpui::VisualTestContext| {
            let description = match follow_up {
                Some(_) => "Delete Person ada".to_owned(),
                None => format!("Agent write {n}"),
            };
            let outcome = OutcomeKind::Committed {
                commit: Some(format!("c{n}")),
            };
            log_outcome(&people, &description, outcome, follow_up, cx);
        };
        let shown = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .toasts
                    .iter()
                    .map(|toast| toast.message.to_string())
                    .collect::<Vec<_>>()
            })
        };

        toast(ToastKind::Error, "A write to people is still running.", cx);
        commit(2, Some(graph_store::FollowUp::UndoDelete), cx);
        toast(
            ToastKind::Info,
            "The branch fix is gone, so main is shown.",
            cx,
        );
        for n in 3..7 {
            commit(n, None, cx);
        }
        assert_eq!(
            shown(cx),
            [
                "A write to people is still running.",
                "Delete Person ada · committed to main",
                "Agent write 5 · committed to main",
                "Agent write 6 · committed to main",
            ],
            "the info went first, then the oldest commits"
        );

        toast(ToastKind::Error, "Second error", cx);
        toast(ToastKind::Error, "Third error", cx);
        commit(7, None, cx);
        commit(8, None, cx);
        assert_eq!(
            shown(cx),
            [
                "2 earlier errors",
                "Delete Person ada · committed to main",
                "Third error",
                "Agent write 8 · committed to main",
            ],
            "with kept toasts filling the stack, the two oldest errors folded; the newest commit \
             took the place of the one before it"
        );
    }

    /// An agent's writes refused one after another fold into one toast once the stack is full,
    /// rather than pushing what follows them below the window's edge: a delete's Undo among a
    /// dozen refusals stays within [`TOASTS_SHOWN`], beside the newest refusals, and the folded
    /// toast counts the rest and opens the activity log.
    #[gpui::test]
    fn a_dozen_errors_fold_and_leave_an_undo_within_the_stack(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let people = idle_session("http://b.test", "people", cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update(cx, |workspace, cx| {
            workspace.graph_order.push(people.entity_id());
            workspace.subscribe_session(people.clone(), cx);
            for id in ["nodes", ACTIVITY_PANEL] {
                let view = cx.new(|_| Field);
                workspace.add_panel(
                    PanelPosition::Bottom,
                    id,
                    id,
                    IconName::Table,
                    view.into(),
                    cx,
                );
            }
            workspace.activate_panel("nodes", cx);
        });
        let refuse = |n: usize, cx: &mut gpui::VisualTestContext| {
            let outcome = OutcomeKind::Refused {
                message: "the policy denies it".into(),
            };
            log_outcome(&people, &format!("Agent write {n}"), outcome, None, cx);
        };
        for n in 1..=6 {
            refuse(n, cx);
        }
        log_outcome(
            &people,
            "Delete Person ada",
            OutcomeKind::Committed {
                commit: Some("c7".into()),
            },
            Some(graph_store::FollowUp::UndoDelete),
            cx,
        );
        for n in 8..=13 {
            refuse(n, cx);
        }
        let refused = |n: usize| {
            format!("Agent write {n} · refused: the policy denies it. Nothing was written.")
        };
        let (shown, fold) = workspace.read_with(cx, |workspace, _| {
            let shown = workspace
                .toasts
                .iter()
                .map(|toast| {
                    let label = toast.action.as_ref().map(|f| f.label.to_string());
                    (toast.message.to_string(), label)
                })
                .collect::<Vec<_>>();
            (shown, workspace.toasts[0].id)
        });
        assert_eq!(
            shown,
            [
                (
                    "people: 10 earlier errors".to_owned(),
                    Some("Open activity".to_owned())
                ),
                (
                    "Delete Person ada · committed to main".to_owned(),
                    Some("Undo".to_owned())
                ),
                (refused(12), None),
                (refused(13), None),
            ],
            "the Undo is up within the stack, beside the two newest refusals; the ten before \
             them are counted"
        );

        // Pressed in the window, which holds the keyboard, as the toast's button is.
        workspace.update_in(cx, |workspace, window, cx| {
            window.focus(&workspace.focus_handle, cx)
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.run_follow_up(fold, window, cx)
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                (workspace.bottom.open, workspace.bottom.active_id()),
                (true, Some(ACTIVITY_PANEL.into())),
                "Open activity shows the activity log"
            );
            assert_eq!(workspace.toasts.len(), 3, "and the folded toast goes");
        });
    }

    /// Errors fold by graph: a fold stands for one graph's errors and names it, and its Open
    /// activity shows that graph first, so the log it opens lists the errors it counts. An
    /// outcome nobody knows folded with them is in that log, with its branch. Before, one fold
    /// took every graph's errors, and its Open activity showed whichever graph was shown.
    #[gpui::test]
    fn errors_fold_by_graph_and_open_their_graphs_activity(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let notes = idle_session("http://a.test", "notes", cx);
        let people = idle_session("http://b.test", "people", cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update(cx, |workspace, cx| {
            workspace.graph_order.push(people.entity_id());
            workspace.subscribe_session(people.clone(), cx);
            workspace.add_behind(notes.clone(), None, cx);
            for id in ["nodes", ACTIVITY_PANEL] {
                let view = cx.new(|_| Field);
                workspace.add_panel(
                    PanelPosition::Bottom,
                    id,
                    id,
                    IconName::Table,
                    view.into(),
                    cx,
                );
            }
            workspace.activate_panel("nodes", cx);
        });
        let refuse =
            |session: &Entity<GraphSession>, n: usize, cx: &mut gpui::VisualTestContext| {
                let outcome = OutcomeKind::Refused {
                    message: "the policy denies it".into(),
                };
                log_outcome(session, &format!("Write {n}"), outcome, None, cx);
            };
        let shown = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .toasts
                    .iter()
                    .map(|toast| toast.message.to_string())
                    .collect::<Vec<_>>()
            })
        };
        refuse(&notes, 1, cx);
        log_outcome(
            &notes,
            "Write 2",
            OutcomeKind::Unknown {
                reason: "the connection closed".into(),
            },
            None,
            cx,
        );
        refuse(&people, 3, cx);
        refuse(&people, 4, cx);
        // The stack is full of errors: the oldest of one graph fold, apart from the other's.
        refuse(&people, 5, cx);
        refuse(&people, 6, cx);
        refuse(&notes, 7, cx);
        let refused =
            |n: usize| format!("Write {n} · refused: the policy denies it. Nothing was written.");
        assert_eq!(
            shown(cx),
            [
                "notes: 2 earlier errors".to_owned(),
                "people: 3 earlier errors".to_owned(),
                refused(6),
                format!("notes: {}", refused(7)),
            ]
        );

        // The notes fold's Open activity shows notes, whose log holds what it counts.
        let fold = workspace.read_with(cx, |workspace, _| workspace.toasts[0].id);
        workspace.update_in(cx, |workspace, window, cx| {
            window.focus(&workspace.focus_handle, cx)
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.run_follow_up(fold, window, cx)
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let session = workspace.session().unwrap().read(cx);
            assert_eq!(session.graph_id, "notes", "the fold's graph is shown");
            assert_eq!(session.branch, "main");
            let logged: Vec<_> = session
                .outcomes
                .iter()
                .map(|entry| entry.description.as_str())
                .collect();
            assert_eq!(logged, ["Write 1", "Write 2", "Write 7"]);
            assert!(matches!(
                session.outcomes[1].outcome,
                OutcomeKind::Unknown { .. }
            ));
            assert_eq!(
                (workspace.bottom.open, workspace.bottom.active_id()),
                (true, Some(ACTIVITY_PANEL.into())),
                "Open activity shows the activity log"
            );
        });
        // Errors about no graph fold only with each other, and offer nothing to open.
        workspace.update(cx, |workspace, cx| {
            workspace.toasts.clear();
            for n in 1..=5 {
                workspace.show_toast(ToastKind::Error, format!("Notice {n}"), cx);
            }
            let fold = &workspace.toasts[0];
            assert_eq!(fold.message.as_ref(), "2 earlier errors");
            assert!(fold.action.is_none());
        });
    }

    /// Errors kept from four sources, three graphs and a notice about the servers, leave no
    /// two of one graph to fold: they fold across graphs instead, so every new error still
    /// makes room within [`TOASTS_SHOWN`], and a delete's Undo stays up. The graphs' errors fold
    /// first, each kept in its graph's activity log, so the notice, in no log, stays up. The
    /// fold counts what it took and opens the activity log of the graph the window shows.
    /// Before, nothing folded, and each new error ran the stack past the limit, below the
    /// window's edge; then the notice was folded with the rest, and its text was gone.
    #[gpui::test]
    fn errors_of_four_sources_fold_across_graphs_within_the_stack(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let notes = idle_session("http://a.test", "notes", cx);
        let people = idle_session("http://b.test", "people", cx);
        let ops = idle_session("http://c.test", "ops", cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update(cx, |workspace, cx| {
            workspace.graph_order.push(people.entity_id());
            workspace.subscribe_session(people.clone(), cx);
            workspace.add_behind(notes.clone(), None, cx);
            workspace.add_behind(ops.clone(), None, cx);
            for id in ["nodes", ACTIVITY_PANEL] {
                let view = cx.new(|_| Field);
                workspace.add_panel(
                    PanelPosition::Bottom,
                    id,
                    id,
                    IconName::Table,
                    view.into(),
                    cx,
                );
            }
            workspace.activate_panel("nodes", cx);
        });
        let refuse =
            |session: &Entity<GraphSession>, n: usize, cx: &mut gpui::VisualTestContext| {
                let outcome = OutcomeKind::Refused {
                    message: "the policy denies it".into(),
                };
                log_outcome(session, &format!("Write {n}"), outcome, None, cx);
            };
        let shown = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .toast_messages()
                    .iter()
                    .map(|message| message.to_string())
                    .collect::<Vec<_>>()
            })
        };
        let refused =
            |n: usize| format!("Write {n} · refused: the policy denies it. Nothing was written.");
        log_outcome(
            &people,
            "Delete Person ada",
            OutcomeKind::Committed {
                commit: Some("c1".into()),
            },
            Some(graph_store::FollowUp::UndoDelete),
            cx,
        );
        refuse(&notes, 2, cx);
        refuse(&people, 3, cx);
        refuse(&ops, 4, cx);
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(ToastKind::Error, "A notice about the servers", cx)
        });
        assert_eq!(
            shown(cx),
            [
                "Delete Person ada · committed to main".to_owned(),
                "2 earlier errors".to_owned(),
                format!("ops: {}", refused(4)),
                "A notice about the servers".to_owned(),
            ],
            "the two oldest errors, of two graphs, folded"
        );
        refuse(&people, 5, cx);
        refuse(&notes, 6, cx);
        assert_eq!(
            shown(cx),
            [
                "Delete Person ada · committed to main".to_owned(),
                "4 earlier errors".to_owned(),
                "A notice about the servers".to_owned(),
                format!("notes: {}", refused(6)),
            ],
            "each graph's error after joins the fold, the Undo and the notice still up"
        );

        let fold = workspace.read_with(cx, |workspace, _| workspace.toasts[1].id);
        workspace.update_in(cx, |workspace, window, cx| {
            window.focus(&workspace.focus_handle, cx)
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.run_follow_up(fold, window, cx)
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.session().unwrap().read(cx).graph_id,
                "people",
                "the graph shown stays"
            );
            assert_eq!(
                (workspace.bottom.open, workspace.bottom.active_id()),
                (true, Some(ACTIVITY_PANEL.into())),
                "Open activity shows the activity log"
            );
        });
    }

    /// Toasts are told apart by the outcome, never by their text: two deletes described alike
    /// are two toasts, each undoing its own commit, and two outcomes nobody knows, alike but on
    /// two branches, each name their branch and open its history.
    #[gpui::test]
    fn outcomes_described_alike_are_toasted_each_with_its_own_follow_up(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let people = idle_session("http://b.test", "people", cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        workspace.update(cx, |workspace, cx| {
            workspace.graph_order.push(people.entity_id());
            workspace.subscribe_session(people.clone(), cx);
        });
        for commit in ["c2", "c3"] {
            log_outcome(
                &people,
                "Delete Person ada",
                OutcomeKind::Committed {
                    commit: Some(commit.into()),
                },
                Some(graph_store::FollowUp::UndoDelete),
                cx,
            );
        }
        for branch in ["fix", "main"] {
            people.update(cx, |session, cx| session.set_branch(branch.into(), cx));
            log_outcome(
                &people,
                "Edit Person ada",
                OutcomeKind::Unknown {
                    reason: "the connection closed".into(),
                },
                None,
                cx,
            );
        }
        workspace.read_with(cx, |workspace, _| {
            let toasts = &workspace.toasts;
            assert_eq!(toasts.len(), 4, "one toast per outcome");
            for (toast, commit) in toasts[..2].iter().zip(["c2", "c3"]) {
                let follow_up = toast.action.as_ref().unwrap();
                assert!(
                    follow_up
                        .action
                        .partial_eq(&studio_actions::graph::UndoDelete {
                            commit: commit.into(),
                            branch: "main".into(),
                            what: "Delete Person ada".into(),
                        }),
                    "Undo puts back {commit}"
                );
            }
            for (toast, branch) in toasts[2..].iter().zip(["fix", "main"]) {
                assert_eq!(
                    toast.message.as_ref(),
                    format!(
                        "Edit Person ada · outcome unknown on {branch}. Check its history before \
                         trying again."
                    )
                );
                let follow_up = toast.action.as_ref().unwrap();
                assert_eq!(follow_up.label.as_ref(), "Open history");
                let on = follow_up.on.as_ref().unwrap();
                assert_eq!(
                    (on.graph.as_str(), on.branch.as_deref()),
                    ("people", Some(branch))
                );
            }
        });
    }

    /// A tab of a parked graph that asks to close (a conversation deleted in another window)
    /// closes there, and the pane it leaves empty with it: the graph shows without it when
    /// shown again.
    #[gpui::test]
    fn a_parked_graphs_tab_closes_when_it_asks(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let notes = idle_session("http://a.test", "notes", cx);
        let people = idle_session("http://b.test", "people", cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let split = |cx: &mut Context<Split>| Split {
            field: cx.new(|_| Field),
            part: cx.new(|_| Part { renders: 0 }),
            caches: false,
            focus_handle: cx.focus_handle(),
        };
        let (kept, closing) = workspace.update_in(cx, |workspace, window, cx| {
            workspace.graph_order.push(notes.entity_id());
            workspace.subscribe_session(notes.clone(), cx);
            let kept = workspace.add_item(window, cx, |_, cx| split(cx));
            workspace.split(SplitDirection::Right, window, cx);
            let closing = workspace.add_item(window, cx, |_, cx| split(cx));
            assert_eq!(workspace.panes.len(), 2);
            workspace.add_behind(people.clone(), None, cx);
            workspace.show_graph(people.entity_id(), window, cx);
            (kept, closing)
        });
        closing.update(cx, |_, cx| cx.emit(ItemEvent::Close));
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.show_graph(notes.entity_id(), window, cx);
            let tabs: Vec<EntityId> = workspace.all_items().map(|item| item.item_id()).collect();
            assert_eq!(tabs, [kept.entity_id()], "the tab that asked is gone");
            assert_eq!(workspace.panes.len(), 1, "and its pane with it");
            assert_eq!(workspace.center, Member::Pane(workspace.active_pane));
        });
    }

    #[gpui::test]
    fn leaving_the_window_pauses_the_cpu_reading(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            let mut settings = StudioSettings::default();
            settings.studio.show_performance = true;
            cx.set_global(settings);
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

    /// The top bar makes room for traffic lights only where the window shows them: off macOS and
    /// in full screen it starts at its own edge, as Zed's title bar does.
    #[test]
    fn the_top_bar_makes_room_for_traffic_lights_only_when_they_show() {
        assert_eq!(lights_room(false), None, "no lights");
        assert_eq!(lights_room(true), Some(TRAFFIC_LIGHTS_WIDTH));
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
