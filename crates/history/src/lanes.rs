//! Lane assignment for the history graph (spec 009).
//!
//! The idea follows Zed's `crates/git_ui/src/git_graph.rs` (`GraphData::add_commits`) at
//! v1.23.2: commits arrive newest first; a commit takes the lowest lane that expects it, or the
//! first free lane; its first parent continues in that lane and each merged parent opens a new
//! lane. This is a fresh implementation with a simpler output: per row, the lanes that pass
//! through, the lanes that end at the commit and the lanes that leave it, so each row draws
//! itself and the drawing is virtualized with the rows. The property tests below check it over
//! random commit graphs.

use std::collections::HashMap;

/// One commit as the graph sees it: its id and parents (first parent first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphCommit {
    pub id: String,
    pub parents: Vec<String>,
}

/// A line in one lane, with the color index of the branch it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneLine {
    pub lane: usize,
    pub color: usize,
}

/// How one row of the graph is drawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphRow {
    /// The commit's lane and color.
    pub lane: usize,
    pub color: usize,
    /// Lines that cross the row from top to bottom without touching the commit.
    pub through: Vec<LaneLine>,
    /// Lines from the top of the row that end at the commit.
    pub incoming: Vec<LaneLine>,
    /// Lines from the commit to the bottom of the row: the first parent's, then merged parents'.
    pub outgoing: Vec<LaneLine>,
}

/// The lanes at a row's edges, which the lane tests check meet from row to row.
#[cfg(test)]
impl GraphRow {
    /// Lanes occupied at the top edge of the row.
    pub fn top(&self) -> impl Iterator<Item = LaneLine> + '_ {
        self.through.iter().chain(&self.incoming).copied()
    }

    /// Lanes occupied at the bottom edge of the row.
    pub fn bottom(&self) -> impl Iterator<Item = LaneLine> + '_ {
        self.through.iter().chain(&self.outgoing).copied()
    }
}

/// The laid-out graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    pub rows: Vec<GraphRow>,
    pub lane_count: usize,
}

/// Lays out commits given newest first (every parent after its children, as `GET /commits`
/// orders them). Parents missing from the list, or listed before their child, are not drawn.
pub fn assign_lanes(commits: &[GraphCommit]) -> Graph {
    let row_of: HashMap<&str, usize> = commits
        .iter()
        .enumerate()
        .map(|(row, commit)| (commit.id.as_str(), row))
        .collect();
    // Each lane, when busy, holds the parent it waits for and its color.
    let mut lanes: Vec<Option<(&str, usize)>> = Vec::new();
    let mut next_color = 0;
    let mut rows = Vec::with_capacity(commits.len());
    let mut lane_count = 0;

    for (row, commit) in commits.iter().enumerate() {
        let waiting: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, lane)| lane.is_some_and(|(parent, _)| parent == commit.id))
            .map(|(ix, _)| ix)
            .collect();
        let incoming: Vec<LaneLine> = waiting
            .iter()
            .map(|&lane| LaneLine {
                lane,
                color: lanes[lane].expect("busy lane").1,
            })
            .collect();
        for &lane in &waiting {
            lanes[lane] = None;
        }
        let (lane, color) = match incoming.first() {
            Some(first) => (first.lane, first.color),
            None => {
                let color = next_color;
                next_color += 1;
                (first_free(&mut lanes), color)
            }
        };
        let through: Vec<LaneLine> = lanes
            .iter()
            .enumerate()
            .filter_map(|(ix, lane)| lane.map(|(_, color)| LaneLine { lane: ix, color }))
            .collect();

        let mut outgoing = Vec::new();
        let drawn = commit.parents.iter().enumerate().filter(|(_, parent)| {
            row_of
                .get(parent.as_str())
                .is_some_and(|&parent_row| parent_row > row)
        });
        for (index, parent) in drawn {
            if index == 0 {
                lanes[lane] = Some((parent.as_str(), color));
                outgoing.push(LaneLine { lane, color });
            } else if outgoing.iter().any(|line: &LaneLine| {
                lanes[line.lane].is_some_and(|(waiting_for, _)| waiting_for == parent)
            }) {
                // The same parent twice: one line is enough.
            } else {
                let color = next_color;
                next_color += 1;
                // The commit's lane may still be free when the first parent is not drawn.
                let new_lane = first_free_except(&mut lanes, lane);
                lanes[new_lane] = Some((parent.as_str(), color));
                outgoing.push(LaneLine {
                    lane: new_lane,
                    color,
                });
            }
        }
        lane_count = lane_count.max(lanes.len()).max(lane + 1);
        // Trailing free lanes need no slot.
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        rows.push(GraphRow {
            lane,
            color,
            through,
            incoming,
            outgoing,
        });
    }
    Graph { rows, lane_count }
}

fn first_free(lanes: &mut Vec<Option<(&str, usize)>>) -> usize {
    match lanes.iter().position(Option::is_none) {
        Some(ix) => ix,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

fn first_free_except(lanes: &mut Vec<Option<(&str, usize)>>, except: usize) -> usize {
    match lanes
        .iter()
        .enumerate()
        .position(|(ix, lane)| lane.is_none() && ix != except)
    {
        Some(ix) => ix,
        None => {
            lanes.push(None);
            if lanes.len() - 1 == except {
                lanes.push(None);
            }
            lanes.len() - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    /// SplitMix64: a small deterministic generator, so failures name a reproducible seed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.next() % 100 < percent
        }
    }

    /// A random commit graph, newest first: every parent is older than its child. Some parents
    /// are outside the list, as when history is cut off.
    fn random_dag(rng: &mut Rng) -> Vec<GraphCommit> {
        let n = 1 + rng.below(60);
        (0..n)
            .map(|row| {
                let mut parents = Vec::new();
                let older = n - row - 1;
                if older > 0 && rng.chance(90) {
                    parents.push(format!("c{}", row + 1 + rng.below(older.min(6))));
                } else if rng.chance(20) {
                    parents.push(format!("missing{row}"));
                }
                while older > 0 && rng.chance(25) && parents.len() < 4 {
                    let parent = format!("c{}", row + 1 + rng.below(older));
                    parents.push(parent);
                }
                GraphCommit {
                    id: format!("c{row}"),
                    parents,
                }
            })
            .collect()
    }

    fn check(commits: &[GraphCommit], graph: &Graph, seed: u64) {
        let row_of: HashMap<&str, usize> = commits
            .iter()
            .enumerate()
            .map(|(r, c)| (c.id.as_str(), r))
            .collect();
        assert_eq!(graph.rows.len(), commits.len(), "seed {seed}");
        for (r, row) in graph.rows.iter().enumerate() {
            // Every lane index is inside the graph's width.
            assert!(row.lane < graph.lane_count, "seed {seed} row {r}");
            for line in row.top().chain(row.bottom()) {
                assert!(line.lane < graph.lane_count, "seed {seed} row {r}");
            }
            // No two lines share a lane at either edge of a row.
            let top: Vec<usize> = row.top().map(|l| l.lane).collect();
            let bottom: Vec<usize> = row.bottom().map(|l| l.lane).collect();
            assert_eq!(
                top.len(),
                top.iter().collect::<HashSet<_>>().len(),
                "seed {seed} row {r} top"
            );
            assert_eq!(
                bottom.len(),
                bottom.iter().collect::<HashSet<_>>().len(),
                "seed {seed} row {r} bottom"
            );
            // A passing line never runs through the commit's dot.
            assert!(
                row.through.iter().all(|l| l.lane != row.lane),
                "seed {seed} row {r}"
            );
            // A line that ends at the commit in its own lane carries the commit's color.
            for line in &row.incoming {
                if line.lane == row.lane {
                    assert_eq!(line.color, row.color, "seed {seed} row {r}");
                }
            }
            // The first parent continues in the commit's lane.
            let first_drawn = commits[r]
                .parents
                .first()
                .is_some_and(|p| row_of.get(p.as_str()).is_some_and(|&pr| pr > r));
            if first_drawn {
                assert_eq!(row.outgoing[0].lane, row.lane, "seed {seed} row {r}");
            }
        }
        // Lines connect: what leaves the bottom of a row enters the top of the next, in the
        // same lane with the same color; nothing enters the first row or leaves the last.
        assert_eq!(graph.rows[0].top().count(), 0, "seed {seed}");
        assert_eq!(
            graph.rows.last().unwrap().bottom().count(),
            0,
            "seed {seed}"
        );
        for r in 1..graph.rows.len() {
            let mut above: Vec<(usize, usize)> = graph.rows[r - 1]
                .bottom()
                .map(|l| (l.lane, l.color))
                .collect();
            let mut below: Vec<(usize, usize)> =
                graph.rows[r].top().map(|l| (l.lane, l.color)).collect();
            above.sort();
            below.sort();
            assert_eq!(above, below, "seed {seed} between rows {} and {r}", r - 1);
        }
        // Every drawn parent edge is one continuous line: it leaves the child in some lane,
        // passes through every row between, and ends at the parent in that lane.
        for (child, commit) in commits.iter().enumerate() {
            let mut seen = HashSet::new();
            for parent in &commit.parents {
                let Some(&p) = row_of.get(parent.as_str()) else {
                    continue;
                };
                if p <= child || !seen.insert(parent) {
                    continue;
                }
                let lanes: Vec<usize> = graph.rows[child].outgoing.iter().map(|l| l.lane).collect();
                let connected = lanes.iter().any(|&lane| {
                    (child + 1..p).all(|mid| graph.rows[mid].through.iter().any(|l| l.lane == lane))
                        && graph.rows[p].incoming.iter().any(|l| l.lane == lane)
                });
                assert!(
                    connected,
                    "seed {seed}: edge c{child} -> {parent} is broken"
                );
            }
        }
        // The graph is no wider than the most lines alive at once, plus the commit's own lane.
        let widest = graph
            .rows
            .iter()
            .map(|row| row.top().count().max(row.bottom().count()) + 1)
            .max()
            .unwrap_or(0);
        assert!(
            graph.lane_count <= widest,
            "seed {seed}: {} > {widest}",
            graph.lane_count
        );
    }

    #[test]
    fn lanes_hold_over_random_commit_graphs() {
        for seed in 0..2_000 {
            let mut rng = Rng(seed);
            let commits = random_dag(&mut rng);
            let graph = assign_lanes(&commits);
            check(&commits, &graph, seed);
        }
    }

    fn commit(id: &str, parents: &[&str]) -> GraphCommit {
        GraphCommit {
            id: id.into(),
            parents: parents.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn a_linear_history_is_one_lane() {
        let commits = vec![commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])];
        let graph = assign_lanes(&commits);
        assert_eq!(graph.lane_count, 1);
        assert!(graph.rows.iter().all(|r| r.lane == 0 && r.color == 0));
    }

    #[test]
    fn a_merge_opens_a_lane_that_closes_at_the_fork() {
        // m merges b2 into main; b2 and m1 both fork from base.
        let commits = vec![
            commit("m", &["m1", "b2"]),
            commit("b2", &["b1"]),
            commit("m1", &["base"]),
            commit("b1", &["base"]),
            commit("base", &[]),
        ];
        let graph = assign_lanes(&commits);
        assert_eq!(graph.lane_count, 2);
        let lanes: Vec<usize> = graph.rows.iter().map(|r| r.lane).collect();
        assert_eq!(lanes, vec![0, 1, 0, 1, 0]);
        assert_eq!(graph.rows[0].outgoing.len(), 2);
        assert_eq!(graph.rows[4].incoming.len(), 2);
    }
}
