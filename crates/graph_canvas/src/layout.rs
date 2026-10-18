//! Canvas layout, without GPUI, so it can run on a background thread and be tested.
//!
//! The ontology is laid out by stress majorization ([`ontology_positions`]): every pair of types
//! is placed as far apart as the relationships between them say, so related types sit together
//! and the most connected ones in the middle. It is a function of the schema alone (names,
//! relationships, label lengths) and the types the person placed: counts never move a type
//! (constitution V). The exploration places each new node in a column beside the node it hangs
//! off, on the side away from where the walk came from, then relaxes overlaps for a fixed number
//! of iterations. Nodes laid out before keep their positions exactly, so expanding never moves
//! what the person already sees. No randomness: the same input gives the same output.

use std::collections::HashMap;
use std::f32::consts::{PI, TAU};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    fn polar(angle: f32, length: f32) -> Self {
        Self::new(angle.cos() * length, angle.sin() * length)
    }
}

impl std::ops::Add for Vec2 {
    type Output = Vec2;
    fn add(self, other: Vec2) -> Vec2 {
        Vec2::new(self.x + other.x, self.y + other.y)
    }
}

impl std::ops::Sub for Vec2 {
    type Output = Vec2;
    fn sub(self, other: Vec2) -> Vec2 {
        Vec2::new(self.x - other.x, self.y - other.y)
    }
}

impl std::ops::Mul<f32> for Vec2 {
    type Output = Vec2;
    fn mul(self, scale: f32) -> Vec2 {
        Vec2::new(self.x * scale, self.y * scale)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    Node,
    Hub,
    More,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutNode {
    /// The node this one hangs off; it must come earlier in the list.
    pub parent: Option<usize>,
    pub kind: LayoutKind,
    /// The room the node and its label need.
    pub space: f32,
    /// Where it was drawn before; such a node does not move.
    pub previous: Option<Vec2>,
}

/// A type on the ontology map.
#[derive(Debug, Clone, PartialEq)]
pub struct OntologyNode {
    /// Half the width of the type's chip.
    pub half_width: f32,
    /// Where the person placed it: it stays there, and the rest is laid out around it.
    pub pinned: Option<Vec2>,
}

/// A relationship between two types. A type's relationships to itself are drawn as loops and
/// take no part in the layout.
#[derive(Debug, Clone, PartialEq)]
pub struct OntologyEdge {
    pub from: usize,
    pub to: usize,
    /// Half the width of the relationship's label.
    pub label_half_width: f32,
}

/// Half a chip's height, for overlap removal.
const CHIP_HALF_HEIGHT: f32 = 16.;
/// A relationship is at least this long, and longer as its chips and label need.
const HOP_MIN: f32 = 150.;
const HOP_PAD: f32 = 48.;
/// The room kept between chips once the layout settles.
const CHIP_GAP: f32 = 18.;
/// Between a component and the next, and around the grid of unrelated types.
const COMPONENT_GAP: f32 = 120.;
const STRESS_ITERATIONS: usize = 500;
const STRESS_SETTLED: f32 = 0.05;

/// Positions for the ontology: stress majorization (Gansner, Koren and North, 2004) over the
/// relationship graph, so geometric distance follows graph distance. Each relationship is as
/// long as its two chips and its label need, so the label fits between them; several
/// relationships between two types pull them together harder. Each connected group of types is
/// laid out alone, from a classical multidimensional scaling start (deterministic: no
/// randomness), turned so its long axis runs across the screen. The largest group is the map;
/// smaller groups follow below it, and types with no relationship stand in columns beside all
/// of them, in the order given. Chips are then pushed apart until none overlap. A placed type never
/// moves: it anchors its group, which keeps the place the person gave it.
pub fn ontology_positions(nodes: &[OntologyNode], edges: &[OntologyEdge]) -> Vec<Vec2> {
    let n = nodes.len();
    let mut position = vec![Vec2::default(); n];
    if n == 0 {
        return position;
    }
    // Pairs of related types: the longest label between them, and how many relationships.
    let mut pairs: std::collections::BTreeMap<(usize, usize), (f32, u32)> =
        std::collections::BTreeMap::new();
    for edge in edges {
        if edge.from == edge.to || edge.from >= n || edge.to >= n {
            continue;
        }
        let key = (edge.from.min(edge.to), edge.from.max(edge.to));
        let entry = pairs.entry(key).or_insert((0., 0));
        entry.0 = entry.0.max(edge.label_half_width);
        entry.1 += 1;
    }
    let mut adjacency: Vec<Vec<(usize, f32, u32)>> = vec![Vec::new(); n];
    for (&(a, b), &(label, count)) in &pairs {
        let hop = (nodes[a].half_width + nodes[b].half_width + 2. * label + HOP_PAD).max(HOP_MIN);
        adjacency[a].push((b, hop, count));
        adjacency[b].push((a, hop, count));
    }

    // Connected groups, each in index order; the largest first.
    let mut group_of = vec![usize::MAX; n];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for start in 0..n {
        if group_of[start] != usize::MAX {
            continue;
        }
        let id = groups.len();
        let mut members = vec![start];
        group_of[start] = id;
        let mut next = 0;
        while next < members.len() {
            let at = members[next];
            next += 1;
            for &(other, _, _) in &adjacency[at] {
                if group_of[other] == usize::MAX {
                    group_of[other] = id;
                    members.push(other);
                }
            }
        }
        members.sort_unstable();
        groups.push(members);
    }
    let (mut related, alone): (Vec<Vec<usize>>, Vec<Vec<usize>>) =
        groups.into_iter().partition(|members| members.len() > 1);
    related.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
    let alone: Vec<usize> = alone.into_iter().flatten().collect();

    let half = |i: usize| {
        Vec2::new(
            nodes[i].half_width + CHIP_GAP / 2.,
            CHIP_HALF_HEIGHT + CHIP_GAP / 2.,
        )
    };
    // Each group laid out on its own; a group holding a placed type keeps the place's frame.
    let mut laid: Vec<(Vec<usize>, Vec<Vec2>, bool)> = Vec::new();
    for members in &related {
        let mut local = stress_layout(members, nodes, &adjacency);
        let anchored = members.iter().any(|&i| nodes[i].pinned.is_some());
        let halves: Vec<Vec2> = members.iter().map(|&i| half(i)).collect();
        let fixed: Vec<bool> = members.iter().map(|&i| nodes[i].pinned.is_some()).collect();
        remove_overlaps(&mut local, &halves, &fixed);
        if !anchored {
            // Stress fixes how far apart types are relative to each other, not the frame's
            // proportions: widen a squarish map towards the screen's, so it fits a window at a
            // size where names can be read.
            widen(&mut local, &halves);
            remove_overlaps(&mut local, &halves, &fixed);
        }
        laid.push((members.clone(), local, anchored));
    }

    // The first group is centred unless a placed type anchors it; anchored groups stay where
    // their placed types put them; the rest stack below, left-aligned, in rows as wide as the
    // map.
    let bounds_of = |members: &[usize], at: &[Vec2]| -> (Vec2, Vec2) {
        let mut min = Vec2::new(f32::MAX, f32::MAX);
        let mut max = Vec2::new(f32::MIN, f32::MIN);
        for (k, &i) in members.iter().enumerate() {
            let h = half(i);
            min = Vec2::new(min.x.min(at[k].x - h.x), min.y.min(at[k].y - h.y));
            max = Vec2::new(max.x.max(at[k].x + h.x), max.y.max(at[k].y + h.y));
        }
        (min, max)
    };
    let mut placed_min = Vec2::new(f32::MAX, f32::MAX);
    let mut placed_max = Vec2::new(f32::MIN, f32::MIN);
    let grow = |min: Vec2, max: Vec2, placed_min: &mut Vec2, placed_max: &mut Vec2| {
        *placed_min = Vec2::new(placed_min.x.min(min.x), placed_min.y.min(min.y));
        *placed_max = Vec2::new(placed_max.x.max(max.x), placed_max.y.max(max.y));
    };
    let mut stacked: Vec<usize> = Vec::new();
    for (index, (members, local, anchored)) in laid.iter_mut().enumerate() {
        if *anchored {
            for (k, &i) in members.iter().enumerate() {
                position[i] = local[k];
            }
            let (min, max) = bounds_of(members, local);
            grow(min, max, &mut placed_min, &mut placed_max);
        } else if index == 0 {
            let (min, max) = bounds_of(members, local);
            let centre = (min + max) * 0.5;
            for (k, &i) in members.iter().enumerate() {
                position[i] = local[k] - centre;
            }
            grow(min - centre, max - centre, &mut placed_min, &mut placed_max);
        } else {
            stacked.push(index);
        }
    }
    if placed_min.x == f32::MAX {
        placed_min = Vec2::default();
        placed_max = Vec2::default();
    }
    let row_width = (placed_max.x - placed_min.x).max(900.);
    let mut cursor = Vec2::new(placed_min.x, placed_max.y + COMPONENT_GAP);
    let mut row_height: f32 = 0.;
    // A row's first group can be wider than the row: the unrelated types stand beyond it.
    let mut right = placed_max.x;
    for index in stacked {
        let (members, local, _) = &laid[index];
        let (min, max) = bounds_of(members, local);
        let size = max - min;
        if cursor.x > placed_min.x && cursor.x + size.x > placed_min.x + row_width {
            cursor = Vec2::new(placed_min.x, cursor.y + row_height + COMPONENT_GAP);
            row_height = 0.;
        }
        for (k, &i) in members.iter().enumerate() {
            position[i] = local[k] - min + cursor;
        }
        right = right.max(cursor.x + size.x);
        cursor.x += size.x + COMPONENT_GAP;
        row_height = row_height.max(size.y);
    }
    let bottom = if row_height > 0. {
        cursor.y + row_height
    } else {
        placed_max.y
    };

    // Types with no relationship: columns beside the map (screens are wide), in the order given,
    // unless placed; a grid when nothing is related at all.
    let free: Vec<usize> = alone
        .iter()
        .copied()
        .filter(|&i| nodes[i].pinned.is_none())
        .collect();
    for &i in &alone {
        if let Some(at) = nodes[i].pinned {
            position[i] = at;
        }
    }
    if !free.is_empty() {
        let cell = free.iter().map(|&i| 2. * half(i).x).fold(0., f32::max) + CHIP_GAP;
        let pitch = 2. * CHIP_HALF_HEIGHT + CHIP_GAP * 2.;
        if related.is_empty() {
            let columns = ((free.len() as f32).sqrt().ceil() as usize).max(1);
            for (k, &i) in free.iter().enumerate() {
                let (row, column) = (k / columns, k % columns);
                position[i] = Vec2::new(cell * (column as f32 + 0.5), row as f32 * pitch);
            }
        } else {
            let rows = (((bottom - placed_min.y) / pitch).floor() as usize).max(1);
            let left = right + COMPONENT_GAP;
            for (k, &i) in free.iter().enumerate() {
                let (column, row) = (k / rows, k % rows);
                position[i] = Vec2::new(
                    left + cell * (column as f32 + 0.5),
                    placed_min.y + pitch * (row as f32 + 0.5),
                );
            }
        }
    }
    position
}

/// One connected group by stress majorization, in the group's own frame (or, when a type in
/// it is placed, in the frame of the placed types).
fn stress_layout(
    members: &[usize],
    nodes: &[OntologyNode],
    adjacency: &[Vec<(usize, f32, u32)>],
) -> Vec<Vec2> {
    let m = members.len();
    let local: HashMap<usize, usize> = members.iter().enumerate().map(|(k, &i)| (i, k)).collect();
    // Graph distances along relationship lengths (Dijkstra from each type; groups are small).
    let mut distance = vec![vec![f32::INFINITY; m]; m];
    let mut strength = vec![vec![1f32; m]; m];
    for source in 0..m {
        let row = &mut distance[source];
        row[source] = 0.;
        let mut done = vec![false; m];
        for _ in 0..m {
            let Some(at) = (0..m)
                .filter(|&k| !done[k] && row[k].is_finite())
                .min_by(|&a, &b| row[a].total_cmp(&row[b]))
            else {
                break;
            };
            done[at] = true;
            for &(other, hop, count) in &adjacency[members[at]] {
                let Some(&k) = local.get(&other) else {
                    continue;
                };
                if row[at] + hop < row[k] {
                    row[k] = row[at] + hop;
                }
                if at == source {
                    // Several relationships between two types pull them together harder.
                    strength[source][k] = 1. + 0.5 * (count.saturating_sub(1)) as f32;
                }
            }
        }
    }
    let fixed: Vec<Option<Vec2>> = members.iter().map(|&i| nodes[i].pinned).collect();
    let mut x = classical_mds(&distance);
    let anchored: Vec<usize> = (0..m).filter(|&k| fixed[k].is_some()).collect();
    if anchored.is_empty() {
        x = stress_iterate(x, &distance, &strength, &fixed);
        align_to_long_axis(&mut x);
    } else {
        // Shift the start onto the placed types, then let the rest settle around them.
        let mut shift = Vec2::default();
        for &k in &anchored {
            shift = shift + (fixed[k].unwrap_or_default() - x[k]);
        }
        let shift = shift * (1. / anchored.len() as f32);
        for (k, point) in x.iter_mut().enumerate() {
            *point = fixed[k].unwrap_or(*point + shift);
        }
        x = stress_iterate(x, &distance, &strength, &fixed);
    }
    x
}

/// Classical multidimensional scaling: the two leading eigenvectors of the double-centred
/// squared distances, by power iteration from fixed starts.
fn classical_mds(distance: &[Vec<f32>]) -> Vec<Vec2> {
    let m = distance.len();
    if m == 1 {
        return vec![Vec2::default()];
    }
    let squared: Vec<Vec<f64>> = distance
        .iter()
        .map(|row| row.iter().map(|d| f64::from(*d) * f64::from(*d)).collect())
        .collect();
    let row_mean: Vec<f64> = squared
        .iter()
        .map(|r| r.iter().sum::<f64>() / m as f64)
        .collect();
    let total_mean = row_mean.iter().sum::<f64>() / m as f64;
    let mut b: Vec<Vec<f64>> = (0..m)
        .map(|i| {
            (0..m)
                .map(|j| -0.5 * (squared[i][j] - row_mean[i] - row_mean[j] + total_mean))
                .collect()
        })
        .collect();
    // Power iteration finds the largest eigenvalue; shift by a bound so it is the largest
    // positive one.
    let shift = b
        .iter()
        .map(|row| row.iter().map(|v| v.abs()).sum::<f64>())
        .fold(0., f64::max);
    let mut axes: Vec<(Vec<f64>, f64)> = Vec::new();
    for axis in 0..2 {
        let mut v: Vec<f64> = (0..m)
            .map(|i| (((i * 7919 + axis * 104_729) % 1009) as f64) - 504.)
            .collect();
        let mut value = 0.;
        for _ in 0..300 {
            let mut next: Vec<f64> = (0..m)
                .map(|i| (0..m).map(|j| b[i][j] * v[j]).sum::<f64>() + shift * v[i])
                .collect();
            let norm = next.iter().map(|x| x * x).sum::<f64>().sqrt();
            if norm < 1e-12 {
                break;
            }
            next.iter_mut().for_each(|x| *x /= norm);
            value = norm - shift;
            v = next;
        }
        // A consistent sign: the largest entry positive.
        if let Some(big) = v.iter().copied().max_by(|a, c| a.abs().total_cmp(&c.abs()))
            && big < 0.
        {
            v.iter_mut().for_each(|x| *x = -*x);
        }
        // Deflate, for the next axis.
        for i in 0..m {
            for j in 0..m {
                b[i][j] -= value * v[i] * v[j];
            }
        }
        axes.push((v, value.max(0.)));
    }
    (0..m)
        .map(|i| {
            Vec2::new(
                (axes[0].0[i] * axes[0].1.sqrt()) as f32,
                (axes[1].0[i] * axes[1].1.sqrt()) as f32,
            )
        })
        .collect()
}

/// Localized stress majorization: each type moves to the weighted mean of where every other
/// type says it should be, weights 1/d², until nothing moves. Fixed types stay.
fn stress_iterate(
    mut x: Vec<Vec2>,
    distance: &[Vec<f32>],
    strength: &[Vec<f32>],
    fixed: &[Option<Vec2>],
) -> Vec<Vec2> {
    let m = x.len();
    for _ in 0..STRESS_ITERATIONS {
        let mut moved: f32 = 0.;
        for a in 0..m {
            if fixed[a].is_some() {
                continue;
            }
            let (mut sum, mut weights) = (Vec2::default(), 0.);
            for b in 0..m {
                let d = distance[a][b];
                if a == b || !d.is_finite() || d <= 0. {
                    continue;
                }
                let w = strength[a][b] / (d * d);
                let mut apart = x[a] - x[b];
                let mut length = apart.length();
                if length < 1e-3 {
                    // Coincident: separate along a direction fixed by the pair.
                    apart = Vec2::polar((a * 31 + b * 17) as f32, 1.);
                    length = 1.;
                }
                sum = sum + (x[b] + apart * (d / length)) * w;
                weights += w;
            }
            if weights > 0. {
                let next = sum * (1. / weights);
                moved = moved.max((next - x[a]).length());
                x[a] = next;
            }
        }
        if moved < STRESS_SETTLED {
            break;
        }
    }
    x
}

/// The proportions a free map is widened towards (a 16:10 screen), and the most it is stretched.
const TARGET_ASPECT: f32 = 1.6;
const MOST_STRETCH: f32 = 1.4;

/// Widens a map narrower than [`TARGET_ASPECT`]: across by `s`, down by `1/s`, keeping its area.
fn widen(x: &mut [Vec2], half: &[Vec2]) {
    if x.len() < 3 {
        return;
    }
    let (mut min, mut max) = (Vec2::new(f32::MAX, f32::MAX), Vec2::new(f32::MIN, f32::MIN));
    for (p, h) in x.iter().zip(half) {
        min = Vec2::new(min.x.min(p.x - h.x), min.y.min(p.y - h.y));
        max = Vec2::new(max.x.max(p.x + h.x), max.y.max(p.y + h.y));
    }
    let aspect = (max.x - min.x) / (max.y - min.y).max(1.);
    if aspect >= TARGET_ASPECT {
        return;
    }
    let stretch = (TARGET_ASPECT / aspect).sqrt().min(MOST_STRETCH);
    let centre = (min + max) * 0.5;
    for p in x.iter_mut() {
        let d = *p - centre;
        *p = centre + Vec2::new(d.x * stretch, d.y / stretch);
    }
}

/// Turns a layout so its long axis runs across the screen (screens are wider than tall).
fn align_to_long_axis(x: &mut [Vec2]) {
    let m = x.len() as f32;
    if m < 2. {
        return;
    }
    let centre = x.iter().fold(Vec2::default(), |acc, p| acc + *p) * (1. / m);
    let (mut sxx, mut syy, mut sxy) = (0f32, 0f32, 0f32);
    for p in x.iter() {
        let d = *p - centre;
        sxx += d.x * d.x;
        syy += d.y * d.y;
        sxy += d.x * d.y;
    }
    let angle = 0.5 * (2. * sxy).atan2(sxx - syy);
    let (sin, cos) = (-angle).sin_cos();
    for p in x.iter_mut() {
        let d = *p - centre;
        *p = Vec2::new(d.x * cos - d.y * sin, d.x * sin + d.y * cos);
    }
}

/// Pushes overlapping chips apart along the axis they overlap least, until none overlap.
/// Fixed chips stay; the other one of a pair takes the whole push.
fn remove_overlaps(x: &mut [Vec2], half: &[Vec2], fixed: &[bool]) {
    let m = x.len();
    for _ in 0..300 {
        let mut any = false;
        for a in 0..m {
            for b in a + 1..m {
                let d = x[b] - x[a];
                let overlap_x = half[a].x + half[b].x - d.x.abs();
                let overlap_y = half[a].y + half[b].y - d.y.abs();
                if overlap_x <= 0. || overlap_y <= 0. || (fixed[a] && fixed[b]) {
                    continue;
                }
                any = true;
                let push = if overlap_x < overlap_y {
                    Vec2::new(if d.x >= 0. { overlap_x } else { -overlap_x }, 0.)
                } else {
                    Vec2::new(0., if d.y >= 0. { overlap_y } else { -overlap_y })
                };
                match (fixed[a], fixed[b]) {
                    (true, _) => x[b] = x[b] + push,
                    (_, true) => x[a] = x[a] - push,
                    _ => {
                        x[a] = x[a] - push * 0.5;
                        x[b] = x[b] + push * 0.5;
                    }
                }
            }
        }
        if !any {
            break;
        }
    }
}

/// Types on a circle in the order given (sorted by name), so positions depend only on names.
pub fn schema_positions(count: usize) -> Vec<Vec2> {
    match count {
        0 => Vec::new(),
        1 => vec![Vec2::default()],
        2 => vec![Vec2::new(-170., 0.), Vec2::new(170., 0.)],
        _ => {
            // Room for a chip per type around the ring.
            let radius = (count as f32 * 210. / TAU).max(190.);
            (0..count)
                .map(|i| Vec2::polar(-PI / 2. + TAU * i as f32 / count as f32, radius))
                .collect()
        }
    }
}

/// Where a query result's roots stand (spec 006): a ring for a few, as the ontology's types
/// do; a landscape grid for more, since a ring sized for thousands of roots is wider than the
/// canvas can zoom out to and its middle, all the view shows, is empty. Each root's subtree
/// then grows beside it, moved to free room where it would land on another.
pub fn result_root_positions(count: usize) -> Vec<Vec2> {
    const RING_MAX: usize = 12;
    const ACROSS: f32 = 640.;
    const DOWN: f32 = 200.;
    if count <= RING_MAX {
        return schema_positions(count);
    }
    // Columns for a grid about 16:9 on screen, given each cell's shape.
    let columns = ((count as f32 * 16. / 9. * DOWN / ACROSS).sqrt().ceil() as usize).max(1);
    let rows = count.div_ceil(columns);
    let origin = Vec2::new(
        -(columns as f32 - 1.) * ACROSS / 2.,
        -(rows as f32 - 1.) * DOWN / 2.,
    );
    (0..count)
        .map(|i| origin + Vec2::new((i % columns) as f32 * ACROSS, (i / columns) as f32 * DOWN))
        .collect()
}

const RELAX_ITERATIONS: usize = 60;
const GAP: f32 = 6.;

/// Positions for the exploration, as a mind map: a node's relationship hubs stand in a column
/// beside it, its neighbours through a hub in a column beyond the hub, each further step outward
/// on the same side. A root's hubs share its two sides, the heavier side filled last. Chips are
/// wide and short, so columns hold a neighbour every [`EXPLORE_ROW`] pixels where a ring would
/// need a chip's width: the map stays close enough to read. Each column is as tall as what hangs
/// off its members, so subtrees do not interleave. Nodes laid out before keep their places
/// exactly (constitution V); new ones that would overlap them are relaxed away.
pub fn explore_positions(nodes: &[LayoutNode]) -> Vec<Vec2> {
    let count = nodes.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (index, node) in nodes.iter().enumerate() {
        if let Some(parent) = node.parent.filter(|parent| *parent < index) {
            children[parent].push(index);
        }
    }
    // How tall each new subtree is: its chip, or its children's subtrees stacked.
    let mut height = vec![EXPLORE_ROW; count];
    for index in (0..count).rev() {
        let new_children: Vec<usize> = children[index]
            .iter()
            .copied()
            .filter(|c| nodes[*c].previous.is_none())
            .collect();
        if !new_children.is_empty() {
            height[index] = stacked_height(&new_children, &height).max(EXPLORE_ROW);
        }
    }
    let mut position = vec![Vec2::default(); count];
    let mut fixed = vec![false; count];
    let mut placed = vec![false; count];
    // Every placed chip's box (left, right, top, bottom), kept as chips are placed, so a column
    // looks for room among them without listing every node again.
    let mut taken: Vec<[f32; 4]> = Vec::with_capacity(count);
    let chip_box = |index: usize, at: Vec2| {
        [
            at.x - nodes[index].space - GAP / 2.,
            at.x + nodes[index].space + GAP / 2.,
            at.y - CHIP_HALF_HEIGHT - GAP / 2.,
            at.y + CHIP_HALF_HEIGHT + GAP / 2.,
        ]
    };
    let mut roots = 0;
    // Drawn nodes first, all of them: where a column ends is read from them.
    for index in 0..count {
        if let Some(previous) = nodes[index].previous {
            position[index] = previous;
            fixed[index] = true;
            placed[index] = true;
            taken.push(chip_box(index, previous));
        }
    }
    for index in 0..count {
        if !fixed[index] && nodes[index].parent.filter(|p| *p < index).is_none() {
            position[index] = Vec2::new(roots as f32 * 900., 0.);
            roots += 1;
            placed[index] = true;
            taken.push(chip_box(index, position[index]));
        }
        if !placed[index] {
            continue;
        }
        let mut new_children: Vec<usize> = children[index]
            .iter()
            .copied()
            .filter(|c| nodes[*c].previous.is_none())
            .collect();
        if new_children.is_empty() {
            continue;
        }
        // "+K more" ends its column, after the neighbours it stands for.
        new_children.sort_by_key(|c| nodes[*c].kind == LayoutKind::More);
        // Which way this node's children go: outward, away from where it hangs; a root's go
        // both ways.
        let outward = nodes[index].parent.filter(|p| *p < index).map(|p| {
            if position[index].x >= position[p].x {
                1.
            } else {
                -1.
            }
        });
        let mut sides: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
        match outward {
            Some(direction) => sides[usize::from(direction < 0.)] = new_children,
            None => {
                let mut weight = [0f32; 2];
                for child in new_children {
                    let side = usize::from(weight[1] < weight[0]);
                    weight[side] += height[child] + sibling_gap(height[child]);
                    sides[side].push(child);
                }
            }
        }
        // Where each side's column already ends: new members join it at the bottom, so a drawn
        // column only grows.
        let mut drawn_bottom: [Option<f32>; 2] = [None, None];
        for &child in &children[index] {
            if nodes[child].previous.is_none() {
                continue;
            }
            let side = usize::from(position[child].x < position[index].x);
            let bottom = position[child].y + EXPLORE_ROW / 2.;
            drawn_bottom[side] = Some(drawn_bottom[side].map_or(bottom, |b: f32| b.max(bottom)));
        }
        for (side, members) in sides.iter().enumerate() {
            if members.is_empty() {
                continue;
            }
            let direction = if side == 0 { 1. } else { -1. };
            let total = stacked_height(members, &height);
            let xs: Vec<f32> = members
                .iter()
                .map(|&child| {
                    let gap = match nodes[child].kind {
                        LayoutKind::Hub => EXPLORE_HUB_GAP,
                        LayoutKind::Node | LayoutKind::More => EXPLORE_NODE_GAP,
                    };
                    position[index].x + direction * (nodes[index].space + gap + nodes[child].space)
                })
                .collect();
            let span = members.iter().zip(&xs).fold(
                (f32::MAX, f32::MIN),
                |(left, right), (&child, &x)| {
                    (
                        left.min(x - nodes[child].space),
                        right.max(x + nodes[child].space),
                    )
                },
            );
            // Beside its parent, or under the column it joins; moved to the nearest room when
            // a chip already placed (drawn earlier, or another column of this layout) is in
            // the way. A column opened later beside one that reaches past its parent would
            // otherwise land on it, and drawn chips never move.
            let mut top = match drawn_bottom[side] {
                Some(bottom) => free_top(bottom, total, span, &taken, true),
                None => free_top(position[index].y - total / 2., total, span, &taken, false),
            };
            for (&child, &x) in members.iter().zip(&xs) {
                position[child] = Vec2::new(x, top + height[child] / 2.);
                placed[child] = true;
                taken.push(chip_box(child, position[child]));
                top += height[child] + sibling_gap(height[child]);
            }
        }
    }
    relax(nodes, &mut position, &fixed);
    position
}

/// Where a column `total` tall starts: at `ideal` when no chip in `taken` (left, right, top,
/// bottom) crosses it within `span` (the column's left and right), else at the nearest start
/// that no chip crosses. A column that only grows (`downward`) starts no higher than `ideal`.
/// Below every chip always fits, so there is always an answer.
fn free_top(ideal: f32, total: f32, span: (f32, f32), taken: &[[f32; 4]], downward: bool) -> f32 {
    // The blocked stretches of the column's span, merged; the room is the gaps between them.
    let mut blocks: Vec<(f32, f32)> = taken
        .iter()
        .filter(|[left, right, _, _]| *left < span.1 && *right > span.0)
        .map(|[_, _, top, bottom]| (*top, *bottom))
        .collect();
    blocks.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f32, f32)> = Vec::with_capacity(blocks.len());
    for (top, bottom) in blocks {
        match merged.last_mut() {
            Some(last) if top < last.1 => last.1 = last.1.max(bottom),
            _ => merged.push((top, bottom)),
        }
    }
    // Each gap from one stretch's bottom to the next one's top, and the open ends; the nearest
    // start in a gap the column fits in.
    let mut best: Option<f32> = None;
    let mut consider = |low: f32, high: f32| {
        let low = if downward { low.max(ideal) } else { low };
        // Compared as the clamp's own bounds: `high - low >= total` can hold in f32 while
        // `high - total` rounds below `low`, and `clamp` panics on reversed bounds.
        let last = high - total;
        if last < low {
            return;
        }
        let start = ideal.clamp(low, last);
        if best.is_none_or(|best| (start - ideal).abs() < (best - ideal).abs()) {
            best = Some(start);
        }
    };
    let mut floor = f32::NEG_INFINITY;
    for (top, bottom) in &merged {
        consider(floor, *top);
        floor = floor.max(*bottom);
    }
    consider(floor, f32::INFINITY);
    best.unwrap_or(ideal)
}

/// One row of an exploration column: a chip and the room under it.
pub const EXPLORE_ROW: f32 = 40.;
/// Between a subtree and the next in a column; single chips already have their row's room.
const EXPLORE_GROUP_GAP: f32 = 16.;

/// The room after a column member as tall as `height`: none for a single chip, a gap after a
/// subtree, so groups read as groups.
fn sibling_gap(height: f32) -> f32 {
    if height > EXPLORE_ROW {
        EXPLORE_GROUP_GAP
    } else {
        0.
    }
}

/// How tall a column of `members` stands.
fn stacked_height(members: &[usize], height: &[f32]) -> f32 {
    let mut total: f32 = members
        .iter()
        .map(|c| height[*c] + sibling_gap(height[*c]))
        .sum();
    if let Some(last) = members.last() {
        total -= sibling_gap(height[*last]);
    }
    total
}
/// From a node to its hubs' near edges, and from a hub to its neighbours'.
const EXPLORE_HUB_GAP: f32 = 44.;
const EXPLORE_NODE_GAP: f32 = 36.;

/// Pushes overlapping chips apart, pulling each moved node back towards its seed.
fn relax(nodes: &[LayoutNode], position: &mut [Vec2], fixed: &[bool]) {
    let count = nodes.len();
    if count < 2 || fixed.iter().all(|f| *f) {
        return;
    }
    let seeds = position.to_vec();
    let cell = nodes
        .iter()
        .map(|node| node.space)
        .fold(0., f32::max)
        .max(1.)
        * 2.
        + GAP;
    for _ in 0..RELAX_ITERATIONS {
        // Back towards where each was put, before the pushes: a round that ended on the pull
        // would leave every pushed pair overlapping again by a part of its push.
        for i in 0..count {
            if !fixed[i] {
                position[i] = position[i] + (seeds[i] - position[i]) * 0.05;
            }
        }
        let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for (index, point) in position.iter().enumerate() {
            grid.entry(cell_of(*point, cell)).or_default().push(index);
        }
        let mut moved = false;
        for i in 0..count {
            let (cx, cy) = cell_of(position[i], cell);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    let Some(bucket) = grid.get(&(cx + dx, cy + dy)) else {
                        continue;
                    };
                    for &j in bucket {
                        if j <= i || (fixed[i] && fixed[j]) {
                            continue;
                        }
                        // Chips are wide and short: they overlap as rectangles, and are pushed
                        // apart along the axis they overlap least, so a column stays a column.
                        let delta = position[j] - position[i];
                        let overlap_x = nodes[i].space + nodes[j].space + GAP - delta.x.abs();
                        let overlap_y = 2. * CHIP_HALF_HEIGHT + GAP - delta.y.abs();
                        if overlap_x <= 0. || overlap_y <= 0. {
                            continue;
                        }
                        let (direction, push) = if delta.length() < 1e-3 {
                            // Coincident: separate downward, the way columns grow.
                            (Vec2::new(0., 1.), overlap_y)
                        } else if overlap_y < overlap_x {
                            (
                                Vec2::new(0., if delta.y >= 0. { 1. } else { -1. }),
                                overlap_y,
                            )
                        } else {
                            (
                                Vec2::new(if delta.x >= 0. { 1. } else { -1. }, 0.),
                                overlap_x,
                            )
                        };
                        match (fixed[i], fixed[j]) {
                            (false, false) => {
                                position[i] = position[i] - direction * (push / 2.);
                                position[j] = position[j] + direction * (push / 2.);
                            }
                            (true, false) => position[j] = position[j] + direction * push,
                            (false, true) => position[i] = position[i] - direction * push,
                            (true, true) => {}
                        }
                        moved = true;
                    }
                }
            }
        }
        if !moved {
            break;
        }
    }
}

fn cell_of(point: Vec2, cell: f32) -> (i32, i32) {
    (
        (point.x / cell).floor() as i32,
        (point.y / cell).floor() as i32,
    )
}

/// The smallest box around every node, with its room: `(min, max)`.
pub fn extent(positions: &[Vec2], space: impl Fn(usize) -> f32) -> Option<(Vec2, Vec2)> {
    let mut iter = positions.iter().enumerate();
    let (first, point) = iter.next()?;
    let pad = space(first);
    let mut min = Vec2::new(point.x - pad, point.y - pad);
    let mut max = Vec2::new(point.x + pad, point.y + pad);
    for (index, point) in iter {
        let pad = space(index);
        min = Vec2::new(min.x.min(point.x - pad), min.y.min(point.y - pad));
        max = Vec2::new(max.x.max(point.x + pad), max.y.max(point.y + pad));
    }
    Some((min, max))
}

/// The node an arrow key moves to: the nearest in a 45° cone around `direction`, scored
/// `along + 2 × across` so straight ahead wins over diagonal.
pub fn nearest_in_direction(positions: &[Vec2], from: usize, direction: Vec2) -> Option<usize> {
    let origin = *positions.get(from)?;
    positions
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != from)
        .filter_map(|(index, point)| {
            let delta = *point - origin;
            let along = delta.x * direction.x + delta.y * direction.y;
            let across = (delta.x * direction.y - delta.y * direction.x).abs();
            (along > 0. && across <= along).then_some((index, along + 2. * across))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query result drawn as the canvas draws one (`rebuild_results`): `count` nodes, each
    /// linked to about two others picked by a fixed sequence, roots on a ring.
    fn result_like(count: usize) -> Vec<LayoutNode> {
        let mut next = 7_u64;
        let mut pick = |below: usize| {
            next = next
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (next >> 33) as usize % below
        };
        let mut parent = vec![None; count];
        for from in 0..count {
            for _ in 0..2 {
                let to = pick(count);
                if from < to && parent[to].is_none() {
                    parent[to] = Some(from);
                }
            }
        }
        let roots: Vec<usize> = (0..count).filter(|i| parent[*i].is_none()).collect();
        let mut fixed = vec![None; count];
        for (root, position) in roots.iter().zip(result_root_positions(roots.len())) {
            fixed[*root] = Some(position);
        }
        (0..count)
            .map(|i| LayoutNode {
                parent: parent[i],
                kind: LayoutKind::Node,
                space: 40.,
                previous: fixed[i],
            })
            .collect()
    }

    /// `free_top` as it was first written, testing every block for every candidate: the
    /// oracle for the merged-gap search.
    fn free_top_by_testing(
        ideal: f32,
        total: f32,
        span: (f32, f32),
        taken: &[[f32; 4]],
        downward: bool,
    ) -> f32 {
        let blocks: Vec<(f32, f32)> = taken
            .iter()
            .filter(|[left, right, _, _]| *left < span.1 && *right > span.0)
            .map(|[_, _, top, bottom]| (*top, *bottom))
            .collect();
        let fits = |start: f32| {
            blocks
                .iter()
                .all(|(top, bottom)| start + total <= *top || start >= *bottom)
        };
        if fits(ideal) {
            return ideal;
        }
        blocks
            .iter()
            .flat_map(|(top, bottom)| [*bottom, *top - total])
            .filter(|start| (!downward || *start >= ideal) && fits(*start))
            .min_by(|a, b| (a - ideal).abs().total_cmp(&(b - ideal).abs()))
            .unwrap_or(ideal)
    }

    #[test]
    fn many_result_roots_stand_in_a_landscape_grid_a_view_can_hold() {
        assert_eq!(result_root_positions(5), schema_positions(5));
        let roots = result_root_positions(4_000);
        let (min, max) = extent(&roots, |_| 0.).unwrap();
        let (width, height) = (max.x - min.x, max.y - min.y);
        // Wider than tall, and within what the canvas shows at its smallest zoom (0.05) on a
        // 1,300-pixel view, unlike a ring of 4,000.
        assert!(width > height, "{width} × {height}");
        assert!(width < 1_300. / 0.05 * 2., "{width}");
        let ring = schema_positions(4_000);
        let (ring_min, ring_max) = extent(&ring, |_| 0.).unwrap();
        assert!(ring_max.x - ring_min.x > 4. * width);
        // Every root has its own cell.
        let distinct: std::collections::HashSet<(i32, i32)> = roots
            .iter()
            .map(|p| (p.x.round() as i32, p.y.round() as i32))
            .collect();
        assert_eq!(distinct.len(), 4_000);
    }

    #[test]
    fn free_room_is_found_as_testing_every_block_finds_it() {
        let mut next = 11_u64;
        let mut draw = |below: u32| {
            next = next
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((next >> 33) % u64::from(below)) as f32
        };
        for case in 0..2_000 {
            let taken: Vec<[f32; 4]> = (0..(case % 12))
                .map(|_| {
                    let (x, y) = (draw(400), draw(600));
                    [x, x + 20. + draw(120), y, y + 10. + draw(60)]
                })
                .collect();
            let span = {
                let left = draw(400);
                (left, left + 20. + draw(200))
            };
            let (ideal, total, downward) = (draw(600), 10. + draw(150), case % 3 == 0);
            let found = free_top(ideal, total, span, &taken, downward);
            let expected = free_top_by_testing(ideal, total, span, &taken, downward);
            // Both answers are the nearest fitting start; on a tie either side is right.
            assert!(
                (found - ideal).abs() == (expected - ideal).abs(),
                "case {case}: {found} vs {expected} (ideal {ideal}, total {total})"
            );
        }
    }

    /// A gap whose height equals the column's in f32 (`high - low == total`) while
    /// `high - total` rounds below `low`: the clamp's bounds would be reversed.
    #[test]
    fn a_gap_one_rounding_short_of_a_column_is_passed_over() {
        let (low, high, total) = (202.546_78_f32, 922.546_75_f32, 720.);
        assert!(high - low >= total && high - total < low, "the rounding this guards");
        let taken = [[0., 100., 0., low], [0., 100., high, 1_000.]];
        let start = free_top(300., total, (0., 100.), &taken, false);
        assert_eq!(start, 1_000., "below every chip");
    }

    /// Instrument for spec 006's scale check: how long a result's layout takes as it grows.
    /// Run with `cargo test -p graph_canvas --release explore_layout_time -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "instrument: layout time by node count"]
    fn explore_layout_time() {
        for count in [500, 1_000, 2_000, 4_000, 10_000] {
            let nodes = result_like(count);
            let start = std::time::Instant::now();
            let positions = explore_positions(&nodes);
            eprintln!("{count} nodes: {:.2?}", start.elapsed());
            assert_eq!(positions.len(), count);
        }
    }

    fn star(hubs: usize, per_hub: usize) -> Vec<LayoutNode> {
        let mut nodes = vec![LayoutNode {
            parent: None,
            kind: LayoutKind::Node,
            space: 24.,
            previous: None,
        }];
        for _ in 0..hubs {
            let hub = nodes.len();
            nodes.push(LayoutNode {
                parent: Some(0),
                kind: LayoutKind::Hub,
                space: 34.,
                previous: None,
            });
            for _ in 0..per_hub {
                nodes.push(LayoutNode {
                    parent: Some(hub),
                    kind: LayoutKind::Node,
                    space: 22.,
                    previous: None,
                });
            }
        }
        nodes
    }

    #[test]
    fn the_same_input_lays_out_the_same() {
        let nodes = star(4, 8);
        assert_eq!(explore_positions(&nodes), explore_positions(&nodes));
        assert_eq!(schema_positions(5), schema_positions(5));
    }

    #[test]
    fn nodes_laid_out_before_do_not_move() {
        let nodes = star(3, 8);
        let first = explore_positions(&nodes);
        let mut grown = nodes.clone();
        for (node, point) in grown.iter_mut().zip(&first) {
            node.previous = Some(*point);
        }
        grown.extend(star(1, 8).into_iter().skip(1).map(|mut node| {
            node.parent = node
                .parent
                .map(|p| if p == 0 { 0 } else { p + nodes.len() - 1 });
            node
        }));
        let second = explore_positions(&grown);
        assert_eq!(&second[..first.len()], &first[..]);
    }

    #[test]
    fn new_members_join_the_bottom_of_a_drawn_column() {
        let mut nodes = star(1, 4);
        let first = explore_positions(&nodes);
        for (node, at) in nodes.iter_mut().zip(&first) {
            node.previous = Some(*at);
        }
        // Two more neighbours and a "+K more" come in under the hub.
        for kind in [LayoutKind::Node, LayoutKind::More, LayoutKind::Node] {
            nodes.push(LayoutNode {
                parent: Some(1),
                kind,
                space: 22.,
                previous: None,
            });
        }
        let next = explore_positions(&nodes);
        assert_eq!(&next[..first.len()], &first[..], "drawn nodes moved");
        let lowest_drawn = first[2..].iter().map(|p| p.y).fold(f32::MIN, f32::max);
        let (a, more, b) = (
            next[first.len()],
            next[first.len() + 1],
            next[first.len() + 2],
        );
        assert!(
            a.y > lowest_drawn && b.y > a.y,
            "{a:?} {b:?} below {lowest_drawn}"
        );
        assert!(more.y > b.y, "+K more ends the column");
    }

    fn assert_apart(nodes: &[LayoutNode], positions: &[Vec2]) {
        for i in 0..nodes.len() {
            for j in i + 1..nodes.len() {
                let d = positions[j] - positions[i];
                let apart_x = d.x.abs() >= nodes[i].space + nodes[j].space;
                let apart_y = d.y.abs() >= 2. * CHIP_HALF_HEIGHT;
                assert!(apart_x || apart_y, "{i} and {j} overlap: {positions:?}");
            }
        }
    }

    #[test]
    fn exploration_chips_do_not_overlap() {
        let nodes = star(6, 8);
        assert_apart(&nodes, &explore_positions(&nodes));

        // Opened one at a time, as a person clicks: a column added later under a hub whose
        // neighbour's column already reaches past it finds room rather than landing on a drawn
        // chip (a person's four acquaintances, then the one who knows them).
        let chip = |parent: Option<usize>, kind: LayoutKind, space: f32| LayoutNode {
            parent,
            kind,
            space,
            previous: None,
        };
        let mut nodes = vec![chip(None, LayoutKind::Node, 70.)];
        for _ in 0..4 {
            nodes.push(chip(Some(0), LayoutKind::Hub, 50.));
        }
        let settle = |nodes: &mut Vec<LayoutNode>| {
            let positions = explore_positions(nodes);
            assert_apart(nodes, &positions);
            for (node, at) in nodes.iter_mut().zip(&positions) {
                node.previous = Some(*at);
            }
        };
        settle(&mut nodes);
        let same_side: Vec<usize> = (2..5)
            .filter(|hub| {
                (nodes[*hub].previous.unwrap().x > 0.) == (nodes[1].previous.unwrap().x > 0.)
            })
            .collect();
        for _ in 0..4 {
            nodes.push(chip(Some(1), LayoutKind::Node, 80.));
        }
        settle(&mut nodes);
        nodes.push(chip(Some(same_side[0]), LayoutKind::Node, 80.));
        settle(&mut nodes);
    }

    #[test]
    fn an_exploration_reads_as_a_compact_mind_map() {
        let (hubs, per_hub) = (4, 8);
        let nodes = star(hubs, per_hub);
        let at = explore_positions(&nodes);
        let hub_index = |h: usize| 1 + h * (per_hub + 1);
        // The root's hubs share its two sides.
        let right = (0..hubs).filter(|h| at[hub_index(*h)].x > at[0].x).count();
        assert_eq!(right, hubs / 2);
        for h in 0..hubs {
            let hub = hub_index(h);
            let side = (at[hub].x - at[0].x).signum();
            for k in 1..=per_hub {
                let node = hub + k;
                // Each neighbour is beyond its hub, on the same side, in one column.
                assert_eq!((at[node].x - at[hub].x).signum(), side);
                assert!((at[node].x - at[hub + 1].x).abs() < 1.);
                // Close: a hub's width and a short gap away, not a ring's radius.
                let reach = nodes[hub].space + nodes[node].space + EXPLORE_NODE_GAP;
                assert!((at[node].x - at[hub].x).abs() <= reach + 1.);
            }
        }
        // The whole map is as tall as its rows, and about three chips wide on each side.
        let (min, max) = extent(&at, |i| nodes[i].space).unwrap();
        let rows_per_side = (hubs / 2 * per_hub) as f32;
        assert!(max.y - min.y <= rows_per_side * EXPLORE_ROW + EXPLORE_GROUP_GAP + 40.);
        assert!(max.x - min.x < 2. * (24. + 34. * 2. + 22. * 2. + 44. + 36.) + 48.);
    }

    #[test]
    fn a_graph_at_the_node_cap_lays_out_finitely() {
        // 1 + 222 × 9 = 1,999 nodes, just under the canvas cap.
        let nodes = star(222, 8);
        let started = std::time::Instant::now();
        let positions = explore_positions(&nodes);
        eprintln!(
            "explore_positions: {} nodes in {:?}",
            nodes.len(),
            started.elapsed()
        );
        assert_eq!(positions.len(), nodes.len());
        assert!(positions.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
    }

    #[test]
    fn arrows_move_to_the_nearest_node_in_their_cone() {
        let points = [
            Vec2::new(0., 0.),
            Vec2::new(100., 10.),
            Vec2::new(60., 55.),
            Vec2::new(-80., 0.),
            Vec2::new(0., 90.),
        ];
        assert_eq!(nearest_in_direction(&points, 0, Vec2::new(1., 0.)), Some(1));
        assert_eq!(
            nearest_in_direction(&points, 0, Vec2::new(-1., 0.)),
            Some(3)
        );
        assert_eq!(nearest_in_direction(&points, 0, Vec2::new(0., 1.)), Some(4));
        assert_eq!(nearest_in_direction(&points, 0, Vec2::new(0., -1.)), None);
    }

    /// A schema shaped like a CRM's: four hubs, satellites tied to one to three of them, a few
    /// ties between satellites, two relationships between one pair, loops, and a type with no
    /// relationship at all.
    fn crm_like() -> (Vec<OntologyNode>, Vec<OntologyEdge>) {
        let n = 26;
        let nodes: Vec<OntologyNode> = (0..n)
            .map(|i| OntologyNode {
                half_width: 40. + (i % 5) as f32 * 9.,
                pinned: None,
            })
            .collect();
        let edge = |from: usize, to: usize| OntologyEdge {
            from,
            to,
            label_half_width: 30. + ((from * 3 + to) % 4) as f32 * 8.,
        };
        let hubs = [0, 1, 2, 3];
        let mut edges = vec![edge(0, 1), edge(1, 2), edge(2, 0), edge(2, 3), edge(3, 0)];
        for s in 4..25 {
            edges.push(edge(s, hubs[s % 4]));
            if s % 3 == 0 {
                edges.push(edge(s, hubs[(s + 1) % 4]));
            }
            if s % 5 == 0 {
                edges.push(edge(s, s - 1));
            }
        }
        edges.push(edge(7, 3));
        edges.push(edge(0, 0));
        edges.push(edge(1, 1));
        // Type 25 has no relationship.
        (nodes, edges)
    }

    fn crossings(positions: &[Vec2], edges: &[OntologyEdge]) -> usize {
        let cross =
            |p: Vec2, q: Vec2, r: Vec2| (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x);
        let mut count = 0;
        for (i, a) in edges.iter().enumerate() {
            for b in &edges[i + 1..] {
                let shared = [a.from, a.to].iter().any(|v| *v == b.from || *v == b.to);
                if shared || a.from == a.to || b.from == b.to {
                    continue;
                }
                let (p1, p2) = (positions[a.from], positions[a.to]);
                let (p3, p4) = (positions[b.from], positions[b.to]);
                let d1 = cross(p3, p4, p1);
                let d2 = cross(p3, p4, p2);
                let d3 = cross(p1, p2, p3);
                let d4 = cross(p1, p2, p4);
                if d1 * d2 < 0. && d3 * d4 < 0. {
                    count += 1;
                }
            }
        }
        count
    }

    fn edge_length(positions: &[Vec2], edges: &[OntologyEdge]) -> f32 {
        edges
            .iter()
            .filter(|e| e.from != e.to)
            .map(|e| (positions[e.from] - positions[e.to]).length())
            .sum()
    }

    #[test]
    fn the_ontology_lays_out_the_same_every_time_and_nothing_overlaps() {
        let (nodes, edges) = crm_like();
        let first = ontology_positions(&nodes, &edges);
        assert_eq!(first, ontology_positions(&nodes, &edges));
        assert!(first.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
        for a in 0..nodes.len() {
            for b in a + 1..nodes.len() {
                let d = first[b] - first[a];
                let apart_x = d.x.abs() >= nodes[a].half_width + nodes[b].half_width;
                let apart_y = d.y.abs() >= 2. * CHIP_HALF_HEIGHT;
                assert!(apart_x || apart_y, "types {a} and {b} overlap");
            }
        }
    }

    #[test]
    fn related_types_sit_together_and_hubs_in_the_middle() {
        let (nodes, edges) = crm_like();
        let at = ontology_positions(&nodes, &edges);
        let related: Vec<(usize, usize)> = edges
            .iter()
            .filter(|e| e.from != e.to)
            .map(|e| (e.from.min(e.to), e.from.max(e.to)))
            .collect();
        let (mut near, mut near_count, mut far, mut far_count) = (0., 0, 0., 0);
        for a in 0..25 {
            for b in a + 1..25 {
                let d = (at[a] - at[b]).length();
                if related.contains(&(a, b)) {
                    near += d;
                    near_count += 1;
                } else {
                    far += d;
                    far_count += 1;
                }
            }
        }
        assert!(near / near_count as f32 * 1.5 < far / far_count as f32);
        // The hubs are nearer the map's centre than the satellites on average.
        let centre = (0..25).fold(Vec2::default(), |c, i| c + at[i]) * (1. / 25.);
        let spread = |range: std::ops::Range<usize>| {
            let len = range.len() as f32;
            range.map(|i| (at[i] - centre).length()).sum::<f32>() / len
        };
        assert!(spread(0..4) < spread(4..25));
        // Landscape: the map is wider than it is tall.
        let (min, max) = extent(&at[..25], |i| nodes[i].half_width).unwrap();
        assert!(max.x - min.x > max.y - min.y);
    }

    #[test]
    fn the_ontology_reads_better_than_a_ring() {
        let (nodes, edges) = crm_like();
        let map = ontology_positions(&nodes, &edges);
        let ring = schema_positions(nodes.len());
        assert!(crossings(&map, &edges) * 2 < crossings(&ring, &edges));
        assert!(edge_length(&map, &edges) < edge_length(&ring, &edges));
    }

    #[test]
    fn unrelated_types_sit_beside_the_map_and_placed_types_stay() {
        let (mut nodes, edges) = crm_like();
        let at = ontology_positions(&nodes, &edges);
        let rightmost_related = (0..25).map(|i| at[i].x).fold(f32::MIN, f32::max);
        assert!(at[25].x > rightmost_related + COMPONENT_GAP / 2.);

        let target = Vec2::new(-900., -400.);
        nodes[0].pinned = Some(target);
        let pinned = ontology_positions(&nodes, &edges);
        assert_eq!(pinned[0], target);
        // Its neighbours gather around the place it was given.
        let neighbour = pinned[1];
        assert!((neighbour - target).length() < (at[1] - target).length());
    }

    /// A narrow map with rows of smaller groups stacked under it, as wide as the row they make:
    /// the unrelated types stand beyond them all, overlapping no chip.
    #[test]
    fn unrelated_types_stand_clear_of_the_groups_stacked_under_the_map() {
        let chip = |half_width| OntologyNode {
            half_width,
            pinned: None,
        };
        // The map: five compact types around a hub. Under it: six related pairs.
        let mut nodes: Vec<OntologyNode> = (0..5).map(|_| chip(30.)).collect();
        nodes.extend((0..12).map(|_| chip(50.)));
        let unrelated = nodes.len()..nodes.len() + 8;
        nodes.extend(unrelated.clone().map(|_| chip(40.)));
        let mut edges: Vec<OntologyEdge> = (1..5)
            .map(|to| OntologyEdge {
                from: 0,
                to,
                label_half_width: 10.,
            })
            .collect();
        edges.extend((0..6).map(|pair| OntologyEdge {
            from: 5 + 2 * pair,
            to: 6 + 2 * pair,
            label_half_width: 40.,
        }));
        let at = ontology_positions(&nodes, &edges);
        let overlaps = |i: usize, j: usize| {
            (at[i].x - at[j].x).abs() < nodes[i].half_width + nodes[j].half_width
                && (at[i].y - at[j].y).abs() < 2. * CHIP_HALF_HEIGHT
        };
        for alone in unrelated {
            for other in (0..nodes.len()).filter(|&other| other != alone) {
                assert!(
                    !overlaps(alone, other),
                    "type {alone} at {:?} overlaps type {other} at {:?}",
                    at[alone],
                    at[other]
                );
            }
        }
    }

    #[test]
    fn small_ontologies_lay_out() {
        assert!(ontology_positions(&[], &[]).is_empty());
        let one = [OntologyNode {
            half_width: 40.,
            pinned: None,
        }];
        assert_eq!(ontology_positions(&one, &[]).len(), 1);
        let two = [one[0].clone(), one[0].clone()];
        let edge = [OntologyEdge {
            from: 0,
            to: 1,
            label_half_width: 30.,
        }];
        let at = ontology_positions(&two, &edge);
        // Side by side, with room for the label between them.
        assert!((at[0] - at[1]).length() >= 40. + 40. + 60.);
        assert!((at[0].y - at[1].y).abs() < 1.);
    }

    #[test]
    fn schema_positions_depend_only_on_order() {
        let positions = schema_positions(3);
        assert_eq!(positions.len(), 3);
        assert!((positions[0].x).abs() < 1e-3 && positions[0].y < 0.);
        let (min, max) = extent(&positions, |_| 10.).unwrap();
        assert!(min.x < 0. && max.x > 0.);
    }
}
