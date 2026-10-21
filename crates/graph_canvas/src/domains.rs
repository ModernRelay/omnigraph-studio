//! The ontology's domains (spec 006, "Ontology page"): a schema's types grouped by the
//! relationships among them, so a large ontology reads as a few named groups of types rather
//! than a web of lines. Without GPUI, and deterministic: a function of the schema alone.
//!
//! A type related to a large share of the others (a person, a document) belongs to every group
//! and so to none: it is *shared*, set apart. The rest are grouped by modularity (Blondel,
//! Guillaume, Lambiotte and Lefebvre 2008, "Louvain"): each type joins the group it has the most
//! relationships with beyond what chance would give, then the groups are grouped the same way.
//! A type left alone joins the group it is most related to; one related to shared types only
//! goes to a last group of its own kind.

use std::collections::BTreeMap;

/// Up to this many types, a schema is one group: there is nothing to sort.
pub const SMALL_SCHEMA: usize = 12;
/// A type related to more than this share of the other types is shared.
const SHARED_SHARE: f64 = 0.3;
/// How many times types are grouped: types, then their groups. A third level merged
/// recognisable domains of a 101-type schema into groups of 23 and 32.
const LEVELS: usize = 2;

/// A schema's types, grouped.
#[derive(Debug, Clone, PartialEq)]
pub struct Domains {
    /// Types related to so many others that they belong to every group: most related first.
    pub shared: Vec<usize>,
    /// Each group's types, most related first; the largest group first, and the types related
    /// to shared types only (or to none) last.
    pub groups: Vec<Vec<usize>>,
    /// Whether the last group holds the types related to shared types only, or to none.
    pub rest: bool,
    /// For each group, how many relationships it has with each other group, most first.
    pub links: Vec<Vec<(usize, usize)>>,
    /// Each type's group, or `None` for a shared type.
    pub group_of: Vec<Option<usize>>,
    /// How many relationships each type has with other types.
    pub degree: Vec<usize>,
}

/// Groups `types` types by `relationships` (from, to); a relationship from a type to itself
/// takes no part.
pub fn domains(types: usize, relationships: &[(usize, usize)]) -> Domains {
    let mut weight: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    let mut degree = vec![0usize; types];
    for &(from, to) in relationships {
        if from == to || from >= types || to >= types {
            continue;
        }
        *weight.entry((from.min(to), from.max(to))).or_default() += 1.;
        degree[from] += 1;
        degree[to] += 1;
    }
    let by_degree = |list: &mut Vec<usize>| {
        list.sort_by(|a, b| degree[*b].cmp(&degree[*a]).then(a.cmp(b)));
    };
    if types <= SMALL_SCHEMA {
        let mut all: Vec<usize> = (0..types).collect();
        by_degree(&mut all);
        return Domains {
            shared: Vec::new(),
            groups: if all.is_empty() {
                Vec::new()
            } else {
                vec![all]
            },
            rest: false,
            links: if types == 0 {
                Vec::new()
            } else {
                vec![Vec::new()]
            },
            group_of: vec![Some(0); types],
            degree,
        };
    }
    let mut shared: Vec<usize> = (0..types)
        .filter(|&t| degree[t] as f64 > SHARED_SHARE * types as f64)
        .collect();
    by_degree(&mut shared);
    let is_shared = |t: usize| shared.contains(&t);

    // The relationships among the types that are not shared.
    let mut adjacency: Vec<Vec<(usize, f64)>> = vec![Vec::new(); types];
    for (&(a, b), &w) in &weight {
        if !is_shared(a) && !is_shared(b) {
            adjacency[a].push((b, w));
            adjacency[b].push((a, w));
        }
    }
    let members: Vec<usize> = (0..types).filter(|&t| !is_shared(t)).collect();
    let community = louvain(&members, &adjacency);

    // Groups by community; a type alone joins the group it is most related to.
    let mut by_community: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &t in &members {
        by_community.entry(community[t]).or_default().push(t);
    }
    let mut group_of: Vec<Option<usize>> = vec![None; types];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut alone: Vec<usize> = Vec::new();
    for list in by_community.into_values() {
        if list.len() == 1 {
            alone.push(list[0]);
        } else {
            for &t in &list {
                group_of[t] = Some(groups.len());
            }
            groups.push(list);
        }
    }
    let mut rest: Vec<usize> = Vec::new();
    for t in alone {
        let mut toward: BTreeMap<usize, f64> = BTreeMap::new();
        for &(other, w) in &adjacency[t] {
            if let Some(g) = group_of[other] {
                *toward.entry(g).or_default() += w;
            }
        }
        match toward
            .into_iter()
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
        {
            Some((g, _)) => {
                group_of[t] = Some(g);
                groups[g].push(t);
            }
            None => rest.push(t),
        }
    }
    for list in groups.iter_mut() {
        by_degree(list);
    }
    groups.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
    let has_rest = !rest.is_empty();
    if has_rest {
        by_degree(&mut rest);
        groups.push(rest);
    }
    for (g, list) in groups.iter().enumerate() {
        for &t in list {
            group_of[t] = Some(g);
        }
    }
    let mut between: Vec<BTreeMap<usize, usize>> = vec![BTreeMap::new(); groups.len()];
    for (&(a, b), &w) in &weight {
        if let (Some(ga), Some(gb)) = (group_of[a], group_of[b])
            && ga != gb
        {
            *between[ga].entry(gb).or_default() += w as usize;
            *between[gb].entry(ga).or_default() += w as usize;
        }
    }
    let links = between
        .into_iter()
        .map(|map| {
            let mut list: Vec<(usize, usize)> = map.into_iter().collect();
            list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            list
        })
        .collect();
    Domains {
        shared,
        groups,
        rest: has_rest,
        links,
        group_of,
        degree,
    }
}

/// Each member's community by modularity: members move, in order, to the neighbouring
/// community that gains the most, until none moves; then communities are grouped as members
/// were, for up to [`LEVELS`] levels. Ties go to the lower community.
fn louvain(members: &[usize], adjacency: &[Vec<(usize, f64)>]) -> Vec<usize> {
    let mut community: Vec<usize> = (0..adjacency.len()).collect();
    // The graph at the current level: nodes, and weighted links between them (a node's own
    // links are kept as a self link, which counts in its degree).
    let mut nodes: Vec<usize> = members.to_vec();
    let mut links: BTreeMap<usize, BTreeMap<usize, f64>> = BTreeMap::new();
    for &a in members {
        let entry = links.entry(a).or_default();
        for &(b, w) in &adjacency[a] {
            *entry.entry(b).or_default() += w;
        }
    }
    for _ in 0..LEVELS {
        let assignment = one_level(&nodes, &links);
        let moved = nodes.iter().any(|n| assignment[n] != *n);
        // Members follow their node.
        for &m in members {
            community[m] = assignment[&community[m]];
        }
        if !moved {
            break;
        }
        // The next level: one node per community.
        let mut next: BTreeMap<usize, BTreeMap<usize, f64>> = BTreeMap::new();
        for (&a, out) in &links {
            for (&b, &w) in out {
                *next
                    .entry(assignment[&a])
                    .or_default()
                    .entry(assignment[&b])
                    .or_default() += w;
            }
        }
        let mut unique: Vec<usize> = assignment.values().copied().collect();
        unique.sort_unstable();
        unique.dedup();
        for &c in &unique {
            next.entry(c).or_default();
        }
        nodes = unique;
        links = next;
    }
    community
}

/// One level of [`louvain`]: each node's community, keyed by node.
fn one_level(
    nodes: &[usize],
    links: &BTreeMap<usize, BTreeMap<usize, f64>>,
) -> BTreeMap<usize, usize> {
    let degree: BTreeMap<usize, f64> = nodes
        .iter()
        .map(|n| (*n, links.get(n).map_or(0., |out| out.values().sum())))
        .collect();
    let total: f64 = degree.values().sum();
    let mut community: BTreeMap<usize, usize> = nodes.iter().map(|n| (*n, *n)).collect();
    if total <= 0. {
        return community;
    }
    let mut weight_of: BTreeMap<usize, f64> = degree.clone();
    for _ in 0..nodes.len().max(1) * 4 {
        let mut moved = false;
        for &n in nodes {
            let from = community[&n];
            let k = degree[&n];
            *weight_of.entry(from).or_default() -= k;
            let mut toward: BTreeMap<usize, f64> = BTreeMap::new();
            for (&other, &w) in links.get(&n).into_iter().flatten() {
                if other != n {
                    *toward.entry(community[&other]).or_default() += w;
                }
            }
            let gain = |c: usize, w: f64| w - weight_of.get(&c).copied().unwrap_or(0.) * k / total;
            let mut best = (from, gain(from, toward.get(&from).copied().unwrap_or(0.)));
            for (&c, &w) in &toward {
                let g = gain(c, w);
                if g > best.1 + 1e-9 {
                    best = (c, g);
                }
            }
            *weight_of.entry(best.0).or_default() += k;
            if best.0 != from {
                community.insert(n, best.0);
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    community
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `count` cliques of `size` types, each joined to the next by one relationship from its
    /// last type to the next one's first, and a hub related to every type.
    fn cliques(count: usize, size: usize) -> (usize, Vec<(usize, usize)>) {
        let mut relationships = Vec::new();
        for c in 0..count {
            let base = 1 + c * size;
            for a in 0..size {
                for b in a + 1..size {
                    relationships.push((base + a, base + b));
                }
            }
            if c + 1 < count {
                relationships.push((base + size - 1, base + size));
            }
        }
        let types = 1 + count * size;
        relationships.extend((1..types).map(|t| (0, t)));
        (types, relationships)
    }

    #[test]
    fn related_types_group_and_a_hub_is_shared() {
        let (types, relationships) = cliques(4, 6);
        let found = domains(types, &relationships);
        assert_eq!(found.shared, vec![0], "the hub is related to every type");
        assert_eq!(found.groups.len(), 4, "{:?}", found.groups);
        assert!(!found.rest);
        for clique in 0..4 {
            let base = 1 + clique * 6;
            let group = found.group_of[base];
            assert!((base..base + 6).all(|t| found.group_of[t] == group));
        }
        // Each group is linked to its neighbours by the one relationship between them.
        let middle = found.group_of[7].unwrap();
        let linked: usize = found.links[middle].iter().map(|l| l.1).sum();
        assert_eq!(linked, 2);
        assert_eq!(found, domains(types, &relationships), "deterministic");
    }

    #[test]
    fn every_type_has_one_place() {
        let (types, mut relationships) = cliques(4, 5);
        // A type related to the hub only, and one related to nothing.
        let (lonely, unrelated) = (types, types + 1);
        relationships.push((lonely, 0));
        let found = domains(types + 2, &relationships);
        let mut seen = vec![0; types + 2];
        for &t in found.shared.iter().chain(found.groups.iter().flatten()) {
            seen[t] += 1;
        }
        assert!(seen.iter().all(|&n| n == 1), "{seen:?}");
        assert!(found.rest);
        let last = found.groups.last().unwrap();
        assert!(last.contains(&lonely) && last.contains(&unrelated));
    }

    #[test]
    fn a_small_schema_is_one_group() {
        let found = domains(3, &[(0, 1), (1, 2), (2, 0), (1, 1)]);
        assert_eq!(found.groups.len(), 1);
        assert!(found.shared.is_empty());
        assert_eq!(found.degree, vec![2, 2, 2]);
        assert!(domains(0, &[]).groups.is_empty());
    }
}
