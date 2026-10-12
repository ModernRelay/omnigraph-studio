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

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, BorderStyle, Bounds, ContentMask, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathBuilder, PinchEvent, Pixels, Point, Render, ScrollWheelEvent, SharedString, Subscription,
    Task, TextAlign, TextRun, Window, canvas, point, px, quad, size,
};
use graph_store::{
    Exploration, ExplorationEvent, ExplorePath, GraphSession, GroupKey, Loadable, NodeRef,
    PageStatus, SessionEvent,
};
use studio_actions::canvas::{
    CollapseNode, ExpandNode, FitToWindow, MoveDown, MoveLeft, MoveRight, MoveUp, ZoomIn, ZoomOut,
};
use studio_actions::graph::{OpenCanvas, OpenNode, OpenType, ShowNodeOnCanvas};
use studio_ui::{empty_state, type_color};
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace};
use ui::Tooltip;
use ui::prelude::*;

use layout::{LayoutKind, LayoutNode, Vec2};
use model::{HUB_STEP, NODE_CAP, Scene, SceneId, SceneKind};

/// Labels of graph nodes show from this zoom up; hubs from a little further out.
const NODE_LABEL_ZOOM: f32 = 0.6;
const HUB_LABEL_ZOOM: f32 = 0.45;
const MAX_ZOOM: f32 = 2.;
const ZOOM_STEP: f32 = 1.25;
const LABEL_SIZE: f32 = 12.;

/// Registers the canvas's keys and the actions that open it.
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, "canvas", |_, workspace, window, cx| {
        open_canvas(workspace, window, cx);
    });
    cx.observe_new(|workspace: &mut Workspace, window, _cx| {
        if window.is_none() {
            return;
        }
        workspace.register_action(|workspace, _: &OpenCanvas, window, cx| {
            open_canvas(workspace, window, cx);
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
}

struct Drag {
    start: Point<Pixels>,
    center: Vec2,
    moved: bool,
}

/// The canvas tab of one graph.
pub struct GraphCanvas {
    session: Entity<GraphSession>,
    exploration: Entity<Exploration>,
    focus_handle: FocusHandle,
    /// The person asked to see the schema although an exploration exists.
    show_schema: bool,
    mode: Mode,
    scene: Arc<Scene>,
    positions: Arc<Vec<Vec2>>,
    /// Where each node was drawn, for the current mode and root: such nodes never move.
    known: HashMap<SceneId, Vec2>,
    layout_key: Option<(Mode, Option<NodeRef>)>,
    layout_generation: u64,
    _layout_task: Option<Task<()>>,
    hub_limits: HashMap<GroupKey, usize>,
    center: Vec2,
    zoom: f32,
    fit_zoom: f32,
    fit_pending: bool,
    center_on: Option<SceneId>,
    selected: Option<SceneId>,
    hovered: Option<usize>,
    drag: Option<Drag>,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
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
        "Canvas".into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::GitGraph)
    }

    fn dedupe_key(&self, cx: &App) -> Option<SharedString> {
        Some(canvas_key(&self.session.read(cx).graph_id))
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "canvas".into(),
            data: serde_json::Value::Null,
        })
    }

    fn is_stale(&self, cx: &App) -> bool {
        self.exploration.read(cx).newer_available()
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
            focus_handle: cx.focus_handle(),
            show_schema: false,
            mode: Mode::Schema,
            scene: Arc::default(),
            positions: Arc::default(),
            known: HashMap::new(),
            layout_key: None,
            layout_generation: 0,
            _layout_task: None,
            hub_limits: HashMap::new(),
            center: Vec2::default(),
            zoom: 1.,
            fit_zoom: 1.,
            fit_pending: true,
            center_on: None,
            selected: None,
            hovered: None,
            drag: None,
            bounds: Rc::new(Cell::new(None)),
            _subscriptions: subscriptions,
        };
        canvas.rebuild(cx);
        canvas.follow_selection(cx);
        canvas
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
            Mode::Schema => {
                let session = self.session.read(cx);
                let scene = model::schema_scene(&schema, |name| session.count_of(name));
                let positions = layout::schema_positions(scene.nodes.len());
                self._layout_task = None;
                self.apply_layout(scene, positions, cx);
            }
            Mode::Explore => {
                let entries = exploration.entries(cx);
                let scene = model::explore_scene(
                    &entries,
                    exploration.state(),
                    &schema,
                    &self.hub_limits,
                    NODE_CAP,
                );
                let nodes: Vec<LayoutNode> = scene
                    .nodes
                    .iter()
                    .map(|node| LayoutNode {
                        parent: node.parent,
                        kind: match node.kind {
                            SceneKind::Hub { .. } => LayoutKind::Hub,
                            SceneKind::More { .. } => LayoutKind::More,
                            _ => LayoutKind::Node,
                        },
                        space: layout_space(&node.kind, node.radius),
                        previous: self.known.get(&node.id).copied(),
                    })
                    .collect();
                let positions =
                    cx.background_spawn(async move { layout::explore_positions(&nodes) });
                self._layout_task = Some(cx.spawn(async move |this, cx| {
                    let positions = positions.await;
                    this.update(cx, |this, cx| {
                        if this.layout_generation == generation {
                            this.apply_layout(scene, positions, cx);
                        }
                    })
                    .ok();
                }));
            }
        }
    }

    fn apply_layout(&mut self, scene: Scene, positions: Vec<Vec2>, cx: &mut Context<Self>) {
        for (node, position) in scene.nodes.iter().zip(&positions) {
            self.known.insert(node.id.clone(), *position);
        }
        self.hovered = None;
        self.scene = Arc::new(scene);
        self.positions = Arc::new(positions);
        if let Some(target) = self.center_on.clone()
            && let Some(index) = self.scene.index_of(&target)
        {
            self.center = self.positions[index];
            self.zoom = self.zoom.max(1.);
            self.center_on = None;
            self.fit_pending = false;
        }
        cx.notify();
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
        let scene = self.scene.clone();
        let Some((min, max)) = layout::extent(&self.positions, |index| {
            layout_space(&scene.nodes[index].kind, scene.nodes[index].radius)
        }) else {
            return;
        };
        let width = f32::from(viewport.size.width).max(1.);
        let height = f32::from(viewport.size.height).max(1.);
        let zoom = ((width - 80.) / (max.x - min.x).max(1.))
            .min((height - 120.) / (max.y - min.y).max(1.))
            .clamp(0.05, 1.5);
        self.zoom = zoom;
        self.fit_zoom = zoom;
        self.center = Vec2::new((min.x + max.x) / 2., (min.y + max.y) / 2.);
        self.fit_pending = false;
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

    fn hit(&self, position: Point<Pixels>, window: &Window) -> Option<usize> {
        let viewport = self.viewport(window);
        let mut best: Option<(usize, f32)> = None;
        for (index, node) in self.scene.nodes.iter().enumerate().rev() {
            let Some(world) = self.positions.get(index) else {
                continue;
            };
            let screen = to_screen(*world, viewport, self.center, self.zoom);
            let dx = f32::from(position.x - screen.x);
            let dy = f32::from(position.y - screen.y);
            let radius = (node.radius * self.zoom).max(6.) + 3.;
            let distance = (dx * dx + dy * dy).sqrt();
            let on_label = match node.kind {
                SceneKind::Hub { .. } | SceneKind::More { .. } if self.zoom >= HUB_LABEL_ZOOM => {
                    let owner = match node.kind {
                        SceneKind::Hub { .. } => node.parent.and_then(|parent| {
                            let world = self.positions.get(parent)?;
                            Some(to_screen(*world, viewport, self.center, self.zoom))
                        }),
                        _ => None,
                    };
                    let width = px(node.label.chars().count() as f32 * 7.);
                    let line_height = px(LABEL_SIZE + 4.);
                    let origin = Placement::Side {
                        at: screen,
                        owner,
                        radius: (node.radius * self.zoom).max(3.),
                    }
                    .origin(width, line_height);
                    Bounds::new(origin, size(width, line_height)).contains(&position)
                }
                _ => false,
            };
            if distance <= radius || on_label {
                let score = if on_label { radius } else { distance };
                if best.is_none_or(|(_, b)| score < b) {
                    best = Some((index, score));
                }
            }
        }
        best.map(|(index, _)| index)
    }

    // ---- interaction ----

    fn select(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
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
        self.drag = Some(Drag {
            start: event.position,
            center: self.center,
            moved: false,
        });
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(drag), true) = (&mut self.drag, event.dragging()) {
            let dx = f32::from(event.position.x - drag.start.x);
            let dy = f32::from(event.position.y - drag.start.y);
            if drag.moved || dx.abs() + dy.abs() > 3. {
                drag.moved = true;
                self.center = drag.center - Vec2::new(dx, dy) * (1. / self.zoom);
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
            return;
        }
        let hit = self.hit(event.position, window);
        self.select(hit, cx);
        if let Some(index) = hit {
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
        let delta = event.delta.pixel_delta(window.line_height());
        let factor = (-f32::from(delta.y) * 0.004).exp();
        self.zoom_around(factor, Some(event.position), window);
        cx.notify();
    }

    fn pinch(&mut self, event: &PinchEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_around(1. + event.delta, Some(event.position), window);
        cx.notify();
    }

    fn fit_to_window(&mut self, _: &FitToWindow, window: &mut Window, cx: &mut Context<Self>) {
        let viewport = self.viewport(window);
        self.fit(viewport);
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_around(ZOOM_STEP, None, window);
        cx.notify();
    }

    fn zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_around(1. / ZOOM_STEP, None, window);
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

    fn render_canvas(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let palette = Palette {
            text: colors.text,
            muted: colors.text_muted,
            background: colors.editor_background,
            selection: colors.border_focused,
            hover: colors.border,
            surface: colors.elevated_surface_background,
            border: colors.border_variant,
        };
        let scene = self.scene.clone();
        let positions = self.positions.clone();
        let center = self.center;
        let zoom = self.zoom;
        let selected = self.selected_index();
        let hovered = self.hovered;
        let bounds_cell = self.bounds.clone();
        let _ = window;
        canvas(
            move |bounds, _window, _cx| bounds_cell.set(Some(bounds)),
            move |bounds, (), window, cx| {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    paint_scene(
                        &scene, &positions, bounds, center, zoom, selected, hovered, &palette,
                        window, cx,
                    );
                });
            },
        )
        .size_full()
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
        let surface = cx.theme().colors().elevated_surface_background;
        let border = cx.theme().colors().border_variant;
        let panel = |id: &'static str| {
            h_flex()
                .id(id)
                .gap_1()
                .p_1()
                .rounded_md()
                .bg(surface)
                .border_1()
                .border_color(border)
                .shadow_sm()
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
                            .child(
                                Button::new("mode-schema", "Schema")
                                    .label_size(LabelSize::Small)
                                    .toggle_state(mode == Mode::Schema)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_schema = true;
                                        this.rebuild(cx);
                                    })),
                            )
                            .child(
                                Button::new("mode-explore", "Exploration")
                                    .label_size(LabelSize::Small)
                                    .toggle_state(mode == Mode::Explore)
                                    .disabled(!has_root)
                                    .tooltip(Tooltip::text(if has_root {
                                        "The nodes walked in the tree and here"
                                    } else {
                                        "Open a node first: ↵ on a table row or tree row"
                                    }))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_schema = false;
                                        this.rebuild(cx);
                                    })),
                            )
                            .child(
                                div().px_1().child(
                                    Label::new(summary)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .single_line(),
                                ),
                            ),
                    )
                    .when(newer, |d| {
                        d.child(
                            panel("canvas-newer").child(
                                Button::new("canvas-refresh", "Newer data · Refresh")
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
                        panel("canvas-zoom")
                            .child(
                                IconButton::new("zoom-out", IconName::Dash)
                                    .icon_size(IconSize::Small)
                                    .tooltip(|_window, cx| {
                                        Tooltip::for_action("Zoom out", &ZoomOut, cx)
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.zoom_out(&ZoomOut, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("zoom-fit", "Fit")
                                    .label_size(LabelSize::Small)
                                    .tooltip(|_window, cx| {
                                        Tooltip::for_action("Fit to window", &FitToWindow, cx)
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.fit_to_window(&FitToWindow, window, cx)
                                    })),
                            )
                            .child(
                                IconButton::new("zoom-in", IconName::Plus)
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
                            Button::new("canvas-retry", "Retry")
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
                                "Too many nodes to draw · {hidden} not shown (limit {NODE_CAP})"
                            ))
                            .size(LabelSize::Small),
                        )
                        .child(
                            Button::new("canvas-table", "Open as table")
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Filled)
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(OpenType {
                                            type_name: type_name.clone(),
                                            at_commit: None,
                                        }),
                                        cx,
                                    )
                                }),
                        ),
                )
            }))
    }
}

/// The room a node needs in layout: its shape plus its label.
fn layout_space(kind: &SceneKind, radius: f32) -> f32 {
    match kind {
        SceneKind::Type { .. } => radius + 24.,
        SceneKind::Node { .. } => radius + 24.,
        SceneKind::Hub { .. } => 40.,
        SceneKind::More { .. } => 30.,
    }
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

struct Palette {
    text: Hsla,
    muted: Hsla,
    background: Hsla,
    selection: Hsla,
    hover: Hsla,
    surface: Hsla,
    border: Hsla,
}

/// One straight edge on screen.
type Segment = (Point<Pixels>, Point<Pixels>);

/// Segments per path: GPUI caps one path near 65k vertices; a stroked segment is a few.
const SEGMENTS_PER_PATH: usize = 2_000;

#[allow(clippy::too_many_arguments)]
fn paint_scene(
    scene: &Scene,
    positions: &[Vec2],
    bounds: Bounds<Pixels>,
    center: Vec2,
    zoom: f32,
    selected: Option<usize>,
    hovered: Option<usize>,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    if positions.len() != scene.nodes.len() {
        return;
    }
    let screen: Vec<Point<Pixels>> = positions
        .iter()
        .map(|world| to_screen(*world, bounds, center, zoom))
        .collect();
    let radius: Vec<f32> = scene
        .nodes
        .iter()
        .map(|node| (node.radius * zoom).max(3.))
        .collect();
    let visible = |p: Point<Pixels>, margin: f32| {
        p.x >= bounds.left() - px(margin)
            && p.x <= bounds.right() + px(margin)
            && p.y >= bounds.top() - px(margin)
            && p.y <= bounds.bottom() + px(margin)
    };

    // Edges, batched by color into a few paths in one layer.
    let mut lines: HashMap<&str, Vec<Segment>> = HashMap::new();
    let mut loops: Vec<(usize, &str, &str)> = Vec::new();
    let mut arrows: Vec<(Point<Pixels>, Point<Pixels>, &str)> = Vec::new();
    let mut labels: Vec<(Point<Pixels>, &str)> = Vec::new();
    // Offsets for parallel labelled edges between the same pair.
    let mut pair_count: HashMap<(usize, usize), usize> = HashMap::new();
    for edge in &scene.edges {
        let (from, to) = (screen[edge.from], screen[edge.to]);
        if !visible(from, 400.) && !visible(to, 400.) {
            continue;
        }
        if edge.from == edge.to {
            loops.push((
                edge.from,
                &edge.color_key,
                edge.label.as_deref().unwrap_or(""),
            ));
            continue;
        }
        let pair = (edge.from.min(edge.to), edge.from.max(edge.to));
        let nth = pair_count.entry(pair).or_default();
        let offset = *nth as f32 * 14.;
        *nth += 1;
        let (from, to) = if offset > 0. {
            let normal = perpendicular(from, to);
            (
                point(
                    from.x + px(normal.x * offset),
                    from.y + px(normal.y * offset),
                ),
                point(to.x + px(normal.x * offset), to.y + px(normal.y * offset)),
            )
        } else {
            (from, to)
        };
        // From boundary to boundary, so a line never shows through a node.
        let start = shorten(to, from, radius[edge.from]);
        let end = shorten(from, to, radius[edge.to] + 1.);
        lines.entry(&edge.color_key).or_default().push((start, end));
        if let Some(label) = &edge.label {
            arrows.push((start, end, &edge.color_key));
            labels.push((point((from.x + to.x) / 2., (from.y + to.y) / 2.), label));
        }
    }
    let stroke = px((1.5 * zoom.sqrt()).clamp(1., 2.));
    window.paint_layer(bounds, |window| {
        for (color_key, segments) in &lines {
            let color = type_color(color_key).opacity(0.55);
            for chunk in segments.chunks(SEGMENTS_PER_PATH) {
                let mut builder = PathBuilder::stroke(stroke);
                for (from, to) in chunk {
                    builder.move_to(*from);
                    builder.line_to(*to);
                }
                if let Ok(path) = builder.build() {
                    window.paint_path(path, color);
                }
            }
        }
        for (node, color_key, _) in &loops {
            let p = screen[*node];
            let r = radius[*node];
            let start = point(p.x - px(r * 0.6), p.y - px(r * 0.8));
            let end = point(p.x + px(r * 0.6), p.y - px(r * 0.8));
            let mut builder = PathBuilder::stroke(stroke);
            builder.move_to(start);
            builder.cubic_bezier_to(
                end,
                point(p.x - px(r * 2.2), p.y - px(r * 3.2)),
                point(p.x + px(r * 2.2), p.y - px(r * 3.2)),
            );
            if let Ok(path) = builder.build() {
                window.paint_path(path, type_color(color_key).opacity(0.7));
            }
            let tip = end;
            arrows.push((point(tip.x + px(4.), tip.y - px(8.)), tip, color_key));
        }
        let mut heads: HashMap<&str, PathBuilder> = HashMap::new();
        for (from, tip, color_key) in &arrows {
            let direction = unit(*from, *tip);
            let size = 7.;
            let base = point(
                tip.x - px(direction.x * size),
                tip.y - px(direction.y * size),
            );
            let normal = Vec2::new(-direction.y, direction.x);
            let left = point(
                base.x + px(normal.x * size * 0.5),
                base.y + px(normal.y * size * 0.5),
            );
            let right = point(
                base.x - px(normal.x * size * 0.5),
                base.y - px(normal.y * size * 0.5),
            );
            heads
                .entry(color_key)
                .or_insert_with(PathBuilder::fill)
                .add_polygon(&[*tip, left, right], true);
        }
        for (color_key, builder) in heads {
            if let Ok(path) = builder.build() {
                window.paint_path(path, type_color(color_key).opacity(0.8));
            }
        }
    });

    // Nodes.
    for (index, node) in scene.nodes.iter().enumerate() {
        let p = screen[index];
        let r = radius[index];
        if !visible(p, r + 4.) {
            continue;
        }
        let color = type_color(&node.color_key);
        let is_selected = selected == Some(index);
        let is_hovered = hovered == Some(index);
        let (fill, border_width, border_color): (Hsla, f32, Hsla) = match &node.kind {
            SceneKind::Type { .. } => (color, 2., palette.background),
            SceneKind::Node { root, .. } => {
                (color, if *root { 3. } else { 1.5 }, palette.background)
            }
            SceneKind::Hub { open, .. } => {
                (if *open { color } else { palette.background }, 2., color)
            }
            SceneKind::More { .. } => (palette.surface, 1., palette.border),
        };
        let (border_width, border_color) = if is_selected {
            (3., palette.selection)
        } else if is_hovered {
            (border_width.max(2.), palette.hover)
        } else {
            (border_width, border_color)
        };
        let circle = Bounds::new(
            point(p.x - px(r), p.y - px(r)),
            size(px(2. * r), px(2. * r)),
        );
        window.paint_quad(quad(
            circle,
            px(r),
            fill,
            px(border_width),
            border_color,
            BorderStyle::Solid,
        ));
    }

    // Labels, above the zoom threshold or for what the person is looking at.
    let font = theme::theme_settings(cx).ui_font(cx).clone();
    let line_height = px(LABEL_SIZE + 4.);
    // Paints one label; with `backdrop`, on a small pill so it reads over edges.
    let text = |window: &mut Window,
                cx: &mut App,
                content: &str,
                color: Hsla,
                placement: Placement,
                backdrop: Option<Hsla>| {
        let run = TextRun {
            len: content.len(),
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window.text_system().shape_line(
            SharedString::from(content.to_owned()),
            px(LABEL_SIZE),
            &[run],
            None,
        );
        let origin = placement.origin(line.width, line_height);
        let x = origin.x;
        if let Some(backdrop) = backdrop {
            let pill = Bounds::new(
                point(x - px(3.), origin.y),
                size(line.width + px(6.), line_height),
            );
            window.paint_quad(quad(
                pill,
                px(4.),
                backdrop,
                px(0.),
                backdrop,
                BorderStyle::Solid,
            ));
        }
        line.paint(
            point(x, origin.y),
            line_height,
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();
    };
    for (index, node) in scene.nodes.iter().enumerate() {
        let p = screen[index];
        let r = radius[index];
        if !visible(p, 200.) {
            continue;
        }
        let focus = selected == Some(index) || hovered == Some(index);
        match &node.kind {
            SceneKind::Type { count } => {
                text(
                    window,
                    cx,
                    &node.label,
                    palette.text,
                    Placement::Below(point(p.x, p.y + px(r + 4.))),
                    None,
                );
                if let Some(count) = count {
                    text(
                        window,
                        cx,
                        &format_count(*count),
                        palette.muted,
                        Placement::Below(point(p.x, p.y + px(r + 4.) + line_height)),
                        None,
                    );
                }
            }
            SceneKind::Node { root, .. } => {
                if zoom >= NODE_LABEL_ZOOM || focus || *root {
                    text(
                        window,
                        cx,
                        &node.label,
                        palette.text,
                        Placement::Below(point(p.x, p.y + px(r + 3.))),
                        Some(palette.background.opacity(0.85)),
                    );
                }
            }
            SceneKind::Hub { .. } | SceneKind::More { .. } => {
                if zoom >= HUB_LABEL_ZOOM || focus {
                    let color = if matches!(node.kind, SceneKind::More { .. }) {
                        palette.muted
                    } else {
                        palette.text
                    };
                    let owner = match node.kind {
                        SceneKind::Hub { .. } => node.parent.map(|parent| screen[parent]),
                        _ => None,
                    };
                    text(
                        window,
                        cx,
                        &node.label,
                        color,
                        Placement::Side {
                            at: p,
                            owner,
                            radius: r,
                        },
                        Some(palette.background.opacity(0.85)),
                    );
                }
            }
        }
    }
    if zoom >= HUB_LABEL_ZOOM {
        for (at, label) in labels {
            if visible(at, 50.) {
                text(
                    window,
                    cx,
                    label,
                    palette.muted,
                    Placement::Below(point(at.x, at.y - line_height / 2.)),
                    Some(palette.background),
                );
            }
        }
        for (node, _, label) in loops {
            let p = screen[node];
            let r = radius[node];
            text(
                window,
                cx,
                label,
                palette.muted,
                Placement::Below(point(p.x, p.y - px(r * 3.0) - line_height)),
                Some(palette.background),
            );
        }
    }
}

/// Where a label goes, given its width once shaped.
#[derive(Clone, Copy)]
enum Placement {
    /// Centered horizontally, with its top at the point.
    Below(Point<Pixels>),
    /// Beside a hub or chip at `at`, on the side facing `owner` (the node it hangs off), so it
    /// never sits in the fan of neighbours; to the right when there is no owner.
    Side {
        at: Point<Pixels>,
        owner: Option<Point<Pixels>>,
        radius: f32,
    },
}

impl Placement {
    fn origin(self, width: Pixels, line_height: Pixels) -> Point<Pixels> {
        match self {
            Placement::Below(top) => point(top.x - width / 2., top.y),
            Placement::Side { at, owner, radius } => {
                let gap = px(radius + 5.);
                let toward = owner.map(|owner| unit(at, owner));
                match toward {
                    Some(d) if d.y < -0.6 => point(at.x - width / 2., at.y - gap - line_height),
                    Some(d) if d.y > 0.6 => point(at.x - width / 2., at.y + gap),
                    Some(d) if d.x < 0. => point(at.x - gap - width, at.y - line_height / 2.),
                    _ => point(at.x + gap, at.y - line_height / 2.),
                }
            }
        }
    }
}

fn format_count(count: i64) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn unit(from: Point<Pixels>, to: Point<Pixels>) -> Vec2 {
    let dx = f32::from(to.x - from.x);
    let dy = f32::from(to.y - from.y);
    let length = (dx * dx + dy * dy).sqrt().max(1e-3);
    Vec2::new(dx / length, dy / length)
}

fn perpendicular(from: Point<Pixels>, to: Point<Pixels>) -> Vec2 {
    let direction = unit(from, to);
    Vec2::new(-direction.y, direction.x)
}

fn shorten(from: Point<Pixels>, to: Point<Pixels>, by: f32) -> Point<Pixels> {
    let direction = unit(from, to);
    point(to.x - px(direction.x * by), to.y - px(direction.y * by))
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
                    },
                    None,
                )
                .into_any_element(),
            ),
            Loadable::Loaded(_) => None,
        };
        if self.fit_pending && !self.scene.nodes.is_empty() {
            let viewport = self.viewport(window);
            self.fit(viewport);
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
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::expand_node))
            .on_action(cx.listener(Self::collapse_node))
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
    }
}
