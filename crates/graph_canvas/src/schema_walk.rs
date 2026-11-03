//! The ontology walked on the canvas (spec 006, "Ontology on the canvas"): types grow out of a
//! starting type as a node's neighbours grow out of it, drawn with the canvas's own chips,
//! pills and layout. What differs from a walk of data:
//!
//! - A relationship reaches one type, so expanding a type shows the types related to it at
//!   once: one pill per related type, whichever way its relationships run, named with its
//!   first relationship and how many more ("→ AssignedTo +1", "⇄ Owns +2" both ways), and the
//!   type beyond it. A walk of data takes two steps, a relationship and then its nodes.
//! - Each direction shows [`HUB_STEP`] related types before "+K more", as a relationship shows
//!   its neighbours.
//! - A type is drawn once, and a pill between two types once: expanding the second of two
//!   related types draws no second pill for what the first already shows.
//!
//! No GPUI: the scene is a function of the schema and the walk, laid out and painted as the
//! canvas does any other.

use std::collections::{HashMap, HashSet, VecDeque};

use gq_language::{Direction, RelationshipGroup, SchemaModel};
use graph_store::{GroupKey, NodeRef};

use crate::model::{HUB_STEP, Scene, SceneEdge, SceneId, SceneKind, SceneNode};

/// Which types are open, from which the walk started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaWalk {
    pub root: String,
    /// The types expanded, in the order they were.
    pub expanded: Vec<String>,
}

impl SchemaWalk {
    /// A walk from `root`, expanded.
    pub(crate) fn new(root: impl Into<String>) -> Self {
        let root = root.into();
        Self {
            expanded: vec![root.clone()],
            root,
        }
    }

    /// A walk from `root` with every type expanded: a small schema shown whole.
    pub(crate) fn whole(schema: &SchemaModel, root: impl Into<String>) -> Self {
        let root = root.into();
        let mut expanded = vec![root.clone()];
        expanded.extend(
            schema
                .nodes
                .iter()
                .map(|node| node.name.clone())
                .filter(|name| *name != root),
        );
        Self { root, expanded }
    }

    pub(crate) fn is_expanded(&self, ty: &str) -> bool {
        self.expanded.iter().any(|open| open == ty)
    }

    pub(crate) fn expand(&mut self, ty: &str) {
        if !self.is_expanded(ty) {
            self.expanded.push(ty.to_owned());
        }
    }

    /// Closes `ty`; the root closed shows it alone.
    pub(crate) fn collapse(&mut self, ty: &str) {
        self.expanded.retain(|open| open != ty);
    }

    /// Forgets the types expanded that `scene` no longer draws, so a type closed away does not
    /// come back open when it is reached again.
    pub(crate) fn keep_drawn(&mut self, scene: &Scene) {
        let drawn: HashSet<&str> = scene
            .nodes
            .iter()
            .filter_map(|node| match &node.id {
                SceneId::Node(node) => Some(node.type_name.as_str()),
                _ => None,
            })
            .collect();
        self.expanded.retain(|open| drawn.contains(open.as_str()));
    }
}

/// A type as the canvas's chips name it.
pub(crate) fn type_ref(name: &str) -> NodeRef {
    NodeRef::new(name, name)
}

/// The "+K more" of `ty`'s types in `direction`, by which [`HUB_STEP`]s shown are counted.
pub(crate) fn more_key(ty: &str, direction: Direction) -> GroupKey {
    GroupKey::new(
        type_ref(ty),
        RelationshipGroup {
            edge: String::new(),
            direction,
            other_type: String::new(),
        },
    )
}

/// A type related to another, and the relationships between them each way, by name.
struct Related {
    other: String,
    /// The relationships it is reached by, its relationships to itself among them.
    out: Vec<String>,
    /// Those reaching it.
    into: Vec<String>,
}

impl Related {
    /// The side it stands on: what a type reaches, or reaches and is reached by, on its
    /// right; what only reaches it, on its left.
    fn direction(&self) -> Direction {
        if self.out.is_empty() {
            Direction::Incoming
        } else {
            Direction::Outgoing
        }
    }
}

/// The types related to `ty` in `direction` ([`Related::direction`]): those on its right in
/// the order of their first relationship, by name; those on its left by name.
fn related(schema: &SchemaModel, ty: &str, direction: Direction) -> Vec<Related> {
    let mut edges: Vec<&gq_language::schema::EdgeType> = schema
        .edges
        .iter()
        .filter(|edge| edge.from == ty || edge.to == ty)
        .collect();
    edges.sort_by(|a, b| a.name.cmp(&b.name));
    let mut groups: Vec<Related> = Vec::new();
    for edge in edges {
        let reached = edge.from == ty;
        let other = if reached { &edge.to } else { &edge.from };
        let group = match groups.iter().position(|g| g.other == *other) {
            Some(at) => &mut groups[at],
            None => {
                groups.push(Related {
                    other: other.clone(),
                    out: Vec::new(),
                    into: Vec::new(),
                });
                groups.last_mut().expect("just pushed")
            }
        };
        if reached {
            group.out.push(edge.name.clone());
        } else {
            group.into.push(edge.name.clone());
        }
    }
    groups.retain(|group| group.direction() == direction);
    if direction == Direction::Incoming {
        groups.sort_by(|a, b| a.other.cmp(&b.other));
    }
    groups
}

/// A pill's name: the way its relationships run ("⇄" both ways; "↻" a type's to itself, since
/// the pill hangs off its own chip), its first relationship, and how many more.
fn pill_label(related: &Related, to_itself: bool) -> String {
    let arrow = if to_itself {
        "↻"
    } else if !related.out.is_empty() && !related.into.is_empty() {
        "⇄"
    } else {
        related.direction().arrow()
    };
    let first = related.out.first().or(related.into.first());
    let more = related.out.len() + related.into.len() - 1;
    match (first, more) {
        (Some(name), 0) => format!("{arrow} {name}"),
        (Some(name), more) => format!("{arrow} {name} +{more}"),
        (None, _) => arrow.to_owned(),
    }
}

/// The walk as the canvas draws it: the root, then each expanded type's related types in the
/// order they are reached, under the chip cap. `limits` holds how many related types each
/// "+K more" shows; one without an entry shows [`HUB_STEP`].
pub(crate) fn schema_scene(
    schema: &SchemaModel,
    walk: &SchemaWalk,
    limits: &HashMap<GroupKey, usize>,
    cap: usize,
) -> Scene {
    let mut scene = Scene {
        cap,
        ..Scene::default()
    };
    if !schema.nodes.iter().any(|node| node.name == walk.root) {
        return scene;
    }
    let mut type_at: HashMap<String, usize> = HashMap::new();
    let mut lines: HashSet<(usize, usize)> = HashSet::new();
    // Pairs of types a pill joins already, either way: drawn once.
    let mut joined: HashSet<(String, String)> = HashSet::new();
    let pair = |a: &str, b: &str| {
        if a <= b {
            (a.to_owned(), b.to_owned())
        } else {
            (b.to_owned(), a.to_owned())
        }
    };
    let mut left_out = 0usize;
    let edge = |scene: &mut Scene, lines: &mut HashSet<(usize, usize)>, from: usize, to: usize| {
        if from != to && lines.insert((from, to)) {
            scene.edges.push(SceneEdge { from, to });
        }
    };
    let add_type = |scene: &mut Scene,
                    type_at: &mut HashMap<String, usize>,
                    name: &str,
                    parent: Option<usize>| {
        let index = scene.nodes.len();
        type_at.insert(name.to_owned(), index);
        scene.nodes.push(SceneNode {
            id: SceneId::Node(type_ref(name)),
            kind: SceneKind::Node {
                node: type_ref(name),
                root: parent.is_none(),
                expanded: walk.is_expanded(name),
            },
            label: name.to_owned(),
            color_key: name.to_owned(),
            parent,
            path: None,
        });
        index
    };
    let root = add_type(&mut scene, &mut type_at, &walk.root, None);
    let mut queue = VecDeque::from([(walk.root.clone(), root)]);
    while let Some((ty, at)) = queue.pop_front() {
        if !walk.is_expanded(&ty) {
            continue;
        }
        for direction in [Direction::Outgoing, Direction::Incoming] {
            let groups: Vec<Related> = related(schema, &ty, direction)
                .into_iter()
                .filter(|group| !joined.contains(&pair(&ty, &group.other)))
                .collect();
            let more = more_key(&ty, direction);
            let limit = limits.get(&more).copied().unwrap_or(HUB_STEP);
            for (shown, group) in groups.iter().enumerate() {
                let other = &group.other;
                let names: Vec<String> = group.out.iter().chain(&group.into).cloned().collect();
                if shown >= limit {
                    break;
                }
                if scene.nodes.len() + 2 > cap {
                    left_out += groups.len() - shown;
                    break;
                }
                joined.insert(pair(&ty, other));
                let key = GroupKey::new(
                    type_ref(&ty),
                    RelationshipGroup {
                        edge: names.join(", "),
                        direction,
                        other_type: other.clone(),
                    },
                );
                let pill = scene.nodes.len();
                scene.nodes.push(SceneNode {
                    id: SceneId::Hub(key.clone()),
                    kind: SceneKind::Hub {
                        key,
                        count: Some(names.len() as u64),
                        open: true,
                    },
                    label: pill_label(group, *other == ty),
                    color_key: names[0].clone(),
                    parent: Some(at),
                    path: None,
                });
                let to = match type_at.get(other).copied() {
                    Some(existing) => existing,
                    None => {
                        let new = add_type(&mut scene, &mut type_at, other, Some(pill));
                        queue.push_back((other.clone(), new));
                        new
                    }
                };
                // A type's pill to itself hangs off it alone: its "↻" says where it leads.
                edge(&mut scene, &mut lines, at, pill);
                if to != at {
                    edge(&mut scene, &mut lines, pill, to);
                }
            }
            if groups.len() > limit && scene.nodes.len() < cap {
                let hidden = groups.len() - limit;
                scene.nodes.push(SceneNode {
                    id: SceneId::More(more.clone()),
                    kind: SceneKind::More {
                        key: more,
                        hidden: hidden as u64,
                    },
                    label: format!("+{hidden} more"),
                    color_key: ty.clone(),
                    parent: Some(at),
                    path: None,
                });
                let index = scene.nodes.len() - 1;
                edge(&mut scene, &mut lines, at, index);
            }
        }
    }
    if left_out > 0 {
        scene.capped = Some((left_out, String::new()));
    }
    scene
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = r#"
node Person { slug: String @key }
node Task { slug: String @key }
node Project { slug: String @key }
node Org { slug: String @key }
edge AssignedTo: Task -> Person
edge ReviewedBy: Task -> Person
edge HasTask: Project -> Task
edge BlockedBy: Task -> Task
edge Owns: Person -> Project
edge WorksAt: Person -> Org
"#;

    fn schema() -> SchemaModel {
        SchemaModel::parse(SCHEMA).unwrap()
    }

    fn labels(scene: &Scene) -> Vec<&str> {
        scene.nodes.iter().map(|node| node.label.as_str()).collect()
    }

    /// Expanding a type shows a pill per related type, named and counted, and
    /// the type beyond it, at once; its relationship to itself once, among those it reaches.
    #[test]
    fn a_type_expands_to_its_related_types_at_once() {
        let scene = schema_scene(&schema(), &SchemaWalk::new("Task"), &HashMap::new(), 100);
        assert_eq!(
            labels(&scene),
            [
                "Task",
                "→ AssignedTo +1",
                "Person",
                "↻ BlockedBy",
                "← HasTask",
                "Project",
            ]
        );
        // The pill to itself joins Task to Task: one line, from Task to the pill.
        let pill = labels(&scene)
            .iter()
            .position(|l| *l == "↻ BlockedBy")
            .unwrap();
        assert!(scene.edges.iter().any(|e| e.from == 0 && e.to == pill));
        assert!(!scene.edges.iter().any(|e| e.from == pill));
        assert!(!scene.edges.iter().any(|e| e.from == e.to));
    }

    /// Expanding a second type draws no second pill for a relationship the first shows, and
    /// links a type already drawn rather than drawing it again.
    #[test]
    fn a_pill_and_a_type_are_drawn_once() {
        let mut walk = SchemaWalk::new("Task");
        walk.expand("Person");
        let scene = schema_scene(&schema(), &walk, &HashMap::new(), 100);
        let names = labels(&scene);
        assert_eq!(names.iter().filter(|n| **n == "Person").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "Project").count(), 1);
        // Person's own: Owns to Project (already drawn) and WorksAt to Org; Task's two
        // relationships to Person are not drawn again from Person's side.
        assert!(names.contains(&"→ Owns"));
        assert!(names.contains(&"→ WorksAt"));
        assert!(
            !names
                .iter()
                .any(|n| n.starts_with('←') && n.contains("AssignedTo"))
        );
        let project = names.iter().position(|n| *n == "Project").unwrap();
        let owns = names.iter().position(|n| *n == "→ Owns").unwrap();
        assert!(
            scene
                .edges
                .iter()
                .any(|e| e.from == owns && e.to == project)
        );
    }

    /// Past the step a direction shows "+K more", and its limit shows more.
    #[test]
    fn a_direction_shows_more_past_its_step() {
        let mut text = String::from("node Hub { slug: String @key }\n");
        for t in 0..12 {
            text +=
                &format!("node T{t:02} {{ slug: String @key }}\nedge R{t:02}: Hub -> T{t:02}\n");
        }
        let schema = SchemaModel::parse(&text).unwrap();
        let walk = SchemaWalk::new("Hub");
        let scene = schema_scene(&schema, &walk, &HashMap::new(), 100);
        assert_eq!(
            scene
                .nodes
                .iter()
                .filter(|n| matches!(n.kind, SceneKind::Hub { .. }))
                .count(),
            HUB_STEP
        );
        assert!(labels(&scene).contains(&"+4 more"));
        let limits = HashMap::from([(more_key("Hub", Direction::Outgoing), 12)]);
        let scene = schema_scene(&schema, &walk, &limits, 100);
        assert_eq!(
            scene
                .nodes
                .iter()
                .filter(|n| matches!(n.kind, SceneKind::Hub { .. }))
                .count(),
            12
        );
        assert!(!labels(&scene).iter().any(|l| l.ends_with("more")));
    }

    /// Relationships both ways between two types are one pill, on the side of what the type
    /// reaches, marked "⇄".
    #[test]
    fn both_ways_is_one_pill() {
        let schema = SchemaModel::parse(
            "node Task { slug: String @key }\nnode Media { slug: String @key }\n\
             edge Cites: Task -> Media\nedge MediaForTask: Media -> Task\n",
        )
        .unwrap();
        let scene = schema_scene(&schema, &SchemaWalk::new("Task"), &HashMap::new(), 100);
        assert_eq!(labels(&scene), ["Task", "⇄ Cites +1", "Media"]);
        let SceneKind::Hub { key, count, .. } = &scene.nodes[1].kind else {
            panic!("a pill");
        };
        assert_eq!(key.group.direction, Direction::Outgoing);
        assert_eq!(*count, Some(2));
    }

    /// Closing a type takes away what only it reached; what it reached closes with it.
    #[test]
    fn closing_a_type_takes_its_types_away() {
        let mut walk = SchemaWalk::new("Task");
        walk.expand("Person");
        walk.collapse("Task");
        let scene = schema_scene(&schema(), &walk, &HashMap::new(), 100);
        assert_eq!(labels(&scene), ["Task"]);
        walk.keep_drawn(&scene);
        assert!(walk.expanded.is_empty());
    }
}
