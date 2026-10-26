//! Canvas layout, without GPUI, so it can run on a background thread and be tested.
//!
//! The exploration places each new node in a column beside the node it hangs off, on the side
//! away from where the walk came from, then relaxes overlaps for a fixed number of iterations.
//! Nodes laid out before keep their positions exactly, so expanding never moves what the person
//! already sees. No randomness: the same input gives the same output.

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

/// Half a chip's height, for overlap removal.
const CHIP_HALF_HEIGHT: f32 = 16.;

/// Chips on a circle in the order given, so positions depend only on that order: a few
/// result roots stand this way.
fn ring_positions(count: usize) -> Vec<Vec2> {
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

/// Where a query result's roots stand (spec 006): a ring for a few; a landscape grid for more, since a ring sized for thousands of roots is wider than the
/// canvas can zoom out to and its middle, all the view shows, is empty. Each root's subtree
/// then grows beside it, moved to free room where it would land on another.
pub fn result_root_positions(count: usize) -> Vec<Vec2> {
    const RING_MAX: usize = 12;
    const ACROSS: f32 = 640.;
    const DOWN: f32 = 200.;
    if count <= RING_MAX {
        return ring_positions(count);
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

/// The smallest box around every node, with its room: `(min, max)`. A node takes `space` to
/// each side, and as much above and below up to a chip's half height: a chip is wider than
/// it is tall.
pub fn extent(positions: &[Vec2], space: impl Fn(usize) -> f32) -> Option<(Vec2, Vec2)> {
    let pads = |index: usize| {
        let across = space(index);
        Vec2::new(across, across.min(CHIP_HALF_HEIGHT))
    };
    let mut iter = positions.iter().enumerate();
    let (first, point) = iter.next()?;
    let pad = pads(first);
    let mut min = Vec2::new(point.x - pad.x, point.y - pad.y);
    let mut max = Vec2::new(point.x + pad.x, point.y + pad.y);
    for (index, point) in iter {
        let pad = pads(index);
        min = Vec2::new(min.x.min(point.x - pad.x), min.y.min(point.y - pad.y));
        max = Vec2::new(max.x.max(point.x + pad.x), max.y.max(point.y + pad.y));
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
        assert_eq!(result_root_positions(5), ring_positions(5));
        let roots = result_root_positions(4_000);
        let (min, max) = extent(&roots, |_| 0.).unwrap();
        let (width, height) = (max.x - min.x, max.y - min.y);
        // Wider than tall, and within what the canvas shows at its smallest zoom (0.05) on a
        // 1,300-pixel view, unlike a ring of 4,000.
        assert!(width > height, "{width} × {height}");
        assert!(width < 1_300. / 0.05 * 2., "{width}");
        let ring = ring_positions(4_000);
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
        assert!(
            high - low >= total && high - total < low,
            "the rounding this guards"
        );
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
        assert_eq!(ring_positions(5), ring_positions(5));
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

    #[test]
    fn ring_positions_depend_only_on_order() {
        let positions = ring_positions(3);
        assert_eq!(positions.len(), 3);
        assert!((positions[0].x).abs() < 1e-3 && positions[0].y < 0.);
        let (min, max) = extent(&positions, |_| 10.).unwrap();
        assert!(min.x < 0. && max.x > 0.);
    }
}
