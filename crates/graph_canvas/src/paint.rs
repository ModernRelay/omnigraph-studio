//! How the canvas looks (spec 006): a faint dot grid; hairline grey lines; every node,
//! type, hub and "+K more" a chip, a white pill with a hairline and a soft contact shadow, with
//! the type's letter mark and a muted count. Pointing at a chip lights its lines and fades the
//! rest; the selected chip carries a warm ring. Colour is kept to the small type marks.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use gpui::{
    App, BorderStyle, Bounds, BoxShadow, Corners, Font, FontWeight, Hsla, PathBuilder, Pixels,
    Point, ShapedLine, SharedString, TextAlign, TextRun, Window, point, px, quad, size,
};
use studio_ui::type_color;

use crate::model::{Scene, SceneKind, SceneNode};

pub(crate) struct Palette {
    pub text: Hsla,
    pub muted: Hsla,
    /// The chip fill.
    pub card: Hsla,
    /// The chip hairline.
    pub border: Hsla,
    /// A chip's shadow, the design system's floating one.
    pub shadow: Vec<BoxShadow>,
    /// A type mark's letter on its tint.
    pub on_tint: Hsla,
    /// Where you are: the selected or marked chip's ring, and its halo.
    pub here: Hsla,
    pub here_halo: Hsla,
    /// A pointed chip's hairline.
    pub pointed: Hsla,
    /// The dot grid's dots.
    pub grid_dot: Hsla,
    /// A relationship label's fill, and a closed hub's dot from afar.
    pub label_fill: Hsla,
    pub hub_dot: Hsla,
    pub font: Font,
}

/// The dot grid's dots, across.
const GRID_DOT: f32 = 1.2;

/// A chip's text size at zoom 1, and a hub's or "+K more"'s. The canvas's text is world
/// geometry, not interface text: chips are measured and laid out at these sizes and the view's
/// zoom scales them, so they stay fixed rather than following the interface font size. At zoom
/// 1 they are the design system's body (13 px) and small (11 px) sizes at its default.
const CHIP_TEXT: f32 = 13.;
const HUB_TEXT: f32 = 12.5;
/// A relationship label's text size at zoom 1.
const LABEL_TEXT: f32 = 11.;
/// A pill's inner spacing at zoom 1: between pieces, at each end, and its mark's size.
const PILL_GAP: f32 = studio_ui::ds::space::SM;
const PILL_PAD_X: f32 = studio_ui::ds::space::CONTROL_INSET;
const PILL_MARK: f32 = studio_ui::ds::size::ROW_ICON;
/// The characters a node's chip shows of its name before it cuts it with "…".
pub(crate) const CHIP_LABEL_CHARS: usize = 32;
/// The widest an opened chip grows at zoom 1 before its name wraps: an email or an id of 40
/// characters stays on one line.
const OPEN_CHIP_MAX_WIDTH: f32 = 320.;
/// Below this text size a node is drawn as its mark alone, unless it is selected or pointed at.
const MARK_ONLY_BELOW: f32 = 8.5;
const TYPE_MARK_ONLY_BELOW: f32 = 6.;
/// A closed hub's dot from afar: its radius, in screen pixels.
const HUB_DOT: f32 = 4.;
/// The dot grid's spacing, in screen pixels.
const DOT_STEP: f32 = 22.;
/// Up to this many relationships, the schema labels every line.
pub(crate) const ALWAYS_LABEL: usize = 16;
/// Segments per path: GPUI caps one path near 65k vertices. A batch that still needs more is
/// painted in halves (`stroke_lines`), never dropped.
const SEGMENTS_PER_PATH: usize = 2_000;
/// A dotted line's pattern in screen pixels: a dash, then a gap.
const DASH: f32 = 2.;
const GAP: f32 = 3.;
/// Dashes per path: each is a short straight stroke of a few vertices.
const DASHES_PER_PATH: usize = 6_000;
/// The most dashes one straight piece of a dotted line is cut into (half a million pixels); a
/// longer piece is drawn solid.
const DASHES_PER_LINE: usize = 100_000;

/// How a line runs between its two ends.
#[derive(Clone, Copy)]
enum Curve {
    Straight,
    /// Bowed through one control point: one of several relationships between two types.
    Bowed(Point<Pixels>),
    /// Leaving and arriving level: an exploration's branch, as a mind map draws it.
    Level(Point<Pixels>, Point<Pixels>),
}

/// One line on screen.
type Segment = (Point<Pixels>, Point<Pixels>, Curve);

/// How far apart, at zoom 1, the lines of several relationships between the same two types
/// run at their middle, and how much taller each further loop on one type is.
const FAN: f32 = 30.;
const LOOP_STEP: f32 = 26.;

/// Where one relationship runs on screen: its line (bowed when several join the same two types,
/// so each has its own line and label; a loop when it joins a type to itself) and where its
/// label sits.
struct Route {
    curve: Curve,
    label_at: Point<Pixels>,
    /// For a loop: which of the type's loops this is, from the inside out.
    loop_rank: usize,
}

fn routes(scene: &Scene, screen: &[Point<Pixels>], scale: f32, schema: bool) -> Vec<Route> {
    // Painted every frame: only the ontology bows the lines two types share, so only it counts
    // them, and a rank is kept only where one is drawn.
    let mut pair_count: HashMap<(usize, usize), usize> = HashMap::new();
    if schema {
        for edge in &scene.edges {
            *pair_count
                .entry((edge.from.min(edge.to), edge.from.max(edge.to)))
                .or_default() += 1;
        }
    }
    let mut seen: HashMap<(usize, usize), usize> = HashMap::new();
    let mut rank_of = |key: (usize, usize)| {
        let seen = seen.entry(key).or_default();
        *seen += 1;
        *seen - 1
    };
    scene
        .edges
        .iter()
        .map(|edge| {
            let key = (edge.from.min(edge.to), edge.from.max(edge.to));
            let (a, b) = (screen[key.0], screen[key.1]);
            if edge.from == edge.to {
                let rank = rank_of(key);
                let height = (60. + LOOP_STEP * rank as f32) * scale;
                return Route {
                    curve: Curve::Straight,
                    label_at: point(a.x, a.y - px(height - 10. * scale)),
                    loop_rank: rank,
                };
            }
            let mid = point((a.x + b.x) / 2., (a.y + b.y) / 2.);
            // An exploration's branch leaves its node level and arrives level, so a column of
            // neighbours reads as one fan.
            if !schema {
                let (from, to) = (screen[edge.from], screen[edge.to]);
                let half = (to.x - from.x) / 2.;
                return Route {
                    curve: Curve::Level(point(from.x + half, from.y), point(to.x - half, to.y)),
                    label_at: mid,
                    loop_rank: 0,
                };
            }
            let rank = rank_of(key);
            let count = pair_count[&key];
            if count < 2 {
                return Route {
                    curve: Curve::Straight,
                    label_at: mid,
                    loop_rank: 0,
                };
            }
            // Fan around the straight line, the same side for the same pair whichever way a
            // relationship points.
            let (dx, dy) = (f32::from(b.x - a.x), f32::from(b.y - a.y));
            let length = (dx * dx + dy * dy).sqrt().max(1.);
            let normal = (-dy / length, dx / length);
            let offset = (rank as f32 - (count - 1) as f32 / 2.) * FAN * scale;
            Route {
                // A quadratic curve passes halfway to its control point at its middle.
                curve: Curve::Bowed(point(
                    mid.x + px(normal.0 * offset * 2.),
                    mid.y + px(normal.1 * offset * 2.),
                )),
                label_at: point(mid.x + px(normal.0 * offset), mid.y + px(normal.1 * offset)),
                loop_rank: 0,
            }
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Tone {
    /// Line opacity, in thousandths, so tones batch.
    alpha: u16,
    dotted: bool,
}

/// What a piece of a chip's text is, which sets its colour and weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Name,
    RootName,
    Count,
    OpenHub,
    Muted,
}

impl Role {
    pub(crate) fn weight(self) -> FontWeight {
        match self {
            Role::RootName => studio_ui::ds::text::weight::HEADING,
            _ => studio_ui::ds::text::weight::BODY,
        }
    }
}

/// What a chip says, before colour: the same for painting it and for measuring it, so the
/// layout makes room for the chip the painter draws. Made once per layout, not per frame.
pub(crate) struct ChipText {
    pub mark: bool,
    /// The letter of its mark, drawn from afar too.
    pub initial: SharedString,
    pub pieces: Vec<(SharedString, Role)>,
    /// The text size at zoom 1.
    pub size: f32,
}

pub(crate) fn chip_text(node: &SceneNode) -> ChipText {
    // A label is drawn on one line: a control character shows as its picture.
    let label = ui::utils::replace_control_characters(&node.label);
    let initial = initial(&node.label, &node.color_key);
    match &node.kind {
        SceneKind::Type { count } => ChipText {
            mark: true,
            initial,
            pieces: std::iter::once((label.into_owned().into(), Role::RootName))
                .chain(count.map(|count| (studio_ui::group_count(count).into(), Role::Count)))
                .collect(),
            size: CHIP_TEXT,
        },
        SceneKind::Node { root, .. } => ChipText {
            mark: true,
            initial,
            pieces: vec![(
                studio_ui::text::cut(&label, CHIP_LABEL_CHARS).into(),
                if *root { Role::RootName } else { Role::Name },
            )],
            size: CHIP_TEXT,
        },
        SceneKind::Hub { open, .. } => {
            let (name, count) = match label.rsplit_once(" · ") {
                Some((name, count)) => (name.to_owned(), Some(count.to_owned())),
                None => (label.clone().into_owned(), None),
            };
            ChipText {
                mark: false,
                initial,
                pieces: std::iter::once((
                    name.into(),
                    if *open { Role::OpenHub } else { Role::Muted },
                ))
                .chain(count.map(|count| (count.into(), Role::Count)))
                .collect(),
                size: HUB_TEXT,
            }
        }
        SceneKind::More { .. } => ChipText {
            mark: false,
            initial,
            pieces: vec![(label.into_owned().into(), Role::Muted)],
            size: HUB_TEXT,
        },
    }
}

/// A node's whole name when its chip cuts it: what the chip shows opened (spec 006, "Opened
/// chips").
pub(crate) fn whole_label(node: &SceneNode) -> Option<String> {
    if !matches!(node.kind, SceneKind::Node { .. })
        || studio_ui::text::cut(&node.label, CHIP_LABEL_CHARS) == node.label
    {
        return None;
    }
    Some(ui::utils::replace_control_characters(&node.label).into_owned())
}

/// A chip's width at zoom 1, as [`paint_pill`] draws it, given the width of a text at a size
/// and weight.
pub(crate) fn chip_width(text: &ChipText, measure: impl Fn(&str, f32, FontWeight) -> f32) -> f32 {
    let widths: f32 = text
        .pieces
        .iter()
        .map(|(piece, role)| measure(piece, text.size, role.weight()))
        .sum();
    let gaps = PILL_GAP * text.pieces.len().saturating_sub(1) as f32;
    let mark = if text.mark { PILL_MARK + PILL_GAP } else { 0. };
    PILL_PAD_X * 2. + widths + gaps + mark
}

/// A relationship label's width at zoom 1, as the canvas draws its pill.
pub(crate) fn label_width(label: &str, measure: impl Fn(&str, f32, FontWeight) -> f32) -> f32 {
    chip_width(
        &ChipText {
            mark: false,
            initial: SharedString::default(),
            pieces: vec![(label.to_owned().into(), Role::Muted)],
            size: LABEL_TEXT,
        },
        measure,
    )
}

/// One piece of a chip's text.
struct Piece {
    text: SharedString,
    color: Hsla,
    weight: FontWeight,
}

/// Paints the scene and records each node's chip on screen, for hit testing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint(
    scene: &Scene,
    texts: &[ChipText],
    screen: &[Point<Pixels>],
    bounds: Bounds<Pixels>,
    zoom: f32,
    selected: Option<usize>,
    marked: &HashSet<usize>,
    hovered: Option<usize>,
    opened: Option<usize>,
    palette: &Palette,
    chips: &RefCell<Vec<Bounds<Pixels>>>,
    window: &mut Window,
    cx: &mut App,
) {
    paint_dots(bounds, palette, window);
    if screen.len() != scene.nodes.len() || texts.len() != scene.nodes.len() {
        return;
    }
    // Chips grow with the view up to their size at zoom 1 (13-pixel text), in
    // half-pixel text steps: a label is shaped again only when its size changes, and GPUI keeps
    // a shaped line for one frame, so a smooth zoom would reshape every label on every frame.
    let scale = (zoom.clamp(0.3, 1.) * CHIP_TEXT * 2.).round() / (CHIP_TEXT * 2.);
    let visible = |p: Point<Pixels>, margin: f32| {
        p.x >= bounds.left() - px(margin)
            && p.x <= bounds.right() + px(margin)
            && p.y >= bounds.top() - px(margin)
            && p.y <= bounds.bottom() + px(margin)
    };
    let schema = !scene.nodes.is_empty()
        && scene
            .nodes
            .iter()
            .all(|n| matches!(n.kind, SceneKind::Type { .. }));
    // What pointing at a chip brings out in the ontology: the chip and what its lines reach,
    // the rest fading. The explorer fades nothing under the pointer for now; the pointed chip's
    // own lines still stand out.
    let near: Option<HashSet<usize>> = hovered.filter(|_| schema).map(|h| {
        let mut near = HashSet::from([h]);
        for edge in &scene.edges {
            if edge.from == h {
                near.insert(edge.to);
            }
            if edge.to == h {
                near.insert(edge.from);
            }
        }
        near
    });

    // Lines, batched by tone.
    let routes = routes(scene, screen, scale, schema);
    let far = CHIP_TEXT * scale < MARK_ONLY_BELOW;
    let mut batches: HashMap<Tone, Vec<Segment>> = HashMap::new();
    let mut loops: Vec<(usize, usize, Tone)> = Vec::new();
    for (edge, route) in scene.edges.iter().zip(&routes) {
        let (a, b) = (screen[edge.from], screen[edge.to]);
        // Zoomed in, a line can cross the view with both ends far outside it.
        if !crosses(a, b, bounds, 400.) {
            continue;
        }
        let kinds = (&scene.nodes[edge.from].kind, &scene.nodes[edge.to].kind);
        let hub_link = matches!(kinds.0, SceneKind::Hub { .. } | SceneKind::More { .. })
            || matches!(kinds.1, SceneKind::Hub { .. } | SceneKind::More { .. });
        let member = matches!(
            kinds,
            (SceneKind::Hub { .. }, SceneKind::Node { .. })
                | (SceneKind::Node { .. }, SceneKind::Hub { .. })
        ) && edge_is_member(scene, edge.from, edge.to);
        // A closed hub hangs from its node on a dotted line, and so does "+K more".
        let dotted = matches!(kinds.0, SceneKind::More { .. })
            || matches!(kinds.1, SceneKind::More { .. })
            || matches!(kinds.1, SceneKind::Hub { open: false, .. })
            || matches!(kinds.0, SceneKind::Hub { open: false, .. });
        let base = if schema {
            0.15
        } else if member {
            0.10
        } else if hub_link {
            0.20
        } else {
            0.12
        };
        let touches = |i: Option<usize>| i.is_some_and(|i| edge.from == i || edge.to == i);
        let alpha = match hovered {
            Some(_) if touches(hovered) => {
                if schema {
                    0.40
                } else {
                    0.45
                }
            }
            Some(_) if schema => 0.05,
            _ if !schema && touches(selected) => 0.25,
            _ => base,
        };
        let tone = Tone {
            alpha: (alpha * 1000.) as u16,
            dotted,
        };
        if edge.from == edge.to {
            loops.push((edge.from, route.loop_rank, tone));
        } else {
            // From afar an exploration's lines are straight: at mark size the fan of curves
            // reads the same, and a straight line costs two triangles, a curve dozens.
            let curve = match route.curve {
                Curve::Level(..) if far => Curve::Straight,
                curve => curve,
            };
            batches.entry(tone).or_default().push((a, b, curve));
        }
    }
    window.paint_layer(bounds, |window| {
        for (tone, segments) in &batches {
            let color = palette.text.opacity(f32::from(tone.alpha) / 1000.);
            if tone.dotted {
                // Dotted lines are cut into dashes here, by walking each line once. GPUI's
                // `dash_array` measures the batch's whole path on every frame, and its dashes
                // overflow a path's vertex cap, which would drop the batch.
                let mut pieces = Vec::new();
                for (from, to, curve) in segments {
                    dashes(&flatten(*from, *to, *curve), &mut pieces);
                }
                for chunk in pieces.chunks(DASHES_PER_PATH) {
                    stroke_lines(window, chunk, color);
                }
            } else {
                for chunk in segments.chunks(SEGMENTS_PER_PATH) {
                    stroke_lines(window, chunk, color);
                }
            }
        }
        // A relationship from a type to itself: a loop over its chip, each further loop on the
        // same type a size larger.
        for (node, rank, tone) in &loops {
            let p = screen[*node];
            let s = scale;
            let grow = *rank as f32;
            let spread = (10. + 6. * grow) * s;
            let height = (60. + LOOP_STEP * grow) * s;
            let mut builder = PathBuilder::stroke(px(1.));
            builder.move_to(point(p.x - px(spread), p.y - px(12. * s)));
            builder.cubic_bezier_to(
                point(p.x + px(spread), p.y - px(12. * s)),
                point(p.x - px(30. * s + spread), p.y - px(height)),
                point(p.x + px(30. * s + spread), p.y - px(height)),
            );
            if let Ok(path) = builder.build() {
                window.paint_path(path, palette.text.opacity(f32::from(tone.alpha) / 1000.));
            }
        }
    });

    // A small schema names every line up front, under the chips; a larger one names the lines
    // of the type the pointer is on, over them, so no chip hides a name.
    let label_all = schema && scene.edges.len() <= ALWAYS_LABEL;
    let paint_labels = |window: &mut Window, cx: &mut App| {
        for (edge, route) in scene.edges.iter().zip(&routes) {
            let Some(label) = &edge.label else { continue };
            let touches_hovered = hovered.is_some_and(|h| edge.from == h || edge.to == h);
            if !label_all && !touches_hovered {
                continue;
            }
            let at = route.label_at;
            if !visible(at, 60.) {
                continue;
            }
            let dim = near
                .as_ref()
                .is_some_and(|n| !(n.contains(&edge.from) && n.contains(&edge.to)));
            let alpha = if dim { studio_ui::ds::opacity::FAR } else { 1. };
            let pieces = [Piece {
                text: label.clone().into(),
                color: palette.muted.opacity(alpha),
                weight: studio_ui::ds::text::weight::BODY,
            }];
            paint_pill(
                at,
                &pieces,
                None,
                LABEL_TEXT * scale.max(0.8),
                PillStyle {
                    fill: palette.label_fill.opacity(alpha),
                    border: palette.border.opacity(alpha),
                    shadow: !label_all,
                    ring: None,
                },
                palette,
                window,
                cx,
            );
        }
    };
    if label_all {
        paint_labels(window, cx);
    }

    // Chips. From afar most are a letter mark, and a frame of thousands repeats a few letters:
    // each is shaped once here, not once a chip.
    let mut letters: HashMap<(SharedString, u32), ShapedLine> = HashMap::new();
    let mut rects = chips.borrow_mut();
    rects.clear();
    rects.resize(scene.nodes.len(), Bounds::default());
    let text_size = CHIP_TEXT * scale;
    for (index, node) in scene.nodes.iter().enumerate() {
        let p = screen[index];
        if !visible(p, 300.) {
            continue;
        }
        // A marked chip is drawn as the selected one is.
        let focus = selected == Some(index) || marked.contains(&index);
        let pointed = hovered == Some(index);
        let dim = !marked.contains(&index) && near.as_ref().is_some_and(|n| !n.contains(&index));
        let alpha = if dim { studio_ui::ds::opacity::FAR } else { 1. };
        let root = matches!(node.kind, SceneKind::Node { root: true, .. });
        // From afar a node is its mark alone, and a closed hub a dot on its line. A type keeps
        // its name further out: an ontology has few, and their names are the point.
        let mark_only_below = if matches!(node.kind, SceneKind::Type { .. }) {
            TYPE_MARK_ONLY_BELOW
        } else {
            MARK_ONLY_BELOW
        };
        if text_size < mark_only_below && !focus && !pointed && !root {
            rects[index] = match node.kind {
                SceneKind::Hub { .. } | SceneKind::More { .. } => {
                    paint_dot(p, HUB_DOT, palette.hub_dot.opacity(alpha), window)
                }
                _ => {
                    let initial = &texts[index].initial;
                    let line = letters
                        .entry((initial.clone(), alpha.to_bits()))
                        .or_insert_with(|| shape_letter(initial, 12., alpha, palette, window))
                        .clone();
                    paint_letter_mark(
                        p,
                        type_color(&node.color_key),
                        &line,
                        12.,
                        alpha,
                        window,
                        cx,
                    )
                }
            };
            continue;
        }
        let tint = type_color(&node.color_key);
        let text = &texts[index];
        let mark = text.mark.then(|| (tint, text.initial.clone()));
        let pieces: Vec<Piece> = text
            .pieces
            .iter()
            .map(|(text, role)| Piece {
                text: text.clone(),
                color: match role {
                    Role::Name | Role::RootName | Role::OpenHub => palette.text,
                    Role::Count | Role::Muted => palette.muted,
                }
                .opacity(alpha),
                weight: role.weight(),
            })
            .collect();
        let size = text.size * scale;
        let border = if focus {
            palette.here
        } else if pointed {
            palette.pointed
        } else {
            palette.border
        };
        rects[index] = paint_pill(
            p,
            &pieces,
            mark,
            size,
            PillStyle {
                fill: palette.card.opacity(alpha),
                border: border.opacity(alpha),
                shadow: true,
                ring: focus.then_some(palette.here_halo),
            },
            palette,
            window,
            cx,
        );
    }
    // An opened chip shows its whole name over its neighbours, from where its chip starts, and
    // is hit where it is drawn.
    if let Some(index) = opened
        && let Some(label) = scene.nodes.get(index).and_then(whole_label)
        && rects[index].size.width > px(0.)
    {
        let node = &scene.nodes[index];
        let focus = selected == Some(index) || marked.contains(&index);
        rects[index] = paint_open_chip(
            rects[index],
            &label,
            (type_color(&node.color_key), texts[index].initial.clone()),
            text_size,
            PillStyle {
                fill: palette.card,
                border: if focus { palette.here } else { palette.pointed },
                shadow: true,
                ring: focus.then_some(palette.here_halo),
            },
            palette,
            window,
            cx,
        );
    }
    drop(rects);
    if schema && !label_all {
        paint_labels(window, cx);
    }
}

/// Strokes lines as one path. Lines that need more vertices than a path holds are painted in
/// halves, so a batch is never dropped.
fn stroke_lines(window: &mut Window, lines: &[Segment], color: Hsla) {
    let mut builder = PathBuilder::stroke(px(1.));
    for (from, to, curve) in lines {
        builder.move_to(*from);
        match curve {
            Curve::Straight => builder.line_to(*to),
            Curve::Bowed(control) => builder.curve_to(*to, *control),
            Curve::Level(first, second) => builder.cubic_bezier_to(*to, *first, *second),
        }
    }
    match builder.build() {
        Ok(path) => window.paint_path(path, color),
        Err(_) if lines.len() > 1 => {
            let (first, second) = lines.split_at(lines.len() / 2);
            stroke_lines(window, first, color);
            stroke_lines(window, second, color);
        }
        Err(_) => {}
    }
}

/// Points along a line, close enough that the straight pieces between them follow its curve.
fn flatten(from: Point<Pixels>, to: Point<Pixels>, curve: Curve) -> Vec<Point<Pixels>> {
    let at = |p: Point<Pixels>| (f32::from(p.x), f32::from(p.y));
    let (a, b) = (at(from), at(to));
    let controls: Vec<(f32, f32)> = match curve {
        Curve::Straight => return vec![from, to],
        Curve::Bowed(control) => vec![at(control)],
        Curve::Level(first, second) => vec![at(first), at(second)],
    };
    // The control polygon is at least as long as the curve: a piece every 6 px or so.
    let mut polygon = 0.;
    let mut previous = a;
    for p in controls.iter().copied().chain([b]) {
        polygon += ((p.0 - previous.0).powi(2) + (p.1 - previous.1).powi(2)).sqrt();
        previous = p;
    }
    let steps = ((polygon / 6.).ceil() as usize).clamp(4, 96);
    (0..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let u = 1. - t;
            let (x, y) = match controls.as_slice() {
                [c] => (
                    u * u * a.0 + 2. * u * t * c.0 + t * t * b.0,
                    u * u * a.1 + 2. * u * t * c.1 + t * t * b.1,
                ),
                [c1, c2] => (
                    u * u * u * a.0
                        + 3. * u * u * t * c1.0
                        + 3. * u * t * t * c2.0
                        + t * t * t * b.0,
                    u * u * u * a.1
                        + 3. * u * u * t * c1.1
                        + 3. * u * t * t * c2.1
                        + t * t * t * b.1,
                ),
                _ => (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t),
            };
            point(px(x), px(y))
        })
        .collect()
}

/// The dashes of a dotted line through `points`, as straight pieces: `DASH` on, `GAP` off,
/// continuing across the line's bends. Dash `k` covers `[k·period, k·period + DASH]` of the
/// distance along the whole line, in f64 and stepped by `k`, so the walk always ends: a walk
/// that carried its place in the pattern from piece to piece in f32 stopped advancing when
/// the step left was below the resolution of the distance walked, and pushed empty dashes
/// until memory ran out. A piece longer than `DASHES_PER_LINE` dashes is drawn solid.
fn dashes(points: &[Point<Pixels>], out: &mut Vec<Segment>) {
    let period = f64::from(DASH + GAP);
    let dash = f64::from(DASH);
    // Distance along the line where the current piece starts.
    let mut start = 0f64;
    for pair in points.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let dx = f64::from(f32::from(to.x - from.x));
        let dy = f64::from(f32::from(to.y - from.y));
        let length = (dx * dx + dy * dy).sqrt();
        if !length.is_finite() {
            return;
        }
        if length <= f64::from(f32::EPSILON) {
            continue;
        }
        let end = start + length;
        let first = (start / period).floor();
        let last = (end / period).ceil();
        if last - first > DASHES_PER_LINE as f64 {
            out.push((from, to, Curve::Straight));
            start = end;
            continue;
        }
        let at = |s: f64| {
            let d = (s - start) / length;
            point(from.x + px((dx * d) as f32), from.y + px((dy * d) as f32))
        };
        for k in first as u64..last as u64 {
            let on = (k as f64 * period).max(start);
            let off = (k as f64 * period + dash).min(end);
            if off > on {
                out.push((at(on), at(off), Curve::Straight));
            }
        }
        start = end;
    }
}

/// The dot grid: a dot every [`DOT_STEP`] pixels, fixed to the screen.
fn paint_dots(bounds: Bounds<Pixels>, palette: &Palette, window: &mut Window) {
    let color = palette.grid_dot;
    let left = f32::from(bounds.left());
    let top = f32::from(bounds.top());
    let mut y = (top / DOT_STEP).ceil() * DOT_STEP;
    while y < f32::from(bounds.bottom()) {
        let mut x = (left / DOT_STEP).ceil() * DOT_STEP;
        while x < f32::from(bounds.right()) {
            window.paint_quad(quad(
                Bounds::new(
                    point(px(x - GRID_DOT / 2.), px(y - GRID_DOT / 2.)),
                    size(px(GRID_DOT), px(GRID_DOT)),
                ),
                px(GRID_DOT / 2.),
                color,
                px(0.),
                color,
                BorderStyle::Solid,
            ));
            x += DOT_STEP;
        }
        y += DOT_STEP;
    }
}

struct PillStyle {
    fill: Hsla,
    border: Hsla,
    /// A soft contact shadow.
    shadow: bool,
    /// The focus ring's glow.
    ring: Option<Hsla>,
}

/// A chip centred on `at`: an optional letter mark, then the pieces of text. Returns its bounds.
#[allow(clippy::too_many_arguments)]
fn paint_pill(
    at: Point<Pixels>,
    pieces: &[Piece],
    mark: Option<(Hsla, SharedString)>,
    text_size: f32,
    style: PillStyle,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) -> Bounds<Pixels> {
    let s = text_size / CHIP_TEXT;
    let lines: Vec<_> = pieces
        .iter()
        .map(|piece| {
            let font = Font {
                weight: piece.weight,
                ..palette.font.clone()
            };
            window.text_system().shape_line(
                piece.text.clone(),
                px(text_size),
                &[TextRun {
                    len: piece.text.len(),
                    font,
                    color: piece.color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
        })
        .collect();
    let gap = PILL_GAP * s;
    let pad_x = PILL_PAD_X * s;
    let line_height = text_size * 1.4;
    let height = line_height + 8. * s;
    let mark_size = PILL_MARK * s;
    let text_width: f32 = lines.iter().map(|l| f32::from(l.width)).sum::<f32>()
        + gap * lines.len().saturating_sub(1) as f32;
    let width = pad_x * 2. + text_width + if mark.is_some() { mark_size + gap } else { 0. };
    let rect = Bounds::new(
        point(at.x - px(width / 2.), at.y - px(height / 2.)),
        size(px(width), px(height)),
    );
    // A chip is not a button: the scale's `sm` corner, zoomed with it, never past round.
    let radius = Corners::all(px((studio_ui::ds::radii(cx).sm * s).min(height / 2.)));
    if let Some(ring) = style.ring {
        window.paint_drop_shadows(
            rect,
            radius,
            &[BoxShadow {
                color: ring,
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(3.),
                inset: false,
            }],
        );
    }
    if style.shadow {
        window.paint_drop_shadows(rect, radius, &palette.shadow);
    }
    window.paint_quad(quad(
        rect,
        radius,
        style.fill,
        px(1.),
        style.border,
        BorderStyle::Solid,
    ));
    let mut x = f32::from(rect.left()) + pad_x;
    let middle = at.y;
    if let Some((tint, letter)) = mark {
        let center = point(px(x + mark_size / 2.), middle);
        let line = shape_letter(&letter, mark_size, style.fill.a, palette, window);
        paint_letter_mark(center, tint, &line, mark_size, style.fill.a, window, cx);
        x += mark_size + gap;
    }
    for line in &lines {
        line.paint(
            point(px(x), middle - px(line_height / 2.)),
            px(line_height),
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();
        x += f32::from(line.width) + gap;
    }
    rect
}

/// A chip opened to its whole name: its mark, then the name wrapped at
/// [`OPEN_CHIP_MAX_WIDTH`], drawn from the top left of the chip it opens and never narrower
/// than it. Returns its bounds.
#[allow(clippy::too_many_arguments)]
fn paint_open_chip(
    chip: Bounds<Pixels>,
    label: &str,
    (tint, letter): (Hsla, SharedString),
    text_size: f32,
    style: PillStyle,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) -> Bounds<Pixels> {
    let s = text_size / CHIP_TEXT;
    let (gap, pad_x, mark_size) = (PILL_GAP * s, PILL_PAD_X * s, PILL_MARK * s);
    let line_height = text_size * 1.4;
    // The pill's padding above and below its one line.
    let pad_y = 4. * s;
    let widest = OPEN_CHIP_MAX_WIDTH * s - pad_x * 2. - mark_size - gap;
    let run = TextRun {
        len: label.len(),
        font: Font {
            weight: Role::Name.weight(),
            ..palette.font.clone()
        },
        color: palette.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let text = SharedString::from(label.to_owned());
    let whole = f32::from(
        window
            .text_system()
            .shape_line(
                text.clone(),
                px(text_size),
                std::slice::from_ref(&run),
                None,
            )
            .width,
    );
    let wrap = wrap_width(whole, widest, text_size);
    let lines = window
        .text_system()
        .shape_text(text, px(text_size), &[run], Some(px(wrap)), None)
        .unwrap_or_default();
    let text_width = lines
        .iter()
        .map(|line| f32::from(line.width()))
        .fold(0., f32::max);
    let text_height: f32 = lines
        .iter()
        .map(|line| f32::from(line.size(px(line_height)).height))
        .sum();
    let width = (pad_x * 2. + mark_size + gap + text_width).max(f32::from(chip.size.width));
    let height = (text_height + pad_y * 2.).max(f32::from(chip.size.height));
    let rect = Bounds::new(chip.origin, size(px(width), px(height)));
    let radius = Corners::all(px(studio_ui::ds::radii(cx).sm * s));
    if let Some(ring) = style.ring {
        window.paint_drop_shadows(
            rect,
            radius,
            &[BoxShadow {
                color: ring,
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(3.),
                inset: false,
            }],
        );
    }
    if style.shadow {
        window.paint_drop_shadows(rect, radius, &palette.shadow);
    }
    window.paint_quad(quad(
        rect,
        radius,
        style.fill,
        px(1.),
        style.border,
        BorderStyle::Solid,
    ));
    // The mark stays where the chip had it, beside the first line.
    let left = f32::from(rect.left()) + pad_x;
    let first_middle = rect.top() + px(pad_y + line_height / 2.);
    let mark = shape_letter(&letter, mark_size, 1., palette, window);
    paint_letter_mark(
        point(px(left + mark_size / 2.), first_middle),
        tint,
        &mark,
        mark_size,
        1.,
        window,
        cx,
    );
    let mut y = rect.top() + px(pad_y);
    for line in &lines {
        line.paint(
            point(px(left + mark_size + gap), y),
            px(line_height),
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();
        y += line.size(px(line_height)).height;
    }
    rect
}

/// Where an opened chip's name of `whole` width wraps, at most `widest`: in lines of even
/// length rather than a full line and a scrap, since a name with no spaces (an email, an id)
/// breaks where its room ends and its last line would hold a letter or two. A character's
/// width (`text_size`) of slack keeps a line from wrapping again at its last letter.
fn wrap_width(whole: f32, widest: f32, text_size: f32) -> f32 {
    let lines = (whole / widest).ceil().max(1.);
    (whole / lines + text_size).min(widest)
}

/// A mark's letter, shaped for a mark `size_px` across.
fn shape_letter(
    letter: &str,
    size_px: f32,
    alpha: f32,
    palette: &Palette,
    window: &mut Window,
) -> ShapedLine {
    let font = Font {
        weight: studio_ui::ds::text::weight::HEADING,
        ..palette.font.clone()
    };
    window.text_system().shape_line(
        SharedString::from(letter.to_owned()),
        px(size_px * 9. / 16.),
        &[TextRun {
            len: letter.len(),
            font,
            color: palette.on_tint.opacity(alpha),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    )
}

/// The type mark: the initial in white on the type's tint, a rounded square. A node from afar
/// is this mark alone.
fn paint_letter_mark(
    center: Point<Pixels>,
    tint: Hsla,
    line: &ShapedLine,
    size_px: f32,
    alpha: f32,
    window: &mut Window,
    cx: &mut App,
) -> Bounds<Pixels> {
    let rect = Bounds::new(
        point(center.x - px(size_px / 2.), center.y - px(size_px / 2.)),
        size(px(size_px), px(size_px)),
    );
    // The design system's `xs` corner, at the mark's 16 px size scaled to this one.
    let corner = studio_ui::ds::radii(cx).xs * size_px / studio_ui::ds::size::ROW_ICON;
    window.paint_quad(quad(
        rect,
        px(corner),
        tint.opacity(alpha),
        px(0.),
        tint,
        BorderStyle::Solid,
    ));
    let text_size = size_px * 9. / 16.;
    line.paint(
        point(center.x - line.width / 2., center.y - px(text_size * 0.7)),
        px(text_size * 1.4),
        TextAlign::Left,
        None,
        window,
        cx,
    )
    .ok();
    rect
}

/// A closed hub from afar.
fn paint_dot(at: Point<Pixels>, radius: f32, color: Hsla, window: &mut Window) -> Bounds<Pixels> {
    let rect = Bounds::new(
        point(at.x - px(radius), at.y - px(radius)),
        size(px(radius * 2.), px(radius * 2.)),
    );
    window.paint_quad(quad(
        rect,
        px(radius),
        color,
        px(0.),
        color,
        BorderStyle::Solid,
    ));
    rect
}

/// Whether a hub–node line joins a hub to one of the neighbours it shows (a "member" line),
/// rather than the hub to the node it belongs to.
fn edge_is_member(scene: &Scene, from: usize, to: usize) -> bool {
    let (hub, node) = match (&scene.nodes[from].kind, &scene.nodes[to].kind) {
        (SceneKind::Hub { .. }, _) => (from, to),
        _ => (to, from),
    };
    scene.nodes[hub].parent != Some(node)
}

/// The letter of a mark, as every type mark of Studio draws it.
fn initial(label: &str, type_name: &str) -> SharedString {
    studio_ui::text::initial(label, type_name).into()
}

/// Whether the box around the line from `a` to `b` meets `bounds` grown by `margin`: a curve
/// bends within the margin.
fn crosses(a: Point<Pixels>, b: Point<Pixels>, bounds: Bounds<Pixels>, margin: f32) -> bool {
    let margin = px(margin);
    a.x.max(b.x) >= bounds.left() - margin
        && a.x.min(b.x) <= bounds.right() + margin
        && a.y.max(b.y) >= bounds.top() - margin
        && a.y.min(b.y) <= bounds.bottom() + margin
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An opened chip's name wraps into lines of even length, and one that fits stays whole.
    #[test]
    fn an_opened_name_wraps_into_even_lines() {
        assert_eq!(
            wrap_width(200., 280., 13.),
            213.,
            "fits: one line, with its slack"
        );
        assert_eq!(
            wrap_width(275., 280., 13.),
            280.,
            "nearly full: the whole room"
        );
        // A little more than the room: two half lines, not a full line and a scrap.
        let half = wrap_width(300., 280., 13.);
        assert!((150. ..170.).contains(&half), "{half}");
        // Long text: as many lines as it needs, each under the room.
        let third = wrap_width(700., 280., 13.);
        assert!(third <= 280. && 700. / third <= 3., "{third}");
    }

    /// A line is drawn when it crosses the view, though both its ends are outside it.
    #[test]
    fn a_line_across_the_view_is_drawn_with_both_ends_outside() {
        let view = Bounds::new(point(px(0.), px(0.)), size(px(800.), px(600.)));
        let (left, right) = (point(px(-5000.), px(300.)), point(px(5000.), px(300.)));
        assert!(crosses(left, right, view, 400.));
        let below = (point(px(-5000.), px(5000.)), point(px(5000.), px(5000.)));
        assert!(!crosses(below.0, below.1, view, 400.));
    }

    #[test]
    fn a_dotted_line_is_cut_into_dashes_across_its_bends() {
        let mut pieces = Vec::new();
        dashes(
            &[point(px(0.), px(0.)), point(px(50.), px(0.))],
            &mut pieces,
        );
        assert_eq!(pieces.len(), 10, "a dash every 5 px");
        assert!(
            pieces
                .iter()
                .all(|(a, b, _)| (f32::from(b.x - a.x) - DASH).abs() < 1e-3)
        );
        // A bend in the middle of a dash continues it on the next piece.
        let mut bent = Vec::new();
        dashes(
            &[
                point(px(0.), px(0.)),
                point(px(6.), px(0.)),
                point(px(6.), px(10.)),
            ],
            &mut bent,
        );
        let drawn: f32 = bent
            .iter()
            .map(|(a, b, _)| f32::from(b.x - a.x).abs() + f32::from(b.y - a.y).abs())
            .sum();
        assert!(
            (drawn - 7.).abs() < 1e-3,
            "16 px of line, 7 px of dashes: {drawn}"
        );
        // A curve is followed from end to end.
        let curve = flatten(
            point(px(0.), px(0.)),
            point(px(100.), px(0.)),
            Curve::Bowed(point(px(50.), px(50.))),
        );
        assert_eq!(curve.first(), Some(&point(px(0.), px(0.))));
        assert_eq!(curve.last(), Some(&point(px(100.), px(0.))));
        assert!(curve.len() > 4);
    }

    /// The canvas froze, its memory growing without end, when a dotted line's walk stopped
    /// advancing: at the second bend of this line the step left in the dash fell below the
    /// f32 resolution of the distance walked (found by emulating the old walk's f32 steps;
    /// common on curves, which bend every few pixels). Every line now ends, with a dash count
    /// set by its length.
    #[test]
    fn every_dotted_line_ends_with_dashes_set_by_its_length() {
        let lines: Vec<Vec<Point<Pixels>>> = vec![
            vec![
                point(px(243.6), px(29.3)),
                point(px(204.8), px(66.0)),
                point(px(136.8), px(373.3)),
            ],
            flatten(
                point(px(168.45694), px(355.29034)),
                point(px(160.164), px(355.26978)),
                Curve::Bowed(point(px(120.), px(300.))),
            ),
            flatten(
                point(px(-300.), px(40.)),
                point(px(900.), px(700.)),
                Curve::Level(point(px(100.), px(40.)), point(px(500.), px(700.))),
            ),
        ];
        for points in lines {
            let length: f32 = points
                .windows(2)
                .map(|pair| {
                    let (dx, dy) = (
                        f32::from(pair[1].x - pair[0].x),
                        f32::from(pair[1].y - pair[0].y),
                    );
                    (dx * dx + dy * dy).sqrt()
                })
                .sum();
            let mut pieces = Vec::new();
            dashes(&points, &mut pieces);
            let most = (length / (DASH + GAP)).ceil() as usize + points.len();
            assert!(
                !pieces.is_empty() && pieces.len() <= most,
                "{} dashes on {length} px",
                pieces.len()
            );
            assert!(pieces.iter().all(|(a, b, _)| {
                let (dx, dy) = (f32::from(b.x - a.x), f32::from(b.y - a.y));
                (dx * dx + dy * dy).sqrt() <= DASH + 1e-3
            }));
        }
        // A piece too long to cut is drawn whole; one that is not a number draws nothing.
        let mut pieces = Vec::new();
        dashes(
            &[point(px(0.), px(0.)), point(px(1e7), px(0.))],
            &mut pieces,
        );
        assert_eq!(pieces.len(), 1);
        let mut pieces = Vec::new();
        dashes(
            &[point(px(0.), px(0.)), point(px(f32::INFINITY), px(0.))],
            &mut pieces,
        );
        assert!(pieces.is_empty());
    }
}
