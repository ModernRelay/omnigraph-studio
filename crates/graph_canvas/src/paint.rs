//! How the canvas looks (spec 006): a faint dot grid; dashed grey lines that turn at right
//! angles, each end in a small ring, as interfaces.dev draws its connectors; every node, type,
//! hub and "+K more" a chip, a white pill with interfaces.dev's shadow (a 1 px ring and two soft
//! drops, on its `#fcfcfc` ground), with the type's letter mark and a muted count. Pointing at a
//! chip lights its lines; the selected chip carries a warm ring. Colour is kept to the small
//! type marks.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};

use gpui::{
    App, BorderStyle, Bounds, BoxShadow, Corners, Font, FontWeight, Hsla, Pixels, Point,
    ShapedLine, SharedString, TextAlign, TextRun, Window, WrappedLine, point, px, quad, size,
};
use studio_ui::ds::elevation::Layer;
use studio_ui::type_color;

use crate::model::{Scene, SceneKind, SceneNode};

pub(crate) struct Palette {
    pub text: Hsla,
    pub muted: Hsla,
    /// The chip fill.
    pub card: Hsla,
    /// A chip's shadow, its ring first: interfaces.dev's `--shadow-custom`, painted as a
    /// browser paints it ([`studio_ui::ds::elevation::paint_layers`]).
    pub shadow: Vec<Layer>,
    /// The same under the pointer (`--shadow-custom-hover`).
    pub pointed: Vec<Layer>,
    /// A type mark's letter on its tint.
    pub on_tint: Hsla,
    /// Where you are: the selected or marked chip's ring, and its halo.
    pub here: Hsla,
    pub here_halo: Hsla,
    /// The dot grid's dots.
    pub grid_dot: Hsla,
    /// The canvas behind the chips: a line end's ring is filled with it.
    pub canvas: Hsla,
    /// A hub's or "+K more"'s dot from afar.
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
/// A hub's or "+K more"'s dot from afar: its radius, in screen pixels.
const HUB_DOT: f32 = 4.;
/// A node's letter mark from afar, across, in screen pixels.
const MARK_SIZE: f32 = 12.;
/// The dot grid's spacing, in screen pixels.
const DOT_STEP: f32 = 22.;
/// A line's pattern in screen pixels: dashed as interfaces.dev's 1 px dashed borders are (3 on,
/// 2 off), or dotted (1 on, 2 off) where a hub is closed or a "+K more" stands for what is not
/// drawn.
const DASH: (f32, f32) = (3., 2.);
const DOT: (f32, f32) = (1., 2.);
/// The most dashes one run of a line is cut into; a longer run (only possible far beyond the
/// view, which runs are clipped to first) is drawn solid.
const DASHES_PER_RUN: f64 = 100_000.;
/// A line end's ring: its diameter, its 1 px border included, in screen pixels.
pub(crate) const RING: f32 = 6.;
/// How far from a node an exploration's line turns, at zoom 1: within the narrower of the gaps
/// the exploration leaves between a node and its hubs and a hub and its neighbours.
const ELBOW: f32 = 18.;
/// How much taller each further loop on one type is, and wider, at zoom 1.
const LOOP_STEP: f32 = 6.;

/// What the lines are drawn from, besides the scene: each node's half width at zoom 1, where
/// its lines meet its chip, and the world's origin on screen, so a line's dashes keep their
/// place on it as the view pans.
pub(crate) struct Lines<'a> {
    pub half_widths: &'a [f32],
    pub origin: Point<Pixels>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Tone {
    /// Line opacity, in thousandths, so tones batch.
    pub alpha: u16,
    pub dotted: bool,
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

/// One piece of a chip's text.
struct Piece {
    text: SharedString,
    color: Hsla,
    weight: FontWeight,
}

/// Paints the scene and records each node's chip on screen, for hit testing.
#[expect(clippy::too_many_arguments)]
pub(crate) fn paint(
    scene: &Scene,
    texts: &[ChipText],
    screen: &[Point<Pixels>],
    lines: &Lines,
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
    let text_size = CHIP_TEXT * scale;
    // From afar a node is its mark alone, and a closed hub a dot on its line, unless it is where
    // you are.
    let mark_only = |index: usize| {
        let node = &scene.nodes[index];
        let focus = selected == Some(index) || marked.contains(&index);
        let root = matches!(node.kind, SceneKind::Node { root: true, .. });
        text_size < MARK_ONLY_BELOW && !focus && hovered != Some(index) && !root
    };
    // How far from its centre each node's lines meet it: its chip's side, or its mark's.
    let reach: Vec<f32> = (0..scene.nodes.len())
        .map(|index| {
            if mark_only(index) {
                match scene.nodes[index].kind {
                    SceneKind::Hub { .. } | SceneKind::More { .. } => HUB_DOT,
                    SceneKind::Node { .. } => MARK_SIZE / 2.,
                }
            } else {
                lines.half_widths.get(index).copied().unwrap_or(0.) * scale
            }
        })
        .collect();

    // What each chip says, in its colours.
    let pieces_of = |index: usize| -> Vec<Piece> {
        texts[index]
            .pieces
            .iter()
            .map(|(text, role)| Piece {
                text: text.clone(),
                color: match role {
                    Role::Name | Role::RootName | Role::OpenHub => palette.text,
                    Role::Count | Role::Muted => palette.muted,
                },
                weight: role.weight(),
            })
            .collect()
    };
    // An opened chip shows its whole name over its neighbours, from where its chip starts; it is
    // measured before the lines, which meet it where it is drawn.
    let open: Option<(usize, OpenChip)> = opened
        .filter(|&index| index < scene.nodes.len() && !mark_only(index))
        .and_then(|index| {
            let label = whole_label(&scene.nodes[index])?;
            let size = texts[index].size * scale;
            let (chip, _) = pill_layout(
                screen[index],
                &pieces_of(index),
                texts[index].mark,
                size,
                palette,
                window,
            );
            Some((
                index,
                open_chip_layout(chip, &label, text_size, palette, window),
            ))
        });
    // Where each node's lines meet it: its centre and how far its sides are from it; an opened
    // chip's, its whole card's, so its lines and their rings meet its sides level with its
    // middle however tall the name makes it.
    let mut anchor: Vec<Point<Pixels>> = screen.to_vec();
    let mut reach = reach;
    if let Some((index, open)) = &open {
        (anchor[*index], reach[*index]) = meets(open.rect);
    }

    // Each line as right-angle pieces, with its tone: out of the node it hangs from, turning
    // near it, so a column of neighbours shares one upright. A scene has no edge from a node to
    // itself (a node is drawn once, and a result's row joins distinct nodes).
    let mut drawn: Vec<(usize, Vec<Point<Pixels>>, Tone)> = Vec::new();
    for (index, edge) in scene.edges.iter().enumerate() {
        let tone = line_tone(scene, edge.from, edge.to, hovered, selected);
        let from_is_parent = scene.nodes[edge.from].parent != Some(edge.to);
        let (parent, child) = if from_is_parent {
            (edge.from, edge.to)
        } else {
            (edge.to, edge.from)
        };
        let points = elbow(
            (anchor[parent], reach[parent]),
            (anchor[child], reach[child]),
            ELBOW * zoom,
        );
        if !crosses(&points, bounds, 40.) {
            continue;
        }
        drawn.push((index, points, tone));
    }
    window.paint_layer(bounds, |window| {
        paint_runs(&drawn, bounds, lines.origin, palette.text, window);
        // Each line end's ring, once where ends meet, in its strongest tone; none at a mark.
        let mut rings: BTreeMap<(i64, i64), (Point<Pixels>, u16)> = BTreeMap::new();
        for (index, points, tone) in &drawn {
            let edge = &scene.edges[*index];
            if points.len() < 2 {
                continue;
            }
            // Drawn from the node a line hangs from to the other.
            let (parent, child) = if scene.nodes[edge.from].parent != Some(edge.to) {
                (edge.from, edge.to)
            } else {
                (edge.to, edge.from)
            };
            let last = points.len() - 1;
            for (end, next, node) in [(0, 1, parent), (last, last - 1, child)] {
                if mark_only(node) {
                    continue;
                }
                let centre = ring_centre(points[end], points[next]);
                let key = (
                    (f32::from(centre.x) * 4.).round() as i64,
                    (f32::from(centre.y) * 4.).round() as i64,
                );
                let ring = rings.entry(key).or_insert((centre, 0));
                ring.1 = ring.1.max(tone.alpha);
            }
        }
        for (centre, alpha) in rings.into_values() {
            if !visible(centre, RING) {
                continue;
            }
            paint_ring(
                centre,
                palette.text.opacity(f32::from(alpha) / 1000.),
                palette.canvas,
                window,
            );
        }
    });

    // Chips. From afar most are a letter mark, and a frame of thousands repeats a few letters:
    // each is shaped once here, not once a chip.
    let mut letters: HashMap<SharedString, ShapedLine> = HashMap::new();
    let mut rects = chips.borrow_mut();
    rects.clear();
    rects.resize(scene.nodes.len(), Bounds::default());
    for (index, node) in scene.nodes.iter().enumerate() {
        let p = screen[index];
        if !visible(p, 300.) {
            continue;
        }
        // A marked chip is drawn as the selected one is.
        let focus = selected == Some(index) || marked.contains(&index);
        let pointed = hovered == Some(index);
        if mark_only(index) {
            rects[index] = match node.kind {
                SceneKind::Hub { .. } | SceneKind::More { .. } => {
                    paint_dot(p, HUB_DOT, palette.hub_dot, window)
                }
                SceneKind::Node { .. } => {
                    let initial = &texts[index].initial;
                    let line = letters
                        .entry(initial.clone())
                        .or_insert_with(|| shape_letter(initial, MARK_SIZE, palette, window))
                        .clone();
                    paint_letter_mark(p, type_color(&node.color_key), &line, MARK_SIZE, window, cx)
                }
            };
            continue;
        }
        // An opened chip is drawn once, opened, over its neighbours below: not also cut here,
        // where its glow would show around the card.
        if open.as_ref().is_some_and(|(opened, _)| *opened == index) {
            continue;
        }
        let tint = type_color(&node.color_key);
        let text = &texts[index];
        let mark = text.mark.then(|| (tint, text.initial.clone()));
        let size = text.size * scale;
        rects[index] = paint_pill(
            p,
            &pieces_of(index),
            mark,
            size,
            PillStyle {
                fill: palette.card,
                border: focus.then_some(palette.here),
                shadow: if pointed {
                    &palette.pointed
                } else {
                    &palette.shadow
                },
                ring: focus.then_some(palette.here_halo),
            },
            palette,
            window,
            cx,
        );
    }
    // The opened chip, over its neighbours, hit where it is drawn.
    if let Some((index, open)) = &open {
        let node = &scene.nodes[*index];
        let focus = selected == Some(*index) || marked.contains(index);
        paint_open_chip(
            open,
            (type_color(&node.color_key), texts[*index].initial.clone()),
            PillStyle {
                fill: palette.card,
                border: focus.then_some(palette.here),
                shadow: &palette.pointed,
                ring: focus.then_some(palette.here_halo),
            },
            palette,
            window,
            cx,
        );
        rects[*index] = open.rect;
    }
}

/// Where lines meet a chip drawn in `rect`: at its centre's height, `reach` either side of it.
fn meets(rect: Bounds<Pixels>) -> (Point<Pixels>, f32) {
    (rect.center(), f32::from(rect.size.width) / 2.)
}

/// Where the ring at a line's end `at` sits: on the line, half a ring back along its last
/// piece toward `next`, so the line stops at the ring and the ring touches what the line
/// reaches. Pieces run at right angles, so the ring moves along one axis only; a piece of no
/// length leaves it on the end. (`f32::signum` of zero is 1, which moved every ring at a
/// level line's end half a ring down, off the line.)
pub(crate) fn ring_centre(at: Point<Pixels>, next: Point<Pixels>) -> Point<Pixels> {
    let step = |from: Pixels, to: Pixels| match f32::from(to - from) {
        d if d > 0. => RING / 2.,
        d if d < 0. => -RING / 2.,
        _ => 0.,
    };
    point(at.x + px(step(at.x, next.x)), at.y + px(step(at.y, next.y)))
}

/// A line end's ring at `centre`: a small circle in `ink`, filled with the ground (`fill`) so
/// the line stops at it, as interfaces.dev ends its connectors.
pub(crate) fn paint_ring(centre: Point<Pixels>, ink: Hsla, fill: Hsla, window: &mut Window) {
    window.paint_quad(quad(
        Bounds::new(
            point(centre.x - px(RING / 2.), centre.y - px(RING / 2.)),
            size(px(RING), px(RING)),
        ),
        px(RING / 2.),
        fill,
        px(1.),
        ink,
        BorderStyle::Solid,
    ));
}

/// A line's tone: how strong it is drawn, and whether dotted. A hub's line to its node is a
/// little stronger than its lines to its neighbours, the pointed and the selected chip's lines
/// stand out, and a closed hub hangs on a dotted line.
fn line_tone(
    scene: &Scene,
    from: usize,
    to: usize,
    hovered: Option<usize>,
    selected: Option<usize>,
) -> Tone {
    let touches = |i: Option<usize>| i.is_some_and(|i| from == i || to == i);
    let kinds = (&scene.nodes[from].kind, &scene.nodes[to].kind);
    let hub_link = matches!(kinds.0, SceneKind::Hub { .. } | SceneKind::More { .. })
        || matches!(kinds.1, SceneKind::Hub { .. } | SceneKind::More { .. });
    let member = matches!(
        kinds,
        (SceneKind::Hub { .. }, SceneKind::Node { .. })
            | (SceneKind::Node { .. }, SceneKind::Hub { .. })
    ) && edge_is_member(scene, from, to);
    let alpha = if touches(hovered) {
        0.45
    } else if touches(selected) {
        0.28
    } else if member {
        0.13
    } else if hub_link {
        0.22
    } else {
        0.16
    };
    let dotted = matches!(
        kinds.0,
        SceneKind::More { .. } | SceneKind::Hub { open: false, .. }
    ) || matches!(
        kinds.1,
        SceneKind::More { .. } | SceneKind::Hub { open: false, .. }
    );
    Tone {
        alpha: (alpha * 1000.) as u16,
        dotted,
    }
}

/// A line from a parent node to a child as right-angle pieces: out of the parent's side towards
/// the child, turning `turn` beyond it (or halfway, where the two are closer), and into the
/// child's side.
fn elbow(
    (parent, parent_reach): (Point<Pixels>, f32),
    (child, child_reach): (Point<Pixels>, f32),
    turn: f32,
) -> Vec<Point<Pixels>> {
    let side = if child.x >= parent.x { 1. } else { -1. };
    let start = point(parent.x + px(side * parent_reach), parent.y);
    let end = point(child.x - px(side * child_reach), child.y);
    let room = f32::from(end.x - start.x).abs();
    let x = start.x + px(side * turn.min(room / 2.));
    right_angles(vec![start, point(x, parent.y), point(x, child.y), end])
}

/// A loop over a type's chip, its `rank`th from the inside out: up from the chip's top near its
/// right end, across, and down again, each further loop wider and taller.
pub(crate) fn loop_points(
    at: Point<Pixels>,
    reach: f32,
    scale: f32,
    rank: usize,
) -> Vec<Point<Pixels>> {
    let grow = LOOP_STEP * rank as f32 * scale;
    let top = at.y - px(13. * scale);
    let right = at.x + px(reach - 12. * scale + grow);
    let left = right - px(36. * scale + grow * 2.);
    let height = px(16. * scale + grow);
    vec![
        point(left, top),
        point(left, top - height),
        point(right, top - height),
        point(right, top),
    ]
}

/// Where a loop's label goes: over its top.
pub(crate) fn loop_label_at(points: &[Point<Pixels>]) -> Point<Pixels> {
    match points {
        [_, a, b, ..] => point((a.x + b.x) / 2., a.y - px(10.)),
        _ => points.first().copied().unwrap_or_default(),
    }
}

/// The same line with every slanted piece turned at right angles halfway across, and repeated
/// points dropped.
fn right_angles(points: Vec<Point<Pixels>>) -> Vec<Point<Pixels>> {
    let mut out: Vec<Point<Pixels>> = Vec::with_capacity(points.len() + 2);
    for p in points {
        if let Some(&last) = out.last() {
            let (dx, dy) = (f32::from(p.x - last.x).abs(), f32::from(p.y - last.y).abs());
            if dx < 0.01 && dy < 0.01 {
                continue;
            }
            if dx >= 0.01 && dy >= 0.01 {
                let mid = (last.x + p.x) / 2.;
                out.push(point(mid, last.y));
                out.push(point(mid, p.y));
            }
        }
        out.push(p);
    }
    out
}

/// Straight runs of one tone: level runs by their height, upright ones by their x, each as
/// spans along it, merged where they meet. Lines that share a stretch (a trunk) are drawn once
/// there, so their dashes neither double nor blur.
#[derive(Default)]
struct Runs {
    level: BTreeMap<i64, (f32, Vec<(f32, f32)>)>,
    upright: BTreeMap<i64, (f32, Vec<(f32, f32)>)>,
}

impl Runs {
    fn add(&mut self, a: Point<Pixels>, b: Point<Pixels>) {
        let (ax, ay, bx, by) = (
            f32::from(a.x),
            f32::from(a.y),
            f32::from(b.x),
            f32::from(b.y),
        );
        if !(ax.is_finite() && ay.is_finite() && bx.is_finite() && by.is_finite()) {
            return;
        }
        let key = |v: f32| (v * 8.).round() as i64;
        if (ay - by).abs() < 0.01 {
            let run = self.level.entry(key(ay)).or_insert((ay, Vec::new()));
            run.1.push((ax.min(bx), ax.max(bx)));
        } else if (ax - bx).abs() < 0.01 {
            let run = self.upright.entry(key(ax)).or_insert((ax, Vec::new()));
            run.1.push((ay.min(by), ay.max(by)));
        }
    }

    /// Merges each run's spans, less what `covered` already holds there.
    fn settle(&mut self, covered: &Runs) {
        for (runs, cover) in [
            (&mut self.level, &covered.level),
            (&mut self.upright, &covered.upright),
        ] {
            for (key, (_, spans)) in runs.iter_mut() {
                *spans = merge(std::mem::take(spans));
                if let Some((_, taken)) = cover.get(key) {
                    *spans = subtract(spans, taken);
                }
            }
        }
    }

    fn absorb(&mut self, other: &Runs) {
        for (mine, theirs) in [
            (&mut self.level, &other.level),
            (&mut self.upright, &other.upright),
        ] {
            for (key, (at, spans)) in theirs {
                let run = mine.entry(*key).or_insert((*at, Vec::new()));
                run.1.extend(spans.iter().copied());
                run.1 = merge(std::mem::take(&mut run.1));
            }
        }
    }
}

/// Spans merged where they overlap or touch, in order.
fn merge(mut spans: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f32, f32)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match out.last_mut() {
            Some(last) if start <= last.1 + 0.01 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// Merged `spans` less merged `taken`.
fn subtract(spans: &[(f32, f32)], taken: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    for &(start, end) in spans {
        let mut from = start;
        for &(t0, t1) in taken {
            if t1 <= from || t0 >= end {
                continue;
            }
            if t0 > from {
                out.push((from, t0));
            }
            from = from.max(t1);
        }
        if from < end {
            out.push((from, end));
        }
    }
    out
}

/// The dashes of a span from `start` to `end` along an axis, `origin` being where the pattern
/// starts on that axis: dash `k` covers `[origin + k·period, origin + k·period + on]`, so every
/// line along the same stretch has the same dashes. The walk counts dashes in f64 and is
/// bounded by [`DASHES_PER_RUN`]: a longer span comes back whole, drawn solid.
fn dashes_along(start: f32, end: f32, origin: f32, (on, off): (f32, f32)) -> Vec<(f32, f32)> {
    if !(start.is_finite() && end.is_finite() && origin.is_finite()) || end <= start {
        return Vec::new();
    }
    let period = f64::from(on + off);
    let (s, e) = (f64::from(start - origin), f64::from(end - origin));
    let first = (s / period).floor();
    let last = (e / period).ceil();
    if last - first > DASHES_PER_RUN {
        return vec![(start, end)];
    }
    let mut out = Vec::with_capacity((last - first) as usize);
    let mut k = first;
    while k < last {
        let a = (k * period).max(s);
        let b = (k * period + f64::from(on)).min(e);
        if b > a {
            out.push((a as f32 + origin, b as f32 + origin));
        }
        k += 1.;
    }
    out
}

/// Paints the lines: by tone, strongest first, each tone's runs merged and less what a
/// stronger tone already drew there, clipped to the view, then cut into dashes or dots, each a
/// 1 px quad on the pixel grid.
pub(crate) fn paint_runs(
    drawn: &[(usize, Vec<Point<Pixels>>, Tone)],
    bounds: Bounds<Pixels>,
    origin: Point<Pixels>,
    ink: Hsla,
    window: &mut Window,
) {
    let mut tones: BTreeMap<std::cmp::Reverse<Tone>, Runs> = BTreeMap::new();
    for (_, points, tone) in drawn {
        let runs = tones.entry(std::cmp::Reverse(*tone)).or_default();
        for pair in points.windows(2) {
            runs.add(pair[0], pair[1]);
        }
    }
    let scale = window.scale_factor();
    // A line's top edge on a device pixel, so it is one crisp pixel row wide.
    let snap = |at: f32| (((at - 0.5) * scale).round() / scale, 1.);
    let (left, right) = (
        f32::from(bounds.left()) - 2.,
        f32::from(bounds.right()) + 2.,
    );
    let (top, bottom) = (
        f32::from(bounds.top()) - 2.,
        f32::from(bounds.bottom()) + 2.,
    );
    let mut covered = Runs::default();
    for (std::cmp::Reverse(tone), mut runs) in tones {
        runs.settle(&covered);
        let color = ink.opacity(f32::from(tone.alpha) / 1000.);
        let pattern = if tone.dotted { DOT } else { DASH };
        for (y, spans) in runs.level.values() {
            if *y < top || *y > bottom {
                continue;
            }
            let (row, thick) = snap(*y);
            for &(a, b) in spans {
                for (s, e) in dashes_along(a.max(left), b.min(right), f32::from(origin.x), pattern)
                {
                    window.paint_quad(gpui::fill(
                        Bounds::new(point(px(s), px(row)), size(px(e - s), px(thick))),
                        color,
                    ));
                }
            }
        }
        for (x, spans) in runs.upright.values() {
            if *x < left || *x > right {
                continue;
            }
            let (column, thick) = snap(*x);
            for &(a, b) in spans {
                for (s, e) in dashes_along(a.max(top), b.min(bottom), f32::from(origin.y), pattern)
                {
                    window.paint_quad(gpui::fill(
                        Bounds::new(point(px(column), px(s)), size(px(thick), px(e - s))),
                        color,
                    ));
                }
            }
        }
        covered.absorb(&runs);
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

struct PillStyle<'a> {
    fill: Hsla,
    /// A hairline inside the chip: only where you are; otherwise its shadow's ring outlines it.
    border: Option<Hsla>,
    /// Its shadow, the ring first.
    shadow: &'a [Layer],
    /// The focus ring's glow.
    ring: Option<Hsla>,
}

/// Paints a chip's glow, shadow and body: the shadow as a browser draws a CSS `box-shadow`
/// (each layer's corner with its spread), then the fill and, where you are, the hairline.
fn paint_chip_body(rect: Bounds<Pixels>, radius: f32, style: &PillStyle, window: &mut Window) {
    if let Some(ring) = style.ring {
        window.paint_drop_shadows(
            rect,
            Corners::all(px(radius)),
            &[BoxShadow {
                color: ring,
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(3.),
                inset: false,
            }],
        );
    }
    studio_ui::ds::elevation::paint_layers(rect, radius, style.shadow, window);
    window.paint_quad(quad(
        rect,
        Corners::all(px(radius)),
        style.fill,
        px(if style.border.is_some() { 1. } else { 0. }),
        style.border.unwrap_or(gpui::transparent_black()),
        BorderStyle::Solid,
    ));
}

/// A chip's pieces shaped at `text_size`, and the pill around them centred on `at`: one line
/// high, its mark (when it has one) before them. What painting the chip draws, and what an
/// opened chip grows from.
fn pill_layout(
    at: Point<Pixels>,
    pieces: &[Piece],
    mark: bool,
    text_size: f32,
    palette: &Palette,
    window: &mut Window,
) -> (Bounds<Pixels>, Vec<ShapedLine>) {
    let s = text_size / CHIP_TEXT;
    let lines: Vec<ShapedLine> = pieces
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
    let height = text_size * 1.4 + 8. * s;
    let text_width: f32 = lines.iter().map(|l| f32::from(l.width)).sum::<f32>()
        + gap * lines.len().saturating_sub(1) as f32;
    let width = PILL_PAD_X * s * 2. + text_width + if mark { PILL_MARK * s + gap } else { 0. };
    let rect = Bounds::new(
        point(at.x - px(width / 2.), at.y - px(height / 2.)),
        size(px(width), px(height)),
    );
    (rect, lines)
}

/// A chip centred on `at`: an optional letter mark, then the pieces of text. Returns its bounds.
#[expect(clippy::too_many_arguments)]
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
    let (rect, lines) = pill_layout(at, pieces, mark.is_some(), text_size, palette, window);
    let s = text_size / CHIP_TEXT;
    let gap = PILL_GAP * s;
    let line_height = text_size * 1.4;
    let mark_size = PILL_MARK * s;
    // A chip is not a button: the scale's `sm` corner, zoomed with it, never past round.
    let radius = (studio_ui::ds::radii(cx).sm * s).min(f32::from(rect.size.height) / 2.);
    paint_chip_body(rect, radius, &style, window);
    let mut x = f32::from(rect.left()) + PILL_PAD_X * s;
    let middle = at.y;
    if let Some((tint, letter)) = mark {
        let center = point(px(x + mark_size / 2.), middle);
        let line = shape_letter(&letter, mark_size, palette, window);
        paint_letter_mark(center, tint, &line, mark_size, window, cx);
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

/// A chip opened to its whole name, laid out: its card, drawn from the top left of the chip it
/// opens and never smaller than it, and its name wrapped at [`OPEN_CHIP_MAX_WIDTH`].
struct OpenChip {
    rect: Bounds<Pixels>,
    lines: Vec<WrappedLine>,
    text_size: f32,
}

/// Lays out `chip` opened to its whole `label` at `text_size`.
fn open_chip_layout(
    chip: Bounds<Pixels>,
    label: &str,
    text_size: f32,
    palette: &Palette,
    window: &mut Window,
) -> OpenChip {
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
    let lines: Vec<WrappedLine> = window
        .text_system()
        .shape_text(text, px(text_size), &[run], Some(px(wrap)), None)
        .unwrap_or_default()
        .into_iter()
        .collect();
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
    OpenChip {
        rect: Bounds::new(chip.origin, size(px(width), px(height))),
        lines,
        text_size,
    }
}

/// Paints an opened chip: its card, its mark beside the first line, and its name.
fn paint_open_chip(
    open: &OpenChip,
    (tint, letter): (Hsla, SharedString),
    style: PillStyle,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    let (rect, text_size) = (open.rect, open.text_size);
    let s = text_size / CHIP_TEXT;
    let (gap, pad_x, mark_size) = (PILL_GAP * s, PILL_PAD_X * s, PILL_MARK * s);
    let line_height = text_size * 1.4;
    let pad_y = 4. * s;
    let radius = studio_ui::ds::radii(cx).sm * s;
    paint_chip_body(rect, radius, &style, window);
    // The mark stays where the chip had it, beside the first line.
    let left = f32::from(rect.left()) + pad_x;
    let first_middle = rect.top() + px(pad_y + line_height / 2.);
    let mark = shape_letter(&letter, mark_size, palette, window);
    paint_letter_mark(
        point(px(left + mark_size / 2.), first_middle),
        tint,
        &mark,
        mark_size,
        window,
        cx,
    );
    let mut y = rect.top() + px(pad_y);
    for line in &open.lines {
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
fn shape_letter(letter: &str, size_px: f32, palette: &Palette, window: &mut Window) -> ShapedLine {
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
            color: palette.on_tint,
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
        tint,
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

/// Whether the box around a line's points meets `bounds` grown by `margin`.
fn crosses(points: &[Point<Pixels>], bounds: Bounds<Pixels>, margin: f32) -> bool {
    let margin = px(margin);
    let Some(first) = points.first() else {
        return false;
    };
    let (mut min, mut max) = (*first, *first);
    for p in points {
        min = point(min.x.min(p.x), min.y.min(p.y));
        max = point(max.x.max(p.x), max.y.max(p.y));
    }
    max.x >= bounds.left() - margin
        && min.x <= bounds.right() + margin
        && max.y >= bounds.top() - margin
        && min.y <= bounds.bottom() + margin
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
        let across = [point(px(-5000.), px(300.)), point(px(5000.), px(300.))];
        assert!(crosses(&across, view, 400.));
        let below = [point(px(-5000.), px(5000.)), point(px(5000.), px(5000.))];
        assert!(!crosses(&below, view, 400.));
        // A route that turns around the view's corner crosses it.
        let around = [
            point(px(-100.), px(-100.)),
            point(px(900.), px(-100.)),
            point(px(900.), px(700.)),
        ];
        assert!(crosses(&around, view, 0.));
    }

    /// Two lines along the same stretch are cut into the same dashes, wherever each starts, so
    /// a shared trunk is drawn once; the walk ends, and a span too long to cut is drawn whole.
    #[test]
    fn lines_along_one_stretch_share_their_dashes() {
        let one = dashes_along(10., 60., 0., DASH);
        let other = dashes_along(10., 60., 0., DASH);
        assert_eq!(one, other);
        assert_eq!(one.first(), Some(&(10., 13.)));
        assert!(one.iter().all(|(a, b)| b - a <= 3. + 1e-4));
        // A dash is where the origin puts it, not where the span begins.
        assert_eq!(dashes_along(11., 20., 0., DASH).first(), Some(&(11., 13.)));
        assert_eq!(dashes_along(0., 20., 1., DASH).first(), Some(&(1., 4.)));
        assert!(dashes_along(0., 1e7, 0., DASH) == vec![(0., 1e7)]);
        assert!(dashes_along(0., f32::INFINITY, 0., DASH).is_empty());
        assert!(dashes_along(5., 5., 0., DASH).is_empty());
    }

    /// Lines of one tone that overlap are merged into one span; a stronger tone's span is taken
    /// out of a weaker one's, so each stretch is drawn once, in its strongest tone.
    #[test]
    fn overlapping_lines_are_drawn_once_in_their_strongest_tone() {
        let at = |x: f32, y: f32| point(px(x), px(y));
        let mut faint = Runs::default();
        faint.add(at(0., 10.), at(50., 10.));
        faint.add(at(40., 10.), at(100., 10.));
        faint.add(at(20., 0.), at(20., 30.));
        let mut strong = Runs::default();
        strong.add(at(30., 10.), at(60., 10.));
        let mut covered = Runs::default();
        strong.settle(&covered);
        covered.absorb(&strong);
        faint.settle(&covered);
        let level: Vec<&(f32, f32)> = faint.level.values().flat_map(|(_, s)| s).collect();
        assert_eq!(level, vec![&(0., 30.), &(60., 100.)]);
        let upright: Vec<&(f32, f32)> = faint.upright.values().flat_map(|(_, s)| s).collect();
        assert_eq!(upright, vec![&(0., 30.)]);
    }

    /// A slanted piece turns at right angles halfway across, and a line from a parent to its
    /// child leaves and enters their sides level, turning near the parent.
    #[test]
    fn lines_turn_at_right_angles() {
        let at = |x: f32, y: f32| point(px(x), px(y));
        let slanted = right_angles(vec![at(0., 0.), at(10., 20.)]);
        assert_eq!(
            slanted,
            vec![at(0., 0.), at(5., 0.), at(5., 20.), at(10., 20.)]
        );
        let line = elbow((at(0., 0.), 20.), (at(200., 50.), 30.), 18.);
        assert_eq!(line.first(), Some(&at(20., 0.)));
        assert_eq!(line.last(), Some(&at(170., 50.)));
        assert_eq!(line[1], at(38., 0.), "turns 18 px beyond the parent");
        assert!(
            line.windows(2)
                .all(|p| p[0].x == p[1].x || p[0].y == p[1].y)
        );
        // Two children in one column share the parent's upright.
        let other = elbow((at(0., 0.), 20.), (at(210., -40.), 40.), 18.);
        assert_eq!(other[1].x, line[1].x);
    }

    /// A line's end ring sits on the line, half a ring in from where it meets the chip: at the
    /// chip's vertical middle, whatever the zoom, the chip's size or where the chip stands.
    #[test]
    fn a_line_ends_in_a_ring_on_it() {
        let at = |x: f32, y: f32| point(px(x), px(y));
        for zoom in [0.3, 0.5, 1., 1.6, 3.] {
            for (child, reach) in [
                (at(240., 0.), 40.),
                (at(240., 90.), 80.),
                (at(-260., -70.), 140.),
                (at(30., 12.), 12.),
            ] {
                let parent = (at(0., 0.), 60. * zoom);
                let line = elbow(parent, (child, reach * zoom), ELBOW * zoom);
                let last = line.len() - 1;
                for (end, next, middle) in [(0, 1, parent.0.y), (last, last - 1, child.y)] {
                    let (end, next) = (line[end], line[next]);
                    let centre = ring_centre(end, next);
                    assert_eq!(centre.y, middle, "on the chip's middle at zoom {zoom}");
                    assert_eq!(centre.y, end.y, "on the line");
                    assert_eq!(f32::from(centre.x - end.x).abs(), RING / 2.);
                    assert!(
                        f32::from(centre.x - end.x) * f32::from(next.x - end.x) > 0.,
                        "inside the line, toward its other end"
                    );
                }
            }
        }
        // A chip opened to a name of several lines, taller and wider than it was: its lines and
        // their rings meet its sides level with its middle, not its first line.
        let card = Bounds::new(at(300., 100.), size(px(320.), px(124.)));
        let (middle, reach) = meets(card);
        let line = elbow((at(0., 0.), 60.), (middle, reach), ELBOW);
        let end = *line.last().unwrap();
        assert_eq!(end, at(300., 162.), "its left side, at its middle");
        assert_eq!(ring_centre(end, line[line.len() - 2]).y, px(162.));
        // An upright end: the ring moves along it only.
        assert_eq!(
            ring_centre(at(5., 10.), at(5., 40.)),
            at(5., 10. + RING / 2.)
        );
        assert_eq!(
            ring_centre(at(5., 10.), at(5., -40.)),
            at(5., 10. - RING / 2.)
        );
        // A piece of no length leaves it on the end.
        assert_eq!(ring_centre(at(5., 10.), at(5., 10.)), at(5., 10.));
    }
}
