//! Canvas layout, without GPUI, so it can run on a background thread and be tested.
//!
//! The schema view places types on a circle by sorted name only: counts never move a type
//! (constitution V). The exploration places each new node radially around the node it hangs
//! off, fanning away from where the walk came from, then relaxes overlaps for a fixed number of
//! iterations. Nodes laid out before keep their positions exactly, so expanding never moves
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

    fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    fn angle(self) -> f32 {
        self.y.atan2(self.x)
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

const RELAX_ITERATIONS: usize = 60;
const GAP: f32 = 6.;

/// Positions for the exploration.
pub fn explore_positions(nodes: &[LayoutNode]) -> Vec<Vec2> {
    let count = nodes.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (index, node) in nodes.iter().enumerate() {
        if let Some(parent) = node.parent.filter(|parent| *parent < index) {
            children[parent].push(index);
        }
    }
    let mut position = vec![Vec2::default(); count];
    let mut fixed = vec![false; count];
    let mut roots = 0;
    for index in 0..count {
        if let Some(previous) = nodes[index].previous {
            position[index] = previous;
            fixed[index] = true;
            continue;
        }
        let Some(parent) = nodes[index].parent.filter(|parent| *parent < index) else {
            position[index] = Vec2::new(roots as f32 * 400., 0.);
            roots += 1;
            continue;
        };
        let siblings = &children[parent];
        let slot = siblings.iter().position(|s| *s == index).unwrap_or(0);
        let total = siblings.len();
        let from_grandparent = nodes[parent]
            .parent
            .filter(|grandparent| *grandparent < parent)
            .map(|grandparent| (position[parent] - position[grandparent]).angle());
        let space = nodes[index].space;
        let distance = match nodes[index].kind {
            // Far enough out for the hub's label, which sits between it and its node.
            LayoutKind::Hub => nodes[parent].space + space + 70.,
            LayoutKind::Node | LayoutKind::More => {
                let arc = total as f32 * (2. * space + GAP) / (1.4 * PI);
                (nodes[parent].space + space + 50.).max(arc)
            }
        };
        let angle = match from_grandparent {
            None => -PI / 2. + TAU * slot as f32 / total as f32,
            Some(away) => {
                let step = (2. * space + GAP) / distance;
                let spread = ((total.saturating_sub(1)) as f32 * step).min(1.4 * PI);
                if total > 1 {
                    away - spread / 2. + spread * slot as f32 / (total - 1) as f32
                } else {
                    away
                }
            }
        };
        position[index] = position[parent] + Vec2::polar(angle, distance);
    }
    relax(nodes, &mut position, &fixed);
    position
}

/// Pushes overlapping nodes apart, pulling each moved node back towards its seed.
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
                        let delta = position[j] - position[i];
                        let distance = delta.length();
                        let wanted = nodes[i].space + nodes[j].space + GAP;
                        if distance >= wanted {
                            continue;
                        }
                        let direction = if distance > 1e-3 {
                            delta * (1. / distance)
                        } else {
                            // Coincident: separate along a direction fixed by the pair.
                            Vec2::polar((i * 31 + j * 17) as f32 * 0.618 * TAU, 1.)
                        };
                        let push = wanted - distance;
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
        for i in 0..count {
            if !fixed[i] {
                position[i] = position[i] + (seeds[i] - position[i]) * 0.05;
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
    fn relaxation_leaves_little_overlap() {
        let nodes = star(6, 8);
        let positions = explore_positions(&nodes);
        let mut overlapping = 0;
        for i in 0..nodes.len() {
            for j in i + 1..nodes.len() {
                let distance = (positions[j] - positions[i]).length();
                if distance < (nodes[i].space + nodes[j].space) * 0.6 {
                    overlapping += 1;
                }
            }
        }
        assert_eq!(overlapping, 0, "{positions:?}");
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
    fn schema_positions_depend_only_on_order() {
        let positions = schema_positions(3);
        assert_eq!(positions.len(), 3);
        assert!((positions[0].x).abs() < 1e-3 && positions[0].y < 0.);
        let (min, max) = extent(&positions, |_| 10.).unwrap();
        assert!(min.x < 0. && max.x > 0.);
    }
}
