//! What the canvas draws, without GPUI: the exploration as nodes, hubs (one per opened
//! relationship group) and "+K more" chips, each node drawn once, and a query's result nodes.

use std::collections::{HashMap, HashSet};

use graph_store::{Entry, EntryKind, ExplorePath, ExploreState, GroupKey, Loadable, NodeRef};

/// Neighbours a hub shows before "+K more", and how many each click on it adds.
pub const HUB_STEP: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SceneId {
    Node(NodeRef),
    Hub(GroupKey),
    More(GroupKey),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SceneKind {
    /// A node of the graph. `root` marks where the exploration started.
    Node {
        node: NodeRef,
        root: bool,
        expanded: bool,
    },
    /// One relationship group of a node.
    Hub {
        key: GroupKey,
        count: Option<u64>,
        open: bool,
    },
    /// The neighbours of a hub not drawn yet.
    More { key: GroupKey, hidden: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SceneNode {
    pub id: SceneId,
    pub kind: SceneKind,
    pub label: String,
    /// The type or edge name that picks the color.
    pub color_key: String,
    /// The node this one hangs off, for layout.
    pub parent: Option<usize>,
    /// Where this node sits in the exploration: a node path, or a hub's group path.
    pub path: Option<ExplorePath>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SceneEdge {
    pub from: usize,
    pub to: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scene {
    pub nodes: Vec<SceneNode>,
    pub edges: Vec<SceneEdge>,
    /// Nodes left out because of the node cap, and the type most of them belong to.
    pub capped: Option<(usize, String)>,
    /// The node cap the scene was built under, which its banner names: a setting changed
    /// since is not this scene's until it is built again.
    pub cap: usize,
}

impl Scene {
    #[cfg(test)]
    pub fn index_of(&self, id: &SceneId) -> Option<usize> {
        self.nodes.iter().position(|node| node.id == *id)
    }

    /// Where each chip is, for finding many by id at once: ⌘A can mark thousands, and a walk
    /// of the scene for each would be quadratic.
    pub fn indices(&self) -> HashMap<&SceneId, usize> {
        self.nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (&node.id, index))
            .collect()
    }
}

/// Every open path the exploration reaches any of the chips `ids` by, in one pass over the
/// entries; "+K more" stands for its relationship. A node or relationship reached by more than
/// one path is drawn once and shows open while any of them is, so closing it closes them all.
pub fn open_paths(entries: &[Entry], state: &ExploreState, ids: &[SceneId]) -> Vec<ExplorePath> {
    let mut nodes = HashSet::new();
    let mut hubs = HashSet::new();
    for id in ids {
        match id {
            SceneId::Node(node) => {
                nodes.insert(node);
            }
            SceneId::Hub(key) | SceneId::More(key) => {
                hubs.insert(key);
            }
        }
    }
    entries
        .iter()
        .filter(|entry| match &entry.kind {
            EntryKind::Node { node, expanded, .. } => *expanded && nodes.contains(node),
            EntryKind::Group { key, expanded, .. } => *expanded && hubs.contains(key),
            EntryKind::Field {
                key,
                target: Some(_),
                ..
            } => hubs.contains(key) && state.is_expanded(&entry.path),
            _ => false,
        })
        .map(|entry| entry.path.clone())
        .collect()
}

/// A query result as the canvas draws it: each node once, a line between the nodes of a row.
/// No hubs: a double click explores from a node.
pub fn results_scene(
    nodes: &[(NodeRef, Option<String>)],
    links: &[(usize, usize)],
    cap: usize,
) -> Scene {
    let capped = nodes.len().saturating_sub(cap);
    let scene_nodes: Vec<SceneNode> = nodes
        .iter()
        .take(cap)
        .map(|(node, label)| SceneNode {
            id: SceneId::Node(node.clone()),
            kind: SceneKind::Node {
                node: node.clone(),
                root: false,
                expanded: false,
            },
            label: label.clone().unwrap_or_else(|| node.id.clone()),
            color_key: node.type_name.clone(),
            parent: None,
            path: None,
        })
        .collect();
    let shown = scene_nodes.len();
    let edges = links
        .iter()
        .filter(|(from, to)| *from < shown && *to < shown && from != to)
        .map(|(from, to)| SceneEdge {
            from: *from,
            to: *to,
        })
        .collect();
    let capped = (capped > 0).then(|| {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for (node, _) in nodes.iter().skip(cap) {
            *counts.entry(node.type_name.as_str()).or_default() += 1;
        }
        (capped, most_left_out(counts).unwrap_or_default())
    });
    Scene {
        nodes: scene_nodes,
        edges,
        capped,
        cap,
    }
}

/// The exploration as the canvas draws it. `limits` holds how many neighbours each opened hub
/// shows; a hub without an entry shows [`HUB_STEP`].
pub fn explore_scene(
    entries: &[Entry],
    state: &ExploreState,
    limits: &HashMap<GroupKey, usize>,
    cap: usize,
) -> Scene {
    let mut scene = Scene {
        cap,
        ..Scene::default()
    };
    let mut by_id: HashMap<SceneId, usize> = HashMap::new();
    // The scene index each entry maps to (its node or hub), and entries left out.
    let mut placed: Vec<Option<usize>> = vec![None; entries.len()];
    let mut skipped = vec![false; entries.len()];
    // Entries the cap left out, with everything under them: the banner counts them, once each
    // however many paths reach them. Neighbours past a hub's limit are "+K more" instead.
    let mut capped = vec![false; entries.len()];
    let mut hidden: HashSet<SceneId> = HashSet::new();
    let mut shown_per_hub: HashMap<usize, usize> = HashMap::new();
    // The lines drawn so far, so a line is drawn once without searching every line.
    let mut lines: HashSet<(usize, usize)> = HashSet::new();
    let label_of = |node: &NodeRef| {
        state
            .label(node)
            .map(str::to_owned)
            .unwrap_or_else(|| node.id.clone())
    };

    let add = |scene: &mut Scene, by_id: &mut HashMap<SceneId, usize>, node: SceneNode| {
        let index = scene.nodes.len();
        by_id.insert(node.id.clone(), index);
        scene.nodes.push(node);
        index
    };

    for (index, entry) in entries.iter().enumerate() {
        if let Some(parent) = entry.parent
            && skipped[parent]
        {
            skipped[index] = true;
            if capped[parent] {
                capped[index] = true;
                left_out(entry, &by_id, &mut hidden);
            }
            continue;
        }
        let parent_scene = entry.parent.and_then(|parent| placed[parent]);
        match &entry.kind {
            EntryKind::Node { node, expanded, .. } => {
                let id = SceneId::Node(node.clone());
                // A hub draws `limit` neighbour edges, whether or not their nodes are new.
                if let Some(hub) = parent_scene {
                    let shown = shown_per_hub.entry(hub).or_default();
                    let limit = hub_limit(&scene.nodes[hub], limits);
                    if *shown >= limit {
                        skipped[index] = true;
                        continue;
                    }
                    *shown += 1;
                }
                if let Some(existing) = by_id.get(&id).copied() {
                    // Drawn once: another path to it adds an edge only.
                    if let Some(hub) = parent_scene {
                        push_edge(&mut scene, &mut lines, hub, existing);
                    }
                    placed[index] = Some(existing);
                    if *expanded
                        && let SceneKind::Node { expanded, .. } = &mut scene.nodes[existing].kind
                    {
                        *expanded = true;
                    }
                    continue;
                }
                if scene.nodes.len() >= cap {
                    hidden.insert(id);
                    skipped[index] = true;
                    capped[index] = true;
                    continue;
                }
                let root = entry.parent.is_none();
                let new = add(
                    &mut scene,
                    &mut by_id,
                    SceneNode {
                        id,
                        kind: SceneKind::Node {
                            node: node.clone(),
                            root,
                            expanded: *expanded,
                        },
                        label: label_of(node),
                        color_key: node.type_name.clone(),
                        parent: parent_scene,
                        path: Some(entry.path.clone()),
                    },
                );
                if let Some(hub) = parent_scene {
                    push_edge(&mut scene, &mut lines, hub, new);
                }
                placed[index] = Some(new);
            }
            EntryKind::Group {
                key,
                count,
                expanded,
            } => {
                let Some(owner) = parent_scene else {
                    skipped[index] = true;
                    continue;
                };
                // A hub is drawn once it is counted and has something: the pills do not
                // appear for every relationship and then go as their counts come in empty.
                if !matches!(count, Loadable::Loaded(n) if *n > 0) {
                    skipped[index] = true;
                    continue;
                }
                let id = SceneId::Hub(key.clone());
                if let Some(existing) = by_id.get(&id).copied() {
                    placed[index] = Some(existing);
                    if *expanded
                        && let SceneKind::Hub { open, .. } = &mut scene.nodes[existing].kind
                    {
                        *open = true;
                    }
                    continue;
                }
                if scene.nodes.len() >= cap {
                    hidden.insert(id);
                    skipped[index] = true;
                    capped[index] = true;
                    continue;
                }
                let arrow = key.group.direction.arrow();
                // Searched (the Explore tree's "Find among"), the hub's neighbours are the
                // matches alone: counted once all are read, else unknown. The relationship's
                // count would offer "+K more" that no page brings.
                let count_value = match state.searches.contains_key(key) {
                    true => state
                        .neighbours
                        .get(key)
                        .filter(|list| list.exhausted)
                        .map(|list| list.rows.len() as u64),
                    false => count.loaded().copied(),
                };
                let label = match count_value {
                    Some(count) => format!("{arrow} {} · {count}", key.group.edge),
                    None => format!("{arrow} {} · …", key.group.edge),
                };
                let hub = add(
                    &mut scene,
                    &mut by_id,
                    SceneNode {
                        id,
                        kind: SceneKind::Hub {
                            key: key.clone(),
                            count: count_value,
                            open: *expanded,
                        },
                        label,
                        color_key: key.group.edge.clone(),
                        parent: Some(owner),
                        path: Some(entry.path.clone()),
                    },
                );
                push_edge(&mut scene, &mut lines, owner, hub);
                placed[index] = Some(hub);
            }
            // A relationship with at most one target is a hub like any other on the canvas, so
            // expanding a node shows its relationships first; opening the hub shows the one
            // node it reaches. (The tree shows it inline, as a field.)
            EntryKind::Field {
                key,
                target: Some(target),
                ..
            } => {
                let Some(owner) = parent_scene else {
                    continue;
                };
                let open = state.is_expanded(&entry.path);
                let hub_id = SceneId::Hub(key.clone());
                let hub = match by_id.get(&hub_id).copied() {
                    // Drawn by another path: open while either path opens it.
                    Some(existing) => {
                        if open
                            && let SceneKind::Hub { open: shown, .. } =
                                &mut scene.nodes[existing].kind
                        {
                            *shown = true;
                        }
                        existing
                    }
                    None if scene.nodes.len() < cap => {
                        let arrow = key.group.direction.arrow();
                        let hub = add(
                            &mut scene,
                            &mut by_id,
                            SceneNode {
                                id: hub_id,
                                kind: SceneKind::Hub {
                                    key: key.clone(),
                                    count: Some(1),
                                    open,
                                },
                                label: format!("{arrow} {} · 1", key.group.edge),
                                color_key: key.group.edge.clone(),
                                parent: Some(owner),
                                path: Some(entry.path.clone()),
                            },
                        );
                        push_edge(&mut scene, &mut lines, owner, hub);
                        hub
                    }
                    None => {
                        hidden.insert(hub_id);
                        if open {
                            hidden.insert(SceneId::Node(target.node.clone()));
                        }
                        continue;
                    }
                };
                placed[index] = Some(hub);
                if !open {
                    continue;
                }
                let id = SceneId::Node(target.node.clone());
                let to = match by_id.get(&id).copied() {
                    Some(existing) => existing,
                    None if scene.nodes.len() < cap => add(
                        &mut scene,
                        &mut by_id,
                        SceneNode {
                            id,
                            kind: SceneKind::Node {
                                node: target.node.clone(),
                                root: false,
                                expanded: false,
                            },
                            label: label_of(&target.node),
                            color_key: target.node.type_name.clone(),
                            parent: Some(hub),
                            path: None,
                        },
                    ),
                    None => {
                        hidden.insert(id);
                        continue;
                    }
                };
                push_edge(&mut scene, &mut lines, hub, to);
            }
            EntryKind::Cycle { node, .. } => {
                if let (Some(hub), Some(existing)) = (
                    parent_scene,
                    by_id.get(&SceneId::Node(node.clone())).copied(),
                ) {
                    push_edge(&mut scene, &mut lines, hub, existing);
                }
            }
            // The canvas draws a grouped relationship as its hub: a value's neighbours hang
            // from the hub, as they do without grouping.
            EntryKind::Bucket { .. } => placed[index] = parent_scene,
            EntryKind::Field { .. }
            | EntryKind::OtherValues { .. }
            | EntryKind::More { .. }
            | EntryKind::Loading { .. }
            | EntryKind::Error { .. } => {}
        }
    }

    // "+K more" under every open hub with neighbours not drawn.
    let hubs: Vec<usize> = (0..scene.nodes.len())
        .filter(|ix| matches!(scene.nodes[*ix].kind, SceneKind::Hub { open: true, .. }))
        .collect();
    let mut drawn_from: HashMap<usize, u64> = HashMap::new();
    for edge in &scene.edges {
        *drawn_from.entry(edge.from).or_default() += 1;
    }
    for hub in hubs {
        let SceneKind::Hub { key, count, .. } = scene.nodes[hub].kind.clone() else {
            continue;
        };
        let drawn = drawn_from.get(&hub).copied().unwrap_or(0);
        let total = count.unwrap_or(0);
        if total > drawn && scene.nodes.len() >= cap {
            hidden.insert(SceneId::More(key));
        } else if total > drawn {
            let more = SceneNode {
                id: SceneId::More(key.clone()),
                kind: SceneKind::More {
                    key: key.clone(),
                    hidden: total - drawn,
                },
                label: format!("+{} more", total - drawn),
                color_key: key.group.edge.clone(),
                parent: Some(hub),
                path: scene.nodes[hub].path.clone(),
            };
            let index = add(&mut scene, &mut by_id, more);
            push_edge(&mut scene, &mut lines, hub, index);
        }
    }

    // A chip one path left out and another drew is drawn.
    hidden.retain(|id| !by_id.contains_key(id));
    if !hidden.is_empty() {
        // The table offered is the type most of them are, or lead to.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for id in &hidden {
            let type_name = match id {
                SceneId::Node(node) => node.type_name.as_str(),
                SceneId::Hub(key) | SceneId::More(key) => key.group.other_type.as_str(),
            };
            *counts.entry(type_name).or_default() += 1;
        }
        scene.capped = most_left_out(counts).map(|type_name| (hidden.len(), type_name));
    }
    scene
}

/// Records `entry`, under an entry the cap left out, as not drawn, unless it is drawn by
/// another path.
fn left_out(entry: &Entry, drawn: &HashMap<SceneId, usize>, hidden: &mut HashSet<SceneId>) {
    let id = match &entry.kind {
        EntryKind::Node { node, .. } => SceneId::Node(node.clone()),
        EntryKind::Group {
            key,
            count: Loadable::Loaded(n),
            ..
        } if *n > 0 => SceneId::Hub(key.clone()),
        EntryKind::Field {
            key,
            target: Some(_),
            ..
        } => SceneId::Hub(key.clone()),
        _ => return,
    };
    if !drawn.contains_key(&id) {
        hidden.insert(id);
    }
}

/// The type most of the nodes left out belong to; of types as many, the first by name, so the
/// banner names the same one each time the scene is made.
fn most_left_out<S: AsRef<str>>(counts: impl IntoIterator<Item = (S, usize)>) -> Option<String> {
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.as_ref().cmp(a.0.as_ref())))
        .map(|(type_name, _)| type_name.as_ref().to_owned())
}

fn hub_limit(hub: &SceneNode, limits: &HashMap<GroupKey, usize>) -> usize {
    match &hub.kind {
        SceneKind::Hub { key, .. } => limits.get(key).copied().unwrap_or(HUB_STEP),
        _ => usize::MAX,
    }
}

/// An edge, once.
fn push_edge(scene: &mut Scene, lines: &mut HashSet<(usize, usize)>, from: usize, to: usize) {
    if lines.insert((from, to)) {
        scene.edges.push(SceneEdge { from, to });
    }
}

#[cfg(test)]
mod tests {
    use gq_language::Direction;
    use gq_language::SchemaModel;

    /// Of types with as many nodes left out, the banner names the first by name, whatever
    /// order they were counted in.
    #[test]
    fn the_banner_names_one_type_however_ties_are_counted() {
        for counts in [
            vec![("Person", 250), ("Company", 250), ("Note", 3)],
            vec![("Company", 250), ("Note", 3), ("Person", 250)],
        ] {
            assert_eq!(super::most_left_out(counts).as_deref(), Some("Company"));
        }
        assert_eq!(super::most_left_out(Vec::<(&str, usize)>::new()), None);
    }
    use graph_store::{Neighbour, NeighbourList};

    use super::*;

    /// The cap the tests draw under: the `canvas_max_nodes` setting's default, the cap in use.
    fn node_cap() -> usize {
        studio_settings::StudioOptions::default().canvas_max_nodes as usize
    }

    const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n}\n\
        node Company {\n  slug: String @key\n  name: String\n}\n\
        edge WorksAt: Person -> Company @card(0..)\n\
        edge Knows: Person -> Person @card(0..)\n";

    fn person(id: &str) -> NodeRef {
        NodeRef::new("Person", id)
    }

    /// Ada knows 20 people; Bob, the first, knows Ada back.
    fn exploration() -> (SchemaModel, ExploreState) {
        let schema = SchemaModel::parse(SCHEMA).unwrap();
        let mut state = ExploreState::rooted(person("ada"));
        state.root_status = Loadable::Loaded(());
        let knows = schema
            .relationships_of("Person")
            .iter()
            .find(|g| g.edge == "Knows" && g.direction == Direction::Outgoing)
            .cloned()
            .unwrap();
        let ada_knows = GroupKey::new(person("ada"), knows.clone());
        state.counts.insert(ada_knows.clone(), Loadable::Loaded(20));
        state.neighbours.insert(
            ada_knows,
            NeighbourList {
                rows: (0..20)
                    .map(|i| Neighbour {
                        node: person(&format!("p{i:02}")),
                        label: None,
                        edge: Vec::new(),
                    })
                    .collect(),
                exhausted: true,
                ..Default::default()
            },
        );
        let group = ExplorePath::root().child_group(&knows);
        state.expanded.insert(group.clone());
        let p00 = group.child_node(&person("p00"));
        state.expanded.insert(p00.clone());
        state.expanded.insert(p00.child_group(&knows));
        let p00_knows = GroupKey::new(person("p00"), knows);
        state.counts.insert(p00_knows.clone(), Loadable::Loaded(2));
        state.neighbours.insert(
            p00_knows,
            NeighbourList {
                rows: vec![
                    Neighbour {
                        node: person("ada"),
                        label: None,
                        edge: Vec::new(),
                    },
                    Neighbour {
                        node: person("p05"),
                        label: None,
                        edge: Vec::new(),
                    },
                ],
                exhausted: true,
                ..Default::default()
            },
        );
        (schema, state)
    }

    #[test]
    fn a_single_target_relationship_is_a_hub_that_opens_to_its_node() {
        let schema = SchemaModel::parse(&format!(
            "{SCHEMA}edge Employer: Person -> Company @card(0..1)\n"
        ))
        .unwrap();
        let mut state = ExploreState::rooted(person("ada"));
        state.root_status = Loadable::Loaded(());
        let employer = schema
            .relationships_of("Person")
            .iter()
            .find(|g| g.edge == "Employer")
            .cloned()
            .unwrap();
        let key = GroupKey::new(person("ada"), employer.clone());
        let acme = NodeRef::new("Company", "acme");
        state.neighbours.insert(
            key.clone(),
            NeighbourList {
                rows: vec![Neighbour {
                    node: acme.clone(),
                    label: None,
                    edge: Vec::new(),
                }],
                exhausted: true,
                ..Default::default()
            },
        );
        // Expanding Ada shows the relationship, not the company it reaches.
        let closed = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        let hub = closed
            .index_of(&SceneId::Hub(key.clone()))
            .expect("a hub for the relationship");
        assert!(matches!(
            closed.nodes[hub].kind,
            SceneKind::Hub { open: false, .. }
        ));
        assert!(closed.index_of(&SceneId::Node(acme.clone())).is_none());
        // Opening the hub shows the one company, hanging off the hub.
        state
            .expanded
            .insert(ExplorePath::root().child_group(&employer));
        let open = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        let hub = open.index_of(&SceneId::Hub(key)).unwrap();
        let company = open
            .index_of(&SceneId::Node(acme))
            .expect("the company once open");
        assert_eq!(open.nodes[company].parent, Some(hub));
        assert!(open.edges.iter().any(|e| e.from == hub && e.to == company));
    }

    #[test]
    fn hubs_show_eight_then_more_and_draw_each_node_once() {
        let (schema, state) = exploration();
        let entries = state.entries(&schema);
        let scene = explore_scene(&entries, &state, &HashMap::new(), node_cap());
        let people = scene
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, SceneKind::Node { .. }))
            .count();
        // Ada and eight neighbours; p05 is among them and drawn once even though p00 knows it.
        assert_eq!(people, 1 + HUB_STEP);
        let more = scene
            .nodes
            .iter()
            .find(|n| matches!(n.kind, SceneKind::More { .. }))
            .unwrap();
        assert_eq!(more.label, "+12 more");
        // The cycle back to Ada and the second path to p05 are edges, not nodes.
        let ada = scene.index_of(&SceneId::Node(person("ada"))).unwrap();
        let p05 = scene.index_of(&SceneId::Node(person("p05"))).unwrap();
        assert!(scene.edges.iter().any(|e| e.to == ada && e.from != ada));
        assert_eq!(scene.edges.iter().filter(|e| e.to == p05).count(), 2);

        let mut limits = HashMap::new();
        let key = scene
            .nodes
            .iter()
            .find_map(|n| match &n.kind {
                SceneKind::Hub { key, .. }
                    if key.node == person("ada")
                        && key.group.edge == "Knows"
                        && key.group.direction == Direction::Outgoing =>
                {
                    Some(key.clone())
                }
                _ => None,
            })
            .unwrap();
        limits.insert(key, HUB_STEP * 3);
        let wider = explore_scene(&entries, &state, &limits, node_cap());
        assert!(
            !wider
                .nodes
                .iter()
                .any(|n| matches!(n.kind, SceneKind::More { .. }))
        );
    }

    /// A hub the Explore tree searched holds the matches alone: it counts them once all are
    /// read, and offers no "+K more" of the relationship's other neighbours, which no page
    /// brings. Before, it showed the relationship's count beside three matches.
    #[test]
    fn a_searched_hub_counts_its_matches() {
        let (schema, mut state) = exploration();
        let knows = schema
            .relationships_of("Person")
            .iter()
            .find(|g| g.edge == "Knows" && g.direction == Direction::Outgoing)
            .cloned()
            .unwrap();
        let ada_knows = GroupKey::new(person("ada"), knows);
        state.searches.insert(ada_knows.clone(), "p1".into());
        let matches = NeighbourList {
            rows: ["p10", "p11", "p12"]
                .iter()
                .map(|id| Neighbour {
                    node: person(id),
                    label: None,
                    edge: Vec::new(),
                })
                .collect(),
            exhausted: true,
            ..Default::default()
        };
        state.neighbours.insert(ada_knows.clone(), matches);
        let scene = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        let hub = scene.index_of(&SceneId::Hub(ada_knows.clone())).unwrap();
        assert!(matches!(
            scene.nodes[hub].kind,
            SceneKind::Hub { count: Some(3), .. }
        ));
        assert!(scene.index_of(&SceneId::More(ada_knows)).is_none());
    }

    /// p05, reached from Ada and from p00, is one chip, open while either path is: closing it
    /// closes both, so it does not stay open by the path not drawn.
    #[test]
    fn a_node_reached_twice_is_closed_by_both_paths() {
        let (schema, mut state) = exploration();
        let knows = schema
            .relationships_of("Person")
            .iter()
            .find(|g| g.edge == "Knows" && g.direction == Direction::Outgoing)
            .cloned()
            .unwrap();
        let from_ada = ExplorePath::root()
            .child_group(&knows)
            .child_node(&person("p05"));
        let from_p00 = ExplorePath::root()
            .child_group(&knows)
            .child_node(&person("p00"))
            .child_group(&knows)
            .child_node(&person("p05"));
        let p05 = SceneId::Node(person("p05"));
        let opened = |state: &ExploreState| {
            let scene = explore_scene(&state.entries(&schema), state, &HashMap::new(), node_cap());
            let chip = &scene.nodes[scene.index_of(&p05).unwrap()];
            matches!(chip.kind, SceneKind::Node { expanded: true, .. })
        };
        state.expand(&from_p00);
        assert!(opened(&state), "open by the path not drawn first");
        state.expand(&from_ada);
        let mut paths = open_paths(&state.entries(&schema), &state, std::slice::from_ref(&p05));
        paths.sort_by_key(|path| format!("{path:?}"));
        let mut both = vec![from_ada, from_p00];
        both.sort_by_key(|path| format!("{path:?}"));
        assert_eq!(paths, both);
        for path in &paths {
            state.collapse(path);
        }
        assert!(!opened(&state));
        assert!(open_paths(&state.entries(&schema), &state, &[p05]).is_empty());
    }

    /// Bob is reached directly and through Cid; his employer, opened by the second path only,
    /// is one hub that shows open, with the company it reaches.
    #[test]
    fn a_single_target_hub_reached_twice_is_open_by_either_path() {
        let schema = SchemaModel::parse(&format!(
            "{SCHEMA}edge Employer: Person -> Company @card(0..1)\n"
        ))
        .unwrap();
        let group = |edge: &str| {
            schema
                .relationships_of("Person")
                .iter()
                .find(|g| g.edge == edge && g.direction == Direction::Outgoing)
                .cloned()
                .unwrap()
        };
        let (knows, employer) = (group("Knows"), group("Employer"));
        let (ada, bob, cid) = (person("ada"), person("bob"), person("cid"));
        let acme = NodeRef::new("Company", "acme");
        let list = |nodes: Vec<NodeRef>| NeighbourList {
            rows: nodes
                .into_iter()
                .map(|node| Neighbour {
                    node,
                    label: None,
                    edge: Vec::new(),
                })
                .collect(),
            exhausted: true,
            ..Default::default()
        };
        let mut state = ExploreState::rooted(ada.clone());
        state.root_status = Loadable::Loaded(());
        // A hub is drawn once counted.
        let ada_knows_key = GroupKey::new(ada, knows.clone());
        state
            .counts
            .insert(ada_knows_key.clone(), Loadable::Loaded(2));
        state
            .neighbours
            .insert(ada_knows_key, list(vec![bob.clone(), cid.clone()]));
        let cid_knows_key = GroupKey::new(cid.clone(), knows.clone());
        state
            .counts
            .insert(cid_knows_key.clone(), Loadable::Loaded(1));
        state
            .neighbours
            .insert(cid_knows_key, list(vec![bob.clone()]));
        let key = GroupKey::new(bob.clone(), employer.clone());
        state.counts.insert(key.clone(), Loadable::Loaded(1));
        state
            .neighbours
            .insert(key.clone(), list(vec![acme.clone()]));
        let ada_knows = ExplorePath::root().child_group(&knows);
        let direct = ada_knows.child_node(&bob);
        let cid_knows = ada_knows.child_node(&cid).child_group(&knows);
        let via_cid = cid_knows.child_node(&bob);
        for path in [
            ada_knows.clone(),
            direct,
            ada_knows.child_node(&cid),
            cid_knows,
            via_cid.clone(),
            via_cid.child_group(&employer),
        ] {
            state.expanded.insert(path);
        }
        let scene = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        let hub = scene.index_of(&SceneId::Hub(key)).unwrap();
        assert!(matches!(
            scene.nodes[hub].kind,
            SceneKind::Hub { open: true, .. }
        ));
        assert!(scene.index_of(&SceneId::Node(acme)).is_some());
    }

    #[test]
    fn the_cap_stops_drawing_and_says_how_many_are_left_out() {
        let (schema, state) = exploration();
        let entries = state.entries(&schema);
        let mut limits = HashMap::new();
        for entry in &entries {
            if let EntryKind::Group { key, .. } = &entry.kind {
                limits.insert(key.clone(), 100);
            }
        }
        let scene = explore_scene(&entries, &state, &limits, 6);
        assert!(scene.nodes.len() <= 6);
        let (hidden, type_name) = scene.capped.clone().unwrap();
        assert!(hidden > 0);
        assert_eq!(type_name, "Person");
    }

    /// At a cap that leaves only Ada drawn, the banner counts what it left out: Ada's three
    /// counted relationship chip (the two not counted yet are not drawn, so not left out), the
    /// 20 people under it (p05 once, though p00 reaches it too), and p00's counted chip.
    #[test]
    fn the_cap_counts_relationships_and_each_node_once() {
        let (schema, state) = exploration();
        let scene = explore_scene(&state.entries(&schema), &state, &HashMap::new(), 1);
        assert_eq!(scene.nodes.len(), 1);
        assert_eq!(scene.capped, Some((1 + 20 + 1, "Person".into())));
    }
}
