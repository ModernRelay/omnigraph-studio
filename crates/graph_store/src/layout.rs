//! How a node type reads (spec 044): the relationships its page lists in the person's order,
//! those from the type to itself read both ways folded into one, and what a row of the type
//! shows after its name. The person's choices are `studio_settings::TypeLayout`, one a type and
//! graph; the session holds them ([`crate::GraphSession::layout`]).

use std::collections::{BTreeSet, HashMap};

use gq_language::builder::relationship_key;
use gq_language::{Direction, RelationshipGroup, SchemaModel};
use serde_json::Value;
pub use studio_settings::TypeLayout;

/// The values of a row's line, by node `(type, id)`: each property the type's layout names and
/// its value, in the layout's order ([`crate::GraphSession::node_lines`]).
pub type NodeLines = HashMap<(String, String), Vec<(String, Value)>>;

/// The relationship groups of `node_type` under `layout`, in schema order: a relationship from
/// the type to itself that reads both ways is one group, where its outgoing one stood, and its
/// incoming one goes. A view that keeps its groups by place keeps them in this order, and lists
/// them by [`rank`].
pub fn folded_groups(
    schema: &SchemaModel,
    node_type: &str,
    layout: Option<&TypeLayout>,
) -> Vec<RelationshipGroup> {
    let groups = schema.relationships_of(node_type);
    let Some(layout) = layout else {
        return groups.to_vec();
    };
    groups
        .iter()
        .filter_map(|group| {
            if !layout.both_ways(&group.edge) || !group.to_itself(schema) {
                return Some(group.clone());
            }
            (group.direction == Direction::Outgoing).then(|| group.both_ways())
        })
        .collect()
}

/// Where `group` stands in the person's order, when it has a place.
pub fn rank(layout: Option<&TypeLayout>, group: &RelationshipGroup) -> Option<usize> {
    layout?.rank_of(&relationship_key(group))
}

/// [`folded_groups`] in the order a page lists them: the person's first, by their place, the
/// others in schema order after them.
pub fn shown_groups(
    schema: &SchemaModel,
    node_type: &str,
    layout: Option<&TypeLayout>,
) -> Vec<RelationshipGroup> {
    let mut shown = folded_groups(schema, node_type, layout);
    // A stable sort: the rest keep the schema's order.
    shown.sort_by_key(|group| rank(layout, group).unwrap_or(usize::MAX));
    shown
}

/// What a page and a tree call `group`: the person's name for it, else its edge, the way it
/// runs and the type at its other end, `knows → Person`, `blockedBy ← Issue`, `knows ↔
/// Person`. With the name, whether it is the person's.
pub fn group_title(layout: Option<&TypeLayout>, group: &RelationshipGroup) -> (String, bool) {
    match layout.and_then(|layout| layout.name_of(&relationship_key(group))) {
        Some(name) => (name.to_owned(), true),
        None => (schema_title(group), false),
    }
}

/// `group` as the schema names it: `knows → Person`.
pub fn schema_title(group: &RelationshipGroup) -> String {
    format!(
        "{} {} {}",
        gq_language::traversal_name(&group.edge),
        group.direction.arrow(),
        group.other_type
    )
}

/// The node types whose rows read otherwise under `now` than under `was`: their line changed,
/// so a view showing them reads their lines again.
pub fn lines_changed(
    was: &std::collections::BTreeMap<String, TypeLayout>,
    now: &std::collections::BTreeMap<String, TypeLayout>,
) -> BTreeSet<String> {
    let line = |layouts: &std::collections::BTreeMap<String, TypeLayout>, type_name: &str| {
        layouts
            .get(type_name)
            .map(|layout| layout.line.clone())
            .unwrap_or_default()
    };
    was.keys()
        .chain(now.keys())
        .filter(|type_name| line(was, type_name) != line(now, type_name))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brain() -> SchemaModel {
        SchemaModel::parse(
            "node Person {\n  slug: String @key\n  name: String\n}\nnode Place {\n  slug: String @key\n  name: String\n}\nnode Note {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\nedge LivesIn: Person -> Place\nedge NoteAboutPerson: Note -> Person\n",
        )
        .unwrap()
    }

    fn keys(groups: &[RelationshipGroup]) -> Vec<String> {
        groups.iter().map(relationship_key).collect()
    }

    #[test]
    fn a_page_lists_relationships_in_the_persons_order_and_folds_both_ways() {
        let schema = brain();
        assert_eq!(
            keys(&shown_groups(&schema, "Person", None)),
            ["out:Knows", "out:LivesIn", "in:Knows", "in:NoteAboutPerson"]
        );
        let mut layout = TypeLayout::default();
        layout.set_both_ways("Knows", true);
        layout.to_top("in:NoteAboutPerson");
        assert_eq!(
            keys(&shown_groups(&schema, "Person", Some(&layout))),
            ["in:NoteAboutPerson", "both:Knows", "out:LivesIn"]
        );
        // Both ways means nothing for a relationship between two types.
        layout.set_both_ways("LivesIn", true);
        assert_eq!(
            keys(&shown_groups(&schema, "Person", Some(&layout))),
            ["in:NoteAboutPerson", "both:Knows", "out:LivesIn"]
        );
    }

    #[test]
    fn a_line_changed_is_told_by_its_type() {
        let mut was = std::collections::BTreeMap::new();
        let mut now = std::collections::BTreeMap::new();
        was.insert("Person".to_owned(), TypeLayout::default());
        let mut place = TypeLayout::default();
        place.toggle_line("name");
        now.insert("Place".to_owned(), place);
        let person = TypeLayout {
            lead: Some("brief".into()),
            ..TypeLayout::default()
        };
        now.insert("Person".to_owned(), person);
        assert_eq!(
            lines_changed(&was, &now),
            BTreeSet::from(["Place".to_owned()])
        );
    }
}
