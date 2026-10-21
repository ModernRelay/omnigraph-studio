//! The ontology page (spec 006, "Ontology page"): a schema's types grouped into domains, the
//! types related to most of the others set apart ([`crate::domains`]), and any one type in
//! focus: what the schema says it is, its properties, and its relationships, each named and
//! leading to the type at its other end. It draws no web of every relationship: a schema of a
//! hundred types has hundreds, and no layout makes that readable.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Pixels,
    Point, Render, ScrollHandle, SharedString, Subscription, Window, canvas, point, px,
};
use gq_language::SchemaModel;
use graph_store::{EntityKind, GraphSession, SessionEvent};
use studio_actions::graph::{BrowseType, OpenOntology, OpenSchema, OpenType};
use studio_ui::ds::{self, size, space};
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::Tooltip;
use ui::prelude::*;

use crate::domains::{self, Domains};
use crate::layered::{self, Link, Route};
use crate::layout::Vec2;
use crate::paint::{self, RING, Tone};

/// A domain card's width, and the focus view's columns: the related types either side, the
/// lines between, and the type in focus.
const CARD_WIDTH: f32 = 240.;
const SIDE_WIDTH: f32 = 300.;
const GUTTER_WIDTH: f32 = 72.;
const CENTRE_WIDTH: f32 = 340.;
/// Where the lines meet the type in focus: the middle of its card's header.
const CENTRE_HEADER: f32 = 44.;
/// A side groups its lines by domain once it holds more than this many.
const GROUP_ABOVE: usize = 8;
/// Up to this many types and relationships, the page draws the whole ontology as one map, its
/// types laid out in columns ([`crate::layered`]) and every relationship named.
const MAP_TYPES: usize = domains::SMALL_SCHEMA;
const MAP_RELATIONSHIPS: usize = 24;
/// A type's card on the map, and the room around the map.
const MAP_CARD: (f32, f32) = (180., 32.);
const MAP_MARGIN: f32 = 40.;
/// A relationship label's height, and the widest the map makes room for.
const LABEL_HEIGHT: f32 = 20.;
const LABEL_SLOT: f32 = 240.;
/// The lines' tones, in thousandths: at rest, and of the line pointed at.
const LINE_ALPHA: u16 = 160;
const LINE_POINTED_ALPHA: u16 = 450;

/// The ontology of the session's graph.
pub(crate) struct OntologyPage {
    session: Entity<GraphSession>,
    focus_handle: FocusHandle,
    view: Option<Arc<View>>,
    /// The type in focus; none shows the domains.
    focus: Option<String>,
    /// The types in focus before, for Back.
    back: Vec<String>,
    pointed: Option<Pointed>,
    scroll: ScrollHandle,
    /// The page's width as last drawn, for how many columns of domains it holds.
    width: Cell<f32>,
    _subscriptions: Vec<Subscription>,
}

/// What the pointer is on: a type in the domain map, whose related types light up, or one
/// line of the focus view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pointed {
    Type(usize),
    Line(Side, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// The types whose relationships reach the type in focus.
    In,
    /// The types the type in focus reaches.
    Out,
}

/// The schema as the page shows it: its types grouped, each type's related types, and, for a
/// small schema, its map.
struct View {
    schema: Arc<SchemaModel>,
    domains: Domains,
    index: HashMap<String, usize>,
    related: Vec<Vec<usize>>,
    map: Option<Map>,
}

/// A small ontology laid out whole: each type's card centre and each relationship's route
/// (empty for a relationship of a type to itself, drawn as a loop over its card), from the
/// map's top left, and how large the map is.
struct Map {
    at: Vec<Vec2>,
    routes: Vec<Route>,
    size: Vec2,
}

impl Map {
    fn new(schema: &SchemaModel, links: &[(usize, usize)]) -> Self {
        let half = vec![MAP_CARD.0 / 2.; schema.nodes.len()];
        let named: Vec<Link> = links
            .iter()
            .zip(&schema.edges)
            .map(|(&(from, to), edge)| Link {
                from,
                to,
                label_half_width: label_width(&edge.name) / 2.,
            })
            .collect();
        let laid = layered::layered(&half, &named);
        // The box around every card, route, label and loop.
        let (mut min, mut max) = (Vec2::new(f32::MAX, f32::MAX), Vec2::new(f32::MIN, f32::MIN));
        let mut grow = |p: Vec2| {
            min = Vec2::new(min.x.min(p.x), min.y.min(p.y));
            max = Vec2::new(max.x.max(p.x), max.y.max(p.y));
        };
        for p in &laid.positions {
            grow(*p - Vec2::new(MAP_CARD.0 / 2., MAP_CARD.1 / 2. + LOOP_ROOM));
            grow(*p + Vec2::new(MAP_CARD.0 / 2., MAP_CARD.1 / 2.));
        }
        for (route, edge) in laid.routes.iter().zip(&schema.edges) {
            route.points.iter().for_each(|p| grow(*p));
            if let Some(at) = route.label_at {
                let half = Vec2::new(label_width(&edge.name) / 2., LABEL_HEIGHT / 2.);
                grow(at - half);
                grow(at + half);
            }
        }
        let shift = Vec2::new(MAP_MARGIN, MAP_MARGIN) - min;
        Self {
            at: laid.positions.iter().map(|p| *p + shift).collect(),
            routes: laid
                .routes
                .into_iter()
                .map(|route| Route {
                    points: route.points.into_iter().map(|p| p + shift).collect(),
                    label_at: route.label_at.map(|p| p + shift),
                })
                .collect(),
            size: max - min + Vec2::new(2. * MAP_MARGIN, 2. * MAP_MARGIN),
        }
    }
}

/// Above a card: room for its loops and their labels.
const LOOP_ROOM: f32 = 44.;

/// About how wide a relationship's label is drawn: its characters at the small text size, and
/// its padding.
fn label_width(name: &str) -> f32 {
    name.chars().count() as f32 * 6.6 + 2. * space::SM
}

impl View {
    fn new(schema: Arc<SchemaModel>) -> Self {
        let index: HashMap<String, usize> = schema
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (node.name.clone(), i))
            .collect();
        // One per relationship, in order; one naming a type the schema lacks joins nothing.
        let links: Vec<(usize, usize)> = schema
            .edges
            .iter()
            .map(|edge| match (index.get(&edge.from), index.get(&edge.to)) {
                (Some(&from), Some(&to)) => (from, to),
                _ => (usize::MAX, usize::MAX),
            })
            .collect();
        let mut related: Vec<Vec<usize>> = vec![Vec::new(); schema.nodes.len()];
        for &(from, to) in &links {
            if from != to && from != usize::MAX {
                related[from].push(to);
                related[to].push(from);
            }
        }
        for list in related.iter_mut() {
            list.sort_unstable();
            list.dedup();
        }
        let map = (schema.nodes.len() <= MAP_TYPES && schema.edges.len() <= MAP_RELATIONSHIPS)
            .then(|| Map::new(&schema, &links));
        Self {
            domains: domains::domains(schema.nodes.len(), &links),
            schema,
            index,
            related,
            map,
        }
    }

    fn name(&self, ty: usize) -> &str {
        &self.schema.nodes[ty].name
    }

    /// A domain's title: its most related type, and how many more it holds; one domain of a
    /// small schema is its types.
    fn title(&self, group: usize) -> String {
        let types = &self.domains.groups[group];
        if self.domains.rest && group + 1 == self.domains.groups.len() {
            return "Other types".into();
        }
        if self.domains.groups.len() == 1 && self.domains.shared.is_empty() {
            return "Types".into();
        }
        match types.len() {
            0 => String::new(),
            1 => self.name(types[0]).to_owned(),
            n => format!("{} and {} more", self.name(types[0]), n - 1),
        }
    }

    /// A type's place: its domain's title, or the shared types'.
    fn place(&self, ty: usize) -> String {
        match self.domains.group_of[ty] {
            Some(group) => self.title(group),
            None => SHARED.into(),
        }
    }

    /// One side of the focus view: the relationships reaching `focus` (or leaving it), each to
    /// the type at its other end, grouped by that type's domain (the shared types first) once
    /// there are more than [`GROUP_ABOVE`]; within a group by type, then relationship.
    fn side(&self, focus: usize, side: Side) -> Vec<Entry> {
        let mut lines: Vec<(usize, usize)> = self
            .schema
            .edges
            .iter()
            .enumerate()
            .filter_map(|(edge_index, edge)| {
                let (from, to) = (*self.index.get(&edge.from)?, *self.index.get(&edge.to)?);
                match side {
                    // A relationship of the type to itself is listed once, among those it
                    // reaches.
                    Side::In if to == focus && from != focus => Some((edge_index, from)),
                    Side::Out if from == focus => Some((edge_index, to)),
                    _ => None,
                }
            })
            .collect();
        let rank = |ty: usize| match self.domains.group_of[ty] {
            None => 0,
            Some(group) => group + 1,
        };
        lines.sort_by(|a, b| {
            rank(a.1)
                .cmp(&rank(b.1))
                .then_with(|| self.name(a.1).cmp(self.name(b.1)))
                .then_with(|| {
                    self.schema.edges[a.0]
                        .name
                        .cmp(&self.schema.edges[b.0].name)
                })
        });
        let grouped =
            lines.len() > GROUP_ABOVE && lines.iter().any(|l| rank(l.1) != rank(lines[0].1));
        let mut entries = Vec::with_capacity(lines.len() + 8);
        let mut last_rank = None;
        for (edge, other) in lines {
            if grouped && last_rank != Some(rank(other)) {
                last_rank = Some(rank(other));
                entries.push(Entry::Heading(self.place(other)));
            }
            entries.push(Entry::Line { edge, other });
        }
        entries
    }
}

/// The shared types' place.
const SHARED: &str = "Linked to most types";

/// One row of a side of the focus view.
#[derive(Clone, Debug, PartialEq)]
enum Entry {
    /// A domain's title over its lines.
    Heading(String),
    Line {
        edge: usize,
        other: usize,
    },
}

impl OntologyPage {
    pub(crate) fn new(session: Entity<GraphSession>, cx: &mut Context<Self>) -> Self {
        let subscriptions =
            vec![
                cx.subscribe(&session, |this, _, event: &SessionEvent, cx| match event {
                    SessionEvent::SchemaChanged | SessionEvent::SchemaMoved => this.read_schema(cx),
                    SessionEvent::CountsChanged => cx.notify(),
                    _ => {}
                }),
            ];
        let mut page = Self {
            session,
            focus_handle: cx.focus_handle(),
            view: None,
            focus: None,
            back: Vec::new(),
            pointed: None,
            scroll: ScrollHandle::new(),
            width: Cell::new(0.),
            _subscriptions: subscriptions,
        };
        page.read_schema(cx);
        page
    }

    fn read_schema(&mut self, cx: &mut Context<Self>) {
        let schema = self.session.read(cx).schema_model();
        let same = match (&self.view, &schema) {
            (Some(view), Some(schema)) => Arc::ptr_eq(&view.schema, schema),
            _ => false,
        };
        if !same {
            self.view = schema.map(|schema| Arc::new(View::new(schema)));
            self.pointed = None;
        }
        cx.notify();
    }

    /// Shows `type_name` in focus; the type shown before is kept for Back.
    pub(crate) fn focus_on(&mut self, type_name: &str, cx: &mut Context<Self>) {
        if self.focus.as_deref() == Some(type_name) {
            return;
        }
        if let Some(previous) = self.focus.replace(type_name.to_owned()) {
            self.back.push(previous);
        }
        self.pointed = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Back to the type in focus before, or to the domains.
    fn go_back(&mut self, cx: &mut Context<Self>) {
        self.focus = self.back.pop();
        self.pointed = None;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn show_domains(&mut self, cx: &mut Context<Self>) {
        self.focus = None;
        self.back.clear();
        self.pointed = None;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn point(&mut self, pointed: Option<Pointed>, cx: &mut Context<Self>) {
        if self.pointed != pointed {
            self.pointed = pointed;
            cx.notify();
        }
    }

    fn count(&self, type_name: &str, cx: &App) -> Option<String> {
        self.session
            .read(cx)
            .count_of(EntityKind::Node, type_name)
            .map(studio_ui::group_count)
    }

    // ---- the domain map ----

    fn render_domains(
        &self,
        view: &Arc<View>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lit: Vec<usize> = match self.pointed {
            Some(Pointed::Type(ty)) => view.related[ty].clone(),
            _ => Vec::new(),
        };
        let shared = (!view.domains.shared.is_empty()).then(|| {
            v_flex()
                .gap(px(space::SM))
                .child(ds::text::meta(SHARED))
                .child(
                    h_flex().flex_wrap().gap(px(space::SM)).children(
                        view.domains
                            .shared
                            .iter()
                            .map(|&ty| self.render_shared_type(view, ty, &lit, cx))
                            .collect::<Vec<_>>(),
                    ),
                )
        });
        // Cards in columns as many as the page holds, each card to the column with least in
        // it, so a short card does not leave a gap under it as a row would. The page's width is
        // its last frame's; before one, the window's less the sidebar.
        let width = match self.width.get() {
            0. => f32::from(window.viewport_size().width) - size::SIDEBAR_WIDTH,
            width => width,
        } - 2. * space::PAGE;
        let columns = (((width + space::LG) / (CARD_WIDTH + space::LG)).floor() as usize).max(1);
        let mut column_of = vec![0usize; view.domains.groups.len()];
        let mut filled = vec![0usize; columns];
        for (group, types) in view.domains.groups.iter().enumerate() {
            let shortest = (0..columns).min_by_key(|&c| (filled[c], c)).unwrap_or(0);
            column_of[group] = shortest;
            filled[shortest] += types.len() + 3;
        }
        let mut cards: Vec<Vec<AnyElement>> = (0..columns).map(|_| Vec::new()).collect();
        let built: Vec<AnyElement> = (0..view.domains.groups.len())
            .map(|group| {
                let types = &view.domains.groups[group];
                let links = &view.domains.links[group];
                let linked = links
                    .iter()
                    .take(3)
                    .map(|&(other, count)| match view.domains.groups[other].first() {
                        Some(&lead) => format!("{} {count}", view.name(lead)),
                        None => count.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                ds::card(cx)
                    .child(
                        ds::card_header(cx)
                            .child(ds::text::title(view.title(group)))
                            .child(div().flex_1())
                            .child(ds::text::meta(types.len().to_string())),
                    )
                    .child(
                        v_flex().py(px(space::XS)).children(
                            types
                                .iter()
                                .map(|&ty| self.render_type_row(view, ty, &lit, cx))
                                .collect::<Vec<_>>(),
                        ),
                    )
                    .when(!linked.is_empty(), |card| {
                        card.child(
                            ds::card_footer(cx).px(px(space::LG)).child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(ds::text::meta(format!("Linked with {linked}"))),
                            ),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        for (group, card) in built.into_iter().enumerate() {
            cards[column_of[group]].push(card);
        }

        v_flex()
            .gap(px(space::XL))
            .children(shared)
            .child(
                h_flex()
                    .items_start()
                    .gap(px(space::LG))
                    .children(cards.into_iter().map(|column| {
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(space::LG))
                            .children(column)
                    })),
            )
            .into_any_element()
    }

    /// A shared type: a pill, as it stands apart from every domain.
    fn render_shared_type(
        &self,
        view: &Arc<View>,
        ty: usize,
        lit: &[usize],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = view.name(ty).to_owned();
        let count = self.count(&name, cx);
        let pointed = self.pointed == Some(Pointed::Type(ty));
        ds::lifted(
            h_flex()
                .id(SharedString::from(format!("shared-{name}")))
                .h(px(size::PILL))
                .px(px(space::CONTROL_INSET))
                .gap(px(space::SM))
                .rounded(px(ds::radii(cx).sm))
                .bg(ds::surface::document(cx))
                .cursor_pointer()
                .when(lit.contains(&ty) || pointed, |pill| {
                    pill.bg(ds::surface::selected_chrome(cx))
                })
                .child(ds::type_dot(&name))
                .child(ds::text::body(name.clone()))
                .children(count.map(ds::text::meta))
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    this.point(hovered.then_some(Pointed::Type(ty)), cx)
                }))
                .on_click(cx.listener(move |this, _, _, cx| this.focus_on(&name, cx))),
            cx,
        )
        .into_any_element()
    }

    /// A type in a domain card: its colour, its name and how many nodes it has. A type related
    /// to the one pointed at is lit.
    fn render_type_row(
        &self,
        view: &Arc<View>,
        ty: usize,
        lit: &[usize],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = view.name(ty).to_owned();
        let related = view.related[ty].len();
        let target = name.clone();
        div()
            .id(SharedString::from(format!("type-hover-{ty}")))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.point(hovered.then_some(Pointed::Type(ty)), cx)
            }))
            .child(
                ds::SidebarRow::new(SharedString::from(format!("type-{name}")), name.clone())
                    .start(ds::type_dot(&name))
                    .end(ds::sidebar_count(self.count(&name, cx).unwrap_or_default()))
                    .selected(lit.contains(&ty))
                    .tooltip(format!(
                        "{name} · related to {related} {}",
                        if related == 1 { "type" } else { "types" }
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| this.focus_on(&target, cx))),
            )
            .into_any_element()
    }

    // ---- a small ontology's map ----

    /// The whole ontology: each type a card where the layout put it, each relationship a
    /// dashed line between their sides, named on it; the lines of the type pointed at are drawn
    /// stronger. A click brings a type into focus.
    fn render_map(&self, view: &Arc<View>, map: &Map, cx: &mut Context<Self>) -> AnyElement {
        let pointed = match self.pointed {
            Some(Pointed::Type(ty)) => Some(ty),
            _ => None,
        };
        let edges: Vec<(usize, usize)> = view
            .schema
            .edges
            .iter()
            .map(|edge| {
                (
                    view.index.get(&edge.from).copied().unwrap_or(usize::MAX),
                    view.index.get(&edge.to).copied().unwrap_or(usize::MAX),
                )
            })
            .collect();
        // Each loop's rank on its type, inside out.
        let mut loops: HashMap<usize, usize> = HashMap::new();
        let loop_rank: Vec<Option<usize>> = edges
            .iter()
            .map(|&(from, to)| {
                (from == to && from != usize::MAX).then(|| {
                    let rank = loops.entry(from).or_default();
                    *rank += 1;
                    *rank - 1
                })
            })
            .collect();
        let local = |p: Vec2| point(px(p.x), px(p.y));
        // Where each relationship's label goes, from the map's top left.
        let labels: Vec<(Vec2, String, bool)> = edges
            .iter()
            .enumerate()
            .filter_map(|(e, &(from, to))| {
                let name = view.schema.edges[e].name.clone();
                let strong = pointed.is_some_and(|p| p == from || p == to);
                let at = match loop_rank[e] {
                    Some(rank) => {
                        let points =
                            paint::loop_points(local(map.at[from]), MAP_CARD.0 / 2., 1., rank);
                        let at = paint::loop_label_at(&points);
                        Vec2::new(f32::from(at.x), f32::from(at.y))
                    }
                    None => map.routes.get(e)?.label_at?,
                };
                Some((at, name, strong))
            })
            .collect();
        let ink = Color::Default.color(cx);
        let ground = ds::surface::canvas(cx);
        let routes: Vec<Vec<Vec2>> = map.routes.iter().map(|r| r.points.clone()).collect();
        let at = map.at.clone();
        let lines = canvas(
            |_, _, _| {},
            move |bounds: Bounds<Pixels>, (), window, _cx| {
                let origin = bounds.origin;
                let place = |p: Vec2| point(origin.x + px(p.x), origin.y + px(p.y));
                let tone = |strong: bool| Tone {
                    alpha: if strong {
                        LINE_POINTED_ALPHA
                    } else {
                        LINE_ALPHA
                    },
                    dotted: false,
                };
                let mut drawn: Vec<(usize, Vec<Point<Pixels>>, Tone)> = Vec::new();
                let mut rings: Vec<(Point<Pixels>, bool)> = Vec::new();
                for (e, &(from, to)) in edges.iter().enumerate() {
                    if from == usize::MAX {
                        continue;
                    }
                    let strong = pointed.is_some_and(|p| p == from || p == to);
                    if let Some(rank) = loop_rank[e] {
                        drawn.push((
                            e,
                            paint::loop_points(place(at[from]), MAP_CARD.0 / 2., 1., rank),
                            tone(strong),
                        ));
                        continue;
                    }
                    let points: Vec<Point<Pixels>> = routes[e].iter().map(|p| place(*p)).collect();
                    for (end, next) in [
                        (0, 1),
                        (
                            points.len().saturating_sub(1),
                            points.len().saturating_sub(2),
                        ),
                    ] {
                        if points.len() >= 2 {
                            let toward = if points[next].x > points[end].x {
                                1.
                            } else {
                                -1.
                            };
                            rings.push((
                                point(points[end].x + px(toward * RING / 2.), points[end].y),
                                strong,
                            ));
                        }
                    }
                    drawn.push((e, points, tone(strong)));
                }
                paint::paint_runs(&drawn, bounds, origin, ink, window);
                for (centre, strong) in rings {
                    let alpha = f32::from(tone(strong).alpha) / 1000.;
                    paint::paint_ring(centre, ink.opacity(alpha), ground, window);
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let cards: Vec<AnyElement> = (0..view.schema.nodes.len())
            .map(|ty| {
                let name = view.name(ty).to_owned();
                let target = name.clone();
                let count = self.count(&name, cx);
                let centre = map.at[ty];
                ds::lifted(
                    h_flex()
                        .id(SharedString::from(format!("map-{name}")))
                        .absolute()
                        .left(px(centre.x - MAP_CARD.0 / 2.))
                        .top(px(centre.y - MAP_CARD.1 / 2.))
                        .w(px(MAP_CARD.0))
                        .h(px(MAP_CARD.1))
                        .px(px(space::CONTROL_INSET))
                        .gap(px(space::SM))
                        .rounded(px(ds::radii(cx).sm))
                        .bg(ds::surface::document(cx))
                        .cursor_pointer()
                        .when(pointed == Some(ty), |card| {
                            card.shadow(ds::elevation::lift(ds::elevation::Lift::Hover, cx))
                        })
                        .child(ds::type_dot(&name))
                        .child(ds::text::body(name.clone()))
                        .child(div().flex_1())
                        .children(count.map(ds::text::meta))
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            this.point(hovered.then_some(Pointed::Type(ty)), cx)
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| this.focus_on(&target, cx))),
                    cx,
                )
                .into_any_element()
            })
            .collect();
        let labels = labels.into_iter().map(|(at, name, strong)| {
            // Centred on its place whatever its width: a slot wider than any label, its pill in
            // the middle.
            h_flex()
                .absolute()
                .left(px(at.x - LABEL_SLOT / 2.))
                .top(px(at.y - LABEL_HEIGHT / 2.))
                .w(px(LABEL_SLOT))
                .h(px(LABEL_HEIGHT))
                .justify_center()
                .child(
                    div()
                        .h(px(LABEL_HEIGHT))
                        .px(px(space::SM))
                        .flex()
                        .items_center()
                        .rounded(px(ds::radii(cx).xs))
                        .bg(ds::surface::canvas_label(cx))
                        .border_1()
                        .border_color(ds::surface::divider(cx))
                        .child(if strong {
                            ds::text::meta(name).color(Color::Default)
                        } else {
                            ds::text::meta(name)
                        }),
                )
        });
        div()
            .relative()
            .w(px(map.size.x))
            .h(px(map.size.y))
            .child(lines)
            .children(labels)
            .children(cards)
            .into_any_element()
    }

    // ---- one type in focus ----

    fn render_focus(&self, view: &Arc<View>, focus: usize, cx: &mut Context<Self>) -> AnyElement {
        let incoming = view.side(focus, Side::In);
        let outgoing = view.side(focus, Side::Out);
        h_flex()
            .items_start()
            .justify_center()
            .w_full()
            .child(self.render_side(view, Side::In, &incoming, cx))
            .child(self.render_gutter(Side::In, &incoming, cx))
            .child(self.render_centre(view, focus, &incoming, &outgoing, cx))
            .child(self.render_gutter(Side::Out, &outgoing, cx))
            .child(self.render_side(view, Side::Out, &outgoing, cx))
            .into_any_element()
    }

    /// The type in focus: its name, its node count, what the schema says it is, its
    /// properties, and what opens it elsewhere.
    fn render_centre(
        &self,
        view: &Arc<View>,
        focus: usize,
        incoming: &[Entry],
        outgoing: &[Entry],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let node = &view.schema.nodes[focus];
        let name = node.name.clone();
        let lines = |entries: &[Entry]| {
            entries
                .iter()
                .filter(|e| matches!(e, Entry::Line { .. }))
                .count()
        };
        let summary = format!(
            "{} · {} in · {} out",
            view.place(focus),
            lines(incoming),
            lines(outgoing)
        );
        let count = self.count(&name, cx);
        let (browse, table, source) = (name.clone(), name.clone(), name.clone());
        ds::card(cx)
            .w(px(CENTRE_WIDTH))
            .flex_none()
            .child(
                ds::card_header(cx)
                    .h(px(CENTRE_HEADER))
                    .child(ds::type_dot(&name))
                    .child(ds::text::title(name.clone()))
                    .child(div().flex_1())
                    .children(count.map(|c| ds::text::meta(format!("{c} nodes")))),
            )
            .child(
                ds::card_body()
                    .gap(px(space::MD))
                    .child(ds::text::meta(summary))
                    .children(node.about.clone().map(ds::text::muted))
                    .when(!node.properties.is_empty(), |body| {
                        body.child(v_flex().children(node.properties.iter().map(|property| {
                            h_flex()
                                .h(px(size::SIDEBAR_ROW))
                                .gap(px(space::MD))
                                .child(ds::text::body(property.name.clone()))
                                .child(div().flex_1())
                                .child(ds::text::meta(if property.is_key {
                                    format!("{} · key", property.display_type())
                                } else {
                                    property.display_type()
                                }))
                        })))
                    }),
            )
            .child(
                ds::card_footer(cx)
                    .child(ds::row_button("open-table", "Open table").on_click(
                        move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(OpenType {
                                    type_name: table.clone(),
                                    at_commit: None,
                                    filter_id: None,
                                    via: None,
                                    preview: false,
                                }),
                                cx,
                            )
                        },
                    ))
                    .child(ds::row_button("browse-rows", "Browse rows").on_click(
                        move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(BrowseType {
                                    type_name: browse.clone(),
                                }),
                                cx,
                            )
                        },
                    ))
                    .child(
                        ds::row_button("in-schema", "Schema").on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(OpenSchema {
                                    type_name: Some(source.clone()),
                                }),
                                cx,
                            )
                        }),
                    ),
            )
    }

    /// One side's rows: domain headings and lines, each line its relationship and the type at
    /// its other end, which a click brings into focus.
    fn render_side(
        &self,
        view: &Arc<View>,
        side: Side,
        entries: &[Entry],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = match side {
            Side::In => "Reach it",
            Side::Out => "It reaches",
        };
        v_flex()
            .w(px(SIDE_WIDTH))
            .flex_none()
            .child(
                h_flex()
                    .h(px(size::ROW))
                    .px(px(space::SM))
                    .when(side == Side::Out, |row| row.justify_end())
                    .child(ds::text::meta(if entries.is_empty() {
                        format!("{title}: none")
                    } else {
                        title.to_owned()
                    })),
            )
            .children(entries.iter().enumerate().map(|(at, entry)| {
                match entry {
                    Entry::Heading(title) => h_flex()
                        .h(px(size::ROW))
                        .px(px(space::SM))
                        .items_end()
                        .pb(px(space::XS))
                        .when(side == Side::Out, |row| row.justify_end())
                        .child(ds::text::meta(title.clone()))
                        .into_any_element(),
                    Entry::Line { edge, other } => {
                        let edge = &view.schema.edges[*edge];
                        let other_name = view.name(*other).to_owned();
                        let pointed = self.pointed == Some(Pointed::Line(side, at));
                        // A long relationship name gives way to the type's: it is cut, the type is
                        // whole up to most of the row.
                        let relationship = div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .when(side == Side::In, |slot| slot.justify_end())
                            .child(ds::text::meta(edge.name.clone()).single_line().truncate());
                        let ty = h_flex()
                            .flex_none()
                            .max_w(px(SIDE_WIDTH * 0.6))
                            .gap(px(space::SM))
                            .child(ds::type_dot(&other_name))
                            .child(ds::text::body(other_name.clone()).single_line().truncate());
                        let tooltip = match &edge.about {
                            Some(about) => {
                                format!("{} → {} → {}\n{about}", edge.from, edge.name, edge.to)
                            }
                            None => format!("{} → {} → {}", edge.from, edge.name, edge.to),
                        };
                        let target = other_name.clone();
                        h_flex()
                            .id(SharedString::from(format!("{side:?}-{at}")))
                            .h(px(size::ROW))
                            .px(px(space::SM))
                            .gap(px(space::MD))
                            .rounded(px(ds::radii(cx).sm))
                            .cursor_pointer()
                            .when(pointed, |row| row.bg(ds::surface::hover(cx)))
                            .map(|row| match side {
                                Side::In => row.child(ty).child(relationship),
                                Side::Out => row.child(relationship).child(ty),
                            })
                            .tooltip(Tooltip::text(tooltip))
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                this.point(hovered.then_some(Pointed::Line(side, at)), cx)
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| this.focus_on(&target, cx)))
                            .into_any_element()
                    }
                }
            }))
    }

    /// The lines between a side and the type in focus: a dashed line from each row to an
    /// upright in the middle of the gutter, and one from there to the type's card, each end in
    /// a ring; the line pointed at is drawn stronger.
    fn render_gutter(
        &self,
        side: Side,
        entries: &[Entry],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Where each line meets its row, down the side: the side's title row comes first.
        let rows: Vec<(usize, f32)> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry, Entry::Line { .. }))
            .map(|(at, _)| (at, size::ROW * (at as f32 + 1.5)))
            .collect();
        let height = size::ROW * (entries.len() as f32 + 1.).max(2.);
        let pointed = match self.pointed {
            Some(Pointed::Line(s, at)) if s == side => Some(at),
            _ => None,
        };
        let ink = Color::Default.color(cx);
        let ground = ds::surface::canvas(cx);
        canvas(
            |_, _, _| {},
            move |bounds: Bounds<Pixels>, (), window, _cx| {
                if rows.is_empty() {
                    return;
                }
                let (left, width) = (f32::from(bounds.left()), f32::from(bounds.size.width));
                let top = f32::from(bounds.top());
                let middle = left + width / 2.;
                // Rows on the left of the gutter for the types that reach the focus; on its
                // right for those it reaches.
                let (row_x, card_x) = match side {
                    Side::In => (left, left + width),
                    Side::Out => (left + width, left),
                };
                let at = |x: f32, y: f32| point(px(x), px(top + y));
                let mut lines: Vec<(usize, Vec<Point<Pixels>>, Tone)> = Vec::new();
                let tone = |strong: bool| Tone {
                    alpha: if strong {
                        LINE_POINTED_ALPHA
                    } else {
                        LINE_ALPHA
                    },
                    dotted: false,
                };
                for &(entry, y) in &rows {
                    let strong = pointed == Some(entry);
                    lines.push((
                        entry,
                        vec![
                            at(row_x, y),
                            at(middle, y),
                            at(middle, CENTRE_HEADER / 2.),
                            at(card_x, CENTRE_HEADER / 2.),
                        ],
                        tone(strong),
                    ));
                }
                paint::paint_runs(&lines, bounds, bounds.origin, ink, window);
                let ring = |x: f32, y: f32, strong: bool| {
                    let toward = if x < middle { 1. } else { -1. };
                    (
                        at(x + toward * RING / 2., y),
                        ink.opacity(f32::from(tone(strong).alpha) / 1000.),
                    )
                };
                let strong_centre = pointed.is_some();
                let mut rings = vec![ring(card_x, CENTRE_HEADER / 2., strong_centre)];
                for &(entry, y) in &rows {
                    rings.push(ring(row_x, y, pointed == Some(entry)));
                }
                for (centre, colour) in rings {
                    paint::paint_ring(centre, colour, ground, window);
                }
            },
        )
        .w(px(GUTTER_WIDTH))
        .h(px(height))
        .flex_none()
    }

    // ---- the page ----

    fn render_header(&self, view: Option<&Arc<View>>, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        let summary = view.map(|view| {
            let groups = view.domains.groups.len();
            let mut summary = format!(
                "{} types · {} relationships",
                view.schema.nodes.len(),
                view.schema.edges.len()
            );
            if view.map.is_none() {
                summary += &format!(
                    " · {groups} {}",
                    if groups == 1 { "domain" } else { "domains" }
                );
            }
            summary
        });
        h_flex()
            .h(px(size::TOOLBAR))
            .px(px(space::PAGE))
            .gap(px(space::MD))
            .flex_none()
            .map(|bar| match (&focus, view) {
                (Some(focus), Some(view)) => {
                    let place = view.index.get(focus).map(|ty| view.place(*ty));
                    bar.child(
                        ds::icon_button("ontology-back", IconName::ArrowLeft)
                            .tooltip(Tooltip::text("Back"))
                            .on_click(cx.listener(|this, _, _, cx| this.go_back(cx))),
                    )
                    .child(
                        ds::button(
                            "ontology-domains",
                            if view.map.is_some() { "Map" } else { "Domains" },
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.show_domains(cx))),
                    )
                    .children(place.map(|place| ds::text::muted(format!("› {place} ›"))))
                    .child(ds::text::title(focus.clone()))
                }
                _ => bar
                    .child(ds::text::title(
                        if view.is_some_and(|view| view.map.is_some()) {
                            "Map"
                        } else {
                            "Domains"
                        },
                    ))
                    .children(summary.map(ds::text::meta)),
            })
    }
}

impl EventEmitter<ItemEvent> for OntologyPage {}

impl Focusable for OntologyPage {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for OntologyPage {
    fn tab_title(&self, _cx: &App) -> SharedString {
        match &self.focus {
            Some(focus) => format!("Ontology · {focus}").into(),
            None => "Ontology".into(),
        }
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::GitGraph)
    }

    fn dedupe_key(&self, cx: &App) -> Option<SharedString> {
        Some(ontology_key(&self.session.read(cx).graph_id))
    }

    fn opened_by(&self, action: &dyn gpui::Action, _cx: &App) -> bool {
        action.as_any().is::<OpenOntology>()
    }

    /// The page comes back after a restart with the type that was in focus.
    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: ONTOLOGY_KIND.into(),
            data: serde_json::json!({ "focus": self.focus }),
        })
    }
}

/// The saved page's kind, and the type in focus it was saved with.
pub(crate) const ONTOLOGY_KIND: &str = "ontology";

pub(crate) fn saved_focus(data: &serde_json::Value) -> Option<String> {
    data.get("focus")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

pub(crate) fn ontology_key(graph_id: &str) -> SharedString {
    format!("ontology:{graph_id}").into()
}

impl Render for OntologyPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let schema = self.session.read(cx).schema.clone();
        // The scrolling area's width at the last frame, once there was one.
        let laid = f32::from(self.scroll.bounds().size.width);
        if laid > 0. {
            self.width.set(laid);
        }
        let view = self.view.clone();
        // A type in focus that the schema no longer has shows the domains.
        let focus = self
            .focus
            .as_ref()
            .and_then(|name| view.as_ref()?.index.get(name).copied());
        let body = match (&schema, &view) {
            (graph_store::Loadable::Failed(message), _) => {
                ds::error_state("The schema could not be read", message.clone()).into_any_element()
            }
            (_, None) => ds::loading_state("Reading the schema…").into_any_element(),
            (_, Some(view)) if view.schema.nodes.is_empty() => {
                ds::empty_state(IconName::GitGraph, "No node types").into_any_element()
            }
            (_, Some(view)) => match (focus, &view.map) {
                (Some(focus), _) => self.render_focus(view, focus, cx),
                (None, Some(map)) => self.render_map(view, map, cx),
                (None, None) => self.render_domains(view, window, cx),
            },
        };
        v_flex()
            .id("ontology")
            .key_context("Ontology")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(ds::surface::canvas(cx))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.focus.is_some() {
                    this.go_back(cx);
                    cx.stop_propagation();
                }
            }))
            .child(self.render_header(view.as_ref(), cx))
            .child(
                div()
                    .id("ontology-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(div().px(px(space::PAGE)).pb(px(space::PAGE)).child(body)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = r#"
node Person @description("Someone") { slug: String @key }
node Task { slug: String @key }
node Project { slug: String @key }
edge AssignedTo: Task -> Person
edge HasTask: Project -> Task
edge BlockedBy: Task -> Task
edge Owns: Person -> Project
"#;

    fn view() -> View {
        View::new(Arc::new(SchemaModel::parse(SCHEMA).unwrap()))
    }

    /// The focus view lists what reaches a type on one side and what it reaches on the other,
    /// a relationship to itself once, among those it reaches.
    #[test]
    fn a_type_in_focus_shows_each_relationship_once_on_its_side() {
        let view = view();
        let task = view.index["Task"];
        let names = |entries: Vec<Entry>| -> Vec<String> {
            entries
                .into_iter()
                .filter_map(|entry| match entry {
                    Entry::Line { edge, other } => Some(format!(
                        "{} {}",
                        view.schema.edges[edge].name,
                        view.name(other)
                    )),
                    Entry::Heading(_) => None,
                })
                .collect()
        };
        assert_eq!(names(view.side(task, Side::In)), vec!["HasTask Project"]);
        assert_eq!(
            names(view.side(task, Side::Out)),
            vec!["AssignedTo Person", "BlockedBy Task"]
        );
        // A small schema is one domain, drawn whole as a map, and its sides need no headings.
        assert!(view.map.is_some());
        assert_eq!(view.domains.groups.len(), 1);
        assert_eq!(view.title(0), "Types");
        assert!(
            view.side(task, Side::Out)
                .iter()
                .all(|e| matches!(e, Entry::Line { .. }))
        );
    }

    /// Past twelve types the page shows domains, not a map: a map of more is the web of lines
    /// it replaces.
    #[test]
    fn a_larger_schema_shows_domains() {
        let mut source = String::new();
        for t in 0..13 {
            source += &format!("node T{t} {{ slug: String @key }}\n");
        }
        for t in 1..13 {
            source += &format!("edge L{t}: T0 -> T{t}\n");
        }
        let view = View::new(Arc::new(SchemaModel::parse(&source).unwrap()));
        assert!(view.map.is_none());
        assert!(!view.domains.groups.is_empty());
    }
}
