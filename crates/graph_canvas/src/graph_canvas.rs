//! The graph canvas (spec 006): a growing neighbourhood of real nodes, drawn from the
//! session's shared [`Exploration`], so the relationship tree and the canvas always show the
//! same walk, and a query's result nodes. The schema's types have a page of their own, the
//! ontology ([`ontology`]).
//!
//! One `canvas()` element paints everything: dashed lines and chips as quads, a chip's name
//! above a zoom threshold and its letter mark (a hub, a dot) below it. Layout runs on a
//! background thread (`layout::explore_positions`); the main thread only paints. Nodes already
//! drawn never move.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/006-graph-canvas/plan.md`.

mod domains;
mod layered;
mod layout;
mod model;
mod ontology;
mod paint;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    App, AppContext as _, Bounds, ContentMask, Context, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PinchEvent, Pixels,
    Point, Render, ScrollEvent, SharedString, Size, Subscription, Task, Window, canvas, point, px,
};
use graph_store::{
    EntryKind, Exploration, ExplorationEvent, ExplorePath, GraphSession, GroupKey, Loadable,
    NodeRef, PageStatus, SessionEvent,
};
use studio_actions::canvas::{
    ClearSelection, CollapseNode, ExpandNode, FitToWindow, FocusSelected, MoveDown, MoveLeft,
    MoveRight, MoveUp, OpenSelectedPage, PeekSelected, ResetLayout, SelectAll, WalkBack,
    WalkForward, ZoomIn, ZoomOut,
};
use studio_actions::graph::{
    OpenMap, OpenNodePage, OpenType, PeekNode, ShowNodeInTree, ShowNodeOnCanvas,
    ShowResultsOnCanvas,
};
use studio_actions::{ViewKind, ViewLink};
use studio_settings::StudioSettings;
use studio_ui::ds::Tipped;
use studio_ui::ds::space;
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace};
use ui::prelude::*;

use layout::{LayoutKind, LayoutNode, Vec2};
use model::{HUB_STEP, Scene, SceneId, SceneKind};
use studio_ui::menu::ContextMenu;
use studio_ui::motion::{self, Ease, Lerp, Tween};

/// The closest the view zooms in.
const MAX_ZOOM: f32 = 2.;
/// How far one Zoom In or Zoom Out goes.
const ZOOM_STEP: f32 = 1.25;

/// Registers the canvas's keys and the actions that open it.
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(
        cx,
        ontology::ONTOLOGY_KIND,
        |data, workspace, window, cx| {
            let focus = ontology::saved_focus(data);
            if let Some(page) = open_ontology(workspace, false, window, cx)
                && let Some(focus) = focus
            {
                page.update(cx, |page, cx| page.focus_on(&focus, cx));
            }
        },
    );
    studio_workspace::register_item_kind(cx, "canvas", |data, workspace, window, cx| {
        let pinned = pins_from_json(data);
        let root = data
            .get("type_name")
            .and_then(serde_json::Value::as_str)
            .zip(data.get("id").and_then(serde_json::Value::as_str))
            .map(|(type_name, id)| NodeRef::new(type_name, id));
        match root {
            Some(root) => {
                let at_commit = data
                    .get("at_commit")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                if let Some(canvas) = open_canvas(workspace, root, at_commit, false, window, cx) {
                    canvas.update(cx, |canvas, cx| canvas.restore_pins(pinned, cx));
                }
            }
            // A canvas from before each had a root: no tab to come back as, but where the person
            // placed things stays, for each new canvas on this graph (spec 020).
            None => adopt_positions(workspace, pinned, cx),
        }
    });
    cx.observe_new(|workspace: &mut Workspace, window, _cx| {
        if window.is_none() {
            return;
        }
        workspace.register_action(|workspace, action: &ShowResultsOnCanvas, window, cx| {
            let Some(session) = workspace.session().cloned() else {
                return;
            };
            let action = action.clone();
            let seq = NEXT_RESULTS.load(std::sync::atomic::Ordering::Relaxed);
            workspace.open_item::<GraphCanvas>(
                format!("canvas-results:{seq}"),
                window,
                cx,
                move |_, cx| GraphCanvas::with_results(session, &action, cx),
            );
        });
        workspace.register_action(|workspace, action: &OpenMap, window, cx| {
            open_ontology(workspace, action.preview, window, cx);
        });
        workspace.register_action(|workspace, action: &ShowNodeOnCanvas, window, cx| {
            // The tab it was opened from stays (spec 020).
            workspace.keep_active_tab(cx);
            let node = NodeRef::new(action.type_name.clone(), action.id.clone());
            open_canvas(
                workspace,
                node,
                action.at_commit.clone(),
                action.preview,
                window,
                cx,
            );
        });
    })
    .detach();
}

fn open_ontology(
    workspace: &mut Workspace,
    preview: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<ontology::OntologyPage>> {
    let session = workspace.session()?.clone();
    let key = ontology::ontology_key(&session.read(cx).graph_id);
    workspace.open_item_with::<ontology::OntologyPage>(key, preview, window, cx, |_window, cx| {
        ontology::OntologyPage::new(session, cx)
    })
}

fn open_canvas(
    workspace: &mut Workspace,
    root: NodeRef,
    at_commit: Option<String>,
    preview: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<GraphCanvas>> {
    let session = workspace.session()?.clone();
    let key = canvas_key(&session.read(cx).graph_id, &root, at_commit.as_deref());
    workspace.open_item_with::<GraphCanvas>(key, preview, window, cx, |_window, cx| {
        GraphCanvas::new(session, root, at_commit, cx)
    })
}

/// Keeps a saved root-less canvas's placed nodes on the graph's session.
fn adopt_positions(
    workspace: &mut Workspace,
    pinned: HashMap<String, Vec2>,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    session.update(cx, |session, _| {
        for (key, at) in pinned {
            session.canvas_positions.insert(key, [at.x, at.y]);
        }
    });
}

fn canvas_key(graph_id: &str, root: &NodeRef, at_commit: Option<&str>) -> SharedString {
    let key = format!("canvas:{graph_id}:{}:{}", root.type_name, root.id);
    match at_commit {
        Some(commit) => format!("{key}@{commit}").into(),
        None => key.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Explore,
    /// A query's result nodes (specs 006, 007).
    Results,
}

/// What a results canvas draws: a query's nodes, and which ones share a row.
struct ResultsData {
    title: String,
    nodes: Vec<(NodeRef, Option<String>)>,
    links: Vec<(usize, usize)>,
}

/// Results canvases are tabs of their own, one per showing.
static NEXT_RESULTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// Where the view looks and how close: what a camera glide moves.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Camera {
    center: Vec2,
    zoom: f32,
}

impl Lerp for Camera {
    /// Zoom blends on a log scale, so a glide zooms at an even pace.
    fn lerp(&self, to: &Self, t: f32) -> Self {
        Camera {
            center: self.center.lerp(&to.center, t),
            zoom: self.zoom.ln().lerp(&to.zoom.ln(), t).exp(),
        }
    }
}

impl Lerp for Vec2 {
    fn lerp(&self, to: &Self, t: f32) -> Self {
        Vec2::new(self.x.lerp(&to.x, t), self.y.lerp(&to.y, t))
    }
}

struct Drag {
    start: Point<Pixels>,
    center: Vec2,
    moved: bool,
    /// How far the pointer has moved since the drag began, in world units.
    delta: Vec2,
    /// A drag that began on a node or a relationship pill moves it (from where it was)
    /// instead of the view.
    node: Option<(usize, Vec2)>,
    /// It began on a chip: once a relayout removes the chip, it moves nothing, rather than
    /// turning into a pan by the whole distance dragged so far.
    took_chip: bool,
    /// What moves with it: a node's pills not placed by hand, and their "+K more"; with
    /// several chips marked, the other marked chips too.
    followers: Vec<(usize, Vec2)>,
    /// A drag begun with ⇧ on empty space draws an area instead of moving the view.
    area: bool,
    /// The scene the indices above are in. Reads that finish during the drag lay the canvas
    /// out again, which can move a chip to another index or remove it.
    scene: Arc<Scene>,
}

impl Drag {
    /// Whether it pans the view: a drag begun on empty space.
    fn pans(&self) -> bool {
        self.node.is_none() && !self.took_chip
    }

    /// Points the drag's indices into `scene`, by the chips' ids, when the canvas was laid out
    /// again since; a chip no longer drawn stops moving.
    fn follow(&mut self, scene: &Arc<Scene>) {
        if Arc::ptr_eq(&self.scene, scene) {
            return;
        }
        let at = scene.indices();
        let moved = |index: usize| {
            self.scene
                .nodes
                .get(index)
                .and_then(|node| at.get(&node.id).copied())
        };
        self.node = self
            .node
            .and_then(|(index, from)| Some((moved(index)?, from)));
        self.followers = self
            .followers
            .iter()
            .filter_map(|&(index, from)| Some((moved(index)?, from)))
            .collect();
        self.scene = scene.clone();
    }
}

/// What "+K more" does: draws neighbours already read, reads more of a list, opens a value of a
/// grouped relationship, or reads again a page whose read failed.
enum More {
    Drawn,
    Read(GroupKey),
    Open(ExplorePath),
    Retry(GroupKey),
}

/// A layout to work out once the one running is applied ([`GraphCanvas::lay_out`]).
struct PendingLayout {
    generation: u64,
    scene: Scene,
    texts: Vec<paint::ChipText>,
    widths: Vec<f32>,
    place: Box<dyn FnOnce() -> Vec<Vec2> + Send>,
}

/// The key a placed chip is remembered by: a node or a relationship pill. "+K more" follows
/// its pill and is never placed by hand.
fn pin_key(id: &SceneId) -> Option<String> {
    match id {
        SceneId::Node(node) => Some(format!("node:{}/{}", node.type_name, node.id)),
        SceneId::Hub(key) => Some(format!(
            "hub:{}/{}/{}",
            key.node.type_name,
            key.node.id,
            gq_language::builder::relationship_key(&key.group)
        )),
        SceneId::More(_) => None,
    }
}

/// The placed nodes of a saved canvas, `{key: [x, y]}`; anything else reads as none.
fn pins_from_json(data: &serde_json::Value) -> HashMap<String, Vec2> {
    data.get("pinned")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, at)| {
            let at = at.as_array()?;
            let x = at.first()?.as_f64()? as f32;
            let y = at.get(1)?.as_f64()? as f32;
            (x.is_finite() && y.is_finite()).then(|| (key.clone(), Vec2::new(x, y)))
        })
        .collect()
}

fn pins_to_json(pinned: &HashMap<String, Vec2>) -> serde_json::Value {
    let mut keys: Vec<&String> = pinned.keys().collect();
    keys.sort();
    let map: serde_json::Map<String, serde_json::Value> = keys
        .into_iter()
        .map(|key| {
            let at = pinned[key];
            (key.clone(), serde_json::json!([at.x.round(), at.y.round()]))
        })
        .collect();
    serde_json::json!({ "pinned": map })
}

/// A canvas tab: a walk from one node, or a query's result nodes.
pub(crate) struct GraphCanvas {
    session: Entity<GraphSession>,
    exploration: Entity<Exploration>,
    /// The node the walk starts from and the commit it is held at; none on a results canvas.
    walk: Option<(NodeRef, Option<String>)>,
    /// Set for a canvas of query results; such a canvas never shows the exploration.
    results: Option<(usize, Arc<ResultsData>)>,
    focus_handle: FocusHandle,
    scene: Arc<Scene>,
    positions: Arc<Vec<Vec2>>,
    /// Half of each node's chip width at zoom 1, as measured for the layout.
    half_widths: Arc<Vec<f32>>,
    /// What each chip of the scene says, made with the scene rather than on every frame.
    texts: Arc<Vec<paint::ChipText>>,
    /// The scene's node chips, counted with it for the caption and the overlay.
    node_count: usize,
    /// Where each node was drawn, for the explorer's root (or the query, on a results
    /// canvas): such nodes never move.
    known: HashMap<SceneId, Vec2>,
    /// The root the explorer's `known` places are for.
    known_root: Option<NodeRef>,
    /// Nodes the person dragged into place, by [`pin_key`]: kept across layouts and restarts,
    /// with the graph's tabs (spec 006, saved layouts).
    pinned: HashMap<String, Vec2>,
    layout_key: Option<(Mode, Option<NodeRef>)>,
    layout_generation: u64,
    /// The layout being worked out off the main thread, until it is applied: a fit waits
    /// for it, rather than framing the layout it replaces.
    layout_task: Option<Task<()>>,
    /// The newest layout asked for while one runs. A running layout is one poll of the
    /// background executor and cannot be stopped, so the newest waits for it, and those it
    /// replaced never start.
    next_layout: Option<PendingLayout>,
    /// The walk changed since the scene was laid out: it is laid out when the canvas is next
    /// drawn ([`Self::lay_out_owed`]), so a canvas tab behind another lays out nothing.
    layout_owed: bool,
    /// The node cap of the scene last asked for, laid out or on its way.
    requested_cap: usize,
    hub_limits: HashMap<GroupKey, usize>,
    center: Vec2,
    zoom: f32,
    /// The view on its way to `center` and `zoom` (spec 006: the view glides to what it
    /// centres on).
    glide: Option<Tween<Camera>>,
    /// Nodes on their way to their places: new ones grow out of the node they came from.
    appear: HashMap<usize, Tween<Vec2>>,
    /// The person moved the view by hand since the last fit: a resize then leaves it alone.
    camera_touched: bool,
    /// The canvas's size at the last fit, to fit again when it changes.
    fitted_size: Option<Size<Pixels>>,
    fit_zoom: f32,
    fit_pending: bool,
    center_on: Option<SceneId>,
    selected: Option<SceneId>,
    /// Nodes stepped back from with `[`, most recent last, for `]`.
    walked: Vec<SceneId>,
    hovered: Option<usize>,
    /// The node the first click of a double click opened: the second leaves it open.
    clicked_open: Option<SceneId>,
    /// The cut chip the pointer rested on, shown open (spec 006, "Opened chips").
    opened: Option<SceneId>,
    drag: Option<Drag>,
    /// Chips marked together: by an area drawn with ⇧-drag, ⇧-click, or ⌘A.
    marked: HashSet<SceneId>,
    /// The area being drawn, from where the drag began to the pointer, in window pixels.
    area: Option<(Point<Pixels>, Point<Pixels>)>,
    /// The right-click menu and where it opened.
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// The read ahead for the chip the pointer rests on, once it has rested.
    _prefetch: Option<Task<()>>,
    /// Where the chip the pointer is on sits in the exploration, whose read ahead stops when
    /// the pointer leaves it.
    read_ahead: Option<ExplorePath>,
    /// The wait before the chip the pointer rests on opens.
    _open: Option<Task<()>>,
    /// A rebuild waiting for reads that finish together.
    _loaded: Option<Task<()>>,
    /// Each drawn chip's place in `scene`, by its id: made with the scene, so finding a chip
    /// (the selected one, the one opened, a menu's) walks nothing, at every frame and move.
    scene_index: HashMap<SceneId, usize>,
    /// The camera and placed chips the last frame drew: one that moves them under a pointer
    /// resting on a chip finds the chip under it again once painted ([`Self::hover_at`]).
    drawn: Option<(Camera, usize)>,
    char_widths: CharWidths,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Each node's chip on screen as last painted, for hit testing.
    chips: Rc<RefCell<Vec<Bounds<Pixels>>>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ItemEvent> for GraphCanvas {}

impl Focusable for GraphCanvas {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for GraphCanvas {
    fn tab_title(&self, cx: &App) -> SharedString {
        if let Some((_, results)) = &self.results {
            return results.title.clone().into();
        }
        let Some((root, at_commit)) = &self.walk else {
            return "Canvas".into();
        };
        let state = self.exploration.read(cx).state();
        let name = state.label(root).unwrap_or(&root.id);
        match at_commit {
            Some(commit) => {
                format!("Canvas: {name} @ {}", studio_ui::short_commit_id(commit)).into()
            }
            None => format!("Canvas: {name}").into(),
        }
    }

    /// A walk's views. A query's result picture has none: it is that result's, not a thing's.
    fn views(&self, _cx: &App) -> Vec<ViewLink> {
        if self.walk.is_none() {
            return Vec::new();
        }
        vec![
            ViewLink::to(ViewKind::Page),
            ViewLink::to(ViewKind::Tree),
            ViewLink::current(ViewKind::Canvas),
        ]
    }

    fn view_action(&self, kind: ViewKind, _cx: &App) -> Option<Box<dyn gpui::Action>> {
        let (root, at_commit) = self.walk.as_ref()?;
        match kind {
            ViewKind::Page => {
                let node = match &self.selected {
                    Some(SceneId::Node(node)) => node.clone(),
                    _ => root.clone(),
                };
                Some(Box::new(OpenNodePage {
                    type_name: node.type_name,
                    id: node.id,
                    at_commit: at_commit.clone(),
                }))
            }
            ViewKind::Tree => Some(Box::new(ShowNodeInTree {
                type_name: root.type_name.clone(),
                id: root.id.clone(),
                at_commit: at_commit.clone(),
                preview: true,
            })),
            _ => None,
        }
    }

    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        self.walk
            .as_ref()
            .map(|(root, _)| studio_actions::Thing::Node {
                type_name: root.type_name.clone(),
                id: root.id.clone(),
            })
    }

    /// The icons of the Graph panel's rows: the canvas is a walk from a node, a query's result
    /// nodes are a map.
    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(match self.mode() {
            Mode::Explore => IconName::Crosshair,
            Mode::Results => IconName::GitGraph,
        })
    }

    fn dedupe_key(&self, cx: &App) -> Option<SharedString> {
        match (&self.results, &self.walk) {
            (Some((seq, _)), _) => Some(format!("canvas-results:{seq}").into()),
            (None, Some((root, at_commit))) => Some(canvas_key(
                &self.session.read(cx).graph_id,
                root,
                at_commit.as_deref(),
            )),
            (None, None) => None,
        }
    }

    /// The exploration canvas comes back after a restart; a results canvas is transient.
    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        let (root, at_commit) = self.walk.as_ref()?;
        let mut data = pins_to_json(&self.pinned);
        data["type_name"] = root.type_name.clone().into();
        data["id"] = root.id.clone().into();
        data["at_commit"] = at_commit.clone().into();
        Some(SavedItem {
            kind: "canvas".into(),
            data,
        })
    }

    fn is_stale(&self, cx: &App) -> bool {
        self.results.is_none() && self.exploration.read(cx).newer_available()
    }
}

impl GraphCanvas {
    /// Whether the canvas draws the exploration: not a results canvas.
    fn draws_exploration(&self) -> bool {
        self.results.is_none()
    }

    /// What the canvas draws: a query's result nodes when it was opened with them (whether or
    /// not they are laid out yet), else the walk from a node.
    fn mode(&self) -> Mode {
        if self.results.is_some() {
            Mode::Results
        } else {
            Mode::Explore
        }
    }
}

impl GraphCanvas {
    /// A canvas walking from `root`, starting from where the person placed things on the graph's
    /// canvas of before; a copy, so moving a node here moves it in no other tab (spec 020).
    fn new(
        session: Entity<GraphSession>,
        root: NodeRef,
        at_commit: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let exploration = Exploration::for_walk(&session, root.clone(), at_commit.clone(), cx);
        let pinned = session
            .read(cx)
            .canvas_positions
            .iter()
            .map(|(key, at)| (key.clone(), Vec2::new(at[0], at[1])))
            .collect();
        Self::build(
            session,
            exploration,
            Some((root, at_commit)),
            None,
            pinned,
            cx,
        )
    }

    /// The canvas, of `results` when given: built with them, so it lays out only them, not
    /// the exploration first.
    fn build(
        session: Entity<GraphSession>,
        exploration: Entity<Exploration>,
        walk: Option<(NodeRef, Option<String>)>,
        results: Option<(usize, Arc<ResultsData>)>,
        pinned: HashMap<String, Vec2>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(&exploration, |this, _, event, cx| match event {
                // A results canvas draws none of the exploration, and its tab marks no newer
                // data ([`Self::is_stale`]): the walk's changes are nothing to it.
                ExplorationEvent::Changed | ExplorationEvent::Loaded
                    if !this.draws_exploration() => {}
                ExplorationEvent::Changed => {
                    this._loaded = None;
                    cx.emit(ItemEvent::UpdateTab);
                    this.owe_layout(cx);
                }
                // Reads that finish together (a node's counts) are laid out in one go.
                ExplorationEvent::Loaded => {
                    if this._loaded.is_none() {
                        this._loaded = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(COALESCE_LOADS).await;
                            this.update(cx, |this, cx| {
                                this._loaded = None;
                                cx.emit(ItemEvent::UpdateTab);
                                this.owe_layout(cx);
                            })
                            .ok();
                        }));
                    }
                }
                // A results canvas keeps its own selection.
                ExplorationEvent::SelectionChanged => {
                    if this.draws_exploration() {
                        this.follow_selection(cx);
                    }
                }
            }),
            // Another writer's schema change shows as newer data, not at once.
            cx.subscribe(&session, |this, _, event: &SessionEvent, cx| {
                if let SessionEvent::SchemaChanged = event {
                    this.rebuild(cx)
                }
            }),
            // A new node cap (the setting, or the graph's own) is drawn at once, so the canvas
            // and its banner never show different limits.
            cx.observe_global::<StudioSettings>(|this, cx| {
                if this.requested_cap != this.node_cap(cx) {
                    this.rebuild(cx);
                }
            }),
        ];
        let mut canvas = Self {
            session,
            exploration,
            walk,
            results,
            focus_handle: cx.focus_handle(),
            scene: Arc::default(),
            positions: Arc::default(),
            half_widths: Arc::default(),
            texts: Arc::default(),
            node_count: 0,
            known: HashMap::new(),
            known_root: None,
            pinned,
            layout_key: None,
            layout_generation: 0,
            layout_task: None,
            next_layout: None,
            layout_owed: false,
            requested_cap: 0,
            hub_limits: HashMap::new(),
            center: Vec2::default(),
            zoom: 1.,
            glide: None,
            appear: HashMap::new(),
            camera_touched: false,
            fitted_size: None,
            fit_zoom: 1.,
            fit_pending: true,
            center_on: None,
            selected: None,
            walked: Vec::new(),
            hovered: None,
            clicked_open: None,
            opened: None,
            drag: None,
            marked: HashSet::new(),
            area: None,
            context_menu: None,
            _prefetch: None,
            read_ahead: None,
            _open: None,
            _loaded: None,
            scene_index: HashMap::new(),
            drawn: None,
            char_widths: CharWidths::default(),
            bounds: Rc::new(Cell::new(None)),
            chips: Rc::new(RefCell::new(Vec::new())),
            _subscriptions: subscriptions,
        };
        canvas.rebuild(cx);
        canvas
    }

    /// A canvas of a query's result nodes.
    fn with_results(
        session: Entity<GraphSession>,
        action: &ShowResultsOnCanvas,
        cx: &mut Context<Self>,
    ) -> Self {
        let nodes = action
            .nodes
            .iter()
            .map(|n| {
                (
                    NodeRef::new(n.type_name.clone(), n.id.clone()),
                    n.label.clone(),
                )
            })
            .collect();
        let seq = NEXT_RESULTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let results = Arc::new(ResultsData {
            title: action.title.clone(),
            nodes,
            links: action.links.clone(),
        });
        let exploration = Exploration::unrooted(&session, cx);
        Self::build(
            session,
            exploration,
            None,
            Some((seq, results)),
            HashMap::new(),
            cx,
        )
    }

    /// The commit this canvas is held at, which what it opens is held at too (spec 020).
    fn held_at(&self) -> Option<String> {
        self.walk
            .as_ref()
            .and_then(|(_, at_commit)| at_commit.clone())
    }

    /// The explorer's scene as the exploration stands, under the node cap; none without a
    /// schema.
    fn explore_scene(&self, cx: &App) -> Option<Scene> {
        self.session.read(cx).schema_model()?;
        let exploration = self.exploration.read(cx);
        let entries = exploration.entries();
        Some(model::explore_scene(
            &entries,
            exploration.state(),
            &self.hub_limits,
            self.node_cap(cx),
        ))
    }

    /// Lays the walk out when the canvas is next drawn ([`Self::layout_owed`]).
    fn owe_layout(&mut self, cx: &mut Context<Self>) {
        self.layout_owed = true;
        cx.notify();
    }

    /// Lays out what the walk's changes left to lay out: called when the canvas is drawn. A
    /// change that leaves the walk's scene as it is drawn (a page asked for and not in yet, a
    /// selection, a read that changes no chip) lays out nothing: with no layout under way and
    /// no chip being dragged, the chips stay where they are. Pins and Reset layout ask for a
    /// layout of the same scene, and go through [`Self::rebuild`].
    fn lay_out_owed(&mut self, cx: &mut Context<Self>) {
        if !self.layout_owed {
            return;
        }
        // A layout under way is let finish and shown, and what the walk changed meanwhile is
        // laid out after it, from where it put the chips. Thrown away at each change, as it
        // was, a walk that changed faster than a layout ran (Expand all, pages landing near the
        // cap) showed none until the changes stopped.
        if self.layout_task.is_some() {
            return;
        }
        let settled = self.draws_exploration()
            && self.layout_task.is_none()
            && self.next_layout.is_none()
            && !self.drag.as_ref().is_some_and(|drag| drag.moved);
        if settled && let Some(scene) = self.explore_scene(cx) {
            let root = self.exploration.read(cx).root().cloned();
            if self.layout_key == Some((Mode::Explore, root)) && *self.scene == scene {
                self.layout_owed = false;
                return;
            }
            self.layout_owed = false;
            self.lay_out_explore(scene, cx);
            return;
        }
        self.rebuild(cx);
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        // A layout of the scene as it stands covers what is owed.
        self.layout_owed = false;
        if self.session.read(cx).schema_model().is_none() {
            self.scene = Arc::default();
            self.scene_index.clear();
            self.texts = Arc::default();
            self.node_count = 0;
            self.positions = Arc::default();
            self.hovered = None;
            self.chips.borrow_mut().clear();
            cx.notify();
            return;
        }
        if let Some((_, results)) = self.results.clone() {
            self.rebuild_results(&results, cx);
            return;
        }
        if let Some(scene) = self.explore_scene(cx) {
            self.lay_out_explore(scene, cx);
        }
    }

    /// Lays out `scene`, the exploration's as it stands, off the main thread: drawn chips keep
    /// their places, and what is new grows from beside them.
    fn lay_out_explore(&mut self, scene: Scene, cx: &mut Context<Self>) {
        let root = self.exploration.read(cx).root().cloned();
        let key = (Mode::Explore, root);
        if self.layout_key.as_ref() != Some(&key) {
            // Another root: the chips' places and the hubs widened are the old walk's.
            if self.known_root != key.1 {
                self.known.clear();
                self.hub_limits.clear();
                self.known_root = key.1.clone();
            }
            self.fit_pending = true;
            self.layout_key = Some(key);
        }
        self.layout_generation += 1;
        // Chips being dragged are laid out from where they are now, so what opens on
        // them meanwhile grows from there.
        let dragged: HashMap<SceneId, Vec2> = match self.drag.as_mut() {
            Some(drag) if drag.moved => {
                drag.follow(&self.scene);
                drag.node
                    .iter()
                    .chain(&drag.followers)
                    .filter_map(|&(index, _)| {
                        let id = self.scene.nodes.get(index)?.id.clone();
                        Some((id, *self.positions.get(index)?))
                    })
                    .collect()
            }
            _ => HashMap::new(),
        };
        let (texts, widths) = chip_texts(&scene, &measurer(cx, &self.char_widths));
        let nodes: Vec<LayoutNode> = scene
            .nodes
            .iter()
            .zip(&widths)
            .map(|(node, width)| LayoutNode {
                parent: node.parent,
                kind: match node.kind {
                    SceneKind::Hub { .. } => LayoutKind::Hub,
                    SceneKind::More { .. } => LayoutKind::More,
                    _ => LayoutKind::Node,
                },
                space: *width,
                // "+K more" stands for what is not drawn yet: it moves to the end of
                // its column as neighbours come in, while drawn nodes stay put.
                previous: if matches!(node.kind, SceneKind::More { .. }) {
                    None
                } else {
                    dragged.get(&node.id).copied().or_else(|| {
                        pin_key(&node.id)
                            .and_then(|key| self.pinned.get(&key))
                            .or_else(|| self.known.get(&node.id))
                            .copied()
                    })
                },
            })
            .collect();
        self.lay_out(
            scene,
            texts,
            widths,
            move || layout::explore_positions(&nodes),
            cx,
        );
    }

    /// Places `scene` off the main thread with `place`, and shows it unless a newer layout
    /// was asked for meanwhile. One runs at a time: asked for while one runs, it waits for it,
    /// in place of any other waiting.
    fn lay_out(
        &mut self,
        scene: Scene,
        texts: Vec<paint::ChipText>,
        widths: Vec<f32>,
        place: impl FnOnce() -> Vec<Vec2> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.requested_cap = scene.cap;
        // A layout of the scene as it stands covers what is owed.
        self.layout_owed = false;
        let layout = PendingLayout {
            generation: self.layout_generation,
            scene,
            texts,
            widths,
            place: Box::new(place),
        };
        if self.layout_task.is_some() {
            self.next_layout = Some(layout);
            return;
        }
        self.start_layout(layout, cx);
    }

    fn start_layout(&mut self, layout: PendingLayout, cx: &mut Context<Self>) {
        let PendingLayout {
            generation,
            scene,
            texts,
            widths,
            place,
        } = layout;
        let positions = cx.background_spawn(async move { place() });
        self.layout_task = Some(cx.spawn(async move |this, cx| {
            let positions = positions.await;
            this.update(cx, |this, cx| {
                this.layout_task = None;
                if this.layout_generation == generation {
                    this.apply_layout(scene, texts, widths, positions, cx);
                }
                if let Some(next) = this.next_layout.take() {
                    this.start_layout(next, cx);
                } else if this.layout_owed {
                    // Drawn again, the canvas lays out what the walk changed meanwhile.
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Lays out a query's nodes: a node first reached through a row hangs off the node before
    /// it in that row; the rest sit on a ring.
    fn rebuild_results(&mut self, results: &ResultsData, cx: &mut Context<Self>) {
        let mut scene = model::results_scene(&results.nodes, &results.links, self.node_cap(cx));
        let count = scene.nodes.len();
        let mut parent = vec![None; count];
        for (from, to) in &results.links {
            if *from < count && *to < count && from < to && parent[*to].is_none() {
                parent[*to] = Some(*from);
            }
        }
        let roots: Vec<usize> = (0..count).filter(|i| parent[*i].is_none()).collect();
        let ring = layout::result_root_positions(roots.len());
        let mut fixed = vec![None; count];
        for (root, position) in roots.iter().zip(ring) {
            fixed[*root] = Some(position);
        }
        for (node, parent) in scene.nodes.iter_mut().zip(&parent) {
            node.parent = *parent;
        }
        let (texts, widths) = chip_texts(&scene, &measurer(cx, &self.char_widths));
        let nodes: Vec<LayoutNode> = scene
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| LayoutNode {
                parent: node.parent,
                kind: LayoutKind::Node,
                space: widths[i],
                previous: pin_key(&node.id)
                    .and_then(|key| self.pinned.get(&key).copied())
                    .or(fixed[i]),
            })
            .collect();
        if self.layout_key.is_none() {
            self.fit_pending = true;
        }
        self.layout_key = Some((Mode::Results, None));
        self.layout_generation += 1;
        self.lay_out(
            scene,
            texts,
            widths,
            move || layout::explore_positions(&nodes),
            cx,
        );
    }

    fn apply_layout(
        &mut self,
        scene: Scene,
        texts: Vec<paint::ChipText>,
        widths: Vec<f32>,
        mut positions: Vec<Vec2>,
        cx: &mut Context<Self>,
    ) {
        self.half_widths = Arc::new(widths);
        // A placed node stays exactly where it was put.
        for (node, position) in scene.nodes.iter().zip(positions.iter_mut()) {
            if let Some(pinned) = pin_key(&node.id).and_then(|key| self.pinned.get(&key)) {
                *position = *pinned;
            }
        }
        // The chips drawn now keep their places; one no longer drawn is placed again when it
        // comes back, beside what is drawn then, not over it.
        self.known = scene
            .nodes
            .iter()
            .zip(&positions)
            .map(|(node, position)| (node.id.clone(), *position))
            .collect();
        // So do the marks: an action on the marked chips acts on chips drawn.
        let known = &self.known;
        self.marked.retain(|id| known.contains_key(id));
        // Every node glides from where it is drawn now to its new place; a new one from where
        // the node it hangs off is drawn, so expanding reads as growing.
        let now = Instant::now();
        let drawn: HashMap<SceneId, Vec2> = self
            .scene
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.clone(), self.drawn_position(index, now)))
            .collect();
        self.appear.clear();
        if motion::enabled(cx) && !self.fit_pending {
            for (index, node) in scene.nodes.iter().enumerate() {
                let to = positions[index];
                let from = drawn.get(&node.id).copied().or_else(|| {
                    let parent = node.parent?;
                    drawn
                        .get(&scene.nodes.get(parent)?.id)
                        .copied()
                        .or_else(|| positions.get(parent).copied())
                });
                if let Some(from) = from
                    && (from - to).length() > 0.5
                {
                    self.appear.insert(
                        index,
                        Tween::glide(from, to, motion::GLIDE, Ease::OutCubic, now),
                    );
                }
            }
        }
        // The pointer is on the same chip, wherever the new scene puts it: dropping it would
        // hide its line labels and let a nudge close the chip shown open.
        let hovered = self
            .hovered
            .and_then(|index| self.scene.nodes.get(index))
            .map(|node| node.id.clone());
        self.texts = Arc::new(texts);
        self.node_count = scene
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, SceneKind::Node { .. }))
            .count();
        self.scene_index = scene
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.clone(), index))
            .collect();
        self.scene = Arc::new(scene);
        self.hovered = hovered.and_then(|id| self.index_of(&id));
        self.positions = Arc::new(positions);
        // Chips being dragged stay under the pointer, not where the new layout put them.
        if let Some(drag) = self.drag.as_mut().filter(|drag| drag.moved) {
            // Followed first: a drag whose chip the layout removed moves nothing more.
            drag.follow(&self.scene);
            if drag.node.is_some() {
                let positions = Arc::make_mut(&mut self.positions);
                for &(index, from) in drag.node.iter().chain(&drag.followers) {
                    self.appear.remove(&index);
                    if let Some(position) = positions.get_mut(index) {
                        *position = from + drag.delta;
                    }
                }
            }
        }
        // The chips painted were the old scene's, by its indices: a click before the next
        // paint hits nothing rather than another node.
        self.chips.borrow_mut().clear();
        if let Some(target) = self.center_on.clone()
            && let Some(index) = self.index_of(&target)
        {
            let from = self.camera_now(now);
            self.center = self.positions[index];
            self.zoom = self.zoom.max(1.);
            self.center_on = None;
            self.fit_pending = false;
            self.camera_touched = true;
            self.glide_from(from, cx);
        }
        cx.notify();
    }

    /// Where node `index` is drawn at `now`: on its way, or at its place.
    fn drawn_position(&self, index: usize, now: Instant) -> Vec2 {
        match self.appear.get(&index) {
            Some(tween) => tween.value(now),
            None => self.positions.get(index).copied().unwrap_or_default(),
        }
    }

    /// The camera as drawn at `now`: mid-glide, or at rest.
    fn camera_now(&self, now: Instant) -> Camera {
        match &self.glide {
            Some(glide) if glide.is_running(now) => glide.value(now),
            _ => Camera {
                center: self.center,
                zoom: self.zoom,
            },
        }
    }

    /// Glides the view from `from` (where it was drawn) to the camera just set.
    fn glide_from(&mut self, from: Camera, cx: &App) {
        let to = Camera {
            center: self.center,
            zoom: self.zoom,
        };
        self.glide = (motion::enabled(cx) && from != to)
            .then(|| Tween::glide(from, to, motion::GLIDE, Ease::InOutCubic, Instant::now()));
    }

    /// The person takes the camera: a glide under way stops where it is drawn, and a node still
    /// to be centred on once drawn is let go, so the view does not move off on its own later.
    fn settle_camera(&mut self) {
        self.center_on = None;
        if let Some(glide) = self.glide.take() {
            let now = Instant::now();
            if glide.is_running(now) {
                let at = glide.value(now);
                self.center = at.center;
                self.zoom = at.zoom;
            }
        }
    }

    /// Mirrors the exploration's selection (a tree row, a reveal).
    fn follow_selection(&mut self, cx: &mut Context<Self>) {
        let exploration = self.exploration.read(cx);
        let (Some(root), Some(path)) = (exploration.root(), exploration.selection()) else {
            return;
        };
        self.selected = Some(scene_id_of(path, root));
        cx.notify();
    }

    // ---- camera ----

    fn viewport(&self, window: &Window) -> Bounds<Pixels> {
        self.bounds.get().unwrap_or_else(|| {
            let size = window.viewport_size();
            Bounds::new(point(px(0.), px(0.)), size)
        })
    }

    fn fit(&mut self, viewport: Bounds<Pixels>) {
        let widths = self.half_widths.clone();
        let Some((min, max)) = layout::extent(&self.positions, |index| {
            widths.get(index).copied().unwrap_or(60.)
        }) else {
            return;
        };
        let width = f32::from(viewport.size.width).max(1.);
        let height = f32::from(viewport.size.height).max(1.);
        let zoom = ((width - 80.) / (max.x - min.x).max(1.))
            .min((height - 120.) / (max.y - min.y).max(1.))
            // A fit never zooms a small walk past 1.1.
            .clamp(0.05, 1.1);
        self.zoom = zoom;
        self.fit_zoom = zoom;
        self.center = Vec2::new((min.x + max.x) / 2., (min.y + max.y) / 2.);
        self.fit_pending = false;
        self.fitted_size = Some(viewport.size);
    }

    fn min_zoom(&self) -> f32 {
        (self.fit_zoom * 0.8).min(0.2)
    }

    fn zoom_around(&mut self, factor: f32, anchor: Option<Point<Pixels>>, window: &Window) {
        let viewport = self.viewport(window);
        let new_zoom = (self.zoom * factor).clamp(self.min_zoom(), MAX_ZOOM);
        if let Some(anchor) = anchor {
            let before = to_world(anchor, viewport, self.center, self.zoom);
            let after = to_world(anchor, viewport, self.center, new_zoom);
            self.center = self.center + (before - after);
        }
        self.zoom = new_zoom;
    }

    /// The node whose chip is under `position`, topmost first, by the chips as last painted.
    fn hit(&self, position: Point<Pixels>) -> Option<usize> {
        let chips = self.chips.borrow();
        // An opened chip lies over its neighbours.
        if let Some(index) = self.opened_index()
            && chips
                .get(index)
                .is_some_and(|rect| rect.dilate(px(3.)).contains(&position))
        {
            return Some(index);
        }
        chips
            .iter()
            .enumerate()
            .rev()
            .find(|(index, rect)| {
                *index < self.scene.nodes.len()
                    && rect.size.width > px(0.)
                    && rect.dilate(px(3.)).contains(&position)
            })
            .map(|(index, _)| index)
    }

    // ---- interaction ----

    fn select(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        // A new selection starts a new walk; `[` and `]` keep theirs.
        self.walked.clear();
        let Some(index) = index else {
            self.selected = None;
            cx.notify();
            return;
        };
        let node = &self.scene.nodes[index];
        self.selected = Some(node.id.clone());
        if let Some(path) = node.path.clone()
            && !matches!(node.kind, SceneKind::More { .. })
        {
            self.exploration
                .update(cx, |exploration, cx| exploration.select(Some(path), cx));
        }
        cx.notify();
    }

    /// The drawn chip with this id, by [`Self::scene_index`].
    fn index_of(&self, id: &SceneId) -> Option<usize> {
        self.scene_index.get(id).copied()
    }

    fn selected_index(&self) -> Option<usize> {
        self.index_of(self.selected.as_ref()?)
    }

    /// The pointer left the canvas, or went over something drawn on it: no chip is under it,
    /// and what resting on one would read or open is let go.
    fn pointer_left(&mut self, cx: &mut Context<Self>) {
        self._prefetch = None;
        self.end_read_ahead(None, cx);
        self._open = None;
        if self.hovered.take().is_some() | self.opened.take().is_some() {
            cx.notify();
        }
    }

    /// A cut chip the pointer rests on opens after a moment; one it leaves closes at once.
    fn open_after_rest(&mut self, cx: &mut Context<Self>) {
        self.opened = None;
        self._open = None;
        let Some(node) = self.hovered.and_then(|index| self.scene.nodes.get(index)) else {
            return;
        };
        if paint::whole_label(node).is_none() {
            return;
        }
        let id = node.id.clone();
        self._open = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(OPEN_AFTER).await;
            this.update(cx, |this, cx| {
                this.opened = Some(id);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The chip shown open: the one the pointer rested on, else the selected one, when its name
    /// is cut (spec 006, "Opened chips").
    fn opened_index(&self) -> Option<usize> {
        let cut = |index: &usize| {
            self.scene
                .nodes
                .get(*index)
                .is_some_and(|node| paint::whole_label(node).is_some())
        };
        self.opened
            .as_ref()
            .and_then(|id| self.index_of(id))
            .filter(cut)
            .or_else(|| self.selected_index().filter(cut))
    }

    /// Double click and ↵: expand a node or open a hub, or show more.
    fn activate(
        &mut self,
        index: usize,
        expand_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let node = self.scene.nodes[index].clone();
        match node.kind {
            SceneKind::Node {
                node: graph_node,
                expanded,
                ..
            } => match node.path {
                Some(_) if expanded && !expand_only => self.close(&node.id, cx),
                Some(path) => {
                    self.exploration.update(cx, |e, cx| e.expand(&path, cx));
                }
                None => {
                    window.dispatch_action(
                        Box::new(OpenNodePage {
                            type_name: graph_node.type_name,
                            id: graph_node.id,
                            at_commit: self.held_at(),
                        }),
                        cx,
                    );
                }
            },
            SceneKind::Hub { open, .. } => {
                if let Some(path) = node.path {
                    if open && !expand_only {
                        self.close(&node.id, cx);
                    } else {
                        self.exploration.update(cx, |e, cx| e.expand(&path, cx));
                    }
                }
            }
            SceneKind::More { key, .. } => self.show_more(key, cx),
        }
    }

    /// Closes the node or relationship `id` by every path the exploration opened it by.
    fn close(&mut self, id: &SceneId, cx: &mut Context<Self>) {
        self.close_all(std::slice::from_ref(id), cx);
    }

    /// Closes every one of `ids` (nodes and relationships; "+K more" closes its relationship),
    /// as one change.
    fn close_all(&mut self, ids: &[SceneId], cx: &mut Context<Self>) {
        // A results canvas draws none of the walk, which it does not close.
        if !self.draws_exploration() {
            return;
        }
        let exploration = self.exploration.read(cx);
        let entries = exploration.entries();
        let paths = model::open_paths(&entries, exploration.state(), ids);
        self.exploration
            .update(cx, |e, cx| e.set_open(&paths, false, cx));
    }

    fn show_more(&mut self, key: GroupKey, cx: &mut Context<Self>) {
        let limit = self.hub_limits.entry(key.clone()).or_insert(HUB_STEP);
        *limit += HUB_STEP;
        let wanted = *limit;
        let exploration = self.exploration.read(cx);
        let state = exploration.state();
        let ready = |key: &GroupKey| {
            state
                .neighbours
                .get(key)
                .is_some_and(|list| list.status == PageStatus::Idle && !list.exhausted)
        };
        let loaded = |key: &GroupKey| state.neighbours.get(key).map_or(0, |list| list.rows.len());
        let failed = |key: &GroupKey| {
            state
                .neighbours
                .get(key)
                .is_some_and(|list| matches!(list.status, PageStatus::Failed(_)))
        };
        let more = if exploration.grouping(&key.group).is_some() {
            // A grouped relationship is read a value at a time (spec 005), and its hub draws
            // the neighbours of the values open: more are those of an open value with more to
            // read, else those of the next value not open.
            let entries = exploration.entries();
            let values: Vec<(&GroupKey, &ExplorePath, bool)> = entries
                .iter()
                .filter_map(|entry| match &entry.kind {
                    EntryKind::Bucket {
                        key: value,
                        count,
                        expanded,
                    } if *count > 0 && value.node == key.node && value.group == key.group => {
                        Some((value, &entry.path, *expanded))
                    }
                    _ => None,
                })
                .collect();
            let drawn: usize = values
                .iter()
                .filter(|(.., open)| *open)
                .map(|(value, ..)| loaded(value))
                .sum();
            if drawn >= wanted {
                More::Drawn
            } else if let Some((value, ..)) = values
                .iter()
                .find(|(value, _, open)| *open && failed(value))
            {
                More::Retry((*value).clone())
            } else if let Some((value, ..)) =
                values.iter().find(|(value, _, open)| *open && ready(value))
            {
                More::Read((*value).clone())
            } else if let Some((_, path, _)) = values.iter().find(|(.., open)| !open) {
                More::Open((*path).clone())
            } else {
                More::Drawn
            }
        } else if failed(&key) {
            More::Retry(key)
        } else if loaded(&key) < wanted && ready(&key) {
            More::Read(key)
        } else {
            More::Drawn
        };
        match more {
            More::Drawn => self.rebuild(cx),
            More::Read(key) => self.exploration.update(cx, |e, cx| e.load_more(&key, cx)),
            More::Open(path) => self.exploration.update(cx, |e, cx| e.expand(&path, cx)),
            More::Retry(key) => self
                .exploration
                .update(cx, |e, cx| e.retry(&graph_store::LoadTarget::Page(key), cx)),
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        self.context_menu = None;
        self.settle_camera();
        let hit = self.hit(event.position);
        let node = hit
            .filter(|index| pin_key(&self.scene.nodes[*index].id).is_some())
            .and_then(|index| Some((index, *self.positions.get(index)?)));
        // A marked chip moves with every other marked chip; any chip moves with its pills.
        let followers = match node {
            Some((index, _)) if self.is_marked(index) && self.marked.len() > 1 => {
                let mut moving: Vec<usize> = self.marked_indices();
                moving.retain(|other| *other != index);
                let mut followers: Vec<(usize, Vec2)> = moving
                    .iter()
                    .filter_map(|ix| Some((*ix, *self.positions.get(*ix)?)))
                    .collect();
                moving.push(index);
                followers.extend(self.followers(&moving));
                followers
            }
            Some((index, _)) => self.followers(&[index]),
            None => Vec::new(),
        };
        self.drag = Some(Drag {
            start: event.position,
            center: self.center,
            moved: false,
            delta: Vec2::default(),
            node,
            took_chip: node.is_some(),
            followers,
            area: hit.is_none() && event.modifiers.shift,
            scene: self.scene.clone(),
        });
        // Drawn again, so the caption lets the drag pass over it.
        cx.notify();
    }

    /// A pointer that rests on a node or a relationship pill reads ahead what opening it will
    /// show, so it opens at once; passing over chips reads nothing, and leaving one after stops
    /// what was read ahead for it and is still out.
    fn prefetch_hovered(&mut self, cx: &mut Context<Self>) {
        let path = self
            .hovered
            .and_then(|index| self.scene.nodes.get(index))
            .filter(|node| matches!(node.kind, SceneKind::Node { .. } | SceneKind::Hub { .. }))
            .and_then(|node| node.path.clone());
        self.end_read_ahead(path.as_ref(), cx);
        self.read_ahead = path.clone();
        self._prefetch = path.map(|path| {
            let exploration = self.exploration.clone();
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(PREFETCH_AFTER).await;
                exploration.update(cx, |e, cx| e.prefetch(&path, cx));
            })
        });
    }

    /// The pointer left the chip it was on for `now` (another chip, or none): what was read
    /// ahead for the chip left and is still out stops, unless it was opened since
    /// ([`Exploration::end_prefetch`]).
    fn end_read_ahead(&mut self, now: Option<&ExplorePath>, cx: &mut Context<Self>) {
        if let Some(left) = self.read_ahead.take().filter(|left| Some(left) != now) {
            self.exploration
                .update(cx, |exploration, cx| exploration.end_prefetch(&left, cx));
        }
    }

    fn is_marked(&self, index: usize) -> bool {
        self.scene
            .nodes
            .get(index)
            .is_some_and(|node| self.marked.contains(&node.id))
    }

    /// The marked chips that are drawn, in scene order.
    fn marked_indices(&self) -> Vec<usize> {
        self.scene
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| self.marked.contains(&node.id))
            .map(|(index, _)| index)
            .collect()
    }

    /// The chips that move with the dragged ones (`owners`), from where they are, each once: a
    /// node's relationship pills, unless placed by hand, and each pill's "+K more".
    fn followers(&self, owners: &[usize]) -> Vec<(usize, Vec2)> {
        // Each chip's children, found once: with every chip marked, a drag starts at once.
        let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
        for (child, node) in self.scene.nodes.iter().enumerate() {
            if let Some(parent) = node.parent {
                children.entry(parent).or_default().push(child);
            }
        }
        let mut taken: HashSet<usize> = owners.iter().copied().collect();
        let mut out = Vec::new();
        let mut stack = owners.to_vec();
        while let Some(owner) = stack.pop() {
            for &child in children.get(&owner).into_iter().flatten() {
                let node = &self.scene.nodes[child];
                let follows = match node.kind {
                    SceneKind::Hub { .. } => {
                        pin_key(&node.id).is_none_or(|key| !self.pinned.contains_key(&key))
                    }
                    SceneKind::More { .. } => true,
                    _ => false,
                };
                if follows
                    && taken.insert(child)
                    && let Some(at) = self.positions.get(child)
                {
                    out.push((child, *at));
                    stack.push(child);
                }
            }
        }
        out
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let (Some(drag), true) = (&mut self.drag, event.dragging()) {
            drag.follow(&self.scene);
            let dx = f32::from(event.position.x - drag.start.x);
            let dy = f32::from(event.position.y - drag.start.y);
            if drag.area {
                if drag.moved || dx.abs() + dy.abs() > 3. {
                    drag.moved = true;
                    self.area = Some((drag.start, event.position));
                    cx.notify();
                }
                return;
            }
            if drag.moved || dx.abs() + dy.abs() > 3. {
                drag.moved = true;
                let delta = Vec2::new(dx, dy) * (1. / self.zoom);
                drag.delta = delta;
                match drag.node {
                    // Where the chips end up is remembered once, at the drop.
                    Some(dragged) => {
                        let positions = Arc::make_mut(&mut self.positions);
                        for &(index, from) in std::iter::once(&dragged).chain(&drag.followers) {
                            self.appear.remove(&index);
                            if let Some(position) = positions.get_mut(index) {
                                *position = from + delta;
                            }
                        }
                    }
                    None if drag.pans() => {
                        self.center = drag.center - delta;
                        self.camera_touched = true;
                    }
                    None => {}
                }
                cx.notify();
            }
            return;
        }
        self.hover_at(event.position, cx);
    }

    /// The chip under the pointer at `position` is the one it points at, by the chips as last
    /// painted: what resting on it reads ahead and opens follows it.
    fn hover_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let hovered = self.hit(position);
        if hovered != self.hovered {
            self.hovered = hovered;
            self.prefetch_hovered(cx);
            self.open_after_rest(cx);
            cx.notify();
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut drag) = self.drag.take() else {
            return;
        };
        // Drawn again, so the caption takes its own presses again.
        cx.notify();
        drag.follow(&self.scene);
        if drag.area {
            // The chips the area touches are marked, added to those marked before.
            if let Some((from, to)) = self.area.take() {
                let area = Bounds::from_corners(
                    point(from.x.min(to.x), from.y.min(to.y)),
                    point(from.x.max(to.x), from.y.max(to.y)),
                );
                let inside = chips_in(area, &self.chips.borrow());
                for index in inside {
                    if let Some(node) = self.scene.nodes.get(index) {
                        self.marked.insert(node.id.clone());
                    }
                }
                cx.notify();
            }
            return;
        }
        if drag.moved {
            self.place_dragged(&drag, cx);
            return;
        }
        let hit = self.hit(event.position);
        // ⌘-click marks a chip, or unmarks it, as ⌘-click picks rows in a table.
        if event.modifiers.secondary() {
            if let Some(index) = hit {
                let id = self.scene.nodes[index].id.clone();
                if !self.marked.remove(&id) {
                    self.marked.insert(id);
                }
                cx.notify();
            }
            return;
        }
        if !self.marked.is_empty() {
            self.marked.clear();
            cx.notify();
        }
        self.select(hit, cx);
        let Some(index) = hit else {
            return;
        };
        // ⇧-click shows the chip in the inspector and opens nothing.
        if event.modifiers.shift {
            self.peek(index, window, cx);
            return;
        }
        let clicked_open = self.clicked_open.take().filter(|_| event.click_count >= 2);
        match self.scene.nodes[index].kind {
            // A click opens a relationship or closes it, and shows more; a double click is
            // that one click, not an open and a close.
            SceneKind::Hub { .. } | SceneKind::More { .. } => {
                if event.click_count < 2 {
                    self.activate(index, false, window, cx);
                }
            }
            // A click expands a closed node; an open one is only selected, so a click never
            // folds away what was walked. A double click closes it, unless its first click
            // opened it.
            SceneKind::Node { expanded, .. }
                if self.scene.nodes[index].path.is_some() && !expanded =>
            {
                if event.click_count < 2 {
                    self.clicked_open = Some(self.scene.nodes[index].id.clone());
                    self.activate(index, true, window, cx);
                }
            }
            _ if event.click_count >= 2
                && clicked_open.as_ref() != Some(&self.scene.nodes[index].id) =>
            {
                self.activate(index, false, window, cx)
            }
            _ => {}
        }
    }

    /// Shows a chip's node in the inspector without opening anything: a relationship pill
    /// or "+K more" shows the node it belongs to.
    fn peek(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let node = match &self.scene.nodes[index].kind {
            SceneKind::Node { node, .. } => node.clone(),
            SceneKind::Hub { key, .. } | SceneKind::More { key, .. } => key.node.clone(),
        };
        window.dispatch_action(
            Box::new(PeekNode {
                type_name: node.type_name,
                id: node.id,
                at_commit: self.held_at(),
            }),
            cx,
        );
    }

    /// A drag released outside the canvas ends as one released on it, and an area is dropped.
    fn mouse_up_out(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(mut drag) = self.drag.take() else {
            return;
        };
        if drag.moved && !drag.area {
            drag.follow(&self.scene);
            self.place_dragged(&drag, cx);
        }
        self.area = None;
        cx.notify();
    }

    /// A dragged node or pill stays where it was dropped, and is remembered there, as does
    /// each marked chip that moved with it; the pills that moved with a node keep their new
    /// places without being pinned.
    fn place_dragged(&mut self, drag: &Drag, cx: &mut Context<Self>) {
        let Some((index, _)) = drag.node else {
            return;
        };
        for ix in std::iter::once(index).chain(drag.followers.iter().map(|(ix, _)| *ix)) {
            if let (Some(node), Some(at)) = (self.scene.nodes.get(ix), self.positions.get(ix)) {
                self.known.insert(node.id.clone(), *at);
            }
        }
        let mut placed = vec![index];
        if self.is_marked(index) {
            placed.extend(drag.followers.iter().map(|(ix, _)| *ix));
            placed.retain(|ix| self.is_marked(*ix));
        }
        for ix in placed {
            if let (Some(key), Some(at)) = (
                self.scene.nodes.get(ix).and_then(|node| pin_key(&node.id)),
                self.positions.get(ix),
            ) {
                self.pinned.insert(key, *at);
            }
        }
        cx.emit(ItemEvent::UpdateTab);
        // A layout begun during the drag took where the chips were then: laid out again from
        // where they were dropped.
        if self.layout_task.is_some() {
            self.rebuild(cx);
        }
        // The canvas is drawn cached: Reset layout shows once something is placed.
        cx.notify();
    }

    // ---- the right-click menu ----

    /// A right click: the menu for the marked chips when it lands on one of them, else for the
    /// chip under it (selected first), else for the canvas.
    fn secondary_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.settle_camera();
        let hit = self.hit(event.position);
        let group = hit.is_some_and(|index| self.is_marked(index)) && self.marked.len() > 1;
        if let Some(index) = hit
            && !group
        {
            self.marked.clear();
            self.select(Some(index), cx);
        }
        let menu = if group {
            self.marked_menu(window, cx)
        } else {
            match hit {
                Some(index) => self.chip_menu(index, window, cx),
                None => self.canvas_menu(window, cx),
            }
        };
        let subscription = cx.subscribe_in(
            &menu,
            window,
            |this, _, _: &gpui::DismissEvent, window, cx| {
                this.context_menu = None;
                window.focus(&this.focus_handle, cx);
                cx.notify();
            },
        );
        window.focus(&menu.focus_handle(cx), cx);
        self.context_menu = Some((menu, event.position, subscription));
        cx.notify();
    }

    /// Runs `f` on this canvas from a menu entry.
    fn on_menu(
        this: &gpui::WeakEntity<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let this = this.clone();
        move |window, cx| {
            this.update(cx, |canvas, cx| f(canvas, window, cx)).ok();
        }
    }

    fn chip_menu(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let this = cx.entity().downgrade();
        let node = self.scene.nodes[index].clone();
        let pinned = pin_key(&node.id).is_some_and(|key| self.pinned.contains_key(&key));
        let exploring = self.mode() == Mode::Explore;
        // The schema the rows shown were read under, which can be older than the session's
        // until Refresh, so the grouping choices match the rows.
        let schema = self.exploration.read(cx).schema();
        let grouping = match &node.kind {
            SceneKind::Hub { key, .. } => self.exploration.read(cx).grouping(&key.group).cloned(),
            _ => None,
        };
        let names = match &node.kind {
            SceneKind::Node { node, .. } => self.session.read(cx).type_naming(&node.type_name),
            _ => None,
        };
        let at_commit = self.held_at();
        ContextMenu::build(window, cx, move |mut menu, _, _| {
            let id = node.id.clone();
            match &node.kind {
                SceneKind::Node {
                    node: graph_node,
                    expanded,
                    ..
                } => {
                    let (type_name, key) = (graph_node.type_name.clone(), graph_node.id.clone());
                    menu = menu
                        .entry(
                            "Open",
                            None,
                            open_action(OpenNodePage {
                                type_name: type_name.clone(),
                                id: key.clone(),
                                at_commit: at_commit.clone(),
                            }),
                        )
                        .when(node.path.is_some(), |menu| {
                            if *expanded {
                                menu.entry(
                                    "Collapse",
                                    Some(Box::new(CollapseNode)),
                                    Self::on_menu(&this, move |canvas, _, cx| {
                                        canvas.close(&id, cx)
                                    }),
                                )
                            } else {
                                menu.entry(
                                    "Expand",
                                    Some(Box::new(ExpandNode)),
                                    Self::on_menu(&this, move |canvas, window, cx| {
                                        if let Some(index) = canvas.index_of(&id) {
                                            canvas.activate(index, true, window, cx);
                                        }
                                    }),
                                )
                            }
                        })
                        .entry(
                            "Canvas from here",
                            None,
                            open_action(ShowNodeOnCanvas {
                                type_name: type_name.clone(),
                                id: key.clone(),
                                at_commit: at_commit.clone(),
                                preview: true,
                            }),
                        )
                        .entry(
                            "Tree from here",
                            None,
                            open_action(ShowNodeInTree {
                                type_name: type_name.clone(),
                                id: key.clone(),
                                at_commit: at_commit.clone(),
                                preview: true,
                            }),
                        )
                        .entry(
                            "Open in table",
                            None,
                            open_action(OpenType {
                                type_name,
                                at_commit: at_commit.clone(),
                                filter_id: Some(key.clone()),
                                via: None,
                                preview: false,
                                filters: None,
                            }),
                        )
                        .when_some(names.clone(), |menu, names| {
                            menu.submenu("Show as", move |menu, _, _| {
                                studio_ui::node_names::add_to(menu, &names)
                            })
                        })
                        .separator()
                        .entry("Copy key", None, move |_, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(key.clone()));
                        });
                }
                SceneKind::Hub { key, open, .. } => {
                    let group = key.group.clone();
                    let owner = key.node.clone();
                    menu = menu.entry(
                        if *open { "Close" } else { "Open" },
                        None,
                        Self::on_menu(&this, {
                            move |canvas, window, cx| {
                                if let Some(index) = canvas.index_of(&id) {
                                    canvas.activate(index, false, window, cx);
                                }
                            }
                        }),
                    );
                    let sections = schema
                        .as_ref()
                        .map(|schema| gq_language::builder::GroupBy::sections(schema, &group))
                        .unwrap_or_default();
                    if exploring && !sections.is_empty() {
                        let (this, path, current) =
                            (this.clone(), node.path.clone(), grouping.clone());
                        let group = group.clone();
                        menu = menu.submenu("Group by", move |mut menu, _, _| {
                            let set = |by: Option<gq_language::builder::GroupBy>| {
                                let (group, path) = (group.clone(), path.clone());
                                Self::on_menu(&this, move |canvas, _, cx| {
                                    canvas.exploration.update(cx, |e, cx| {
                                        e.set_grouping(&group, by.clone(), cx);
                                        if let (true, Some(path)) = (by.is_some(), &path) {
                                            e.expand(path, cx);
                                        }
                                    });
                                })
                            };
                            menu = menu.toggleable_entry(
                                "No grouping",
                                current.is_none(),
                                IconPosition::Start,
                                None,
                                set(None),
                            );
                            // As the tree's menu shows them (spec 005).
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
                        });
                    }
                    let back = gq_language::builder::reversed(&group, &owner.type_name);
                    menu = menu.entry(
                        "Open related table",
                        None,
                        open_action(OpenType {
                            type_name: group.other_type,
                            at_commit: None,
                            filter_id: Some(owner.id),
                            via: Some(gq_language::builder::relationship_key(&back)),
                            preview: false,
                            filters: None,
                        }),
                    );
                }
                SceneKind::More { key, .. } => {
                    let key = key.clone();
                    menu = menu.entry(
                        "Show more",
                        None,
                        Self::on_menu(&this, move |canvas, _, cx| {
                            canvas.show_more(key.clone(), cx)
                        }),
                    );
                }
            }
            if pinned {
                let id = node.id.clone();
                menu = menu.separator().entry(
                    "Let the layout place it",
                    None,
                    Self::on_menu(&this, move |canvas, _, cx| {
                        canvas.unpin(std::slice::from_ref(&id), cx)
                    }),
                );
            }
            menu
        })
    }

    /// The menu for several marked chips at once.
    fn marked_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        let this = cx.entity().downgrade();
        let marked: Vec<SceneId> = self.marked.iter().cloned().collect();
        let count = self.marked_indices().len();
        let any_pinned = marked
            .iter()
            .any(|id| pin_key(id).is_some_and(|key| self.pinned.contains_key(&key)));
        let exploring = self.mode() == Mode::Explore;
        ContextMenu::build(window, cx, move |menu, _, _| {
            let ids = marked.clone();
            menu.header(format!("{count} selected"))
                .when(exploring, |menu| {
                    let (expand, collapse) = (ids.clone(), ids.clone());
                    menu.entry(
                        "Expand all",
                        None,
                        Self::on_menu(&this, move |canvas, _, cx| {
                            canvas.expand_marked(&expand, true, cx)
                        }),
                    )
                    .entry(
                        "Collapse all",
                        None,
                        Self::on_menu(&this, move |canvas, _, cx| {
                            canvas.expand_marked(&collapse, false, cx)
                        }),
                    )
                })
                .entry("Keep where they are", None, {
                    let ids = ids.clone();
                    Self::on_menu(&this, move |canvas, _, cx| canvas.pin_marked(&ids, cx))
                })
                .when(any_pinned, |menu| {
                    let ids = ids.clone();
                    menu.entry(
                        "Let the layout place them",
                        None,
                        Self::on_menu(&this, move |canvas, _, cx| canvas.unpin(&ids, cx)),
                    )
                })
                .separator()
                .entry(
                    "Clear selection",
                    Some(Box::new(ClearSelection)),
                    Self::on_menu(&this, |canvas, _, cx| {
                        canvas.marked.clear();
                        cx.notify();
                    }),
                )
        })
    }

    fn canvas_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        let any_pinned = !self.pinned.is_empty();
        let empty = self.scene.nodes.is_empty();
        ContextMenu::build(window, cx, move |menu, _, _| {
            menu.when(!empty, |menu| {
                menu.action("Fit to window", Box::new(FitToWindow))
                    .action("Select all", Box::new(SelectAll))
            })
            .when(any_pinned, |menu| {
                menu.action("Reset layout", Box::new(ResetLayout))
            })
        })
    }

    /// Opens or closes every marked node and relationship.
    fn expand_marked(&mut self, ids: &[SceneId], open: bool, cx: &mut Context<Self>) {
        if !open {
            self.close_all(ids, cx);
            return;
        }
        let at = self.scene.indices();
        let paths: Vec<ExplorePath> = ids
            .iter()
            .filter_map(|id| at.get(id).copied())
            .filter_map(|index| {
                let node = &self.scene.nodes[index];
                match node.kind {
                    SceneKind::Node { .. } | SceneKind::Hub { .. } => node.path.clone(),
                    _ => None,
                }
            })
            .collect();
        self.exploration
            .update(cx, |e, cx| e.set_open(&paths, true, cx));
    }

    /// Keeps the marked chips where they are drawn now.
    fn pin_marked(&mut self, ids: &[SceneId], cx: &mut Context<Self>) {
        let at = self.scene.indices();
        for id in ids {
            if let (Some(key), Some(index)) = (pin_key(id), at.get(id))
                && let Some(position) = self.positions.get(*index)
            {
                self.pinned.insert(key, *position);
            }
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Lets the layout place chips again.
    fn unpin(&mut self, ids: &[SceneId], cx: &mut Context<Self>) {
        for id in ids {
            if let Some(key) = pin_key(id) {
                self.pinned.remove(&key);
            }
            self.known.remove(id);
        }
        cx.emit(ItemEvent::UpdateTab);
        self.rebuild(cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = self
            .scene
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect();
        cx.notify();
    }

    fn clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        if self.marked.is_empty() {
            cx.propagate();
            return;
        }
        self.marked.clear();
        cx.notify();
    }

    fn scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.settle_camera();
        self.camera_touched = true;
        let delta = event.delta.pixel_delta(window.line_height());
        let factor = (-f32::from(delta.y) * 0.004).exp();
        self.zoom_around(factor, Some(event.position), window);
        cx.notify();
    }

    fn pinch(&mut self, event: &PinchEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.settle_camera();
        self.camera_touched = true;
        self.zoom_around(1. + event.delta, Some(event.position), window);
        cx.notify();
    }

    /// Takes the placed nodes of a restored canvas, and lays out again around them.
    fn restore_pins(&mut self, pinned: HashMap<String, Vec2>, cx: &mut Context<Self>) {
        self.pinned = pinned;
        self.fit_pending = true;
        self.rebuild(cx);
    }

    fn reset_layout(&mut self, _: &ResetLayout, _window: &mut Window, cx: &mut Context<Self>) {
        self.pinned.clear();
        self.known.clear();
        self.hub_limits.clear();
        self.fit_pending = true;
        cx.emit(ItemEvent::UpdateTab);
        self.rebuild(cx);
    }

    fn fit_to_window(&mut self, _: &FitToWindow, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.camera_now(Instant::now());
        let viewport = self.viewport(window);
        self.fit(viewport);
        // Fitted on request: a resize fits again until the view is moved by hand.
        self.camera_touched = false;
        self.glide_from(from, cx);
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.camera_now(Instant::now());
        self.zoom_around(ZOOM_STEP, None, window);
        self.camera_touched = true;
        self.glide_from(from, cx);
        cx.notify();
    }

    fn zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.camera_now(Instant::now());
        self.zoom_around(1. / ZOOM_STEP, None, window);
        self.camera_touched = true;
        self.glide_from(from, cx);
        cx.notify();
    }

    fn expand_node(&mut self, _: &ExpandNode, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected_index() {
            self.activate(index, true, window, cx);
        }
    }

    fn collapse_node(&mut self, _: &CollapseNode, _: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_index() else {
            return;
        };
        let id = self.scene.nodes[index].id.clone();
        self.close(&id, cx);
    }

    /// The node a node was reached from, past its hub: what `[` walks back to.
    fn reached_from(&self, index: usize) -> Option<usize> {
        let mut current = self.scene.nodes.get(index)?.parent;
        while let Some(ix) = current {
            match self.scene.nodes[ix].kind {
                SceneKind::Hub { .. } | SceneKind::More { .. } => {
                    current = self.scene.nodes[ix].parent
                }
                _ => return Some(ix),
            }
        }
        None
    }

    fn walk_back(&mut self, _: &WalkBack, _: &mut Window, cx: &mut Context<Self>) {
        let Some(from) = self.selected_index() else {
            return;
        };
        if let Some(parent) = self.reached_from(from) {
            let mut walked = std::mem::take(&mut self.walked);
            walked.push(self.scene.nodes[from].id.clone());
            self.select(Some(parent), cx);
            self.walked = walked;
        }
    }

    fn walk_forward(&mut self, _: &WalkForward, _: &mut Window, cx: &mut Context<Self>) {
        let mut walked = std::mem::take(&mut self.walked);
        while let Some(id) = walked.pop() {
            if let Some(index) = self.index_of(&id) {
                self.select(Some(index), cx);
                break;
            }
        }
        self.walked = walked;
    }

    /// Space: inspects the selected chip as ⇧-click does, a relationship's pill its node.
    fn peek_selected(&mut self, _: &PeekSelected, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected_index() {
            self.peek(index, window, cx);
        }
    }

    /// ⌘↵: the selected node's page (spec 014).
    fn open_selected_page(
        &mut self,
        _: &OpenSelectedPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.selected_index() else {
            return;
        };
        if let SceneKind::Node { node, .. } = &self.scene.nodes[index].kind {
            window.dispatch_action(
                Box::new(studio_actions::graph::OpenNodePage {
                    type_name: node.type_name.clone(),
                    id: node.id.clone(),
                    at_commit: None,
                }),
                cx,
            );
        }
    }

    /// F: the selected node in the middle, close enough to read.
    fn focus_selected(&mut self, _: &FocusSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_index() else {
            return;
        };
        if let Some(position) = self.positions.get(index).copied() {
            let from = self.camera_now(Instant::now());
            self.center = position;
            self.zoom = self.zoom.clamp(1.25, 2.);
            self.camera_touched = true;
            self.glide_from(from, cx);
            cx.notify();
        }
    }

    fn move_selection(&mut self, direction: Vec2, cx: &mut Context<Self>) {
        let from = self.selected_index();
        let next = match from {
            Some(from) => layout::nearest_in_direction(&self.positions, from, direction),
            None => (!self.scene.nodes.is_empty()).then_some(0),
        };
        if let Some(next) = next {
            self.select(Some(next), cx);
        }
    }

    // ---- painting ----

    fn render_canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = paint::Palette {
            text: Color::Default.color(cx),
            muted: Color::Muted.color(cx),
            // Chips are white cards in the light theme, with interfaces.dev's shadow.
            card: studio_ui::ds::surface::solid(cx),
            shadow: studio_ui::ds::elevation::layers(studio_ui::ds::elevation::Lift::Rest, cx),
            pointed: studio_ui::ds::elevation::layers(studio_ui::ds::elevation::Lift::Hover, cx),
            on_tint: studio_ui::ds::surface::on_tint(cx),
            here: studio_ui::ds::surface::here(cx),
            here_halo: studio_ui::ds::surface::here_halo(cx),
            grid_dot: studio_ui::ds::surface::grid_dot(cx),
            canvas: studio_ui::ds::surface::canvas(cx),
            hub_dot: studio_ui::ds::surface::canvas_hub(cx),
            font: theme::theme_settings(cx).ui_font(cx).clone(),
        };
        let scene = self.scene.clone();
        let texts = self.texts.clone();
        let now = Instant::now();
        let positions: Arc<Vec<Vec2>> = if self.appear.is_empty() {
            self.positions.clone()
        } else {
            Arc::new(
                (0..self.positions.len())
                    .map(|index| self.drawn_position(index, now))
                    .collect(),
            )
        };
        let camera = self.camera_now(now);
        let (center, zoom) = (camera.center, camera.zoom);
        let moving = self.glide.as_ref().is_some_and(|g| g.is_running(now))
            || self.appear.values().any(|t| t.is_running(now));
        let selected = self.selected_index();
        let marked: HashSet<usize> = self.marked_indices().into_iter().collect();
        let hovered = self.hovered;
        let opened = self.opened_index();
        let bounds_cell = self.bounds.clone();
        let chips = self.chips.clone();
        let half_widths = self.half_widths.clone();
        canvas(
            move |bounds, window, _cx| {
                // A new size is fitted on the next frame, once the view knows it. Only this view
                // draws again: a whole-window refresh would redraw every cached tab and dock on
                // each frame of a dock's glide.
                let resized = bounds_cell.get().map(|b| b.size) != Some(bounds.size);
                bounds_cell.set(Some(bounds));
                if resized {
                    window.request_animation_frame();
                }
            },
            move |bounds, (), window, cx| {
                motion::keep_animating(moving, window);
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    let screen: Vec<Point<Pixels>> = positions
                        .iter()
                        .map(|world| to_screen(*world, bounds, center, zoom))
                        .collect();
                    let lines = paint::Lines {
                        half_widths: &half_widths,
                        origin: to_screen(Vec2::default(), bounds, center, zoom),
                    };
                    paint::paint(
                        &scene, &texts, &screen, &lines, bounds, zoom, selected, &marked, hovered,
                        opened, &palette, &chips, window, cx,
                    );
                });
            },
        )
        .size_full()
    }

    /// The caption, bottom left: what is drawn, and what a click does.
    fn render_caption(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // What a click does; the summary at the top says what is drawn.
        let text = "click to open · ⇧-click to inspect · ⇧-drag to select";
        div()
            .absolute()
            .bottom(px(space::LG))
            .left(px(space::LG))
            // A press on the caption is not one on a chip drawn under it; a drag passes over it.
            .when(self.drag.is_none(), |caption| caption.occlude())
            .px(px(space::CONTROL_INSET))
            .py(px(space::XS))
            .rounded(px(studio_ui::ds::radii(cx).bar()))
            .bg(studio_ui::ds::surface::solid(cx))
            .map(|caption| studio_ui::ds::lifted(caption, cx))
            .child(studio_ui::ds::text::meta(text))
    }

    /// The most nodes this graph's canvas draws (setting `studio.canvas_max_nodes`, or the
    /// graph's own).
    fn node_cap(&self, cx: &App) -> usize {
        self.session.read(cx).options(cx).canvas_max_nodes.max(1) as usize
    }

    fn render_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let exploration = self.exploration.read(cx);
        let state = exploration.state();
        let newer = exploration.newer_available();
        let summary = match self.mode() {
            Mode::Results => {
                let count = self.scene.nodes.len();
                let title = self
                    .results
                    .as_ref()
                    .map(|(_, r)| r.title.clone())
                    .unwrap_or_default();
                format!("{title} · {}", studio_ui::plural(count, "node", "nodes"))
            }
            Mode::Explore => {
                let root = exploration
                    .root()
                    .map(|root| {
                        state
                            .label(root)
                            .map(str::to_owned)
                            .unwrap_or_else(|| root.id.clone())
                    })
                    .unwrap_or_default();
                let nodes = studio_ui::plural(self.node_count, "node", "nodes");
                format!("From {root} · {nodes}")
            }
        };
        let root_error = match (&state.root_status, self.mode()) {
            (Loadable::Failed(message), Mode::Explore) => Some(message.clone()),
            _ => None,
        };
        let capped = self.scene.capped.clone();
        let cap = self.scene.cap;
        // Floating pills are white cards in the light theme, like the chips. The canvas's
        // floating controls (what is drawn, search and zoom) read as one family, each
        // concentric around what it holds.
        // A press on one is not one on a chip drawn under it; a drag passes over them, as over
        // the caption.
        let dragging = self.drag.is_some();
        let panel = |id: &'static str| {
            studio_ui::ds::float_bar(id, cx).when(!dragging, |bar| bar.occlude())
        };
        let mode = self.mode();
        v_flex()
            .absolute()
            .top(px(space::MD))
            .left(px(space::MD))
            .right(px(space::MD))
            .gap(px(space::MD))
            .child(
                h_flex()
                    .w_full()
                    .gap(px(space::MD))
                    // What is drawn, in a floating pill: the walk's root and its nodes, or the
                    // query.
                    .child(
                        panel("canvas-summary").child(
                            div()
                                .px(px(space::MD))
                                .child(studio_ui::ds::text::muted(summary).single_line()),
                        ),
                    )
                    .when(newer && mode != Mode::Results, |d| {
                        d.child(
                            panel("canvas-newer").child(
                                studio_ui::ds::bar_button("canvas-refresh", "Newer data · Refresh")
                                    .tip(studio_ui::ds::tooltip(
                                        "The branch moved since this exploration was read",
                                    ))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.exploration.update(cx, |e, cx| e.refresh(cx));
                                    })),
                            ),
                        )
                    })
                    .children(
                        studio_workspace::view_strip(Item::views(self, cx), cx)
                            .map(|strip| panel("canvas-views").child(strip)),
                    )
                    .child(div().flex_1())
                    .child(
                        panel("canvas-zoom")
                            .when(!self.pinned.is_empty(), |panel| {
                                panel.child(
                                    studio_ui::ds::icon_button("reset-layout", IconName::RotateCcw)
                                        .tip_fn(|_window, cx| {
                                            studio_ui::ds::tooltip_for_action_view(
                                                "Reset the layout: forget the nodes you placed",
                                                &ResetLayout,
                                                cx,
                                            )
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reset_layout(&ResetLayout, window, cx)
                                        })),
                                )
                            })
                            .child(
                                studio_ui::ds::icon_button("zoom-out", IconName::Dash)
                                    .tip_fn(|_window, cx| {
                                        studio_ui::ds::tooltip_for_action_view(
                                            "Zoom out", &ZoomOut, cx,
                                        )
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.zoom_out(&ZoomOut, window, cx)
                                    })),
                            )
                            .child(
                                studio_ui::ds::bar_button("zoom-fit", "Fit")
                                    .tip_fn(|_window, cx| {
                                        studio_ui::ds::tooltip_for_action_view(
                                            "Fit to window",
                                            &FitToWindow,
                                            cx,
                                        )
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.fit_to_window(&FitToWindow, window, cx)
                                    })),
                            )
                            .child(
                                studio_ui::ds::icon_button("zoom-in", IconName::Plus)
                                    .tip_fn(|_window, cx| {
                                        studio_ui::ds::tooltip_for_action_view(
                                            "Zoom in", &ZoomIn, cx,
                                        )
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.zoom_in(&ZoomIn, window, cx)
                                    })),
                            ),
                    ),
            )
            .children(root_error.map(|message| {
                h_flex().child(
                    panel("canvas-error")
                        .child(studio_ui::ds::icon::lead(IconName::Warning).color(Color::Error))
                        .child(studio_ui::ds::text::error(message))
                        .child(studio_ui::ds::bar_button("canvas-retry", "Retry").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.exploration.update(cx, |e, cx| {
                                    e.retry(&graph_store::LoadTarget::Root, cx)
                                });
                            }),
                        )),
                )
            }))
            .children(capped.map(|(hidden, type_name)| {
                h_flex().child(
                    panel("canvas-capped")
                        .child(studio_ui::ds::icon::lead(IconName::Warning).color(Color::Warning))
                        .child(studio_ui::ds::text::body(format!(
                            "Too many nodes to draw · {hidden} not shown (limit {cap})"
                        )))
                        // The tree shows the walk, which a results canvas does not draw.
                        .when(mode == Mode::Explore, |banner| {
                            banner.child(
                                studio_ui::ds::bar_button("canvas-tree", "Show in tree")
                                    .style(ButtonStyle::Outlined)
                                    .on_click({
                                        let walk = self.walk.clone();
                                        move |_, window, cx| {
                                            let Some((root, at_commit)) = walk.clone() else {
                                                return;
                                            };
                                            window.dispatch_action(
                                                Box::new(ShowNodeInTree {
                                                    type_name: root.type_name,
                                                    id: root.id,
                                                    at_commit,
                                                    preview: false,
                                                }),
                                                cx,
                                            )
                                        }
                                    }),
                            )
                        })
                        .child(
                            studio_ui::ds::bar_button("canvas-table", "Open as table")
                                .style(ButtonStyle::Filled)
                                .on_click({
                                    let at_commit = self.held_at();
                                    move |_, window, cx| {
                                        window.dispatch_action(
                                            Box::new(OpenType {
                                                type_name: type_name.clone(),
                                                at_commit: at_commit.clone(),
                                                filter_id: None,
                                                via: None,
                                                preview: false,
                                                filters: None,
                                            }),
                                            cx,
                                        )
                                    }
                                }),
                        ),
                )
            }))
    }
}

/// Measures text with the UI font, as the painter shapes chip text, for the layout.
///
/// GPUI's app-level `layout_width` shapes each character with CoreText and keeps nothing, so
/// widths are kept here, per character, size and weight, for as long as the canvas lives.
fn measurer(cx: &App, widths: &CharWidths) -> impl Fn(&str, f32, gpui::FontWeight) -> f32 + use<> {
    let system = cx.text_system().clone();
    let font = theme::theme_settings(cx).ui_font(cx).clone();
    let widths = widths.clone();
    move |text, size, weight| {
        let mut widths = widths.borrow_mut();
        // The font changes with the theme settings: a new font starts afresh.
        if widths.0 != font.family {
            *widths = (font.family.clone(), HashMap::new());
        }
        let mut id = None;
        text.chars()
            .map(|ch| {
                *widths
                    .1
                    .entry((ch, size.to_bits(), weight.0.to_bits()))
                    .or_insert_with(|| {
                        let id = *id.get_or_insert_with(|| {
                            system.resolve_font(&gpui::Font {
                                weight,
                                ..font.clone()
                            })
                        });
                        f32::from(system.layout_width(id, px(size), ch))
                    })
            })
            .sum()
    }
}

/// Character widths measured so far, by character, size and weight, for one font family.
type CharWidths = Rc<RefCell<(SharedString, HashMap<(char, u32, u32), f32>)>>;

/// Half of each node's chip width at zoom 1, measured as the painter draws it.
/// Each chip's text, and half its width as `measure` measures it: made once a layout, for the
/// layout's room and for drawing the chips it places.
fn chip_texts(
    scene: &Scene,
    measure: &impl Fn(&str, f32, gpui::FontWeight) -> f32,
) -> (Vec<paint::ChipText>, Vec<f32>) {
    scene
        .nodes
        .iter()
        .map(|node| {
            let text = paint::chip_text(node);
            let half = paint::chip_width(&text, measure) / 2.;
            (text, half)
        })
        .unzip()
}

/// How long finished reads are gathered before the canvas is laid out again: about a frame.
const COALESCE_LOADS: std::time::Duration = std::time::Duration::from_millis(8);

/// How long the pointer rests on a chip before what it opens is read ahead.
const PREFETCH_AFTER: std::time::Duration = std::time::Duration::from_millis(120);
/// How long the pointer rests on a cut chip before it opens to its whole name: long enough
/// that passing over chips opens none.
const OPEN_AFTER: std::time::Duration = std::time::Duration::from_millis(400);

/// A menu entry that dispatches `action` from the window.
fn open_action<A: gpui::Action + Clone>(action: A) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| window.dispatch_action(Box::new(action.clone()), cx)
}

/// The chips an area touches, by their bounds as last painted.
fn chips_in(area: Bounds<Pixels>, chips: &[Bounds<Pixels>]) -> Vec<usize> {
    chips
        .iter()
        .enumerate()
        .filter(|(_, chip)| chip.size.width > px(0.) && chip.intersects(&area))
        .map(|(index, _)| index)
        .collect()
}

fn scene_id_of(path: &ExplorePath, root: &NodeRef) -> SceneId {
    // A value of a grouped relationship is drawn as the relationship's hub.
    match path.group_step() {
        Some((group, _)) => SceneId::Hub(GroupKey::new(path.node(root).clone(), group.clone())),
        None => SceneId::Node(path.node(root).clone()),
    }
}

fn to_screen(world: Vec2, viewport: Bounds<Pixels>, center: Vec2, zoom: f32) -> Point<Pixels> {
    let middle = viewport.center();
    point(
        middle.x + px((world.x - center.x) * zoom),
        middle.y + px((world.y - center.y) * zoom),
    )
}

fn to_world(screen: Point<Pixels>, viewport: Bounds<Pixels>, center: Vec2, zoom: f32) -> Vec2 {
    let middle = viewport.center();
    Vec2::new(
        center.x + f32::from(screen.x - middle.x) / zoom,
        center.y + f32::from(screen.y - middle.y) / zoom,
    )
}

impl Render for GraphCanvas {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // What the session's events left to read is read once the walk is drawn, and what its
        // changes left to lay out is laid out.
        if self.draws_exploration() && self.exploration.read(cx).owes_reads() {
            let exploration = self.exploration.clone();
            cx.defer(move |cx| exploration.update(cx, |e, cx| e.read_owed(cx)));
        }
        if self.layout_owed {
            cx.defer_in(window, |this, _, cx| this.lay_out_owed(cx));
        }
        let schema = self.session.read(cx).schema.clone();
        let body = match schema {
            Loadable::NotLoaded | Loadable::Loading => {
                Some(studio_ui::ds::loading_state("Reading the schema…").into_any_element())
            }
            Loadable::Failed(message) => Some(
                studio_ui::ds::error_state("The schema could not be read", message)
                    .into_any_element(),
            ),
            Loadable::Loaded(_) if self.scene.nodes.is_empty() => Some(
                match self.mode() {
                    Mode::Explore => studio_ui::ds::loading_state("Laying out…"),
                    Mode::Results
                        if self
                            .results
                            .as_ref()
                            .is_some_and(|(_, results)| !results.nodes.is_empty()) =>
                    {
                        studio_ui::ds::loading_state("Laying out…")
                    }
                    Mode::Results => {
                        studio_ui::ds::empty_state(IconName::Hash, "No nodes in the result")
                    }
                }
                .into_any_element(),
            ),
            Loadable::Loaded(_) => None,
        };
        let now = Instant::now();
        // The view or the chips moved under a pointer resting on a chip (a fit, a zoom key, a
        // glide, a layout): the chip under it is found again once this frame is painted, as a
        // pointer move would find it. A drag keeps its own.
        let drawn = (self.camera_now(now), Arc::as_ptr(&self.positions) as usize);
        let moving = !self.appear.is_empty() || self.glide.is_some();
        if self.hovered.is_some() && self.drag.is_none() && (moving || self.drawn != Some(drawn)) {
            let this = cx.entity().downgrade();
            window.on_next_frame(move |window, cx| {
                let position = window.mouse_position();
                this.update(cx, |this, cx| this.hover_at(position, cx)).ok();
            });
        }
        self.drawn = Some(drawn);
        self.appear.retain(|_, tween| tween.is_running(now));
        if self.glide.as_ref().is_some_and(|g| !g.is_running(now)) {
            self.glide = None;
        }
        // The fit waits for the canvas's own bounds, known once it has been painted, and
        // follows them while the person has not moved the view.
        if let Some(viewport) = self.bounds.get()
            && !self.scene.nodes.is_empty()
        {
            let resized = self.fitted_size.is_some_and(|size| size != viewport.size);
            // A fit asked for waits for the layout under way: drawn meanwhile, the canvas
            // still shows the one it replaces.
            let fit = self.fit_pending && self.layout_task.is_none();
            if fit || (resized && !self.camera_touched) {
                if self.fit_pending {
                    self.camera_touched = false;
                }
                self.glide = None;
                self.fit(viewport);
            }
        }
        let background = studio_ui::ds::surface::canvas(cx);
        let canvas = self.render_canvas(cx).into_any_element();
        let overlay = self.render_overlay(cx).into_any_element();
        // The area being drawn: a tinted rectangle from where the drag began to the pointer.
        let area = self
            .area
            .zip(self.bounds.get())
            .map(|((from, to), bounds)| {
                div()
                    .absolute()
                    .left(from.x.min(to.x) - bounds.origin.x)
                    .top(from.y.min(to.y) - bounds.origin.y)
                    .w((to.x - from.x).abs())
                    .h((to.y - from.y).abs())
                    .rounded(px(studio_ui::ds::radii(cx).xs))
                    .border_1()
                    .border_color(studio_ui::ds::surface::marquee_border(cx))
                    .bg(studio_ui::ds::surface::marquee(cx))
            });
        div()
            .id("graph-canvas")
            .key_context("Canvas")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(background)
            .on_action(cx.listener(Self::fit_to_window))
            .on_action(cx.listener(Self::reset_layout))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::expand_node))
            .on_action(cx.listener(Self::collapse_node))
            .on_action(cx.listener(Self::walk_back))
            .on_action(cx.listener(Self::walk_forward))
            .on_action(cx.listener(Self::peek_selected))
            .on_action(cx.listener(Self::open_selected_page))
            .on_action(cx.listener(Self::focus_selected))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(
                cx.listener(|this, _: &MoveLeft, _, cx| {
                    this.move_selection(Vec2::new(-1., 0.), cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &MoveRight, _, cx| {
                    this.move_selection(Vec2::new(1., 0.), cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &MoveUp, _, cx| this.move_selection(Vec2::new(0., -1.), cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveDown, _, cx| this.move_selection(Vec2::new(0., 1.), cx)),
            )
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::secondary_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            // Keys pressed do not count as the pointer leaving.
            .hover_listener_mode(gpui::HoverListenerMode::InputModalityIndependent)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.pointer_left(cx);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up_out))
            .on_scroll(cx.listener(Self::scroll))
            .on_pinch(cx.listener(Self::pinch))
            .child(canvas)
            .children(area)
            .children(body.map(|body| div().absolute().size_full().top_0().left_0().child(body)))
            .child(overlay)
            .children(
                self.context_menu
                    .as_ref()
                    .map(|(menu, position, _)| studio_ui::ds::menu_at(menu.clone(), *position)),
            )
            // Nothing drawn yet: the start state says it all.
            .when(!self.scene.nodes.is_empty(), |canvas| {
                canvas.child(self.render_caption(cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads that finish during a drag lay the canvas out again: the drag keeps moving the
    /// chips it took, wherever they now are, and stops moving one that is gone.
    #[test]
    fn a_drag_follows_its_chips_into_a_new_layout() {
        let scene = |names: &[&str]| {
            Arc::new(Scene {
                nodes: names
                    .iter()
                    .map(|name| crate::model::SceneNode {
                        id: SceneId::Node(NodeRef::new("T", *name)),
                        kind: SceneKind::Node {
                            node: NodeRef::new("T", *name),
                            root: false,
                            expanded: false,
                        },
                        label: (*name).into(),
                        color_key: (*name).into(),
                        parent: None,
                        path: None,
                    })
                    .collect(),
                edges: Vec::new(),
                capped: None,
                cap: 0,
            })
        };
        let before = scene(&["A", "B", "C"]);
        let mut drag = Drag {
            start: point(px(0.), px(0.)),
            center: Vec2::new(0., 0.),
            moved: true,
            delta: Vec2::default(),
            node: Some((1, Vec2::new(1., 1.))),
            took_chip: true,
            followers: vec![(2, Vec2::new(2., 2.)), (0, Vec2::new(0., 0.))],
            area: false,
            scene: before.clone(),
        };
        drag.follow(&before);
        assert_eq!(drag.node, Some((1, Vec2::new(1., 1.))), "the same scene");
        // A chip inserted ahead of B, and A gone.
        let after = scene(&["New", "B", "Other", "C"]);
        drag.follow(&after);
        assert_eq!(drag.node, Some((1, Vec2::new(1., 1.))));
        assert_eq!(drag.followers, vec![(3, Vec2::new(2., 2.))]);
        // The dragged chip itself gone: the drag moves nothing, the view included.
        assert!(!drag.pans());
        drag.follow(&scene(&["C"]));
        assert_eq!(drag.node, None);
        assert_eq!(drag.followers, vec![(0, Vec2::new(2., 2.))]);
        assert!(!drag.pans());
    }

    #[test]
    fn an_area_marks_the_chips_it_touches() {
        let chip = |x: f32, y: f32| Bounds::new(point(px(x), px(y)), gpui::size(px(80.), px(24.)));
        let chips = vec![
            chip(0., 0.),
            chip(200., 0.),
            chip(100., 100.),
            // A chip not painted this frame (off screen) has no width and is never marked.
            Bounds::default(),
        ];
        let area = Bounds::from_corners(point(px(60.), px(-10.)), point(px(150.), px(110.)));
        assert_eq!(chips_in(area, &chips), [0, 2], "touching is enough");
        let nothing = Bounds::from_corners(point(px(500.), px(500.)), point(px(600.), px(600.)));
        assert!(chips_in(nothing, &chips).is_empty());
    }

    #[test]
    fn placed_nodes_round_trip_and_bad_data_reads_as_none() {
        let node = SceneId::Node(NodeRef {
            type_name: "Person".into(),
            id: "person-000001".into(),
        });
        let mut pinned = HashMap::new();
        pinned.insert(pin_key(&node).unwrap(), Vec2::new(-5.4, 7.4));
        let saved = pins_to_json(&pinned);
        let back = pins_from_json(&saved);
        assert_eq!(back.len(), 1);
        assert_eq!(back["node:Person/person-000001"], Vec2::new(-5., 7.));
        // A relationship pill is placed by hand like a node; "+K more" follows its pill.
        let hub = SceneId::Hub(GroupKey::new(
            NodeRef {
                type_name: "Person".into(),
                id: "person-000001".into(),
            },
            gq_language::RelationshipGroup {
                edge: "Knows".into(),
                direction: gq_language::Direction::Outgoing,
                other_type: "Person".into(),
            },
        ));
        assert_eq!(
            pin_key(&hub).as_deref(),
            Some("hub:Person/person-000001/out:Knows")
        );
        pinned.insert(pin_key(&hub).unwrap(), Vec2::new(40., 2.));
        assert_eq!(
            pins_from_json(&pins_to_json(&pinned))["hub:Person/person-000001/out:Knows"],
            Vec2::new(40., 2.)
        );
        assert!(pins_from_json(&serde_json::Value::Null).is_empty());
        assert!(
            pins_from_json(&serde_json::json!({"pinned": {"a": [1, "x"], "b": [1]}})).is_empty()
        );
    }

    /// A canvas over a session serving people, with Ada as the exploration's root.
    fn canvas_of_ada(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<GraphCanvas>, Entity<Exploration>, NodeRef) {
        cx.update(studio_theme::init_test);
        let (session, _served) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\n",
            |_| serde_json::json!([{ "id": "p1", "label": "Ada", "n": 0 }]),
            cx,
        );
        cx.run_until_parked();
        let ada = NodeRef::new("Person", "p1");
        let canvas = cx.new(|cx| GraphCanvas::new(session, ada.clone(), None, cx));
        let exploration = canvas.read_with(cx, |canvas, _| canvas.exploration.clone());
        cx.run_until_parked();
        (canvas, exploration, ada)
    }

    /// The chips' texts of [`scene_of`]'s scene.
    fn texts_of(names: &[&str]) -> Vec<paint::ChipText> {
        scene_of(names).nodes.iter().map(paint::chip_text).collect()
    }

    fn scene_of(names: &[&str]) -> Scene {
        Scene {
            nodes: names
                .iter()
                .map(|name| crate::model::SceneNode {
                    id: SceneId::Node(NodeRef::new("T", *name)),
                    kind: SceneKind::Node {
                        node: NodeRef::new("T", *name),
                        root: false,
                        expanded: false,
                    },
                    label: (*name).into(),
                    color_key: (*name).into(),
                    parent: None,
                    path: None,
                })
                .collect(),
            edges: Vec::new(),
            capped: None,
            cap: 0,
        }
    }

    /// A relayout keeps the pointer on the chip it was on, wherever the chip now is; a drop
    /// redraws the canvas, which is drawn cached, so Reset layout shows.
    #[gpui::test]
    async fn a_relayout_keeps_the_hover_and_the_drag_and_a_drop_redraws(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (canvas, _, _) = canvas_of_ada(cx);
        canvas.update(cx, |canvas, cx| {
            canvas.apply_layout(
                scene_of(&["A", "B"]),
                texts_of(&["A", "B"]),
                vec![10.; 2],
                vec![Vec2::default(); 2],
                cx,
            );
            canvas.hovered = Some(1);
            canvas.apply_layout(
                scene_of(&["New", "A", "B"]),
                texts_of(&["New", "A", "B"]),
                vec![10.; 3],
                vec![Vec2::default(); 3],
                cx,
            );
            assert_eq!(canvas.hovered, Some(2), "still on B");
            // A read that lands mid-drag lays out again: A stays under the pointer.
            canvas.drag = Some(Drag {
                start: point(px(0.), px(0.)),
                center: Vec2::default(),
                moved: true,
                delta: Vec2::new(5., 5.),
                node: Some((1, Vec2::new(1., 1.))),
                took_chip: true,
                followers: Vec::new(),
                area: false,
                scene: canvas.scene.clone(),
            });
            canvas.apply_layout(
                scene_of(&["Z", "New", "A", "B"]),
                texts_of(&["Z", "New", "A", "B"]),
                vec![10.; 4],
                vec![Vec2::default(); 4],
                cx,
            );
            assert_eq!(canvas.positions[2], Vec2::new(6., 6.));
            assert!(
                !canvas.appear.contains_key(&2),
                "no glide away from the pointer"
            );
            let drag = canvas.drag.take().unwrap();
            assert_eq!(drag.node, Some((2, Vec2::new(1., 1.))));
        });
        let notified = Arc::new(AtomicUsize::new(0));
        let counted = notified.clone();
        let _counting = cx.update(|cx| {
            cx.observe(&canvas, move |_, _| {
                counted.fetch_add(1, Ordering::SeqCst);
            })
        });
        canvas.update(cx, |canvas, cx| {
            let drag = Drag {
                start: point(px(0.), px(0.)),
                center: Vec2::default(),
                moved: true,
                delta: Vec2::default(),
                node: Some((1, Vec2::default())),
                took_chip: true,
                followers: Vec::new(),
                area: false,
                scene: canvas.scene.clone(),
            };
            canvas.place_dragged(&drag, cx);
            assert!(!canvas.pinned.is_empty());
        });
        cx.run_until_parked();
        assert!(notified.load(Ordering::SeqCst) > 0);
    }

    /// A new node cap is drawn at once: the canvas lays out again under it, and its banner names
    /// the cap the drawn scene was built under, never a newer one.
    #[gpui::test]
    async fn a_new_node_cap_is_drawn_at_once(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let (session, _served) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\n",
            |_| serde_json::json!([]),
            cx,
        );
        cx.run_until_parked();
        let cap = |limit: u32, cx: &mut gpui::TestAppContext| {
            cx.update(|cx| {
                let mut settings = StudioSettings::default();
                settings.studio.canvas_max_nodes = limit;
                cx.set_global(settings);
            })
        };
        cap(2, cx);
        let person = |id: &str| studio_actions::graph::ResultNode {
            type_name: "Person".into(),
            id: id.into(),
            label: None,
        };
        let action = ShowResultsOnCanvas {
            title: "people".into(),
            nodes: vec![person("p1"), person("p2"), person("p3")],
            links: Vec::new(),
        };
        let canvas = cx.new(|cx| GraphCanvas::with_results(session, &action, cx));
        cx.run_until_parked();
        canvas.read_with(cx, |canvas, _| {
            assert_eq!(canvas.scene.nodes.len(), 2);
            assert_eq!(canvas.scene.cap, 2);
            assert_eq!(canvas.scene.capped.as_ref().map(|c| c.0), Some(1));
        });
        cap(3, cx);
        cx.run_until_parked();
        canvas.read_with(cx, |canvas, _| {
            assert_eq!(
                canvas.scene.nodes.len(),
                3,
                "laid out again under the new cap"
            );
            assert_eq!(canvas.scene.cap, 3);
            assert!(canvas.scene.capped.is_none());
        });
    }

    /// Leaving a chip stops what was read ahead for it and is still out, as leaving a tree row
    /// does: the read is let go, and nothing says it was asked for.
    #[gpui::test]
    async fn leaving_a_chip_stops_its_read_ahead(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let (session, _served) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\n",
            |_| serde_json::json!([{ "id": "p1", "label": "Ada", "n": 1 }]),
            cx,
        );
        cx.run_until_parked();
        let canvas = cx.new(|cx| GraphCanvas::new(session, NodeRef::new("Person", "p1"), None, cx));
        let exploration = canvas.read_with(cx, |canvas, _| canvas.exploration.clone());
        cx.run_until_parked();
        // Answers that come in together are laid out after a short wait, when the canvas is
        // drawn (here, by hand: it has no window).
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        canvas.update(cx, |canvas, cx| canvas.lay_out_owed(cx));
        cx.run_until_parked();
        let (hub, key) = canvas.read_with(cx, |canvas, _| {
            canvas
                .scene
                .nodes
                .iter()
                .enumerate()
                .find_map(|(index, node)| match &node.kind {
                    SceneKind::Hub { key, .. } => Some((index, key.clone())),
                    _ => None,
                })
                .unwrap()
        });
        let asked = |cx: &mut gpui::TestAppContext| {
            exploration.read_with(cx, |exploration, _| {
                exploration.state().neighbours.contains_key(&key)
            })
        };
        // The pointer rests on a relationship pill: its first neighbours are read ahead.
        canvas.update(cx, |canvas, cx| {
            canvas.hovered = Some(hub);
            canvas.prefetch_hovered(cx);
            let path = canvas.read_ahead.clone().unwrap();
            canvas
                .exploration
                .update(cx, |exploration, cx| exploration.prefetch(&path, cx));
        });
        assert!(asked(cx), "read ahead");
        // It leaves before they answer: they stop.
        canvas.update(cx, |canvas, cx| canvas.pointer_left(cx));
        cx.run_until_parked();
        assert!(!asked(cx), "stopped, and nothing says they were asked for");
    }

    /// A walk's change that leaves its scene as drawn lays out nothing: the chips stay put, and
    /// no layout runs off the main thread for them. Before, every change laid the same scene
    /// out again, as "+K more" did while its page was read. A change of the scene lays out.
    #[gpui::test]
    async fn a_change_that_moves_no_chip_lays_out_nothing(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let (session, _served) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\n",
            |_| serde_json::json!([{ "id": "p1", "label": "Ada", "n": 1 }]),
            cx,
        );
        cx.run_until_parked();
        let canvas = cx.new(|cx| GraphCanvas::new(session, NodeRef::new("Person", "p1"), None, cx));
        let exploration = canvas.read_with(cx, |canvas, _| canvas.exploration.clone());
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        canvas.update(cx, |canvas, cx| canvas.lay_out_owed(cx));
        cx.run_until_parked();
        let laid_out = canvas.read_with(cx, |canvas, _| {
            assert!(!canvas.scene.nodes.is_empty());
            canvas.layout_generation
        });

        canvas.update(cx, |canvas, cx| {
            canvas.owe_layout(cx);
            canvas.lay_out_owed(cx);
            assert!(!canvas.layout_owed);
        });
        cx.run_until_parked();
        assert_eq!(
            canvas.read_with(cx, |canvas, _| canvas.layout_generation),
            laid_out,
            "the same scene is not laid out again"
        );

        // A hub widened under the old root is not, under the new one.
        let widened = canvas.read_with(cx, |canvas, cx| {
            let root = NodeRef::new("Person", "p1");
            let group = canvas
                .session
                .read(cx)
                .schema_model()
                .unwrap()
                .relationships_of("Person")[0]
                .clone();
            GroupKey::new(root, group)
        });
        canvas.update(cx, |canvas, _| canvas.hub_limits.insert(widened, 48));
        exploration.update(cx, |exploration, cx| {
            exploration.root_at(NodeRef::new("Person", "p2"), cx)
        });
        let running = canvas.update(cx, |canvas, cx| {
            canvas.lay_out_owed(cx);
            assert!(canvas.layout_task.is_some(), "under way");
            // A change while it runs leaves it to finish and be shown, and is owed after it.
            canvas.owe_layout(cx);
            canvas.lay_out_owed(cx);
            assert!(canvas.layout_owed);
            canvas.layout_generation
        });
        assert!(running > laid_out, "another scene is");
        cx.run_until_parked();
        canvas.read_with(cx, |canvas, _| {
            assert_eq!(
                canvas.layout_generation, running,
                "the layout under way was kept"
            );
            assert!(canvas.hub_limits.is_empty(), "the old root's hubs");
            assert!(canvas.layout_task.is_none());
            assert!(!canvas.scene.nodes.is_empty(), "and shown");
        });
    }

    /// A canvas starts from where the person placed things on the graph's old canvas, as its own
    /// copy, and saves its root with them, so it comes back as the same walk (spec 020).
    #[gpui::test]
    async fn a_canvas_starts_from_the_graphs_saved_positions(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let (session, _served) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\n",
            |_| serde_json::json!([{ "id": "p1", "label": "Ada", "n": 0 }]),
            cx,
        );
        cx.run_until_parked();
        // As a saved root-less canvas leaves them.
        let old = serde_json::json!({ "pinned": { "n:Person:p1": [12.0, 34.0] } });
        let positions = pins_from_json(&old);
        session.update(cx, |session, _| {
            for (key, at) in positions {
                session.canvas_positions.insert(key, [at.x, at.y]);
            }
        });
        let ada = NodeRef::new("Person", "p1");
        let held = Some("c1".to_owned());
        let canvas = cx.new(|cx| GraphCanvas::new(session.clone(), ada.clone(), held, cx));
        canvas.update(cx, |canvas, cx| {
            assert_eq!(canvas.pinned["n:Person:p1"], Vec2::new(12., 34.));
            canvas.pinned.clear();
            let saved = Item::serialize(canvas, cx).expect("a walk is saved");
            assert_eq!(saved.data["type_name"], "Person");
            assert_eq!(saved.data["id"], "p1");
            assert_eq!(saved.data["at_commit"], "c1");
            let key = Item::dedupe_key(canvas, cx).expect("a walk has a key");
            assert_eq!(
                key.as_ref(),
                format!("canvas:{}:Person:p1@c1", canvas.session.read(cx).graph_id)
            );
        });
        session.read_with(cx, |session, _| {
            assert!(
                session.canvas_positions.contains_key("n:Person:p1"),
                "a tab's reset leaves the graph's saved positions alone"
            );
        });
    }
}
