//! A review read by entity (spec 008): each changed node with the relationship changes that came
//! with it, rather than one list of nodes and another of edges.
//!
//! A relationship change is reviewed in one group, its home: with the end this branch added,
//! else with an end this branch changed or removed, else with its source, which then stands in
//! the review unchanged for its relationships. When the other end has a group too, the
//! relationship is mirrored there, so either entity tells its whole story while each change is
//! counted, reviewed and reverted once.

use std::collections::HashMap;

use gq_language::{Direction, SchemaModel};

use crate::changes::{FoldedChange, FoldedChanges, NetChange};

/// Where a group sits: entities the target holds first, then the ones this branch added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupSection {
    /// Changed or removed here, or unchanged with relationships that changed.
    Existing,
    Added,
}

/// One relationship change, seen from a group's entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRelationship {
    /// The edge's change, in [`FoldedChanges::changes`].
    pub change: usize,
    /// `Outgoing` when the group's entity is the edge's source.
    pub direction: Direction,
    pub other_type: String,
    pub other_id: String,
    /// The other end's group, when the review has one. For a mirror, the group the
    /// relationship is reviewed in.
    pub other_group: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityGroup {
    pub section: GroupSection,
    /// The node's type, or the edge's for a relationship with no end to stand under (its
    /// type is not in the schema).
    pub type_name: String,
    pub is_edge: bool,
    pub id: String,
    /// The entity's own change; `None` for a node shown only for its relationships.
    pub change: Option<usize>,
    /// The relationship changes reviewed here.
    pub relationships: Vec<GroupRelationship>,
    /// Relationship changes reviewed in another group that this entity is an end of.
    pub mirrors: Vec<GroupRelationship>,
    /// Relationships moved to another end, `(removed, added)` changes: for an edge type that
    /// allows one target per source (`@card(..1)`), the one removal and one addition of it from
    /// this entity, which read as one change.
    pub repoints: Vec<(usize, usize)>,
}

impl EntityGroup {
    /// Every change reviewed in the group: its own, then its relationships'.
    pub fn changes(&self) -> impl Iterator<Item = usize> + '_ {
        self.change
            .into_iter()
            .chain(self.relationships.iter().map(|r| r.change))
    }

    /// The change that stands for the group when it is selected: its own, else its first
    /// relationship's.
    pub fn primary(&self) -> Option<usize> {
        self.changes().next()
    }
}

/// Names of nodes the review knows only by id, by `(type, id)`, as read from the graph.
pub type NodeNames = HashMap<(String, String), String>;

/// A run of consecutive groups of one type, as a list shows them under one header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRun {
    /// The section the run opens, when it is the first of its section shown.
    pub section: Option<GroupSection>,
    pub type_name: String,
    pub is_edge: bool,
    pub groups: Vec<usize>,
    /// The groups' own changes: added, changed and removed entities.
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    /// Relationship changes reviewed on the run's groups.
    pub relationships: usize,
}

/// One line of a group's card: a relationship change reviewed there, or a mirror of one
/// reviewed on another card, with its other end named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipEntry {
    pub change: usize,
    /// `None` for a mirror.
    pub net: Option<NetChange>,
    /// The edge type in words ([`relationship_words`]).
    pub words: String,
    pub outgoing: bool,
    /// The other end's name, else its id.
    pub other: String,
    pub other_type: String,
    /// The branch added the other end.
    pub other_is_new: bool,
    /// For a relationship moved to another end ([`EntityGroup::repoints`]): the removed
    /// change this entry stands for too, and its other end, named.
    pub replaces: Option<(usize, String)>,
}

/// A review's changes by entity, in display order: [`GroupSection::Existing`] first, each
/// section by type in the fold's type order, and inside a type changed, removed, unchanged,
/// then added, each in fold order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntityGroups {
    pub groups: Vec<EntityGroup>,
    /// Each change's group, by its index in [`FoldedChanges::changes`].
    pub home: Vec<usize>,
}

impl EntityGroups {
    /// Groups `folded` by entity. `schema` gives each edge type's endpoint types; without it,
    /// or for an edge type it does not have, a relationship stands alone.
    pub fn new(folded: &FoldedChanges, schema: Option<&SchemaModel>) -> Self {
        let changes = &folded.changes;
        let mut groups: Vec<EntityGroup> = Vec::new();
        let mut home = vec![usize::MAX; changes.len()];
        // Nodes the review changed, by (type, id).
        let mut nodes: HashMap<(&str, &str), usize> = HashMap::new();
        // Unchanged nodes standing in for their relationships.
        let mut anchors: HashMap<(String, String), usize> = HashMap::new();

        for (ix, change) in changes.iter().enumerate() {
            if change.is_edge() {
                continue;
            }
            nodes.insert(
                (change.type_name.as_str(), change.id.as_str()),
                groups.len(),
            );
            home[ix] = groups.len();
            groups.push(EntityGroup {
                section: section_of(change),
                type_name: change.type_name.clone(),
                is_edge: false,
                id: change.id.clone(),
                change: Some(ix),
                relationships: Vec::new(),
                mirrors: Vec::new(),
                repoints: Vec::new(),
            });
        }
        let net_of = |group: usize, groups: &[EntityGroup]| {
            groups[group].change.and_then(|c| changes[c].net())
        };
        for (ix, change) in changes.iter().enumerate() {
            if !change.is_edge() {
                continue;
            }
            let ends = change.endpoints().and_then(|(from, to)| {
                let edge = schema?.edge(&change.type_name)?;
                Some(((edge.from.clone(), from), (edge.to.clone(), to)))
            });
            let Some((from, to)) = ends else {
                home[ix] = groups.len();
                groups.push(EntityGroup {
                    section: section_of(change),
                    type_name: change.type_name.clone(),
                    is_edge: true,
                    id: change.id.clone(),
                    change: Some(ix),
                    relationships: Vec::new(),
                    mirrors: Vec::new(),
                    repoints: Vec::new(),
                });
                continue;
            };
            let node =
                |end: &(String, String)| nodes.get(&(end.0.as_str(), end.1.as_str())).copied();
            let added = |end: &(String, String)| {
                node(end).is_some_and(|g| net_of(g, &groups) == Some(NetChange::Added))
            };
            let at_source = if added(&from) {
                true
            } else if added(&to) {
                false
            } else {
                node(&from).is_some() || node(&to).is_none()
            };
            let (here, there, direction) = if at_source {
                (&from, &to, Direction::Outgoing)
            } else {
                (&to, &from, Direction::Incoming)
            };
            let group = match node(here) {
                Some(group) => group,
                None => *anchors.entry(here.clone()).or_insert_with(|| {
                    groups.push(EntityGroup {
                        section: GroupSection::Existing,
                        type_name: here.0.clone(),
                        is_edge: false,
                        id: here.1.clone(),
                        change: None,
                        relationships: Vec::new(),
                        mirrors: Vec::new(),
                        repoints: Vec::new(),
                    });
                    groups.len() - 1
                }),
            };
            home[ix] = group;
            groups[group].relationships.push(GroupRelationship {
                change: ix,
                direction,
                other_type: there.0.clone(),
                other_id: there.1.clone(),
                other_group: None,
            });
        }

        // A one-target relationship removed and added again from the same source moved: it
        // reads as one change, the old end to the new.
        if let Some(schema) = schema {
            for group in &mut groups {
                let mut by_type: HashMap<&str, (Vec<usize>, Vec<usize>)> = HashMap::new();
                for relationship in &group.relationships {
                    let edge = &changes[relationship.change];
                    let one_target = schema
                        .edge(&edge.type_name)
                        .is_some_and(|edge| edge.at_most_one);
                    if relationship.direction != Direction::Outgoing || !one_target {
                        continue;
                    }
                    let (removed, added) = by_type.entry(edge.type_name.as_str()).or_default();
                    match edge.net() {
                        Some(NetChange::Removed) => removed.push(relationship.change),
                        Some(NetChange::Added) => added.push(relationship.change),
                        _ => {}
                    }
                }
                let mut repoints: Vec<(usize, usize)> = by_type
                    .into_values()
                    .filter_map(
                        |(removed, added)| match (removed.as_slice(), added.as_slice()) {
                            (&[removed], &[added]) => Some((removed, added)),
                            _ => None,
                        },
                    )
                    .collect();
                repoints.sort_unstable();
                group.repoints = repoints;
            }
        }

        // Display order. A type ranks where its first group was made: node changes come in the
        // fold's type order, then the anchors and lone relationships the edges brought.
        let mut type_rank: HashMap<(bool, &str), usize> = HashMap::new();
        for (ix, group) in groups.iter().enumerate() {
            type_rank
                .entry((group.is_edge, group.type_name.as_str()))
                .or_insert(ix);
        }
        let net_rank = |group: &EntityGroup| match group.change.and_then(|c| changes[c].net()) {
            Some(NetChange::Changed) => 0,
            Some(NetChange::Removed) => 1,
            None => 2,
            Some(NetChange::Added) => 3,
        };
        let mut order: Vec<usize> = (0..groups.len()).collect();
        order.sort_by_key(|&ix| {
            let group = &groups[ix];
            (
                group.section,
                type_rank[&(group.is_edge, group.type_name.as_str())],
                net_rank(group),
                ix,
            )
        });
        let mut position = vec![0; groups.len()];
        for (at, &ix) in order.iter().enumerate() {
            position[ix] = at;
        }
        let mut slots: Vec<Option<EntityGroup>> = groups.into_iter().map(Some).collect();
        let mut groups: Vec<EntityGroup> = order
            .iter()
            .map(|&ix| slots[ix].take().expect("each group is placed once"))
            .collect();
        for group in home.iter_mut() {
            *group = position[*group];
        }

        // Each other end's group, and a mirror there.
        let lookup: HashMap<(&str, &str), usize> = groups
            .iter()
            .enumerate()
            .filter(|(_, group)| !group.is_edge)
            .map(|(ix, group)| ((group.type_name.as_str(), group.id.as_str()), ix))
            .collect();
        let mut mirrors: Vec<(usize, GroupRelationship)> = Vec::new();
        let mut others: Vec<(usize, usize, usize)> = Vec::new();
        for (ix, group) in groups.iter().enumerate() {
            for (at, relationship) in group.relationships.iter().enumerate() {
                let Some(&other) = lookup.get(&(
                    relationship.other_type.as_str(),
                    relationship.other_id.as_str(),
                )) else {
                    continue;
                };
                others.push((ix, at, other));
                if other != ix {
                    mirrors.push((
                        other,
                        GroupRelationship {
                            change: relationship.change,
                            direction: match relationship.direction {
                                Direction::Outgoing => Direction::Incoming,
                                Direction::Incoming => Direction::Outgoing,
                            },
                            other_type: group.type_name.clone(),
                            other_id: group.id.clone(),
                            other_group: Some(ix),
                        },
                    ));
                }
            }
        }
        for (ix, at, other) in others {
            groups[ix].relationships[at].other_group = Some(other);
        }
        for (group, mirror) in mirrors {
            groups[group].mirrors.push(mirror);
        }
        // Inside a group: what it points at, then what points at it, by type and other end.
        let key = |r: &GroupRelationship| {
            (
                r.direction == Direction::Incoming,
                changes[r.change].type_name.clone(),
                r.other_id.clone(),
                r.change,
            )
        };
        for group in &mut groups {
            group.relationships.sort_by_key(key);
            group.mirrors.sort_by_key(key);
        }
        EntityGroups { groups, home }
    }

    /// The groups `keep` leaves, in runs of one type, each counting what it holds.
    pub fn runs(
        &self,
        folded: &FoldedChanges,
        mut keep: impl FnMut(&EntityGroup) -> bool,
    ) -> Vec<GroupRun> {
        let mut runs: Vec<GroupRun> = Vec::new();
        let mut last_section = None;
        for (ix, group) in self.groups.iter().enumerate() {
            if !keep(group) {
                continue;
            }
            let same_run = runs.last().is_some_and(|run| {
                last_section == Some(group.section)
                    && run.is_edge == group.is_edge
                    && run.type_name == group.type_name
            });
            if !same_run {
                runs.push(GroupRun {
                    section: (last_section != Some(group.section)).then_some(group.section),
                    type_name: group.type_name.clone(),
                    is_edge: group.is_edge,
                    groups: Vec::new(),
                    added: 0,
                    changed: 0,
                    removed: 0,
                    relationships: 0,
                });
                last_section = Some(group.section);
            }
            let run = runs.last_mut().expect("a run was opened");
            match group.change.and_then(|c| folded.changes[c].net()) {
                Some(NetChange::Added) => run.added += 1,
                Some(NetChange::Changed) => run.changed += 1,
                Some(NetChange::Removed) => run.removed += 1,
                None => {}
            }
            run.relationships += group.relationships.len();
            run.groups.push(ix);
        }
        runs
    }

    /// The nodes the review knows only by id, `(type, id)`, each once: those standing unchanged
    /// for their relationships, and relationship ends without a change of their own. Their
    /// names are read from the graph ([`NodeNames`]).
    pub fn unnamed_ends(&self) -> Vec<(String, String)> {
        let unchanged =
            |group: Option<usize>| group.is_none_or(|g| self.groups[g].change.is_none());
        let mut seen = std::collections::HashSet::new();
        let mut ends = Vec::new();
        for group in &self.groups {
            if group.is_edge {
                continue;
            }
            if group.change.is_none() && seen.insert((&group.type_name, &group.id)) {
                ends.push((group.type_name.clone(), group.id.clone()));
            }
            for relationship in &group.relationships {
                if unchanged(relationship.other_group)
                    && seen.insert((&relationship.other_type, &relationship.other_id))
                {
                    ends.push((
                        relationship.other_type.clone(),
                        relationship.other_id.clone(),
                    ));
                }
            }
        }
        ends
    }

    /// The lines of group `ix`'s card: the relationship changes reviewed there, then the
    /// mirrors, each with its other end named, from the review or else from `names`.
    pub fn entries(
        &self,
        folded: &FoldedChanges,
        ix: usize,
        schema: Option<&SchemaModel>,
        names: &NodeNames,
    ) -> Vec<RelationshipEntry> {
        let group = &self.groups[ix];
        let other_name = |relationship: &GroupRelationship| {
            relationship
                .other_group
                .and_then(|g| self.groups[g].change)
                .and_then(|c| entity_name(&folded.changes[c], schema))
                .or_else(|| {
                    names
                        .get(&(
                            relationship.other_type.clone(),
                            relationship.other_id.clone(),
                        ))
                        .cloned()
                })
                .unwrap_or_else(|| relationship.other_id.clone())
        };
        let entry = |relationship: &GroupRelationship, mirror: bool| {
            let edge = &folded.changes[relationship.change];
            let other = relationship
                .other_group
                .and_then(|g| self.groups[g].change)
                .map(|c| &folded.changes[c]);
            let replaces = (!mirror)
                .then(|| {
                    group
                        .repoints
                        .iter()
                        .find(|(_, added)| *added == relationship.change)
                })
                .flatten()
                .and_then(|&(removed, _)| {
                    let was = group.relationships.iter().find(|r| r.change == removed)?;
                    Some((removed, other_name(was)))
                });
            RelationshipEntry {
                change: relationship.change,
                net: match (mirror, &replaces) {
                    (true, _) => None,
                    (false, Some(_)) => Some(NetChange::Changed),
                    (false, None) => edge.net(),
                },
                words: match schema.and_then(|s| s.edge(&edge.type_name)) {
                    Some(edge_type) => {
                        relationship_words(&edge_type.name, &edge_type.from, &edge_type.to)
                    }
                    None => edge.type_name.clone(),
                },
                outgoing: relationship.direction == Direction::Outgoing,
                other: other_name(relationship),
                other_type: relationship.other_type.clone(),
                other_is_new: other.and_then(FoldedChange::net) == Some(NetChange::Added),
                replaces,
            }
        };
        // A moved relationship's removal is in its addition's entry.
        let moved_from =
            |change: usize| group.repoints.iter().any(|(removed, _)| *removed == change);
        group
            .relationships
            .iter()
            .filter(|r| !moved_from(r.change))
            .map(|r| entry(r, false))
            .chain(group.mirrors.iter().map(|m| entry(m, true)))
            .collect()
    }
}

/// What a reader knows a node by: its type's label property (`name`, `title`, …) from its
/// latest image. `None` for an edge, or when the label is the key, which a card shows anyway.
pub fn entity_name(change: &FoldedChange, schema: Option<&SchemaModel>) -> Option<String> {
    if change.is_edge() {
        return None;
    }
    let node = schema?.node(&change.type_name)?;
    let label = node.label_property()?;
    if node.key.as_deref() == Some(label) {
        return None;
    }
    let image = change.after.as_ref().or(change.before.as_ref())?;
    match image.properties.get(label)? {
        serde_json::Value::String(text) if !text.is_empty() => Some(text.clone()),
        serde_json::Value::String(_) | serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn section_of(change: &FoldedChange) -> GroupSection {
    match change.net() {
        Some(NetChange::Added) => GroupSection::Added,
        _ => GroupSection::Existing,
    }
}

/// An edge type in words, as a relationship line reads it: `WorksAt` is "works at", and the
/// end types a name repeats are left out (`NoteAboutPerson` from Note to Person is "about").
pub fn relationship_words(edge: &str, from: &str, to: &str) -> String {
    let mut core = edge;
    if let Some(rest) = core.strip_prefix(from)
        && !rest.is_empty()
    {
        core = rest;
    }
    if let Some(rest) = core.strip_suffix(to)
        && !rest.is_empty()
    {
        core = rest;
    }
    let mut words = String::with_capacity(core.len() + 4);
    let mut previous: Option<char> = None;
    for c in core.chars() {
        if c == '_' || c == '-' {
            if !words.ends_with(' ') && !words.is_empty() {
                words.push(' ');
            }
            previous = Some(' ');
            continue;
        }
        if c.is_uppercase()
            && previous.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit())
            && !words.ends_with(' ')
        {
            words.push(' ');
        }
        words.extend(c.to_lowercase());
        previous = Some(c);
    }
    words
}

#[cfg(test)]
mod tests {
    use graph_client::api::{ChangeImageOutput, EntityKind};
    use serde_json::{Value, json};

    use super::*;

    const SCHEMA: &str = "node Person {\n    name: String @key\n    role: String?\n}\n\
        node Note {\n    slug: String @key\n    title: String?\n}\n\
        node Company {\n    name: String @key\n}\n\
        edge WorksAt: Person -> Company @card(0..1)\n\
        edge Knows: Person -> Person\n\
        edge NoteAboutPerson: Note -> Person\n";

    fn image(properties: Value, endpoints: Option<(&str, &str)>) -> Option<ChangeImageOutput> {
        Some(ChangeImageOutput {
            properties: properties.as_object().unwrap().clone(),
            endpoints: endpoints.map(|(from, to)| json!({"from": from, "to": to})),
        })
    }

    fn node(type_name: &str, id: &str, net: NetChange) -> FoldedChange {
        let one = image(json!({"x": 1}), None);
        let two = image(json!({"x": 2}), None);
        let (before, after) = match net {
            NetChange::Added => (None, one),
            NetChange::Removed => (one, None),
            NetChange::Changed => (one, two),
        };
        FoldedChange {
            kind: EntityKind::Node,
            type_id: format!("t-{type_name}"),
            type_name: type_name.into(),
            id: id.into(),
            before,
            after,
            commits: vec!["c1".into()],
        }
    }

    fn edge(type_name: &str, id: &str, from: &str, to: &str, net: NetChange) -> FoldedChange {
        let ends = image(json!({}), Some((from, to)));
        let (before, after) = match net {
            NetChange::Added => (None, ends),
            NetChange::Removed => (ends, None),
            NetChange::Changed => (ends, image(json!({"since": 2}), Some((from, to)))),
        };
        FoldedChange {
            kind: EntityKind::Edge,
            type_id: format!("t-{type_name}"),
            type_name: type_name.into(),
            id: id.into(),
            before,
            after,
            commits: vec!["c1".into()],
        }
    }

    fn folded(changes: Vec<FoldedChange>) -> FoldedChanges {
        FoldedChanges {
            raw_changes: changes.len(),
            changes,
        }
    }

    fn summary(groups: &EntityGroups, folded: &FoldedChanges) -> Vec<String> {
        groups
            .groups
            .iter()
            .map(|group| {
                let mut line = format!(
                    "{:?} {}:{}{}",
                    group.section,
                    group.type_name,
                    group.id,
                    if group.change.is_none() {
                        " (unchanged)"
                    } else {
                        ""
                    }
                );
                for r in &group.relationships {
                    line.push_str(&format!(
                        " | {} {} {}:{}",
                        folded.changes[r.change].type_name,
                        if r.direction == Direction::Outgoing {
                            "->"
                        } else {
                            "<-"
                        },
                        r.other_type,
                        r.other_id
                    ));
                }
                for m in &group.mirrors {
                    line.push_str(&format!(
                        " | mirror {} {} {}:{}",
                        folded.changes[m.change].type_name,
                        if m.direction == Direction::Outgoing {
                            "->"
                        } else {
                            "<-"
                        },
                        m.other_type,
                        m.other_id
                    ));
                }
                line
            })
            .collect()
    }

    /// A relationship is reviewed with the end the branch added, else the end it changed, else
    /// its source; the other end mirrors it when it has a group. Existing data comes first.
    #[test]
    fn relationships_are_reviewed_with_the_entity_they_came_with() {
        let schema = SchemaModel::parse(SCHEMA).unwrap();
        let folded = folded(vec![
            node("Person", "maya", NetChange::Changed),
            node("Person", "theo", NetChange::Added),
            node("Note", "lunch", NetChange::Added),
            // Added with the note, though Maya changed too.
            edge("NoteAboutPerson", "e1", "lunch", "maya", NetChange::Added),
            // Added with Theo, its target.
            edge("Knows", "e2", "maya", "theo", NetChange::Added),
            // Neither end added: with Maya, who changed, though she is its target.
            edge("Knows", "e3", "ana", "maya", NetChange::Removed),
            // Neither end in the review: with its source, unchanged.
            edge("WorksAt", "e4", "bob", "acme", NetChange::Added),
        ]);
        let groups = EntityGroups::new(&folded, Some(&schema));
        assert_eq!(
            summary(&groups, &folded),
            [
                "Existing Person:maya | Knows <- Person:ana | mirror Knows -> Person:theo | mirror NoteAboutPerson <- Note:lunch",
                "Existing Person:bob (unchanged) | WorksAt -> Company:acme",
                "Added Person:theo | Knows <- Person:maya",
                "Added Note:lunch | NoteAboutPerson -> Person:maya",
            ]
        );
        // Each change is reviewed in exactly one group.
        for (change, &group) in groups.home.iter().enumerate() {
            let in_group = groups.groups[group].changes().any(|c| c == change);
            assert!(in_group, "change {change} is in its home group {group}");
        }
        let reviewed: usize = groups.groups.iter().map(|g| g.changes().count()).sum();
        assert_eq!(reviewed, folded.changes.len(), "no change is counted twice");
        // A mirror names the group the relationship is reviewed in.
        let maya = &groups.groups[0];
        let mirror = &maya.mirrors[0];
        assert_eq!(groups.groups[mirror.other_group.unwrap()].id, "theo");
        // Known only by id: the unchanged source standing for its relationship, and the ends
        // with no change of their own, each once.
        let ends: Vec<String> = groups
            .unnamed_ends()
            .into_iter()
            .map(|(type_name, id)| format!("{type_name}:{id}"))
            .collect();
        assert_eq!(ends, ["Person:ana", "Person:bob", "Company:acme"]);
        let names = NodeNames::from([(("Person".into(), "ana".into()), "Ana Lima".into())]);
        let lines = groups.entries(&folded, 0, Some(&schema), &names);
        assert_eq!(lines[0].other, "Ana Lima", "a name read from the graph");
        // Theo's label is his key: the id is his name.
        assert_eq!(lines[1].other, "theo");
        assert_eq!(
            maya.relationships[0].other_group, None,
            "ana is not in the review"
        );
    }

    /// Without the schema, or for an edge type it lacks, a relationship stands alone under its
    /// own type, after the nodes of its section.
    #[test]
    fn a_relationship_without_known_ends_stands_alone() {
        let folded = folded(vec![
            node("Person", "maya", NetChange::Added),
            edge("Knows", "e1", "maya", "theo", NetChange::Added),
        ]);
        let groups = EntityGroups::new(&folded, None);
        assert_eq!(
            summary(&groups, &folded),
            ["Added Person:maya", "Added Knows:e1"]
        );
        assert!(groups.groups[1].is_edge);
        assert_eq!(groups.home, [0, 1]);
    }

    /// A one-target relationship removed and added again from the same source reads as one
    /// change, from the old end to the new; one that allows many targets does not.
    #[test]
    fn a_moved_one_target_relationship_is_one_line() {
        let schema = SchemaModel::parse(SCHEMA).unwrap();
        let folded = folded(vec![
            node("Person", "maya", NetChange::Changed),
            edge("WorksAt", "e1", "maya", "acme", NetChange::Removed),
            edge("WorksAt", "e2", "maya", "globex", NetChange::Added),
            edge("Knows", "e3", "maya", "ana", NetChange::Removed),
            edge("Knows", "e4", "maya", "bob", NetChange::Added),
        ]);
        let groups = EntityGroups::new(&folded, Some(&schema));
        assert_eq!(groups.groups[0].repoints, [(1, 2)]);
        let lines = groups.entries(&folded, 0, Some(&schema), &NodeNames::default());
        let moved: Vec<_> = lines
            .iter()
            .map(|line| (line.change, line.net, line.replaces.clone()))
            .collect();
        assert_eq!(
            moved,
            [
                (3, Some(NetChange::Removed), None),
                (4, Some(NetChange::Added), None),
                (2, Some(NetChange::Changed), Some((1, "acme".to_owned()))),
            ]
        );
        // Both changes are still reviewed on the card.
        assert_eq!(groups.groups[0].changes().count(), 5);
    }

    /// A relationship from an entity to itself is reviewed there once, with no mirror.
    #[test]
    fn a_self_relationship_has_no_mirror() {
        let schema = SchemaModel::parse(SCHEMA).unwrap();
        let folded = folded(vec![
            node("Person", "maya", NetChange::Changed),
            edge("Knows", "e1", "maya", "maya", NetChange::Added),
        ]);
        let groups = EntityGroups::new(&folded, Some(&schema));
        assert_eq!(
            summary(&groups, &folded),
            ["Existing Person:maya | Knows -> Person:maya"]
        );
        assert_eq!(groups.groups[0].relationships[0].other_group, Some(0));
    }

    #[test]
    fn edge_types_read_as_words() {
        assert_eq!(
            relationship_words("WorksAt", "Person", "Company"),
            "works at"
        );
        assert_eq!(
            relationship_words("NoteAboutPerson", "Note", "Person"),
            "about"
        );
        assert_eq!(relationship_words("Knows", "Person", "Person"), "knows");
        assert_eq!(relationship_words("Person", "Person", "Person"), "person");
        assert_eq!(
            relationship_words("member_of", "Person", "Team"),
            "member of"
        );
        assert_eq!(relationship_words("HasV2Key", "Doc", "Key"), "has v2");
    }
}
