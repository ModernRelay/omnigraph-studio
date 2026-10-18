//! What the canvas draws, without GPUI: the schema graph for the landing view, and the
//! exploration as nodes, hubs (one per opened relationship group) and "+K more" chips, each
//! node drawn once.

use std::collections::{HashMap, HashSet};

use gq_language::{Direction, SchemaModel};
use graph_store::{Entry, EntryKind, ExplorePath, ExploreState, GroupKey, Loadable, NodeRef};

/// Neighbours a hub shows before "+K more", and how many each click on it adds.
pub const HUB_STEP: usize = 8;

/// The most nodes the canvas draws by default; past the cap it says so (constitution IX). The
/// cap in use is the graph's `canvas_max_nodes` setting.
#[cfg(test)]
pub const NODE_CAP: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SceneId {
    Type(String),
    Node(NodeRef),
    Hub(GroupKey),
    More(GroupKey),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SceneKind {
    /// A node type in the schema view, with its row count when known.
    Type { count: Option<i64> },
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
    pub radius: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SceneEdge {
    pub from: usize,
    pub to: usize,
    pub color_key: String,
    /// Drawn with an arrowhead and a label (the schema view and single-target fields).
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scene {
    pub nodes: Vec<SceneNode>,
    pub edges: Vec<SceneEdge>,
    /// Nodes left out because of the node cap, and the type most of them belong to.
    pub capped: Option<(usize, String)>,
}

impl Scene {
    pub fn index_of(&self, id: &SceneId) -> Option<usize> {
        self.nodes.iter().position(|node| node.id == *id)
    }
}

/// The radius of a type in the schema view: grows with the log of its count. Counts change
/// sizes, never positions (spec 006, constitution V).
pub fn type_radius(count: Option<i64>) -> f32 {
    let count = count.unwrap_or(0).max(0) as f32;
    22. + 7. * (count + 1.).log10()
}

pub const NODE_RADIUS: f32 = 9.;
pub const ROOT_RADIUS: f32 = 13.;
pub const HUB_RADIUS: f32 = 6.;
pub const MORE_RADIUS: f32 = 7.;

/// The schema graph: one node per type, sorted by name, one edge per edge type.
pub fn schema_scene(schema: &SchemaModel, count_of: impl Fn(&str) -> Option<i64>) -> Scene {
    let mut names: Vec<&str> = schema.nodes.iter().map(|n| n.name.as_str()).collect();
    names.sort_unstable();
    let nodes: Vec<SceneNode> = names
        .iter()
        .map(|name| {
            let count = count_of(name);
            SceneNode {
                id: SceneId::Type((*name).to_owned()),
                kind: SceneKind::Type { count },
                label: (*name).to_owned(),
                color_key: (*name).to_owned(),
                parent: None,
                path: None,
                radius: type_radius(count),
            }
        })
        .collect();
    let index = |name: &str| names.iter().position(|n| *n == name);
    let edges = schema
        .edges
        .iter()
        .filter_map(|edge| {
            Some(SceneEdge {
                from: index(&edge.from)?,
                to: index(&edge.to)?,
                color_key: edge.name.clone(),
                label: Some(edge.name.clone()),
            })
        })
        .collect();
    Scene {
        nodes,
        edges,
        capped: None,
    }
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
            radius: NODE_RADIUS,
        })
        .collect();
    let shown = scene_nodes.len();
    let edges = links
        .iter()
        .filter(|(from, to)| *from < shown && *to < shown && from != to)
        .map(|(from, to)| SceneEdge {
            from: *from,
            to: *to,
            color_key: scene_nodes[*to].color_key.clone(),
            label: None,
        })
        .collect();
    let capped = (capped > 0).then(|| {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for (node, _) in nodes.iter().skip(cap) {
            *counts.entry(node.type_name.as_str()).or_default() += 1;
        }
        let most = counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(t, _)| t.to_owned())
            .unwrap_or_default();
        (capped, most)
    });
    Scene {
        nodes: scene_nodes,
        edges,
        capped,
    }
}

/// The exploration as the canvas draws it. `limits` holds how many neighbours each opened hub
/// shows; a hub without an entry shows [`HUB_STEP`].
pub fn explore_scene(
    entries: &[Entry],
    state: &ExploreState,
    schema: &SchemaModel,
    limits: &HashMap<GroupKey, usize>,
    cap: usize,
) -> Scene {
    let mut scene = Scene::default();
    let mut by_id: HashMap<SceneId, usize> = HashMap::new();
    // The scene index each entry maps to (its node or hub), and entries left out.
    let mut placed: Vec<Option<usize>> = vec![None; entries.len()];
    let mut skipped = vec![false; entries.len()];
    let mut shown_per_hub: HashMap<usize, usize> = HashMap::new();
    // The lines drawn so far, so a line is drawn once without searching every line.
    let mut lines: HashSet<(usize, usize)> = HashSet::new();
    let mut hidden: HashMap<String, usize> = HashMap::new();
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
                    *hidden.entry(node.type_name.clone()).or_default() += 1;
                    skipped[index] = true;
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
                        radius: if root { ROOT_RADIUS } else { NODE_RADIUS },
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
                if *count == Loadable::Loaded(0) {
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
                    skipped[index] = true;
                    continue;
                }
                let arrow = match key.group.direction {
                    Direction::Outgoing => "→",
                    Direction::Incoming => "←",
                };
                let count_value = count.loaded().copied();
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
                        radius: HUB_RADIUS,
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
                    Some(existing) => existing,
                    None if scene.nodes.len() < cap => {
                        let arrow = match key.group.direction {
                            Direction::Outgoing => "→",
                            Direction::Incoming => "←",
                        };
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
                                radius: HUB_RADIUS,
                            },
                        );
                        push_edge(&mut scene, &mut lines, owner, hub);
                        hub
                    }
                    None => continue,
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
                            radius: NODE_RADIUS,
                        },
                    ),
                    None => continue,
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
    for edge in scene.edges.iter().filter(|edge| edge.label.is_none()) {
        *drawn_from.entry(edge.from).or_default() += 1;
    }
    for hub in hubs {
        let SceneKind::Hub { key, count, .. } = scene.nodes[hub].kind.clone() else {
            continue;
        };
        let drawn = drawn_from.get(&hub).copied().unwrap_or(0);
        let total = count.unwrap_or(0);
        if total > drawn && scene.nodes.len() < cap {
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
                radius: MORE_RADIUS,
            };
            let index = add(&mut scene, &mut by_id, more);
            push_edge(&mut scene, &mut lines, hub, index);
        }
    }

    if !hidden.is_empty() {
        let total = hidden.values().sum();
        let (type_name, _) = hidden
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
            .expect("not empty");
        scene.capped = Some((total, type_name));
    }
    let _ = schema;
    scene
}

fn hub_limit(hub: &SceneNode, limits: &HashMap<GroupKey, usize>) -> usize {
    match &hub.kind {
        SceneKind::Hub { key, .. } => limits.get(key).copied().unwrap_or(HUB_STEP),
        _ => usize::MAX,
    }
}

/// An unlabelled edge, in the color of the relationship: the hub's, whichever end it is.
fn push_edge(scene: &mut Scene, lines: &mut HashSet<(usize, usize)>, from: usize, to: usize) {
    let hub_end = if matches!(scene.nodes[to].kind, SceneKind::Hub { .. }) {
        to
    } else {
        from
    };
    let color_key = scene.nodes[hub_end].color_key.clone();
    if lines.insert((from, to)) {
        scene.edges.push(SceneEdge {
            from,
            to,
            color_key,
            label: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use graph_store::{Neighbour, NeighbourList};

    use super::*;

    const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n}\n\
        node Company {\n  slug: String @key\n  name: String\n}\n\
        edge WorksAt: Person -> Company @card(0..)\n\
        edge Knows: Person -> Person @card(0..)\n";

    fn person(id: &str) -> NodeRef {
        NodeRef::new("Person", id)
    }

    #[test]
    fn the_schema_scene_is_sorted_by_name_with_one_edge_per_edge_type() {
        let schema = SchemaModel::parse(SCHEMA).unwrap();
        let scene = schema_scene(&schema, |name| (name == "Person").then_some(200));
        let names: Vec<&str> = scene.nodes.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(names, ["Company", "Person"]);
        assert_eq!(scene.edges.len(), 2);
        assert!(scene.nodes[1].radius > scene.nodes[0].radius);
        let knows = scene
            .edges
            .iter()
            .find(|e| e.label.as_deref() == Some("Knows"))
            .unwrap();
        assert_eq!((knows.from, knows.to), (1, 1));
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
        let closed = explore_scene(
            &state.entries(&schema),
            &state,
            &schema,
            &HashMap::new(),
            NODE_CAP,
        );
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
        let open = explore_scene(
            &state.entries(&schema),
            &state,
            &schema,
            &HashMap::new(),
            NODE_CAP,
        );
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
        let scene = explore_scene(&entries, &state, &schema, &HashMap::new(), NODE_CAP);
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
        let wider = explore_scene(&entries, &state, &schema, &limits, NODE_CAP);
        assert!(
            !wider
                .nodes
                .iter()
                .any(|n| matches!(n.kind, SceneKind::More { .. }))
        );
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
        let scene = explore_scene(&entries, &state, &schema, &limits, 6);
        assert!(scene.nodes.len() <= 6);
        let (hidden, type_name) = scene.capped.clone().unwrap();
        assert!(hidden > 0);
        assert_eq!(type_name, "Person");
    }
}
