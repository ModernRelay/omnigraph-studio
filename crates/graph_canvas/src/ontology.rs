//! The ontology page (spec 006, "Ontology page"): a schema's types grouped into domains, the
//! types related to most of the others set apart ([`crate::domains`]), and any one type in
//! focus: what the schema says it is, its properties, and its relationships, each named and
//! leading to the type at its other end. It draws no web of every relationship: a schema of a
//! hundred types has hundreds, and no layout makes that readable.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton, Pixels, Point,
    Render, ScrollHandle, SharedString, Subscription, Window, canvas, point, px, relative,
};
use gq_language::{Property, SchemaModel};
use graph_store::{EntityKind, GraphSession, SessionEvent};
use studio_actions::graph::{OpenSource, OpenType};
use studio_actions::ontology::{Back, Open, SelectNext, SelectPrevious, ShowDomains};
use studio_actions::{ViewKind, ViewLink};
use studio_ui::ds::Tipped;
use studio_ui::ds::{self, size, space};
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;

use crate::domains::{self, Domains};
use crate::paint::{self, RING, Tone};
use crate::{GraphCanvas, TypeEvent};

/// The focus view's columns: the related types either side, as wide as the page allows
/// between the two bounds, the lines between, and the type in focus.
const SIDE_MIN: f32 = 240.;
const SIDE_MAX: f32 = 440.;
const GUTTER_WIDTH: f32 = 56.;
const CENTRE_WIDTH: f32 = 360.;
/// Narrower than its three columns, the focus view stacks: the type, then each side, no wider
/// than this.
const STACKED_MAX: f32 = 640.;
/// Lines of what the schema says a type is, before the rest waits behind More.
const ABOUT_LINES: usize = 5;
/// Where the lines meet the type in focus: the middle of its card's header.
const CENTRE_HEADER: f32 = 44.;
/// A side groups its lines by domain once it holds more than this many.
const GROUP_ABOVE: usize = 8;
/// Up to this many types, the canvas opens with every type expanded: the schema whole.
const MAP_TYPES: usize = domains::SMALL_SCHEMA;
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
    /// What the pointer, or the arrow keys, are on.
    pointed: Option<Pointed>,
    scroll: ScrollHandle,
    /// The page's width as last laid out, for how many columns of domains it holds; a change
    /// draws the page again.
    width: Rc<Cell<f32>>,
    /// The sides of the type in focus, made once for it rather than on every redraw.
    sides: Option<Rc<Sides>>,
    /// The type whose whole description is shown, past its first lines.
    about_open: Option<String>,
    /// The schema's types on the canvas, walked from one (spec 006, "Ontology on the
    /// canvas"); made with the first schema read, and following the session after.
    canvas: Option<Entity<GraphCanvas>>,
    _subscriptions: Vec<Subscription>,
}

/// The two sides of the focus view for one type.
struct Sides {
    focus: usize,
    incoming: Vec<Entry>,
    outgoing: Vec<Entry>,
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

/// The schema as the page shows it: its types grouped, and each found by name.
struct View {
    schema: Arc<SchemaModel>,
    domains: Domains,
    index: HashMap<String, usize>,
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
        Self {
            domains: domains::domains(schema.nodes.len(), &links),
            schema,
            index,
        }
    }

    /// The type the canvas starts from: the one related to most others, the shared types
    /// aside (they are related to everything), by name among equals.
    fn starting_type(&self) -> Option<usize> {
        (0..self.schema.nodes.len())
            .filter(|ty| self.domains.group_of[*ty].is_some() || self.domains.shared.is_empty())
            .max_by(|&a, &b| {
                self.domains.degree[a]
                    .cmp(&self.domains.degree[b])
                    .then_with(|| self.name(b).cmp(self.name(a)))
            })
            .or_else(|| (!self.schema.nodes.is_empty()).then_some(0))
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
            width: Rc::default(),
            sides: None,
            about_open: None,
            canvas: None,
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
            self.sides = None;
            if self.canvas.is_none()
                && let Some(view) = self.view.clone()
                && let Some(root) = view.starting_type()
            {
                let session = self.session.clone();
                let whole = view.schema.nodes.len() <= MAP_TYPES;
                let canvas =
                    cx.new(|cx| GraphCanvas::for_schema(session, view.name(root), whole, cx));
                self._subscriptions.push(cx.subscribe(
                    &canvas,
                    |this, _, event: &TypeEvent, cx| match event {
                        TypeEvent::Details(name) => this.focus_on(name, cx),
                    },
                ));
                self.canvas = Some(canvas);
            }
            // A type the schema no longer has (dropped, or renamed by another writer) leaves
            // the focus and the way back, so the tab and the header say what is shown.
            if let Some(view) = &self.view {
                self.back.retain(|name| view.index.contains_key(name));
                if self
                    .focus
                    .as_ref()
                    .is_some_and(|name| !view.index.contains_key(name))
                {
                    self.focus = self.back.pop();
                    cx.emit(ItemEvent::UpdateTab);
                }
            }
        }
        cx.notify();
    }

    /// The sides of the type in focus, made when the focus or the schema changes.
    fn sides(&mut self, view: &View, focus: usize) -> Rc<Sides> {
        match &self.sides {
            Some(sides) if sides.focus == focus => sides.clone(),
            _ => {
                let sides = Rc::new(Sides {
                    focus,
                    incoming: view.side(focus, Side::In),
                    outgoing: view.side(focus, Side::Out),
                });
                self.sides = Some(sides.clone());
                sides
            }
        }
    }

    /// What the arrow keys step through, in order: the types as the page lists them, or the
    /// lines of the type in focus, the side reaching it first.
    fn pointable(&mut self) -> Vec<Pointed> {
        let Some(view) = self.view.clone() else {
            return Vec::new();
        };
        match self
            .focus
            .as_ref()
            .and_then(|name| view.index.get(name).copied())
        {
            Some(focus) => {
                let sides = self.sides(&view, focus);
                let lines = |side: Side, entries: &[Entry]| -> Vec<Pointed> {
                    entries
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| matches!(entry, Entry::Line { .. }))
                        .map(|(at, _)| Pointed::Line(side, at))
                        .collect()
                };
                let mut all = lines(Side::In, &sides.incoming);
                all.extend(lines(Side::Out, &sides.outgoing));
                all
            }
            None => view
                .domains
                .shared
                .iter()
                .chain(view.domains.groups.iter().flatten())
                .map(|&ty| Pointed::Type(ty))
                .collect(),
        }
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let pointable = self.pointable();
        if pointable.is_empty() {
            return;
        }
        let at = self
            .pointed
            .and_then(|pointed| pointable.iter().position(|p| *p == pointed));
        let next = match at {
            Some(at) => at.saturating_add_signed(by).min(pointable.len() - 1),
            None if by < 0 => pointable.len() - 1,
            None => 0,
        };
        self.point(Some(pointable[next]), cx);
    }

    /// Brings the type pointed at into focus: a type, or the type at a line's other end.
    fn open_pointed(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.view.clone() else {
            return;
        };
        let target = match self.pointed {
            Some(Pointed::Type(ty)) => Some(ty),
            Some(Pointed::Line(side, at)) => self.sides.as_ref().and_then(|sides| {
                let entries = match side {
                    Side::In => &sides.incoming,
                    Side::Out => &sides.outgoing,
                };
                match entries.get(at)? {
                    Entry::Line { other, .. } => Some(*other),
                    Entry::Heading(_) => None,
                }
            }),
            None => None,
        };
        if let Some(ty) = target {
            let name = view.name(ty).to_owned();
            self.focus_on(&name, cx);
        }
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
        // Back on the canvas, the type that was in focus is selected where it stands.
        if let (Some(canvas), Some(focus)) = (self.canvas.clone(), self.focus.clone()) {
            canvas.update(cx, |canvas, cx| canvas.show_type(&focus, cx));
        }
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

    /// The pointer entered (`hovered`) or left `at`'s row. Leaving lets go only of the row
    /// pointed at: moving down from one row to the next, GPUI tells the row entered before the
    /// row left, which must not take the new row's highlight away.
    fn point_at(&mut self, at: Pointed, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            self.point(Some(at), cx);
        } else if self.pointed == Some(at) {
            self.point(None, cx);
        }
    }

    /// The width the page lays its views out in, as it was last laid out (a change draws the
    /// page again); before that, the window's less the sidebar.
    fn page_width(&self, window: &Window) -> f32 {
        match self.width.get() {
            0. => f32::from(window.viewport_size().width) - size::SIDEBAR_WIDTH - 2. * space::PAGE,
            width => width,
        }
    }

    /// Reads the page's width as it is laid out; a new one draws the page again once this frame
    /// is done.
    fn measure(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let width = self.width.clone();
        let page = cx.entity_id();
        canvas(
            move |bounds, _, cx| {
                let now = f32::from(bounds.size.width);
                if (width.get() - now).abs() > 0.5 {
                    width.set(now);
                    cx.defer(move |cx| cx.notify(page));
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .w_full()
        .h(px(0.))
    }

    fn count(&self, type_name: &str, cx: &App) -> Option<String> {
        self.session
            .read(cx)
            .count_of(EntityKind::Node, type_name)
            .map(studio_ui::group_count)
    }

    // ---- one type in focus ----

    /// The type in focus between its two sides, the lines drawn from each row to it; on a page
    /// too narrow for the three columns, the type above each side in turn, without lines, on the
    /// page's edge as its header is.
    fn render_focus(
        &self,
        view: &Arc<View>,
        sides: &Sides,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (focus, incoming, outgoing) = (sides.focus, &sides.incoming, &sides.outgoing);
        let width = self.page_width(window);
        if width < 2. * SIDE_MIN + 2. * GUTTER_WIDTH + CENTRE_WIDTH {
            let column = width.min(STACKED_MAX);
            return h_flex()
                .w_full()
                .child(
                    v_flex()
                        .w(px(column))
                        .gap(px(space::XL))
                        .child(self.render_centre(view, focus, column, cx))
                        .child(self.render_side(view, Side::In, incoming, true, cx))
                        .child(self.render_side(view, Side::Out, outgoing, true, cx)),
                )
                .into_any_element();
        }
        h_flex()
            .items_start()
            .justify_center()
            .w_full()
            .child(self.render_side(view, Side::In, incoming, false, cx))
            .child(self.render_gutter(Side::In, incoming, cx))
            .child(self.render_centre(view, focus, CENTRE_WIDTH, cx))
            .child(self.render_gutter(Side::Out, outgoing, cx))
            .child(self.render_side(view, Side::Out, outgoing, false, cx))
            .into_any_element()
    }

    /// The type in focus, `width` wide: its name, its node count, what the schema says it is
    /// (the first lines, the rest behind More), its properties, and what opens it elsewhere.
    fn render_centre(
        &self,
        view: &Arc<View>,
        focus: usize,
        width: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let node = &view.schema.nodes[focus];
        let name = node.name.clone();
        let count = self.count(&name, cx);
        let about = node.about.clone().map(|about| {
            // About how many characters a line holds at the body size, less some, so More is
            // offered whenever the text may run past its lines.
            let per_line = (width - 2. * space::LG) / 5.6;
            let long = about.chars().count() as f32 > 0.9 * per_line * ABOUT_LINES as f32;
            let open = self.about_open.as_deref() == Some(name.as_str());
            let target = name.clone();
            v_flex()
                .gap(px(space::XS))
                .items_start()
                .child(if long && !open {
                    ds::text::muted(about).line_clamp(ABOUT_LINES)
                } else {
                    ds::text::muted(about)
                })
                .when(long, |about| {
                    // Its label on the text's edge: the button's own padding sits outside it.
                    about.child(div().ml(px(-space::SM)).child(
                        ds::row_button("about-more", if open { "Less" } else { "More" }).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.about_open = if open { None } else { Some(target.clone()) };
                                cx.notify();
                            }),
                        ),
                    ))
                })
        });
        let (table, source) = (name.clone(), name.clone());
        ds::card(cx)
            .w(px(width))
            .flex_none()
            .child(
                ds::card_header(cx)
                    .h(px(CENTRE_HEADER))
                    .child(ds::type_dot(&name))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(ds::text::title(name.clone()).single_line().truncate()),
                    )
                    .children(count.map(|c| ds::text::meta(format!("{c} nodes")))),
            )
            .child(
                ds::card_body().gap(px(space::MD)).children(about).when(
                    !node.properties.is_empty(),
                    |body| {
                        body.child(v_flex().children(node.properties.iter().map(render_property)))
                    },
                ),
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
                                    filters: None,
                                }),
                                cx,
                            )
                        },
                    ))
                    .child(
                        ds::row_button("in-source", "Source").on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(OpenSource {
                                    type_name: Some(source.clone()),
                                    preview: false,
                                }),
                                cx,
                            )
                        }),
                    ),
            )
    }

    /// One side's rows: domain headings and lines, each line its relationship and the type at
    /// its other end, which a click brings into focus. Beside the type in focus, the side that
    /// it reaches is mirrored, its rows toward the type; `stacked`, both read the same way.
    fn render_side(
        &self,
        view: &Arc<View>,
        side: Side,
        entries: &[Entry],
        stacked: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = match side {
            Side::In => "Reach it",
            Side::Out => "It reaches",
        };
        let mirrored = side == Side::Out && !stacked;
        let lines = entries
            .iter()
            .filter(|e| matches!(e, Entry::Line { .. }))
            .count();
        v_flex()
            .map(|column| {
                if stacked {
                    column.w_full()
                } else {
                    column.flex_1().min_w(px(SIDE_MIN)).max_w(px(SIDE_MAX))
                }
            })
            // The side's title over its domains' headings: in ink, with how many lines it has.
            .child(
                h_flex()
                    .h(px(size::ROW))
                    .px(px(space::SM))
                    .gap(px(space::SM))
                    .when(mirrored, |row| row.justify_end())
                    .child(ds::text::body(title))
                    .child(ds::text::meta(match lines {
                        0 => "none".to_owned(),
                        n => n.to_string(),
                    })),
            )
            .children(entries.iter().enumerate().map(|(at, entry)| {
                match entry {
                    Entry::Heading(title) => h_flex()
                        .h(px(size::ROW))
                        .px(px(space::SM))
                        .items_end()
                        .pb(px(space::XS))
                        .when(mirrored, |row| row.justify_end())
                        .child(ds::text::meta(title.clone()).single_line().truncate())
                        .into_any_element(),
                    Entry::Line { edge, other } => {
                        let edge = &view.schema.edges[*edge];
                        let other_name = view.name(*other).to_owned();
                        let pointed = self.pointed == Some(Pointed::Line(side, at));
                        // The type takes up to half the row and the relationship the rest; past
                        // that, each is cut and the tooltip has both whole.
                        let relationship = div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .when(!mirrored, |slot| slot.justify_end())
                            .child(ds::text::meta(edge.name.clone()).single_line().truncate());
                        let ty = h_flex()
                            .flex_none()
                            .max_w(relative(0.5))
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
                            .map(|row| {
                                if mirrored {
                                    row.child(relationship).child(ty)
                                } else {
                                    row.child(ty).child(relationship)
                                }
                            })
                            .tip(studio_ui::ds::tooltip(tooltip))
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                this.point_at(Pointed::Line(side, at), *hovered, cx)
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
            format!(
                "{} types · {} relationships",
                view.schema.nodes.len(),
                view.schema.edges.len()
            )
        });
        // The toolbar every surface has, so the view strip stands where Source's does.
        ds::toolbar(cx)
            .children(studio_workspace::view_strip(Item::views(self, cx), cx))
            .map(|bar| match (&focus, view) {
                (Some(focus), Some(view)) => {
                    let place = view.index.get(focus).map(|ty| view.place(*ty));
                    bar.child(
                        ds::icon_button("ontology-back", IconName::ArrowLeft)
                            .tip(studio_ui::ds::tooltip("Back"))
                            .on_click(cx.listener(|this, _, _, cx| this.go_back(cx))),
                    )
                    .child(
                        ds::button("ontology-domains", "Every type")
                            .on_click(cx.listener(|this, _, _, cx| this.show_domains(cx))),
                    )
                    // A long domain title gives way to the type's name.
                    .children(place.map(|place| {
                        div().min_w_0().child(
                            ds::text::muted(format!("› {place} ›"))
                                .single_line()
                                .truncate(),
                        )
                    }))
                    .child(div().flex_none().child(ds::text::title(focus.clone())))
                }
                // The view strip names the map, and the canvas the type it is walked from.
                _ => bar.children(summary.map(ds::text::meta)),
            })
    }
}

/// A property of the type in focus: its name and type, and an enumeration's values below,
/// as many lines as they need.
fn render_property(property: &Property) -> impl IntoElement {
    let kind = match property.enum_values {
        Some(_) => Property {
            base_type: "enum".into(),
            enum_values: None,
            ..property.clone()
        }
        .display_type(),
        None => property.display_type(),
    };
    v_flex()
        .child(
            h_flex()
                .h(px(size::SIDEBAR_ROW))
                .gap(px(space::MD))
                .child(
                    div().flex_none().max_w(relative(0.6)).child(
                        ds::text::body(property.name.clone())
                            .single_line()
                            .truncate(),
                    ),
                )
                .child(
                    div().flex_1().min_w_0().flex().justify_end().child(
                        ds::text::meta(if property.is_key {
                            format!("{kind} · key")
                        } else {
                            kind
                        })
                        .single_line()
                        .truncate(),
                    ),
                ),
        )
        .children(property.enum_values.as_ref().map(|values| {
            div()
                .pb(px(space::SM))
                .child(ds::text::meta(values.join(", ")))
        }))
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
            Some(focus) => format!("Map · {focus}").into(),
            None => "Map".into(),
        }
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::GitGraph)
    }

    fn dedupe_key(&self, cx: &App) -> Option<SharedString> {
        Some(ontology_key(&self.session.read(cx).graph_id))
    }

    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        Some(studio_actions::Thing::Schema)
    }

    fn views(&self, _cx: &App) -> Vec<ViewLink> {
        vec![
            ViewLink::current(ViewKind::Map),
            ViewLink::to(ViewKind::Source),
        ]
    }

    fn view_action(&self, kind: ViewKind, _cx: &App) -> Option<Box<dyn gpui::Action>> {
        (kind == ViewKind::Source).then(|| {
            Box::new(OpenSource {
                type_name: self.focus.clone(),
                preview: true,
            }) as Box<dyn gpui::Action>
        })
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
            (_, Some(view)) => match focus {
                Some(focus) => {
                    let sides = self.sides(view, focus);
                    self.render_focus(view, &sides, window, cx)
                }
                // The schema canvas, drawn in place of the body.
                None => div().into_any_element(),
            },
        };
        // A large schema with no type in focus on the canvas: the canvas fills the page and
        // scrolls itself.
        let canvas = match (&view, focus) {
            (Some(view), None) if !view.schema.nodes.is_empty() => self.canvas.clone(),
            _ => None,
        };
        v_flex()
            .id("ontology")
            .key_context("Ontology")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(ds::surface::canvas(cx))
            // A press anywhere on the page gives it the keys.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus_handle, cx)),
            )
            .on_action(cx.listener(|this, _: &Back, _, cx| {
                if this.focus.is_some() {
                    this.go_back(cx);
                } else if this.pointed.is_some() {
                    this.point(None, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &ShowDomains, _, cx| this.show_domains(cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &Open, _, cx| this.open_pointed(cx)))
            .child(self.render_header(view.as_ref(), cx))
            .when_some(canvas.clone(), |page, canvas| {
                page.child(div().flex_1().min_h_0().child(canvas))
            })
            .when(canvas.is_none(), |page| {
                page.child(
                    div()
                        .id("ontology-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        // Room above the first row: the scrolling body clips what is drawn outside
                        // it, and a raised card there (the type in focus, a domain) would lose the
                        // top of its ring and shadow.
                        .child(
                            div()
                                .px(px(space::PAGE))
                                .pt(px(space::MD))
                                .pb(px(space::PAGE))
                                .child(div().relative().child(self.measure(cx)).child(body)),
                        ),
                )
            })
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
        // A small schema is one domain, and its sides need no headings.
        assert_eq!(view.domains.groups.len(), 1);
        assert_eq!(view.title(0), "Types");
        assert!(
            view.side(task, Side::Out)
                .iter()
                .all(|e| matches!(e, Entry::Line { .. }))
        );
    }

    /// Past twelve types the schema is grouped into domains, which a type in focus groups
    /// its lines by.
    #[test]
    fn a_larger_schema_has_domains() {
        let mut source = String::new();
        for t in 0..13 {
            source += &format!("node T{t} {{ slug: String @key }}\n");
        }
        for t in 1..13 {
            source += &format!("edge L{t}: T0 -> T{t}\n");
        }
        let view = View::new(Arc::new(SchemaModel::parse(&source).unwrap()));
        assert!(!view.domains.groups.is_empty());
    }

    /// ↓ points at the types in the order the page lists them, ↵ brings the one pointed at into
    /// focus, and then ↓ walks its lines, the side reaching it first; Esc goes back.
    #[gpui::test]
    async fn the_keys_walk_the_types_and_their_lines(cx: &mut gpui::TestAppContext) {
        cx.update(studio_theme::init_test);
        let (session, _served) =
            graph_store::test_server::followed_session(SCHEMA, |_| serde_json::json!([]), cx);
        cx.run_until_parked();
        let page = cx.new(|cx| OntologyPage::new(session, cx));
        page.update(cx, |page, cx| {
            page.step(1, cx);
            page.step(1, cx);
            page.open_pointed(cx);
        });
        let (second, project) = page.read_with(cx, |page, _| {
            let view = page.view.clone().unwrap();
            (
                view.name(view.domains.groups[0][1]).to_owned(),
                view.index["Project"],
            )
        });
        page.read_with(cx, |page, _| {
            assert_eq!(page.focus.as_deref(), Some(second.as_str()))
        });
        // Task in focus: its first line is the one reaching it, from Project.
        page.update(cx, |page, cx| {
            page.focus_on("Task", cx);
            page.step(1, cx);
            page.open_pointed(cx);
        });
        page.read_with(cx, |page, _| {
            assert_eq!(page.focus.as_deref(), Some("Project"));
            assert_eq!(page.back.last().map(String::as_str), Some("Task"));
            let view = page.view.clone().unwrap();
            assert_eq!(view.index["Project"], project);
        });
        page.update(cx, |page, cx| page.go_back(cx));
        page.read_with(cx, |page, _| {
            assert_eq!(page.focus.as_deref(), Some("Task"))
        });
    }
}
