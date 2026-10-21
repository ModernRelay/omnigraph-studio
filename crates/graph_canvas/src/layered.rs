//! The map of a small ontology (spec 006, "Ontology page"): types stand in columns and every
//! relationship runs between them along right-angle pieces, named on its own upright.
//!
//! It follows the steps Sugiyama, Tagawa and Toda (1981) set out, each deterministic:
//!
//! 1. Each type gets a column, the most connected first, where the sum of its distances to its
//!    related types is least: the most connected stand in the middle, the types around them in
//!    the columns either side. Related types never share a column, and a column holds at most
//!    [`column_cap`] types. A relationship's direction does not place it: its label names it.
//! 2. A relationship that passes columns gets a waypoint in each.
//! 3. Each column is put in an order with few crossings (barycentres swept back and forth,
//!    then neighbours swapped), then placed down the column so lines run as straight as that
//!    order allows: a least-squares placement under the order's spacing, by pooling adjacent
//!    violators column by column.
//! 4. Between two columns each relationship turns on an upright of its own, as wide apart as
//!    their labels, ordered to cross little; each meets its types' sides at a place of its own,
//!    in the order of where it goes.

use std::collections::BTreeMap;

use crate::layout::Vec2;

/// A relationship between two types, by their places in the group laid out.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub from: usize,
    pub to: usize,
    /// Half the width of its label, when every label is shown.
    pub label_half_width: f32,
}

/// Where a relationship runs: right-angle pieces from a side of the type it leaves to a side of
/// the type it reaches, in that order, and where its label sits when every label is shown.
/// Empty for a relationship from a type to itself, which the painter draws over its type.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Route {
    pub points: Vec<Vec2>,
    pub label_at: Option<Vec2>,
}

/// A group laid out: each type's centre, and each link's route, in the links' order.
#[derive(Debug, Clone, PartialEq)]
pub struct Layered {
    pub positions: Vec<Vec2>,
    pub routes: Vec<Route>,
    /// Which column each type stands in, from the left.
    pub columns: Vec<usize>,
}

/// Half a chip's height, with the room a chip keeps above and below it.
const CHIP_HALF: f32 = 16.;
/// Between two chips in a column, beyond their halves.
const ROW_GAP: f32 = 16.;
/// From a chip's edge to a line passing it in the column.
const LINE_CLEAR: f32 = 12.;
/// Between two lines passing through a column.
const LINE_GAP: f32 = 10.;
/// Above a type with a relationship to itself: room for its loop.
const LOOP_ROOM: f32 = 14.;
/// From the widest chip of a column to the first track beside it, and between tracks.
const CHANNEL_PAD: f32 = 24.;
const TRACK_GAP: f32 = 8.;
/// Around a label, when every line is named: to the next label's track, and to a chip.
const LABEL_ROOM: f32 = 14.;
/// The places on a chip's side where its named lines meet it, this far apart, within
/// [`PORT_SPREAD`] of its middle.
const PORT_STEP: f32 = 10.;
const PORT_SPREAD: f32 = 10.;
/// How many times the columns are reordered, and the placement down them relaxed.
const ORDER_SWEEPS: usize = 24;
const PLACE_SWEEPS: usize = 80;
/// A line this close to level is made level where its column has room: a step this small
/// reads as a flaw, not a bend.
const STRAIGHTEN: f32 = 6.;

/// The most types one column holds: about twice the square root of the types, so that a map
/// of columns this tall comes out wider than tall, as screens are.
pub fn column_cap(types: usize) -> usize {
    ((types as f32).sqrt() * 2.).ceil().max(3.) as usize
}

/// Lays out a small ontology. `half_widths` are the chips' half widths; `links` join the types
/// (a link from a type to itself gets an empty route). A route runs from the link's `from` to
/// its `to`, whichever column each stands in.
pub fn layered(half_widths: &[f32], links: &[Link]) -> Layered {
    let n = half_widths.len();
    if n == 0 {
        return Layered {
            positions: Vec::new(),
            routes: vec![Route::default(); links.len()],
            columns: Vec::new(),
        };
    }
    let mut looped = vec![false; n];
    // Related pairs, whichever way they point, with how many links each stands for.
    let mut pairs: BTreeMap<(usize, usize), u32> = BTreeMap::new();
    for link in links {
        if link.from >= n || link.to >= n {
            continue;
        }
        if link.from == link.to {
            looped[link.from] = true;
            continue;
        }
        *pairs
            .entry((link.from.min(link.to), link.from.max(link.to)))
            .or_default() += 1;
    }
    let pairs: Vec<(usize, usize, u32)> = pairs.iter().map(|(&(u, v), &w)| (u, v, w)).collect();
    let column = assign_columns(n, &pairs, column_cap(n));
    let columns = column.iter().copied().max().map_or(0, |m| m + 1);

    // Elements: the types, then the waypoints. A link's path is its elements left to right.
    let mut elements: Vec<Element> = (0..n)
        .map(|i| Element {
            column: column[i],
            node: Some(i),
        })
        .collect();
    let mut paths: Vec<Option<(Vec<usize>, bool)>> = Vec::with_capacity(links.len());
    for link in links {
        if link.from >= n || link.to >= n || link.from == link.to {
            paths.push(None);
            continue;
        }
        let reversed = column[link.from] > column[link.to];
        let (left, right) = if reversed {
            (link.to, link.from)
        } else {
            (link.from, link.to)
        };
        let mut path = vec![left];
        for c in column[left] + 1..column[right] {
            elements.push(Element {
                column: c,
                node: None,
            });
            path.push(elements.len() - 1);
        }
        path.push(right);
        paths.push(Some((path, reversed)));
    }

    // Segments between neighbouring columns, each link's own.
    let mut segments: Vec<Segment> = Vec::new();
    let mut link_segments: Vec<Vec<usize>> = vec![Vec::new(); links.len()];
    for (index, path) in paths.iter().enumerate() {
        let Some((path, _)) = path else { continue };
        for pair in path.windows(2) {
            segments.push(Segment {
                a: pair[0],
                b: pair[1],
                link: index,
            });
            link_segments[index].push(segments.len() - 1);
        }
    }
    let element_count = elements.len();
    // Each element's neighbours on its left and on its right, weighted: waypoints pull harder,
    // so trunks run straight.
    let mut left_of: Vec<Vec<(usize, f32)>> = vec![Vec::new(); element_count];
    let mut right_of: Vec<Vec<(usize, f32)>> = vec![Vec::new(); element_count];
    for segment in &segments {
        let stiff = match (elements[segment.a].node, elements[segment.b].node) {
            (None, None) => 8.,
            (None, _) | (_, None) => 2.,
            _ => 1.,
        };
        right_of[segment.a].push((segment.b, stiff));
        left_of[segment.b].push((segment.a, stiff));
    }

    // Order within columns, starting from the types' order.
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); columns];
    for (e, element) in elements.iter().enumerate() {
        layers[element.column].push(e);
    }
    order_layers(&mut layers, &left_of, &right_of);

    // Placement down the columns.
    let separation = |above: usize, below: usize| -> f32 {
        let room = match (elements[above].node, elements[below].node) {
            (Some(_), Some(_)) => 2. * CHIP_HALF + ROW_GAP,
            (Some(_), None) | (None, Some(_)) => CHIP_HALF + LINE_CLEAR,
            (None, None) => LINE_GAP,
        };
        let looped_below = elements[below].node.is_some_and(|i| looped[i]);
        room + if looped_below { LOOP_ROOM } else { 0. }
    };
    let y = place_down(&layers, &left_of, &right_of, separation);

    // Ports: where each segment meets its two elements' sides.
    let mut port_start = vec![0f32; segments.len()];
    let mut port_end = vec![0f32; segments.len()];
    for (id, segment) in segments.iter().enumerate() {
        port_start[id] = y[segment.a];
        port_end[id] = y[segment.b];
    }
    let mut leaving: Vec<Vec<usize>> = vec![Vec::new(); element_count];
    let mut arriving: Vec<Vec<usize>> = vec![Vec::new(); element_count];
    for (id, segment) in segments.iter().enumerate() {
        leaving[segment.a].push(id);
        arriving[segment.b].push(id);
    }
    for e in 0..element_count {
        if elements[e].node.is_none() {
            continue;
        }
        // Each side's lines in the order of their other ends, so they leave without
        // crossing.
        let mut out = leaving[e].clone();
        out.sort_by(|&s, &t| {
            y[segments[s].b]
                .total_cmp(&y[segments[t].b])
                .then(segments[s].link.cmp(&segments[t].link))
        });
        let toward: Vec<f32> = out.iter().map(|&s| y[segments[s].b]).collect();
        for (k, at) in ports(y[e], &toward).into_iter().enumerate() {
            port_start[out[k]] = at;
        }
        let mut into = arriving[e].clone();
        into.sort_by(|&s, &t| {
            y[segments[s].a]
                .total_cmp(&y[segments[t].a])
                .then(segments[s].link.cmp(&segments[t].link))
        });
        let toward: Vec<f32> = into.iter().map(|&s| y[segments[s].a]).collect();
        for (k, at) in ports(y[e], &toward).into_iter().enumerate() {
            port_end[into[k]] = at;
        }
    }

    // Columns across, and tracks in the channels between them.
    let half_width = |e: usize| elements[e].node.map_or(0., |i| half_widths[i]);
    let widths: Vec<f32> = (0..columns)
        .map(|c| {
            layers[c]
                .iter()
                .map(|&e| 2. * half_width(e))
                .fold(0., f32::max)
        })
        .collect();
    let mut track_of: Vec<Option<usize>> = vec![None; segments.len()];
    let mut channel_width = vec![0f32; columns.saturating_sub(1)];
    let mut channel_pad = vec![CHANNEL_PAD; columns.saturating_sub(1)];
    let mut channel_gap = vec![TRACK_GAP; columns.saturating_sub(1)];
    for c in 0..columns.saturating_sub(1) {
        let in_channel: Vec<usize> = (0..segments.len())
            .filter(|&s| elements[segments[s].a].column == c)
            .collect();
        // Each segment turns on an upright of its own, where its label goes; uprights in an
        // order that crosses little, as far apart as the widest label needs.
        let uprights: Vec<Upright> = in_channel
            .iter()
            .map(|&s| Upright {
                low: port_start[s].min(port_end[s]),
                high: port_start[s].max(port_end[s]),
                entry: port_start[s],
                exit: port_end[s],
            })
            .collect();
        let order = order_tracks(&uprights);
        let widest = in_channel
            .iter()
            .map(|&s| links[segments[s].link].label_half_width)
            .fold(0., f32::max);
        channel_pad[c] = CHANNEL_PAD.max(widest + LABEL_ROOM);
        channel_gap[c] = TRACK_GAP.max(2. * widest + LABEL_ROOM);
        channel_width[c] =
            2. * channel_pad[c] + channel_gap[c] * order.len().saturating_sub(1) as f32;
        for (track, &k) in order.iter().enumerate() {
            track_of[in_channel[k]] = Some(track);
        }
    }
    let mut column_x = vec![0f32; columns];
    for c in 1..columns {
        column_x[c] = column_x[c - 1] + widths[c - 1] / 2. + channel_width[c - 1] + widths[c] / 2.;
    }
    let track_x = |s: usize| -> Option<f32> {
        let c = elements[segments[s].a].column;
        let track = track_of[s]?;
        Some(column_x[c] + widths[c] / 2. + channel_pad[c] + channel_gap[c] * track as f32)
    };

    // Routes.
    let mut routes = vec![Route::default(); links.len()];
    for (index, path) in paths.iter().enumerate() {
        let Some((path, reversed)) = path else {
            continue;
        };
        let ids = &link_segments[index];
        let first = path[0];
        let mut points = vec![Vec2::new(
            column_x[elements[first].column] + half_width(first),
            port_start[ids[0]],
        )];
        let mut label_at = None;
        for (k, &s) in ids.iter().enumerate() {
            if let Some(x) = track_x(s) {
                points.push(Vec2::new(x, port_start[s]));
                points.push(Vec2::new(x, port_end[s]));
                if k == 0 {
                    label_at = Some(Vec2::new(x, (port_start[s] + port_end[s]) / 2.));
                }
            }
            let b = segments[s].b;
            if elements[b].node.is_some() {
                points.push(Vec2::new(
                    column_x[elements[b].column] - half_width(b),
                    port_end[s],
                ));
            }
        }
        let mut points = simplify(points);
        if *reversed {
            points.reverse();
        }
        routes[index] = Route { points, label_at };
    }

    // Centred on the middle of its chips.
    let positions: Vec<Vec2> = (0..n)
        .map(|i| Vec2::new(column_x[column[i]], y[i]))
        .collect();
    let (mut min, mut max) = (Vec2::new(f32::MAX, f32::MAX), Vec2::new(f32::MIN, f32::MIN));
    for (i, p) in positions.iter().enumerate() {
        min = Vec2::new(min.x.min(p.x - half_widths[i]), min.y.min(p.y - CHIP_HALF));
        max = Vec2::new(max.x.max(p.x + half_widths[i]), max.y.max(p.y + CHIP_HALF));
    }
    let centre = (min + max) * 0.5;
    let shift = |p: Vec2| p - centre;
    Layered {
        positions: positions.into_iter().map(shift).collect(),
        routes: routes
            .into_iter()
            .map(|route| Route {
                points: route.points.into_iter().map(shift).collect(),
                label_at: route.label_at.map(shift),
            })
            .collect(),
        columns: column,
    }
}

/// A type, or a waypoint where lines pass a column.
#[derive(Debug, Clone, Copy)]
struct Element {
    column: usize,
    node: Option<usize>,
}

/// A line between elements of neighbouring columns: `a` on the left, `b` on the right.
#[derive(Debug, Clone)]
struct Segment {
    a: usize,
    b: usize,
    /// The link it carries.
    link: usize,
}

/// A segment's upright in a channel, from `low` to `high`: its line comes in from the left at
/// `entry` and leaves to the right at `exit`.
#[derive(Debug, Clone)]
struct Upright {
    low: f32,
    high: f32,
    entry: f32,
    exit: f32,
}

/// Each type's column, so that related types stand in neighbouring columns: the most connected
/// types first, each in the column its placed neighbours pull it to (where the sum of its
/// distances to them is least; on a tie, the column holding fewer, then the one nearer the
/// middle); then each moved again wherever all its neighbours pull it closer, until none moves.
/// The most connected type stands in the middle of about `n / cap` columns, which hold every
/// type that fits; a type opens a column beyond them only when none has room for it. Related
/// types never share a column, and no column holds more than `cap`. Columns are numbered from
/// 0, with none empty.
///
/// Two hubs that share many neighbours are best two columns apart, the shared ones between
/// them, and that is not a single move away from the hubs side by side: the second most
/// connected type is tried one, two and three columns from the first, and the arrangement
/// whose lines are shortest in all is kept.
fn assign_columns(n: usize, pairs: &[(usize, usize, u32)], cap: usize) -> Vec<usize> {
    let mut best: Option<(i64, Vec<usize>)> = None;
    for apart in 1..=3 {
        let column = arrange_columns(n, pairs, cap, apart);
        let length: i64 = pairs
            .iter()
            .map(|&(u, v, w)| i64::from(w) * column[u].abs_diff(column[v]) as i64)
            .sum();
        if best.as_ref().is_none_or(|(shortest, _)| length < *shortest) {
            best = Some((length, column));
        }
    }
    best.map(|(_, column)| column).unwrap_or_default()
}

/// [`assign_columns`] with the second most connected type `apart` columns from the first.
fn arrange_columns(n: usize, pairs: &[(usize, usize, u32)], cap: usize, apart: i64) -> Vec<usize> {
    let mut neighbours: Vec<Vec<(usize, i64)>> = vec![Vec::new(); n];
    for &(u, v, w) in pairs {
        neighbours[u].push((v, i64::from(w)));
        neighbours[v].push((u, i64::from(w)));
    }
    let degree: Vec<i64> = neighbours
        .iter()
        .map(|ns| ns.iter().map(|n| n.1).sum())
        .collect();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| degree[b].cmp(&degree[a]).then(a.cmp(&b)));
    // The columns either side of the middle one, where the most connected type stands.
    let reach = (n.div_ceil(cap) / 2).max(1) as i64;
    let mut column: Vec<Option<i64>> = vec![None; n];
    let mut count: BTreeMap<i64, usize> = BTreeMap::new();
    let cost = |v: usize, c: i64, column: &[Option<i64>]| -> i64 {
        neighbours[v]
            .iter()
            .filter_map(|&(o, w)| Some(w * (c - column[o]?).abs()))
            .sum()
    };
    // The best column for `v`, and its cost, with the others where they stand.
    let best = |v: usize, column: &[Option<i64>], count: &BTreeMap<i64, usize>| -> (i64, i64) {
        let taken: Vec<i64> = neighbours[v]
            .iter()
            .filter_map(|&(o, _)| column[o])
            .collect();
        let held = |c: i64| count.get(&c).copied().unwrap_or(0);
        let room = |c: i64| column[v] == Some(c) || held(c) < cap;
        let pick = |from: i64, to: i64| {
            (from..=to)
                .filter(|c| !taken.contains(c) && room(*c))
                .map(|c| (c, cost(v, c, column)))
                .min_by(|a, b| {
                    a.1.cmp(&b.1)
                        .then(held(a.0).cmp(&held(b.0)))
                        .then(a.0.abs().cmp(&b.0.abs()))
                        .then(a.0.cmp(&b.0))
                })
        };
        let low = count.keys().next().copied().unwrap_or(0).min(-reach);
        let high = count.keys().next_back().copied().unwrap_or(0).max(reach);
        // A column beyond either end is empty, so there is always one.
        pick(low, high)
            .or_else(|| pick(low - 1, high + 1))
            .unwrap_or((high + 1, 0))
    };
    for (k, &v) in order.iter().enumerate() {
        let c = if k == 1 {
            apart
        } else {
            best(v, &column, &count).0
        };
        column[v] = Some(c);
        *count.entry(c).or_default() += 1;
    }
    for _ in 0..32 {
        let mut moved = false;
        for &v in &order {
            let Some(from) = column[v] else { continue };
            let (to, lower) = best(v, &column, &count);
            if to != from && lower < cost(v, from, &column) {
                if let Some(k) = count.get_mut(&from) {
                    *k -= 1;
                    if *k == 0 {
                        count.remove(&from);
                    }
                }
                *count.entry(to).or_default() += 1;
                column[v] = Some(to);
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    let used: Vec<i64> = count.keys().copied().collect();
    column
        .iter()
        .map(|c| {
            let c = c.unwrap_or(0);
            used.iter().position(|u| *u == c).unwrap_or(0)
        })
        .collect()
}

/// Orders each column for few crossings: barycentre sweeps left to right and back, each
/// followed by swapping neighbours where that crosses less; the order that crossed least is
/// kept.
fn order_layers(
    layers: &mut [Vec<usize>],
    left_of: &[Vec<(usize, f32)>],
    right_of: &[Vec<(usize, f32)>],
) {
    let count = left_of.len();
    let mut place = vec![0usize; count];
    let index = |layers: &[Vec<usize>], place: &mut [usize]| {
        for layer in layers {
            for (k, &e) in layer.iter().enumerate() {
                place[e] = k;
            }
        }
    };
    index(layers, &mut place);
    // A first order from the left: each column by its neighbours on the left.
    for layer in layers.iter_mut().skip(1) {
        sort_by_barycentre(layer, left_of, &place);
        for (k, &e) in layer.iter().enumerate() {
            place[e] = k;
        }
    }
    let mut best = layers.to_vec();
    let mut least = crossings(layers, right_of, &place);
    for sweep in 0..ORDER_SWEEPS {
        if sweep % 2 == 0 {
            for layer in layers.iter_mut().skip(1) {
                sort_by_barycentre(layer, left_of, &place);
                for (k, &e) in layer.iter().enumerate() {
                    place[e] = k;
                }
            }
        } else {
            for c in (0..layers.len().saturating_sub(1)).rev() {
                sort_by_barycentre(&mut layers[c], right_of, &place);
                for (k, &e) in layers[c].iter().enumerate() {
                    place[e] = k;
                }
            }
        }
        transpose(layers, left_of, right_of, &mut place);
        let crossed = crossings(layers, right_of, &place);
        if crossed < least {
            least = crossed;
            best = layers.to_vec();
        }
    }
    layers.clone_from_slice(&best);
}

/// Sorts a column by the mean place of each element's neighbours in `side`; an element with
/// none keeps its place.
fn sort_by_barycentre(layer: &mut [usize], side: &[Vec<(usize, f32)>], place: &[usize]) {
    let mut keyed: Vec<(f32, usize, usize)> = layer
        .iter()
        .map(|&e| {
            let (sum, weight) = side[e].iter().fold((0f32, 0f32), |(s, w), &(o, k)| {
                (s + place[o] as f32 * k, w + k)
            });
            let at = if weight > 0. {
                sum / weight
            } else {
                place[e] as f32
            };
            (at, place[e], e)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    for (slot, (_, _, e)) in layer.iter_mut().zip(keyed) {
        *slot = e;
    }
}

/// How much two neighbours `u` above `v` cross their lines to one side: pairs of their
/// neighbours there in the opposite order, weighted.
fn pair_crossings(u: usize, v: usize, side: &[Vec<(usize, f32)>], place: &[usize]) -> f32 {
    let mut crossed = 0.;
    for &(a, wa) in &side[u] {
        for &(b, wb) in &side[v] {
            if place[a] > place[b] {
                crossed += wa * wb;
            }
        }
    }
    crossed
}

/// Swaps neighbours in a column wherever that crosses less on both sides, until none does.
fn transpose(
    layers: &mut [Vec<usize>],
    left_of: &[Vec<(usize, f32)>],
    right_of: &[Vec<(usize, f32)>],
    place: &mut [usize],
) {
    for _ in 0..8 {
        let mut improved = false;
        for layer in layers.iter_mut() {
            for k in 0..layer.len().saturating_sub(1) {
                let (u, v) = (layer[k], layer[k + 1]);
                let now =
                    pair_crossings(u, v, left_of, place) + pair_crossings(u, v, right_of, place);
                let swapped =
                    pair_crossings(v, u, left_of, place) + pair_crossings(v, u, right_of, place);
                if swapped < now {
                    layer.swap(k, k + 1);
                    place[u] = k + 1;
                    place[v] = k;
                    improved = true;
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// Every crossing between neighbouring columns, weighted: segments sorted by their left end,
/// counting for each the weight already seen further down on the right (a Fenwick tree).
fn crossings(layers: &[Vec<usize>], right_of: &[Vec<(usize, f32)>], place: &[usize]) -> f32 {
    let mut total = 0.;
    for c in 0..layers.len().saturating_sub(1) {
        let width = layers[c + 1].len();
        let mut segments: Vec<(usize, usize, f32)> = layers[c]
            .iter()
            .flat_map(|&a| {
                right_of[a]
                    .iter()
                    .map(move |&(b, w)| (place[a], place[b], w))
            })
            .collect();
        segments.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(&y.1)));
        let mut tree = vec![0f32; width + 1];
        let mut seen = 0f32;
        for (_, b, w) in segments {
            // Weight seen at places up to b.
            let mut at_or_above = 0f32;
            let mut i = b + 1;
            while i > 0 {
                at_or_above += tree[i];
                i &= i - 1;
            }
            total += w * (seen - at_or_above);
            seen += w;
            let mut i = b + 1;
            while i <= width {
                tree[i] += w;
                i += i.isolate_lowest_one();
            }
        }
    }
    total
}

/// Each element's place down its column: as near the weighted mean of its neighbours as the
/// column's order and spacing allow, column by column, sweeping across and back.
fn place_down(
    layers: &[Vec<usize>],
    left_of: &[Vec<(usize, f32)>],
    right_of: &[Vec<(usize, f32)>],
    separation: impl Fn(usize, usize) -> f32,
) -> Vec<f32> {
    let count = left_of.len();
    let mut y = vec![0f32; count];
    let offsets: Vec<Vec<f32>> = layers
        .iter()
        .map(|layer| {
            let mut offset = Vec::with_capacity(layer.len());
            let mut at = 0.;
            for (k, &e) in layer.iter().enumerate() {
                if k > 0 {
                    at += separation(layer[k - 1], e);
                }
                offset.push(at);
            }
            offset
        })
        .collect();
    for (layer, offset) in layers.iter().zip(&offsets) {
        let middle = offset.last().copied().unwrap_or(0.) / 2.;
        for (&e, &o) in layer.iter().zip(offset) {
            y[e] = o - middle;
        }
    }
    for sweep in 0..PLACE_SWEEPS {
        let order: Vec<usize> = if sweep % 2 == 0 {
            (0..layers.len()).collect()
        } else {
            (0..layers.len()).rev().collect()
        };
        for c in order {
            let layer = &layers[c];
            let wanted: Vec<(f32, f32)> = layer
                .iter()
                .map(|&e| {
                    let (sum, weight) = left_of[e]
                        .iter()
                        .chain(&right_of[e])
                        .fold((0f32, 0f32), |(s, w), &(o, k)| (s + y[o] * k, w + k));
                    if weight > 0. {
                        (sum / weight, weight)
                    } else {
                        (y[e], 1e-3)
                    }
                })
                .collect();
            let placed = pool_adjacent(
                &wanted
                    .iter()
                    .zip(&offsets[c])
                    .map(|(&(at, w), &o)| (at - o, w))
                    .collect::<Vec<_>>(),
            );
            for ((&e, &o), z) in layer.iter().zip(&offsets[c]).zip(placed) {
                y[e] = z + o;
            }
        }
    }
    straighten(layers, left_of, right_of, &separation, &mut y);
    y
}

/// Makes lines that are nearly level level: an element moves to its neighbour's height when its
/// column has room there and fewer of its lines are left a small step from level.
fn straighten(
    layers: &[Vec<usize>],
    left_of: &[Vec<(usize, f32)>],
    right_of: &[Vec<(usize, f32)>],
    separation: &impl Fn(usize, usize) -> f32,
    y: &mut [f32],
) {
    let count = left_of.len();
    let mut slot = vec![(0usize, 0usize); count];
    for (c, layer) in layers.iter().enumerate() {
        for (k, &e) in layer.iter().enumerate() {
            slot[e] = (c, k);
        }
    }
    let small = |step: f32| step > 0.01 && step <= STRAIGHTEN;
    let steps = |e: usize, at: f32, y: &[f32]| {
        left_of[e]
            .iter()
            .chain(&right_of[e])
            .filter(|&&(o, _)| small((at - y[o]).abs()))
            .count()
    };
    let fits = |e: usize, at: f32, y: &[f32]| {
        let (c, k) = slot[e];
        let layer = &layers[c];
        (k == 0 || y[layer[k - 1]] + separation(layer[k - 1], e) <= at + 0.01)
            && (k + 1 == layer.len() || at + separation(e, layer[k + 1]) <= y[layer[k + 1]] + 0.01)
    };
    for _ in 0..4 {
        let mut moved = false;
        for a in 0..count {
            for &(b, _) in &right_of[a] {
                if !small((y[a] - y[b]).abs()) {
                    continue;
                }
                for (mover, to) in [(b, y[a]), (a, y[b])] {
                    if fits(mover, to, y) && steps(mover, to, y) < steps(mover, y[mover], y) {
                        y[mover] = to;
                        moved = true;
                        break;
                    }
                }
            }
        }
        if !moved {
            break;
        }
    }
}

/// The non-decreasing values nearest `wanted` (value, weight) in weighted least squares, by
/// pooling adjacent violators.
fn pool_adjacent(wanted: &[(f32, f32)]) -> Vec<f32> {
    // Blocks of (weighted sum, weight, length).
    let mut blocks: Vec<(f32, f32, usize)> = Vec::with_capacity(wanted.len());
    for &(value, weight) in wanted {
        blocks.push((value * weight, weight, 1));
        while blocks.len() > 1 {
            let last = blocks[blocks.len() - 1];
            let before = blocks[blocks.len() - 2];
            if before.0 / before.1 <= last.0 / last.1 {
                break;
            }
            blocks.pop();
            let merged = blocks.last_mut().expect("two blocks");
            *merged = (merged.0 + last.0, merged.1 + last.1, merged.2 + last.2);
        }
    }
    blocks
        .into_iter()
        .flat_map(|(sum, weight, length)| std::iter::repeat_n(sum / weight, length))
        .collect()
}

/// An order for a channel's uprights that crosses few lines: an upright's line in from the
/// left crosses the uprights left of it whose stretch it passes inside, and its line out to the
/// right those right of it likewise. Neighbours are swapped while that crosses less, from the
/// order of the uprights' middles.
fn order_tracks(uprights: &[Upright]) -> Vec<usize> {
    let count = uprights.len();
    let inside =
        |value: f32, upright: &Upright| value > upright.low + 0.5 && value < upright.high - 0.5;
    // cost[a][b]: crossings with upright a left of upright b.
    let mut cost = vec![vec![0u32; count]; count];
    for a in 0..count {
        for b in 0..count {
            if a != b {
                cost[a][b] = u32::from(inside(uprights[b].entry, &uprights[a]))
                    + u32::from(inside(uprights[a].exit, &uprights[b]));
            }
        }
    }
    let mut order: Vec<usize> = (0..count).collect();
    let middle = |upright: &Upright| upright.low + upright.high;
    order.sort_by(|&a, &b| {
        middle(&uprights[a])
            .total_cmp(&middle(&uprights[b]))
            .then(a.cmp(&b))
    });
    for _ in 0..count * count {
        let mut swapped = false;
        for k in 0..count.saturating_sub(1) {
            let (a, b) = (order[k], order[k + 1]);
            if cost[b][a] < cost[a][b] {
                order.swap(k, k + 1);
                swapped = true;
            }
        }
        if !swapped {
            break;
        }
    }
    order
}

/// Where named lines meet one side of a chip whose middle is at `centre`, in the order of
/// `toward`, where their other ends are: each as near its other end as the side allows, within
/// [`PORT_SPREAD`] of the middle, and [`PORT_STEP`] apart or as far as the side has room for, so
/// a line whose other end is level with the side runs straight into it rather than stepping.
fn ports(centre: f32, toward: &[f32]) -> Vec<f32> {
    let count = toward.len();
    let step = match count {
        0 | 1 => 0.,
        _ => PORT_STEP.min(2. * PORT_SPREAD / (count - 1) as f32),
    };
    let mut at: Vec<f32> = toward
        .iter()
        .map(|y| (y - centre).clamp(-PORT_SPREAD, PORT_SPREAD))
        .collect();
    // Apart going down, then back within the side going up, then within it going down; the
    // steps fit in the side, so the last pass moves nothing out of it.
    for k in 1..count {
        at[k] = at[k].max(at[k - 1] + step);
    }
    for k in (0..count).rev() {
        let below = at.get(k + 1).map_or(PORT_SPREAD, |next| next - step);
        at[k] = at[k].min(below);
    }
    for k in 0..count {
        let above = k
            .checked_sub(1)
            .map_or(-PORT_SPREAD, |prev| at[prev] + step);
        at[k] = at[k].max(above);
    }
    at.into_iter().map(|offset| centre + offset).collect()
}

/// A route without repeated points or straight-through bends.
fn simplify(points: Vec<Vec2>) -> Vec<Vec2> {
    let mut out: Vec<Vec2> = Vec::with_capacity(points.len());
    for p in points {
        if out
            .last()
            .is_some_and(|q| (q.x - p.x).abs() < 0.01 && (q.y - p.y).abs() < 0.01)
        {
            continue;
        }
        if out.len() >= 2 {
            let (a, b) = (out[out.len() - 2], out[out.len() - 1]);
            let along_x = (a.y - b.y).abs() < 0.01 && (b.y - p.y).abs() < 0.01;
            let along_y = (a.x - b.x).abs() < 0.01 && (b.x - p.x).abs() < 0.01;
            if along_x || along_y {
                out.pop();
            }
        }
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A schema with hubs: `types` types, two hubs nearly everything points to, a third most
    /// point to, a tier of smaller hubs, the rest tied to one to five of them and to each other,
    /// loops on about half, and some pairs joined twice.
    fn star_schema(types: usize) -> (Vec<f32>, Vec<Link>) {
        let mut next = 0x2545_f491_4f6c_dd1d_u64;
        let mut draw = |below: usize| {
            next = next
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (next >> 33) as usize % below.max(1)
        };
        let half_widths: Vec<f32> = (0..types).map(|_| 40. + draw(60) as f32).collect();
        let link = |from: usize, to: usize, draw: &mut dyn FnMut(usize) -> usize| Link {
            from,
            to,
            label_half_width: 30. + draw(30) as f32,
        };
        let mut links = Vec::new();
        for t in 3..types {
            // The big three.
            if draw(10) < 8 {
                links.push(link(t, 0, &mut draw));
            }
            if draw(10) < 8 {
                links.push(link(t, 1, &mut draw));
            }
            if draw(10) < 5 {
                links.push(link(t, 2, &mut draw));
            }
            // A few more, to the smaller hubs and to each other.
            for _ in 0..draw(4) {
                let to = if draw(2) == 0 {
                    3 + draw(8)
                } else {
                    draw(types)
                };
                links.push(link(t, to, &mut draw));
            }
            if draw(2) == 0 {
                links.push(link(t, t, &mut draw));
            }
        }
        links.push(link(0, 1, &mut draw));
        links.push(link(0, 2, &mut draw));
        links.push(link(1, 0, &mut draw));
        (half_widths, links)
    }

    fn crm() -> (Vec<f32>, Vec<Link>) {
        let half_widths = vec![50., 60., 45., 70., 40., 55.];
        let link = |from, to| Link {
            from,
            to,
            label_half_width: 32.,
        };
        let links = vec![
            link(1, 0),
            link(2, 0),
            link(3, 0),
            link(3, 1),
            link(4, 3),
            link(5, 4),
            link(0, 5),
            link(2, 2),
            link(3, 0),
        ];
        (half_widths, links)
    }

    /// Every route is right angles from the side of the type it leaves to the side of the type
    /// it reaches, and passes through no other chip.
    fn assert_routes_are_sound(half_widths: &[f32], links: &[Link], laid: &Layered) {
        let at = &laid.positions;
        for (index, (link, route)) in links.iter().zip(&laid.routes).enumerate() {
            if link.from == link.to {
                assert!(route.points.is_empty());
                continue;
            }
            let points = &route.points;
            assert!(points.len() >= 2, "link {index}: {points:?}");
            for pair in points.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                assert!(
                    (a.x - b.x).abs() < 0.01 || (a.y - b.y).abs() < 0.01,
                    "link {index} has a slanted piece: {points:?}"
                );
            }
            let on_side = |p: Vec2, node: usize| {
                ((p.x - at[node].x).abs() - half_widths[node]).abs() < 0.01
                    && (p.y - at[node].y).abs() <= PORT_SPREAD + 0.01
            };
            assert!(
                on_side(points[0], link.from),
                "link {index} starts off its type"
            );
            assert!(
                on_side(*points.last().unwrap(), link.to),
                "link {index} ends off its type"
            );
            // No piece passes through a chip, its own ends' chips included beyond their sides.
            for pair in points.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                for (node, p) in at.iter().enumerate() {
                    let (left, right) =
                        (p.x - half_widths[node] + 0.5, p.x + half_widths[node] - 0.5);
                    let (top, bottom) = (p.y - 13., p.y + 13.);
                    let crosses = a.x.min(b.x) < right
                        && a.x.max(b.x) > left
                        && a.y.min(b.y) < bottom
                        && a.y.max(b.y) > top;
                    assert!(
                        !crosses,
                        "link {index} runs through type {node}: {points:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_small_schema_runs_left_to_right_in_right_angles() {
        let (half_widths, links) = crm();
        let laid = layered(&half_widths, &links);
        assert_eq!(laid, layered(&half_widths, &links), "deterministic");
        assert_routes_are_sound(&half_widths, &links, &laid);
        // Every label has its own place.
        let labels: Vec<Vec2> = laid.routes.iter().filter_map(|r| r.label_at).collect();
        assert_eq!(
            labels.len(),
            links.iter().filter(|l| l.from != l.to).count()
        );
        for (i, a) in labels.iter().enumerate() {
            for b in &labels[i + 1..] {
                assert!(
                    (a.x - b.x).abs() >= 2. * 32. || (a.y - b.y).abs() >= 20.,
                    "labels at {a:?} and {b:?} overlap"
                );
            }
        }
    }

    /// A line meets a chip's side as near its other end as the side allows, so one whose other
    /// end is level with the side runs straight; crowded ports keep their order, apart and on
    /// the side.
    #[test]
    fn a_line_meets_a_chip_level_with_its_other_end_where_it_can() {
        assert_eq!(ports(100., &[60., 108.]), vec![90., 108.]);
        assert_eq!(ports(0., &[3.]), vec![3.]);
        let crowded = ports(0., &[0.; 5]);
        assert!(crowded.windows(2).all(|w| w[1] - w[0] >= 5. - 0.01));
        assert!(crowded.iter().all(|p| p.abs() <= PORT_SPREAD + 0.01));
    }

    #[test]
    fn related_types_stand_in_different_columns_within_the_cap() {
        let (half_widths, links) = star_schema(60);
        let mut pairs: BTreeMap<(usize, usize), u32> = BTreeMap::new();
        for link in links.iter().filter(|l| l.from != l.to) {
            *pairs
                .entry((link.from.min(link.to), link.from.max(link.to)))
                .or_default() += 1;
        }
        let pairs: Vec<(usize, usize, u32)> =
            pairs.into_iter().map(|((u, v), w)| (u, v, w)).collect();
        let cap = column_cap(half_widths.len());
        let column = assign_columns(half_widths.len(), &pairs, cap);
        for &(u, v, _) in &pairs {
            assert_ne!(column[u], column[v], "{u} and {v} share a column");
        }
        let columns = column.iter().max().unwrap() + 1;
        for c in 0..columns {
            assert!(column.iter().filter(|&&k| k == c).count() <= cap);
        }
        // The most connected types stand in the middle columns.
        let middle = (columns - 1) as f32 / 2.;
        let off = |t: usize| (column[t] as f32 - middle).abs();
        assert!(off(0) <= 1.5 && off(1) <= 1.5, "{column:?}");
    }

    /// Pooling adjacent violators gives the nearest non-decreasing values, as a search over
    /// every split into level stretches does.
    #[test]
    fn pooling_finds_the_nearest_ordered_values() {
        let cases: [&[(f32, f32)]; 4] = [
            &[(1., 1.), (3., 1.), (2., 1.)],
            &[(5., 1.), (4., 2.), (3., 1.), (6., 1.)],
            &[(0., 1.), (0., 1.)],
            &[(2., 3.), (1., 1.)],
        ];
        let expected: [&[f32]; 4] = [&[1., 2.5, 2.5], &[4., 4., 4., 6.], &[0., 0.], &[1.75, 1.75]];
        for (case, want) in cases.iter().zip(expected) {
            let got = pool_adjacent(case);
            for (g, w) in got.iter().zip(want) {
                assert!((g - w).abs() < 1e-5, "{case:?}: {got:?}");
            }
        }
    }

    #[test]
    fn crossings_are_counted_between_neighbouring_columns() {
        // a0 → b1 and a1 → b0 cross once.
        let layers = vec![vec![0, 1], vec![2, 3]];
        let right_of = vec![vec![(3, 1.)], vec![(2, 1.)], vec![], vec![]];
        let place = vec![0, 1, 0, 1];
        assert_eq!(crossings(&layers, &right_of, &place), 1.);
        let straight = vec![vec![(2, 1.)], vec![(3, 1.)], vec![], vec![]];
        assert_eq!(crossings(&layers, &straight, &place), 0.);
    }
}
