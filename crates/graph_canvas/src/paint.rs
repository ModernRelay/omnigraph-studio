//! How the canvas looks (spec 006): a faint dot grid; hairline grey lines; every node,
//! type, hub and "+K more" a chip, a white pill with a hairline and a soft contact shadow, with
//! the type's letter mark and a muted count. Pointing at a chip lights its lines and fades the
//! rest; the selected chip carries a warm ring. Colour is kept to the small type marks.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use gpui::{
    App, BorderStyle, Bounds, BoxShadow, Corners, Font, FontWeight, Hsla, PathBuilder, Pixels,
    Point, SharedString, TextAlign, TextRun, Window, point, px, quad, size,
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
    pub font: Font,
}

/// A chip's text size at zoom 1, and a hub's or "+K more"'s.
const CHIP_TEXT: f32 = 13.;
const HUB_TEXT: f32 = 12.5;
/// A relationship label's text size at zoom 1.
const LABEL_TEXT: f32 = 11.;
/// A pill's inner spacing at zoom 1: between pieces, at each end, and its mark's size.
const PILL_GAP: f32 = 6.;
const PILL_PAD_X: f32 = 10.;
const PILL_MARK: f32 = 16.;
/// Below this text size a node is drawn as its mark alone, unless it is selected or pointed at.
const MARK_ONLY_BELOW: f32 = 8.5;
const TYPE_MARK_ONLY_BELOW: f32 = 6.;
/// The dot grid's spacing, in screen pixels.
const DOT_STEP: f32 = 22.;
/// Up to this many relationships, the schema labels every line.
pub(crate) const ALWAYS_LABEL: usize = 16;
/// The warm ring that marks where you are.
const WARM: u32 = 0xf2b48f;
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
    let mut pair_count: HashMap<(usize, usize), usize> = HashMap::new();
    for edge in &scene.edges {
        *pair_count
            .entry((edge.from.min(edge.to), edge.from.max(edge.to)))
            .or_default() += 1;
    }
    let mut seen: HashMap<(usize, usize), usize> = HashMap::new();
    scene
        .edges
        .iter()
        .map(|edge| {
            let key = (edge.from.min(edge.to), edge.from.max(edge.to));
            let rank = {
                let seen = seen.entry(key).or_default();
                *seen += 1;
                *seen - 1
            };
            let (a, b) = (screen[key.0], screen[key.1]);
            if edge.from == edge.to {
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
            Role::RootName => FontWeight::MEDIUM,
            _ => FontWeight::NORMAL,
        }
    }
}

/// What a chip says, before colour: the same for painting it and for measuring it, so the
/// layout makes room for the chip the painter draws.
pub(crate) struct ChipText {
    pub mark: bool,
    pub pieces: Vec<(String, Role)>,
    /// The text size at zoom 1.
    pub size: f32,
}

pub(crate) fn chip_text(node: &SceneNode) -> ChipText {
    match &node.kind {
        SceneKind::Type { count } => ChipText {
            mark: true,
            pieces: std::iter::once((node.label.clone(), Role::RootName))
                .chain(count.map(|count| (format_count(count), Role::Count)))
                .collect(),
            size: CHIP_TEXT,
        },
        SceneKind::Node { root, .. } => ChipText {
            mark: true,
            pieces: vec![(
                truncate(&node.label, 32),
                if *root { Role::RootName } else { Role::Name },
            )],
            size: CHIP_TEXT,
        },
        SceneKind::Hub { open, .. } => {
            let (name, count) = match node.label.rsplit_once(" · ") {
                Some((name, count)) => (name.to_owned(), Some(count.to_owned())),
                None => (node.label.clone(), None),
            };
            ChipText {
                mark: false,
                pieces: std::iter::once((name, if *open { Role::OpenHub } else { Role::Muted }))
                    .chain(count.map(|count| (count, Role::Count)))
                    .collect(),
                size: HUB_TEXT,
            }
        }
        SceneKind::More { .. } => ChipText {
            mark: false,
            pieces: vec![(node.label.clone(), Role::Muted)],
            size: HUB_TEXT,
        },
    }
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
            pieces: vec![(label.to_owned(), Role::Muted)],
            size: LABEL_TEXT,
        },
        measure,
    )
}

/// One piece of a chip's text.
struct Piece {
    text: String,
    color: Hsla,
    weight: FontWeight,
}

/// Paints the scene and records each node's chip on screen, for hit testing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint(
    scene: &Scene,
    screen: &[Point<Pixels>],
    bounds: Bounds<Pixels>,
    zoom: f32,
    selected: Option<usize>,
    marked: &HashSet<usize>,
    hovered: Option<usize>,
    palette: &Palette,
    chips: &RefCell<Vec<Bounds<Pixels>>>,
    window: &mut Window,
    cx: &mut App,
) {
    paint_dots(bounds, palette, window);
    if screen.len() != scene.nodes.len() {
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
    // What pointing at a chip brings out: the chip and what its lines reach.
    let near: Option<HashSet<usize>> = hovered.map(|h| {
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
    let mut batches: HashMap<Tone, Vec<Segment>> = HashMap::new();
    let mut loops: Vec<(usize, usize, Tone)> = Vec::new();
    for (edge, route) in scene.edges.iter().zip(&routes) {
        let (a, b) = (screen[edge.from], screen[edge.to]);
        if !visible(a, 400.) && !visible(b, 400.) {
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
            Some(_) => {
                if schema {
                    0.05
                } else {
                    0.06
                }
            }
            None if !schema && touches(selected) => 0.25,
            None => base,
        };
        let tone = Tone {
            alpha: (alpha * 1000.) as u16,
            dotted,
        };
        if edge.from == edge.to {
            loops.push((edge.from, route.loop_rank, tone));
        } else {
            batches.entry(tone).or_default().push((a, b, route.curve));
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
            let alpha = if dim { 0.25 } else { 1. };
            let pieces = [Piece {
                text: label.clone(),
                color: palette.muted.opacity(alpha),
                weight: FontWeight::NORMAL,
            }];
            paint_pill(
                at,
                &pieces,
                None,
                LABEL_TEXT * scale.max(0.8),
                PillStyle {
                    fill: palette.card.opacity(0.95 * alpha),
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

    // Chips.
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
        let alpha = if dim { 0.3 } else { 1. };
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
                    paint_dot(p, 4., palette.text.opacity(0.25 * alpha), window)
                }
                _ => paint_mark(
                    p,
                    &node.color_key,
                    &node.label,
                    12.,
                    alpha,
                    palette,
                    window,
                    cx,
                ),
            };
            continue;
        }
        let tint = type_color(&node.color_key);
        let text = chip_text(node);
        let mark = text.mark.then_some(tint);
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
            gpui::rgb(WARM).into()
        } else if pointed {
            palette.text.opacity(0.2)
        } else {
            palette.border
        };
        rects[index] = paint_pill(
            p,
            &pieces,
            mark.map(|tint| (tint, initial(&node.label, &node.color_key))),
            size,
            PillStyle {
                fill: palette.card.opacity(alpha),
                border: border.opacity(alpha),
                shadow: true,
                ring: focus.then(|| Hsla::from(gpui::rgb(WARM)).opacity(0.2)),
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

/// The dot grid: the foreground at 7%, a 1.2-pixel dot every 22 pixels, fixed to the screen.
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

fn paint_dots(bounds: Bounds<Pixels>, palette: &Palette, window: &mut Window) {
    let color = palette.text.opacity(0.07);
    let left = f32::from(bounds.left());
    let top = f32::from(bounds.top());
    let mut y = (top / DOT_STEP).ceil() * DOT_STEP;
    while y < f32::from(bounds.bottom()) {
        let mut x = (left / DOT_STEP).ceil() * DOT_STEP;
        while x < f32::from(bounds.right()) {
            window.paint_quad(quad(
                Bounds::new(point(px(x - 0.6), px(y - 0.6)), size(px(1.2), px(1.2))),
                px(0.6),
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
    mark: Option<(Hsla, String)>,
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
                SharedString::from(piece.text.clone()),
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
    let radius = Corners::all(px(height / 2.));
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
        window.paint_drop_shadows(
            rect,
            radius,
            &[
                BoxShadow {
                    color: gpui::black().opacity(0.05),
                    offset: point(px(0.), px(1.)),
                    blur_radius: px(2.),
                    spread_radius: px(0.),
                    inset: false,
                },
                BoxShadow {
                    color: gpui::black().opacity(0.08),
                    offset: point(px(0.), px(4.)),
                    blur_radius: px(10.),
                    spread_radius: px(-4.),
                    inset: false,
                },
            ],
        );
    }
    window.paint_quad(quad(
        rect,
        px(height / 2.),
        style.fill,
        px(1.),
        style.border,
        BorderStyle::Solid,
    ));
    let mut x = f32::from(rect.left()) + pad_x;
    let middle = at.y;
    if let Some((tint, letter)) = mark {
        let center = point(px(x + mark_size / 2.), middle);
        paint_letter_mark(
            center,
            tint,
            &letter,
            mark_size,
            style.fill.a,
            palette,
            window,
            cx,
        );
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

/// A node from afar: its letter mark alone, ringed in the background so lines stop at it.
#[allow(clippy::too_many_arguments)]
fn paint_mark(
    at: Point<Pixels>,
    type_name: &str,
    label: &str,
    size_px: f32,
    alpha: f32,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) -> Bounds<Pixels> {
    paint_letter_mark(
        at,
        type_color(type_name),
        &initial(label, type_name),
        size_px,
        alpha,
        palette,
        window,
        cx,
    )
}

/// The type mark: the initial in white on the type's tint, a rounded square.
#[allow(clippy::too_many_arguments)]
fn paint_letter_mark(
    center: Point<Pixels>,
    tint: Hsla,
    letter: &str,
    size_px: f32,
    alpha: f32,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) -> Bounds<Pixels> {
    let rect = Bounds::new(
        point(center.x - px(size_px / 2.), center.y - px(size_px / 2.)),
        size(px(size_px), px(size_px)),
    );
    window.paint_quad(quad(
        rect,
        px(size_px / 4.),
        tint.opacity(alpha),
        px(0.),
        tint,
        BorderStyle::Solid,
    ));
    let text_size = size_px * 9. / 16.;
    let font = Font {
        weight: FontWeight::SEMIBOLD,
        ..palette.font.clone()
    };
    let line = window.text_system().shape_line(
        SharedString::from(letter.to_owned()),
        px(text_size),
        &[TextRun {
            len: letter.len(),
            font,
            color: gpui::white().opacity(alpha),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
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

/// The letter of a mark: the label's first character, else the type's.
fn initial(label: &str, type_name: &str) -> String {
    label
        .trim()
        .chars()
        .next()
        .or_else(|| type_name.chars().next())
        .unwrap_or('?')
        .to_uppercase()
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max - 1).collect::<String>())
    }
}

pub(crate) fn format_count(count: i64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
