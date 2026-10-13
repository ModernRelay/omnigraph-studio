//! The graph canvas (spec 006): the schema as a landing view, then a growing neighbourhood of
//! real nodes, drawn from the session's shared [`Exploration`], so the relationship tree and
//! the canvas always show the same walk.
//!
//! One `canvas()` element paints everything: edges batched by color into paths in one paint
//! layer, nodes as quads, labels above a zoom threshold. Layout runs on a background thread
//! (`layout::explore_positions`); the main thread only paints. Nodes already drawn never move.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/006-graph-canvas/plan.md`.

mod layout;
mod model;
mod paint;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    App, AppContext as _, Bounds, ContentMask, Context, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PinchEvent, Pixels,
    Point, Render, ScrollWheelEvent, SharedString, Size, Subscription, Task, Window, canvas, point,
    px,
};
use graph_store::{
    Exploration, ExplorationEvent, ExplorePath, GraphSession, GroupKey, Loadable, NodeRef,
    PageStatus, SessionEvent,
};
use studio_actions::canvas::{
    CollapseNode, ExpandNode, FitToWindow, FocusSelected, MoveDown, MoveLeft, MoveRight, MoveUp,
    PeekSelected, ResetLayout, WalkBack, WalkForward, ZoomIn, ZoomOut,
};
use studio_actions::graph::{
    OpenCanvas, OpenNode, OpenOntology, OpenType, PeekNode, ShowNodeOnCanvas, ShowResultsOnCanvas,
};
use studio_ui::empty_state;
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace};
use ui::Tooltip;
use ui::prelude::*;

use layout::{LayoutKind, LayoutNode, OntologyEdge, OntologyNode, Vec2};
use model::{HUB_STEP, Scene, SceneId, SceneKind};
use studio_ui::motion::{self, Ease, Lerp, Tween};

/// Labels of graph nodes show from this zoom up; hubs from a little further out.
const MAX_ZOOM: f32 = 2.;
const ZOOM_STEP: f32 = 1.25;

/// Registers the canvas's keys and the actions that open it.
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, "canvas", |data, workspace, window, cx| {
        let pinned = pins_from_json(data);
        if let Some(canvas) = open_canvas(workspace, window, cx) {
            canvas.update(cx, |canvas, cx| canvas.restore_pins(pinned, cx));
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
        workspace.register_action(|workspace, _: &OpenCanvas, window, cx| {
            if let Some(canvas) = open_canvas(workspace, window, cx) {
                canvas.update(cx, |canvas, cx| canvas.set_ontology(false, cx));
            }
        });
        workspace.register_action(|workspace, _: &OpenOntology, window, cx| {
            if let Some(canvas) = open_canvas(workspace, window, cx) {
                canvas.update(cx, |canvas, cx| canvas.set_ontology(true, cx));
            }
        });
        workspace.register_action(|workspace, action: &ShowNodeOnCanvas, window, cx| {
            if let Some(canvas) = open_canvas(workspace, window, cx) {
                let node = NodeRef::new(action.type_name.clone(), action.id.clone());
                canvas.update(cx, |canvas, cx| canvas.show_node(node, cx));
            }
        });
    })
    .detach();
}

fn open_canvas(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<GraphCanvas>> {
    let session = workspace.session()?.clone();
    let key = canvas_key(&session.read(cx).graph_id);
    workspace.open_item::<GraphCanvas>(key, window, cx, |_window, cx| GraphCanvas::new(session, cx))
}

fn canvas_key(graph_id: &str) -> SharedString {
    format!("canvas:{graph_id}").into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Schema,
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
    /// A drag that began on a node moves the node (from where it was) instead of the view.
    node: Option<(usize, Vec2)>,
}

/// The key a placed node is remembered by: a type on the schema canvas, a node while
/// exploring. Hubs and "+K more" follow their node and are never placed by hand.
fn pin_key(id: &SceneId) -> Option<String> {
    match id {
        SceneId::Type(name) => Some(format!("type:{name}")),
        SceneId::Node(node) => Some(format!("node:{}/{}", node.type_name, node.id)),
        SceneId::Hub(_) | SceneId::More(_) => None,
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

/// The canvas tab of one graph.
pub struct GraphCanvas {
    session: Entity<GraphSession>,
    exploration: Entity<Exploration>,
    /// Set for a canvas of query results; such a canvas never shows the exploration.
    results: Option<(usize, Arc<ResultsData>)>,
    focus_handle: FocusHandle,
    /// The person asked to see the schema although an exploration exists.
    show_schema: bool,
    mode: Mode,
    scene: Arc<Scene>,
    positions: Arc<Vec<Vec2>>,
    /// Half of each node's chip width at zoom 1, as measured for the layout.
    half_widths: Arc<Vec<f32>>,
    /// Where each node was drawn, for the current mode and root: such nodes never move.
    known: HashMap<SceneId, Vec2>,
    /// Nodes the person dragged into place, by [`pin_key`]: kept across layouts and restarts,
    /// with the graph's tabs (spec 006, saved layouts).
    pinned: HashMap<String, Vec2>,
    layout_key: Option<(Mode, Option<NodeRef>)>,
    layout_generation: u64,
    _layout_task: Option<Task<()>>,
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
    drag: Option<Drag>,
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
    fn tab_title(&self, _cx: &App) -> SharedString {
        match &self.results {
            Some((_, results)) => results.title.clone().into(),
            None if self.mode == Mode::Explore => "Explorer".into(),
            None => "Ontology".into(),
        }
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::GitGraph)
    }

    fn dedupe_key(&self, cx: &App) -> Option<SharedString> {
        Some(match &self.results {
            Some((seq, _)) => format!("canvas-results:{seq}").into(),
            None => canvas_key(&self.session.read(cx).graph_id),
        })
    }

    /// The exploration canvas comes back after a restart; a results canvas is transient.
    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        self.results.is_none().then(|| SavedItem {
            kind: "canvas".into(),
            data: if self.pinned.is_empty() {
                serde_json::Value::Null
            } else {
                pins_to_json(&self.pinned)
            },
        })
    }

    fn is_stale(&self, cx: &App) -> bool {
        self.results.is_none() && self.exploration.read(cx).newer_available()
    }
}

impl GraphCanvas {
    pub fn new(session: Entity<GraphSession>, cx: &mut Context<Self>) -> Self {
        let exploration = Exploration::for_session(&session, cx);
        let subscriptions = vec![
            cx.subscribe(&exploration, |this, _, event, cx| match event {
                ExplorationEvent::Changed => {
                    cx.emit(ItemEvent::UpdateTab);
                    this.rebuild(cx);
                }
                ExplorationEvent::SelectionChanged => this.follow_selection(cx),
            }),
            cx.subscribe(&session, |this, _, event: &SessionEvent, cx| {
                if matches!(
                    event,
                    SessionEvent::SchemaChanged | SessionEvent::CountsChanged
                ) {
                    this.rebuild(cx);
                }
            }),
        ];
        let mut canvas = Self {
            session,
            exploration,
            results: None,
            focus_handle: cx.focus_handle(),
            show_schema: false,
            mode: Mode::Schema,
            scene: Arc::default(),
            positions: Arc::default(),
            half_widths: Arc::default(),
            known: HashMap::new(),
            pinned: HashMap::new(),
            layout_key: None,
            layout_generation: 0,
            _layout_task: None,
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
            drag: None,
            bounds: Rc::new(Cell::new(None)),
            chips: Rc::new(RefCell::new(Vec::new())),
            _subscriptions: subscriptions,
        };
        canvas.rebuild(cx);
        canvas.follow_selection(cx);
        canvas
    }

    /// A canvas of a query's result nodes.
    fn with_results(
        session: Entity<GraphSession>,
        action: &ShowResultsOnCanvas,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut canvas = Self::new(session, cx);
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
        canvas.results = Some((
            seq,
            Arc::new(ResultsData {
                title: action.title.clone(),
                nodes,
                links: action.links.clone(),
            }),
        ));
        canvas.selected = None;
        canvas.rebuild(cx);
        canvas
    }

    /// Shows the ontology (the schema's types and relationships), or the exploration when
    /// there is one.
    pub fn set_ontology(&mut self, ontology: bool, cx: &mut Context<Self>) {
        if self.show_schema != ontology {
            self.show_schema = ontology;
            self.rebuild(cx);
            cx.emit(ItemEvent::UpdateTab);
        }
    }

    /// Shows `node`: selects it where the exploration shows it, else roots the exploration
    /// there; then centers on it.
    pub fn show_node(&mut self, node: NodeRef, cx: &mut Context<Self>) {
        self.show_schema = false;
        self.center_on = Some(SceneId::Node(node.clone()));
        self.selected = Some(SceneId::Node(node.clone()));
        self.exploration
            .update(cx, |exploration, cx| exploration.reveal(node, cx));
        self.rebuild(cx);
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some(schema) = self.session.read(cx).schema_model() else {
            self.scene = Arc::default();
            self.positions = Arc::default();
            cx.notify();
            return;
        };
        if let Some((_, results)) = self.results.clone() {
            self.rebuild_results(&results, cx);
            return;
        }
        let exploration = self.exploration.read(cx);
        let root = exploration.root().cloned();
        let mode = if root.is_some() && !self.show_schema {
            Mode::Explore
        } else {
            Mode::Schema
        };
        let key = (mode, root.clone().filter(|_| mode == Mode::Explore));
        if self.layout_key.as_ref() != Some(&key) {
            self.known.clear();
            self.fit_pending = true;
            self.layout_key = Some(key.clone());
        }
        self.mode = mode;
        self.layout_generation += 1;
        let generation = self.layout_generation;
        match mode {
            // Handled before: a results canvas never reaches here.
            Mode::Results => {}
            Mode::Schema => {
                let session = self.session.read(cx);
                let scene = model::schema_scene(&schema, |name| session.count_of(name));
                let measure = measurer(cx);
                let widths = half_widths(&scene, &measure);
                // From the schema and the placed types only: each type is laid out with room
                // for a six-digit count, whatever its count, so a count never moves a type.
                let nodes: Vec<OntologyNode> = scene
                    .nodes
                    .iter()
                    .map(|node| OntologyNode {
                        half_width: {
                            let mut sized = node.clone();
                            sized.kind = SceneKind::Type {
                                count: Some(888_888),
                            };
                            paint::chip_width(&paint::chip_text(&sized), &measure) / 2.
                        },
                        pinned: pin_key(&node.id).and_then(|key| self.pinned.get(&key).copied()),
                    })
                    .collect();
                // Labels shown at all times get room between their types; a larger ontology
                // names a type's relationships on hover, over the chips, and stays compact.
                let labelled = scene.edges.len() <= paint::ALWAYS_LABEL;
                let edges: Vec<OntologyEdge> = scene
                    .edges
                    .iter()
                    .map(|edge| OntologyEdge {
                        from: edge.from,
                        to: edge.to,
                        label_half_width: match &edge.label {
                            Some(label) if labelled => paint::label_width(label, &measure) / 2.,
                            _ => 0.,
                        },
                    })
                    .collect();
                let positions =
                    cx.background_spawn(async move { layout::ontology_positions(&nodes, &edges) });
                self._layout_task = Some(cx.spawn(async move |this, cx| {
                    let positions = positions.await;
                    this.update(cx, |this, cx| {
                        if this.layout_generation == generation {
                            this.apply_layout(scene, widths, positions, cx);
                        }
                    })
                    .ok();
                }));
            }
            Mode::Explore => {
                let entries = exploration.entries(cx);
                let scene = model::explore_scene(
                    &entries,
                    exploration.state(),
                    &schema,
                    &self.hub_limits,
                    self.node_cap(cx),
                );
                let widths = half_widths(&scene, &measurer(cx));
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
                            pin_key(&node.id)
                                .and_then(|key| self.pinned.get(&key))
                                .or_else(|| self.known.get(&node.id))
                                .copied()
                        },
                    })
                    .collect();
                let positions =
                    cx.background_spawn(async move { layout::explore_positions(&nodes) });
                self._layout_task = Some(cx.spawn(async move |this, cx| {
                    let positions = positions.await;
                    this.update(cx, |this, cx| {
                        if this.layout_generation == generation {
                            this.apply_layout(scene, widths, positions, cx);
                        }
                    })
                    .ok();
                }));
            }
        }
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
        let ring = layout::schema_positions(roots.len());
        let mut fixed = vec![None; count];
        for (root, position) in roots.iter().zip(ring) {
            fixed[*root] = Some(position);
        }
        for (node, parent) in scene.nodes.iter_mut().zip(&parent) {
            node.parent = *parent;
        }
        let widths = half_widths(&scene, &measurer(cx));
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
        self.mode = Mode::Results;
        self.layout_generation += 1;
        let generation = self.layout_generation;
        let positions = cx.background_spawn(async move { layout::explore_positions(&nodes) });
        self._layout_task = Some(cx.spawn(async move |this, cx| {
            let positions = positions.await;
            this.update(cx, |this, cx| {
                if this.layout_generation == generation {
                    this.apply_layout(scene, widths, positions, cx);
                }
            })
            .ok();
        }));
    }

    fn apply_layout(
        &mut self,
        scene: Scene,
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
        for (node, position) in scene.nodes.iter().zip(&positions) {
            self.known.insert(node.id.clone(), *position);
        }
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
        self.hovered = None;
        self.scene = Arc::new(scene);
        self.positions = Arc::new(positions);
        if let Some(target) = self.center_on.clone()
            && let Some(index) = self.scene.index_of(&target)
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

    /// Stops a glide where it is drawn, before the person moves the view by hand.
    fn settle_camera(&mut self) {
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
            // Tower's ontology never zooms a fit past 1.1.
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
    fn hit(&self, position: Point<Pixels>, _window: &Window) -> Option<usize> {
        let chips = self.chips.borrow();
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

    fn selected_index(&self) -> Option<usize> {
        self.scene.index_of(self.selected.as_ref()?)
    }

    /// Double click and ↵: expand a node or open a hub, show more, or open a type's table.
    fn activate(
        &mut self,
        index: usize,
        expand_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let node = self.scene.nodes[index].clone();
        match node.kind {
            SceneKind::Type { .. } => {
                window.dispatch_action(
                    Box::new(OpenType {
                        type_name: node.label.clone(),
                        at_commit: None,
                        filter_id: None,
                        via: None,
                        preview: false,
                    }),
                    cx,
                );
            }
            SceneKind::Node {
                node: graph_node,
                expanded,
                ..
            } => match node.path {
                Some(path) if expanded && !expand_only => {
                    self.exploration.update(cx, |e, cx| e.collapse(&path, cx));
                }
                Some(path) => {
                    self.exploration.update(cx, |e, cx| e.expand(&path, cx));
                }
                None => {
                    window.dispatch_action(
                        Box::new(OpenNode {
                            type_name: graph_node.type_name,
                            id: graph_node.id,
                        }),
                        cx,
                    );
                }
            },
            SceneKind::Hub { open, .. } => {
                if let Some(path) = node.path {
                    if open && !expand_only {
                        self.exploration.update(cx, |e, cx| e.collapse(&path, cx));
                    } else {
                        self.exploration.update(cx, |e, cx| e.expand(&path, cx));
                    }
                }
            }
            SceneKind::More { key, .. } => self.show_more(key, cx),
        }
    }

    fn show_more(&mut self, key: GroupKey, cx: &mut Context<Self>) {
        let limit = self.hub_limits.entry(key.clone()).or_insert(HUB_STEP);
        *limit += HUB_STEP;
        let wanted = *limit;
        let (loaded, ready) = self
            .exploration
            .read(cx)
            .state()
            .neighbours
            .get(&key)
            .map(|list| {
                (
                    list.rows.len(),
                    list.status == PageStatus::Idle && !list.exhausted,
                )
            })
            .unwrap_or((0, false));
        if loaded < wanted && ready {
            self.exploration.update(cx, |e, cx| e.load_more(&key, cx));
        } else {
            self.rebuild(cx);
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        self.settle_camera();
        let node = self
            .hit(event.position, window)
            .filter(|index| pin_key(&self.scene.nodes[*index].id).is_some())
            .and_then(|index| Some((index, *self.positions.get(index)?)));
        self.drag = Some(Drag {
            start: event.position,
            center: self.center,
            moved: false,
            node,
        });
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(drag), true) = (&mut self.drag, event.dragging()) {
            let dx = f32::from(event.position.x - drag.start.x);
            let dy = f32::from(event.position.y - drag.start.y);
            if drag.moved || dx.abs() + dy.abs() > 3. {
                drag.moved = true;
                let delta = Vec2::new(dx, dy) * (1. / self.zoom);
                match drag.node {
                    Some((index, from)) => {
                        self.appear.remove(&index);
                        let at = from + delta;
                        if let Some(position) = Arc::make_mut(&mut self.positions).get_mut(index) {
                            *position = at;
                        }
                        let id = self.scene.nodes[index].id.clone();
                        self.known.insert(id, at);
                    }
                    None => {
                        self.center = drag.center - delta;
                        self.camera_touched = true;
                    }
                }
                cx.notify();
            }
            return;
        }
        let hovered = self.hit(event.position, window);
        if hovered != self.hovered {
            self.hovered = hovered;
            cx.notify();
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.moved {
            // A dragged node stays where it was dropped, and is remembered there.
            if let Some((index, _)) = drag.node
                && let Some(key) = pin_key(&self.scene.nodes[index].id)
                && let Some(at) = self.positions.get(index)
            {
                self.pinned.insert(key, *at);
                cx.emit(ItemEvent::UpdateTab);
            }
            return;
        }
        let hit = self.hit(event.position, window);
        self.select(hit, cx);
        if let Some(index) = hit {
            // A click on a type browses its rows under the canvas, as Tower's does.
            if matches!(self.scene.nodes[index].kind, SceneKind::Type { .. })
                && event.click_count < 2
            {
                window.dispatch_action(
                    Box::new(studio_actions::graph::BrowseType {
                        type_name: self.scene.nodes[index].label.clone(),
                    }),
                    cx,
                );
                return;
            }
            let single_activates = matches!(
                self.scene.nodes[index].kind,
                SceneKind::Hub { .. } | SceneKind::More { .. }
            );
            if single_activates || event.click_count >= 2 {
                self.activate(index, false, window, cx);
            }
        }
    }

    fn mouse_up_out(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.drag = None;
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
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
        if pinned.is_empty() {
            return;
        }
        self.pinned = pinned;
        self.fit_pending = true;
        self.rebuild(cx);
    }

    fn reset_layout(&mut self, _: &ResetLayout, _window: &mut Window, cx: &mut Context<Self>) {
        self.pinned.clear();
        self.known.clear();
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
        let node = &self.scene.nodes[index];
        let path = match &node.kind {
            SceneKind::Node { .. } | SceneKind::Hub { .. } | SceneKind::More { .. } => {
                node.path.clone()
            }
            SceneKind::Type { .. } => None,
        };
        if let Some(path) = path {
            self.exploration.update(cx, |e, cx| e.collapse(&path, cx));
        }
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
            if let Some(index) = self.scene.index_of(&id) {
                self.select(Some(index), cx);
                break;
            }
        }
        self.walked = walked;
    }

    fn peek_selected(&mut self, _: &PeekSelected, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_index() else {
            return;
        };
        if let SceneKind::Node { node, .. } = &self.scene.nodes[index].kind {
            window.dispatch_action(
                Box::new(PeekNode {
                    type_name: node.type_name.clone(),
                    id: node.id.clone(),
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

    fn render_canvas(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let palette = paint::Palette {
            text: colors.text,
            muted: colors.text_muted,
            // Tower's chips are white cards in the light theme.
            card: if cx.theme().appearance.is_light() {
                gpui::white()
            } else {
                colors.elevated_surface_background
            },
            border: colors.border_variant,
            font: theme::theme_settings(cx).ui_font(cx).clone(),
        };
        let scene = self.scene.clone();
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
        let hovered = self.hovered;
        let bounds_cell = self.bounds.clone();
        let chips = self.chips.clone();
        canvas(
            move |bounds, window, _cx| {
                // A new size is fitted on the next frame, once the view knows it.
                let resized = bounds_cell.get().map(|b| b.size) != Some(bounds.size);
                bounds_cell.set(Some(bounds));
                if resized {
                    window.refresh();
                }
            },
            move |bounds, (), window, cx| {
                motion::keep_animating(moving, window);
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    let screen: Vec<Point<Pixels>> = positions
                        .iter()
                        .map(|world| to_screen(*world, bounds, center, zoom))
                        .collect();
                    paint::paint(
                        &scene, &screen, bounds, zoom, selected, hovered, &palette, &chips, window,
                        cx,
                    );
                });
            },
        )
        .size_full()
    }

    /// Tower's caption, bottom left: what is drawn, and what a click does.
    fn render_caption(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let text = match self.mode {
            Mode::Schema => {
                let schema = self.session.read(cx).schema_model();
                let types = schema.as_ref().map_or(0, |s| s.nodes.len());
                let edges = schema.as_ref().map_or(0, |s| s.edges.len());
                format!(
                    "{types} {} · {edges} {} · click a type to browse it",
                    if types == 1 { "type" } else { "types" },
                    if edges == 1 {
                        "relationship"
                    } else {
                        "relationships"
                    }
                )
            }
            Mode::Explore | Mode::Results => {
                let nodes = self
                    .scene
                    .nodes
                    .iter()
                    .filter(|n| matches!(n.kind, SceneKind::Node { .. }))
                    .count();
                format!(
                    "{nodes} {} · click a relationship to open it · double-click a node to expand it",
                    if nodes == 1 { "node" } else { "nodes" }
                )
            }
        };
        div()
            .absolute()
            .bottom_3()
            .left_3()
            .px_2p5()
            .py_1()
            .rounded_full()
            .bg(if cx.theme().appearance.is_light() {
                gpui::white()
            } else {
                colors.elevated_surface_background
            })
            .border_1()
            .border_color(colors.border_variant)
            .shadow(studio_ui::ds::elevation::float())
            .child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
    }

    /// The most nodes this graph's canvas draws (setting `studio.canvas_max_nodes`, or the
    /// graph's own).
    fn node_cap(&self, cx: &App) -> usize {
        self.session.read(cx).options(cx).canvas_max_nodes.max(1) as usize
    }

    fn render_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let exploration = self.exploration.read(cx);
        let state = exploration.state();
        let has_root = exploration.root().is_some();
        let newer = exploration.newer_available();
        let summary = match self.mode {
            Mode::Schema => {
                let schema = self.session.read(cx).schema_model();
                let types = schema.as_ref().map_or(0, |s| s.nodes.len());
                let edges = schema.as_ref().map_or(0, |s| s.edges.len());
                format!("{types} types · {edges} relationships")
            }
            Mode::Results => {
                let count = self.scene.nodes.len();
                let noun = if count == 1 { "node" } else { "nodes" };
                let title = self
                    .results
                    .as_ref()
                    .map(|(_, r)| r.title.clone())
                    .unwrap_or_default();
                format!("{title} · {count} {noun}")
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
                let nodes = self
                    .scene
                    .nodes
                    .iter()
                    .filter(|n| matches!(n.kind, SceneKind::Node { .. }))
                    .count();
                let noun = if nodes == 1 { "node" } else { "nodes" };
                format!("From {root} · {nodes} {noun}")
            }
        };
        let root_error = match (&state.root_status, self.mode) {
            (Loadable::Failed(message), Mode::Explore) => Some(message.clone()),
            _ => None,
        };
        let capped = self.scene.capped.clone();
        let cap = self.node_cap(cx);
        // Tower's floating pills are white cards in the light theme, like its chips.
        let surface = if cx.theme().appearance.is_light() {
            gpui::white()
        } else {
            cx.theme().colors().elevated_surface_background
        };
        let border = cx.theme().colors().border_variant;
        // Tower's floating pills: round, a hairline, a soft float shadow.
        let panel = |id: &'static str| {
            h_flex()
                .id(id)
                .h(px(32.))
                .gap_0p5()
                .px_1()
                .items_center()
                .rounded_full()
                .bg(surface)
                .border_1()
                .border_color(border)
                .shadow(studio_ui::ds::elevation::float())
                .occlude()
        };
        let mode = self.mode;
        v_flex()
            .absolute()
            .top_2()
            .left_2()
            .right_2()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        panel("canvas-mode")
                            .when(mode != Mode::Results, |panel| {
                                panel
                                    .child(
                                        studio_ui::ds::button("mode-schema", "Ontology")
                                            .label_size(LabelSize::Small)
                                            .toggle_state(mode == Mode::Schema)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.set_ontology(true, cx)
                                            })),
                                    )
                                    .child(
                                        studio_ui::ds::button("mode-explore", "Explorer")
                                            .label_size(LabelSize::Small)
                                            .toggle_state(mode == Mode::Explore)
                                            .disabled(!has_root)
                                            .tooltip(Tooltip::text(if has_root {
                                                "The nodes walked in the tree and here"
                                            } else {
                                                "Open a node first: ↵ on a table row or tree row"
                                            }))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.set_ontology(false, cx)
                                            })),
                                    )
                            })
                            .when(mode == Mode::Results, |panel| {
                                panel.child(
                                    div().px_2().child(
                                        Label::new(summary.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .single_line(),
                                    ),
                                )
                            }),
                    )
                    .when(newer && mode != Mode::Results, |d| {
                        d.child(
                            panel("canvas-newer").child(
                                studio_ui::ds::button("canvas-refresh", "Newer data · Refresh")
                                    .label_size(LabelSize::Small)
                                    .tooltip(Tooltip::text(
                                        "The branch moved since this exploration was read",
                                    ))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.exploration.update(cx, |e, cx| e.refresh(cx));
                                    })),
                            ),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        panel("canvas-search")
                            .px_3()
                            .gap_2()
                            .cursor_pointer()
                            .child(
                                Icon::new(IconName::MagnifyingGlass)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new("Search nodes…")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(ui::KeyBinding::for_action(
                                &studio_actions::workspace::GoTo,
                                cx,
                            ))
                            .on_click(|_, window, cx| {
                                window
                                    .dispatch_action(Box::new(studio_actions::workspace::GoTo), cx)
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        panel("canvas-zoom")
                            .when(!self.pinned.is_empty(), |panel| {
                                panel.child(
                                    studio_ui::ds::icon_button("reset-layout", IconName::RotateCcw)
                                        .icon_size(IconSize::Small)
                                        .tooltip(|_window, cx| {
                                            Tooltip::for_action(
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
                                    .icon_size(IconSize::Small)
                                    .tooltip(|_window, cx| {
                                        Tooltip::for_action("Zoom out", &ZoomOut, cx)
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.zoom_out(&ZoomOut, window, cx)
                                    })),
                            )
                            .child(
                                studio_ui::ds::button("zoom-fit", "Fit")
                                    .label_size(LabelSize::Small)
                                    .tooltip(|_window, cx| {
                                        Tooltip::for_action("Fit to window", &FitToWindow, cx)
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.fit_to_window(&FitToWindow, window, cx)
                                    })),
                            )
                            .child(
                                studio_ui::ds::icon_button("zoom-in", IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .tooltip(|_window, cx| {
                                        Tooltip::for_action("Zoom in", &ZoomIn, cx)
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
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        )
                        .child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                        .child(
                            studio_ui::ds::button("canvas-retry", "Retry")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.exploration.update(cx, |e, cx| {
                                        e.retry(&graph_store::LoadTarget::Root, cx)
                                    });
                                })),
                        ),
                )
            }))
            .children(capped.map(|(hidden, type_name)| {
                h_flex().child(
                    panel("canvas-capped")
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Warning),
                        )
                        .child(
                            Label::new(format!(
                                "Too many nodes to draw · {hidden} not shown (limit {cap})"
                            ))
                            .size(LabelSize::Small),
                        )
                        .child(
                            studio_ui::ds::button("canvas-tree", "Show in tree")
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Outlined)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(studio_actions::workspace::ActivatePanel {
                                            id: "explore".into(),
                                        }),
                                        cx,
                                    )
                                }),
                        )
                        .child(
                            studio_ui::ds::button("canvas-table", "Open as table")
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Filled)
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(OpenType {
                                            type_name: type_name.clone(),
                                            at_commit: None,
                                            filter_id: None,
                                            via: None,
                                            preview: false,
                                        }),
                                        cx,
                                    )
                                }),
                        ),
                )
            }))
    }
}

/// The room a node needs in layout: about half its chip's width at zoom 1 (a mark, the label
/// at about seven pixels a character, a count, and the padding).
/// Measures text with the UI font, as the painter shapes chip text, for the layout.
fn measurer(cx: &App) -> impl Fn(&str, f32, gpui::FontWeight) -> f32 + use<> {
    let system = cx.text_system().clone();
    let font = theme::theme_settings(cx).ui_font(cx).clone();
    move |text, size, weight| {
        let id = system.resolve_font(&gpui::Font {
            weight,
            ..font.clone()
        });
        text.chars()
            .map(|ch| f32::from(system.layout_width(id, px(size), ch)))
            .sum()
    }
}

/// Half of each node's chip width at zoom 1, measured as the painter draws it.
fn half_widths(scene: &Scene, measure: &impl Fn(&str, f32, gpui::FontWeight) -> f32) -> Vec<f32> {
    scene
        .nodes
        .iter()
        .map(|node| paint::chip_width(&paint::chip_text(node), measure) / 2.)
        .collect()
}

fn scene_id_of(path: &ExplorePath, root: &NodeRef) -> SceneId {
    match path.last_group() {
        Some(group) => SceneId::Hub(GroupKey {
            node: path.node(root).clone(),
            group: group.clone(),
        }),
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
        let schema = self.session.read(cx).schema.clone();
        let body = match schema {
            Loadable::NotLoaded | Loadable::Loading => {
                Some(empty_state("Reading the schema…", None).into_any_element())
            }
            Loadable::Failed(message) => Some(
                empty_state("Studio could not read the schema", Some(message.into()))
                    .into_any_element(),
            ),
            Loadable::Loaded(_) if self.scene.nodes.is_empty() => Some(
                empty_state(
                    match self.mode {
                        Mode::Schema => "The schema has no node types",
                        Mode::Explore => "Laying out…",
                        Mode::Results => "The result has no nodes",
                    },
                    None,
                )
                .into_any_element(),
            ),
            Loadable::Loaded(_) => None,
        };
        let now = Instant::now();
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
            if self.fit_pending || (resized && !self.camera_touched) {
                if self.fit_pending {
                    self.camera_touched = false;
                }
                self.glide = None;
                self.fit(viewport);
            }
        }
        let background = cx.theme().colors().editor_background;
        let canvas = self.render_canvas(window, cx).into_any_element();
        let overlay = self.render_overlay(cx).into_any_element();
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
            .on_action(cx.listener(Self::focus_selected))
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
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up_out))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_pinch(cx.listener(Self::pinch))
            .child(canvas)
            .children(body.map(|body| div().absolute().size_full().top_0().left_0().child(body)))
            .child(overlay)
            .child(self.render_caption(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placed_nodes_round_trip_and_bad_data_reads_as_none() {
        let node = SceneId::Node(NodeRef {
            type_name: "Person".into(),
            id: "person-000001".into(),
        });
        let mut pinned = HashMap::new();
        pinned.insert(
            pin_key(&SceneId::Type("Person".into())).unwrap(),
            Vec2::new(12.4, -30.6),
        );
        pinned.insert(pin_key(&node).unwrap(), Vec2::new(-5., 7.));
        let saved = pins_to_json(&pinned);
        let back = pins_from_json(&saved);
        assert_eq!(back.len(), 2);
        assert_eq!(back["type:Person"], Vec2::new(12., -31.));
        assert_eq!(back["node:Person/person-000001"], Vec2::new(-5., 7.));
        // Hubs follow their node; they are never placed by hand.
        let hub = SceneId::Hub(GroupKey {
            node: NodeRef {
                type_name: "Person".into(),
                id: "person-000001".into(),
            },
            group: gq_language::RelationshipGroup {
                edge: "Knows".into(),
                direction: gq_language::Direction::Outgoing,
                other_type: "Person".into(),
            },
        });
        assert_eq!(pin_key(&hub), None);
        assert!(pins_from_json(&serde_json::Value::Null).is_empty());
        assert!(
            pins_from_json(&serde_json::json!({"pinned": {"a": [1, "x"], "b": [1]}})).is_empty()
        );
    }
}
