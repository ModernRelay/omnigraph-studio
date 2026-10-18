//! What the tree shows, without GPUI: row identity, the text a filter matches, filtering that
//! keeps ancestors, and the moves the keys make.

use graph_store::{BucketValue, Entry, EntryKind, ExplorePath, ExploreState, NodeRef};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowTag {
    Node,
    Cycle,
    Field,
    Group,
    Bucket,
    OtherValues,
    More,
    Loading,
    Error,
}

/// A row's identity across rebuilds: its path and what it is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RowId {
    pub path: ExplorePath,
    pub tag: RowTag,
}

pub fn row_id(entry: &Entry) -> RowId {
    RowId {
        path: entry.path.clone(),
        tag: row_tag(entry),
    }
}

/// What kind of row an entry draws as.
fn row_tag(entry: &Entry) -> RowTag {
    match entry.kind {
        EntryKind::Node { .. } => RowTag::Node,
        EntryKind::Cycle { .. } => RowTag::Cycle,
        EntryKind::Field { .. } => RowTag::Field,
        EntryKind::Group { .. } => RowTag::Group,
        EntryKind::Bucket { .. } => RowTag::Bucket,
        EntryKind::OtherValues { .. } => RowTag::OtherValues,
        EntryKind::More { .. } => RowTag::More,
        EntryKind::Loading { .. } => RowTag::Loading,
        EntryKind::Error { .. } => RowTag::Error,
    }
}

/// A node's label, else its key.
pub fn node_text(state: &ExploreState, node: &NodeRef) -> String {
    state
        .label(node)
        .map(str::to_owned)
        .unwrap_or_else(|| node.id.clone())
}

/// The text a filter matches against.
pub fn row_text(entry: &Entry, state: &ExploreState) -> String {
    match &entry.kind {
        EntryKind::Node { node, .. } | EntryKind::Cycle { node, .. } => {
            format!("{} {}", node_text(state, node), node.id)
        }
        EntryKind::Field { key, target, .. } => format!(
            "{} {}",
            key.group.edge,
            target
                .as_ref()
                .map(|target| node_text(state, &target.node))
                .unwrap_or_default()
        ),
        EntryKind::Group { key, .. } => format!("{} {}", key.group.edge, key.group.other_type),
        EntryKind::Bucket { key, .. } => state
            .bucket_label(key)
            .map(str::to_owned)
            .or_else(|| key.within.as_ref().map(|within| bucket_text(&within.value)))
            .unwrap_or_default(),
        EntryKind::OtherValues { .. }
        | EntryKind::More { .. }
        | EntryKind::Loading { .. }
        | EntryKind::Error { .. } => String::new(),
    }
}

/// Whether an entry is a relationship group known to have no relationships, or lies under one.
fn under_empty_group(entries: &[Entry], index: usize) -> bool {
    let mut current = Some(index);
    while let Some(ix) = current {
        if let EntryKind::Group {
            count: graph_store::Loadable::Loaded(0),
            ..
        } = entries[ix].kind
        {
            return true;
        }
        current = entries[ix].parent;
    }
    false
}

/// The entries to show for `query`: every match with its ancestors, in tree order. An empty
/// query shows everything. Loading and error rows hide while filtering; a "Show more" row
/// stays under a group that shows, so a group not read in full can be searched on the server.
/// `hide_empty` leaves out groups with no relationships.
pub fn filter(
    entries: &[Entry],
    state: &ExploreState,
    query: &str,
    hide_empty: bool,
) -> Vec<usize> {
    let shown = |ix: &usize| !hide_empty || !under_empty_group(entries, *ix);
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return (0..entries.len()).filter(shown).collect();
    }
    let mut keep = vec![false; entries.len()];
    for (index, entry) in entries.iter().enumerate() {
        if index == 0 || row_text(entry, state).to_lowercase().contains(&query) {
            let mut current = Some(index);
            while let Some(ix) = current {
                if keep[ix] {
                    break;
                }
                keep[ix] = true;
                current = entries[ix].parent;
            }
        }
    }
    for (index, entry) in entries.iter().enumerate() {
        if matches!(entry.kind, EntryKind::More { .. })
            && entry.parent.is_some_and(|parent| keep[parent])
        {
            keep[index] = true;
        }
    }
    (0..entries.len())
        .filter(|ix| keep[*ix])
        .filter(shown)
        .collect()
}

/// Whether `entry` is the row with this identity. Compared in place: the list asks every frame.
pub fn is_row(entry: &Entry, id: &RowId) -> bool {
    row_tag(entry) == id.tag && entry.path == id.path
}

/// The visible row with this identity.
pub fn find(visible: &[usize], entries: &[Entry], id: &RowId) -> Option<usize> {
    visible.iter().position(|ix| is_row(&entries[*ix], id))
}

/// The visible row with this identity, else the closest visible ancestor by path.
pub fn nearest(visible: &[usize], entries: &[Entry], id: &RowId) -> Option<usize> {
    if let Some(found) = find(visible, entries, id) {
        return Some(found);
    }
    visible
        .iter()
        .enumerate()
        .filter(|(_, ix)| {
            let entry = &entries[**ix];
            matches!(
                entry.kind,
                EntryKind::Node { .. } | EntryKind::Group { .. } | EntryKind::Bucket { .. }
            ) && id.path.starts_with(&entry.path)
        })
        .max_by_key(|(_, ix)| entries[**ix].path.depth())
        .map(|(position, _)| position)
}

/// The first visible child of a visible row.
pub fn first_child(visible: &[usize], entries: &[Entry], row: usize) -> Option<usize> {
    let parent = visible[row];
    let next = row + 1;
    (next < visible.len() && entries[visible[next]].parent == Some(parent)).then_some(next)
}

/// The visible row of a visible row's parent. `visible` holds entry indices in order, and a
/// parent comes before its children.
pub fn parent_row(visible: &[usize], entries: &[Entry], row: usize) -> Option<usize> {
    let parent = entries[visible[row]].parent?;
    visible[..row].binary_search(&parent).ok()
}

/// The entry indices from the root down to the parent of `entry`.
pub fn ancestors(entries: &[Entry], entry: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut current = entries[entry].parent;
    while let Some(ix) = current {
        out.push(ix);
        current = entries[ix].parent;
    }
    out.reverse();
    out
}

/// An edge property's value as a row or a hover card shows it: text as written, other values
/// formatted.
pub fn edge_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => studio_ui::format_value(other),
    }
}

/// The first edge property as a short hint at the end of a row, when it fits whole
/// (`role lead`); none when it would be cut.
pub fn edge_hint(edge: &[(String, Value)]) -> Option<String> {
    const LONGEST: usize = 18;
    let (name, value) = edge.first()?;
    let hint = format!("{name} {}", edge_value(value));
    (hint.chars().count() <= LONGEST).then_some(hint)
}

/// A grouping value as its row shows it: text as written, other values formatted, and
/// "No value" for neighbours without one.
pub fn bucket_text(value: &BucketValue) -> String {
    match value.value() {
        serde_json::Value::Null => "No value".to_owned(),
        serde_json::Value::String(text) => text,
        other => studio_ui::format_value(&other),
    }
}

/// Below a group's most common values: how many neighbours the other values hold, when known.
pub fn other_values_label(shown: usize, rest: Option<u64>) -> String {
    match rest {
        Some(rest) if rest > 0 => format!("{rest} more in other values"),
        _ => format!("The {shown} most common values"),
    }
}

/// "Show 50 more of 418": the next page size and how many are not shown yet.
pub fn more_label(shown: usize, total: Option<u64>, page: u32) -> String {
    match total {
        Some(total) => {
            let remaining = total.saturating_sub(shown as u64);
            let next = remaining.min(u64::from(page));
            format!("Show {next} more of {remaining}")
        }
        None => format!("Show {page} more"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use gq_language::{Direction, RelationshipGroup, SchemaModel};
    use graph_store::{GroupKey, Loadable, Neighbour, NeighbourList};
    use serde_json::json;

    use super::*;

    const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n}\n\
        node Company {\n  slug: String @key\n  name: String\n}\n\
        edge WorksAt: Person -> Company @card(0..)\n\
        edge Knows: Person -> Person @card(0..)\n";

    fn person(id: &str) -> NodeRef {
        NodeRef::new("Person", id)
    }

    fn knows() -> RelationshipGroup {
        RelationshipGroup {
            edge: "Knows".into(),
            direction: Direction::Outgoing,
            other_type: "Person".into(),
        }
    }

    /// Ada knows Bob and Cy; Bob is expanded and knows Ada back.
    fn fixture() -> (SchemaModel, ExploreState, Vec<Entry>) {
        let schema = SchemaModel::parse(SCHEMA).unwrap();
        let mut state = ExploreState::rooted(person("ada"));
        state.root_status = Loadable::Loaded(());
        state.labels = HashMap::from([
            (person("ada"), Some("Ada Lovelace".into())),
            (person("bob"), Some("Bob Kahn".into())),
            (person("cy"), Some("Cy Young".into())),
        ]);
        let neighbour = |id: &str| Neighbour {
            node: person(id),
            label: None,
            edge: Vec::new(),
        };
        let ada_knows = GroupKey::new(person("ada"), knows());
        let bob_knows = GroupKey::new(person("bob"), knows());
        state.counts.insert(ada_knows.clone(), Loadable::Loaded(60));
        state.neighbours.insert(
            ada_knows,
            NeighbourList {
                rows: vec![neighbour("bob"), neighbour("cy")],
                ..Default::default()
            },
        );
        state.neighbours.insert(
            bob_knows,
            NeighbourList {
                rows: vec![neighbour("ada")],
                exhausted: true,
                ..Default::default()
            },
        );
        let group = ExplorePath::root().child_group(&knows());
        state.expanded.insert(group.clone());
        let bob = group.child_node(&person("bob"));
        state.expanded.insert(bob.clone());
        state.expanded.insert(bob.child_group(&knows()));
        let entries = state.entries(&schema);
        (schema, state, entries)
    }

    fn texts(entries: &[Entry], state: &ExploreState, visible: &[usize]) -> Vec<String> {
        visible
            .iter()
            .map(|ix| {
                let entry = &entries[*ix];
                let text = match &entry.kind {
                    EntryKind::More { shown, total, .. } => more_label(*shown, *total, 50),
                    EntryKind::Cycle { .. } => format!("↺ {}", row_text(entry, state)),
                    _ => row_text(entry, state),
                };
                format!("{}{}", "  ".repeat(entry.depth), text.trim())
            })
            .collect()
    }

    #[test]
    fn the_tree_flattens_with_cycles_and_paging_rows() {
        let (_, state, entries) = fixture();
        let all = filter(&entries, &state, "", false);
        let shown = texts(&entries, &state, &all);
        assert_eq!(shown[0], "Ada Lovelace ada");
        assert!(
            shown.contains(&"        ↺ Ada Lovelace ada".to_owned()),
            "{shown:#?}"
        );
        assert!(
            shown.contains(&"    Show 50 more of 58".to_owned()),
            "{shown:#?}"
        );
    }

    #[test]
    fn filtering_keeps_matches_and_their_ancestors() {
        let (_, state, entries) = fixture();
        let visible = filter(&entries, &state, "young", false);
        // The group's "Show more" row stays, to search the rest of the group on the server.
        assert_eq!(
            texts(&entries, &state, &visible),
            [
                "Ada Lovelace ada",
                "  Knows Person",
                "    Cy Young cy",
                "    Show 50 more of 58"
            ]
        );
        // The root always stays, so the filter never empties the tree.
        assert_eq!(filter(&entries, &state, "zzz", false), vec![0]);
    }

    #[test]
    fn empty_groups_hide_with_their_rows_when_asked() {
        let (schema, mut state, _) = fixture();
        let bob_knows = GroupKey::new(person("bob"), knows());
        state.counts.insert(bob_knows, Loadable::Loaded(0));
        let entries = state.entries(&schema);
        let all = filter(&entries, &state, "", false);
        let without = filter(&entries, &state, "", true);
        assert!(without.len() < all.len());
        assert!(without.iter().all(|ix| !matches!(
            &entries[*ix].kind,
            EntryKind::Group {
                count: Loadable::Loaded(0),
                ..
            }
        )));
    }

    #[test]
    fn moves_follow_parents_and_children() {
        let (_, state, entries) = fixture();
        let visible = filter(&entries, &state, "", false);
        let group_row = visible
            .iter()
            .position(|ix| matches!(&entries[*ix].kind, EntryKind::Group { key, .. } if key.node.id == "ada" && key.group.edge == "Knows"))
            .unwrap();
        let bob_row = first_child(&visible, &entries, group_row).unwrap();
        assert_eq!(
            texts(&entries, &state, &visible[bob_row..=bob_row]),
            ["    Bob Kahn bob"]
        );
        assert_eq!(parent_row(&visible, &entries, bob_row), Some(group_row));
        assert_eq!(
            ancestors(&entries, visible[bob_row]),
            vec![0, visible[group_row]]
        );
        // A collapsed row's identity falls back to its closest visible ancestor.
        let gone = RowId {
            path: entries[visible[bob_row]]
                .path
                .child_group(&knows())
                .child_node(&person("zed")),
            tag: RowTag::Node,
        };
        let fallback = nearest(&visible, &entries, &gone).unwrap();
        assert_eq!(
            entries[visible[fallback]].path,
            entries[visible[bob_row]].path.child_group(&knows())
        );
        let _ = state;
    }

    #[test]
    fn labels_read_well() {
        assert_eq!(edge_value(&json!("lead")), "lead");
        assert_eq!(edge_value(&json!(12)), "12");
        assert_eq!(more_label(50, Some(418), 50), "Show 50 more of 368");
        assert_eq!(
            edge_hint(&[("role".into(), json!("lead")), ("hours".into(), json!(12))]),
            Some("role lead".into())
        );
        assert_eq!(
            edge_hint(&[("since".into(), json!("2021-01-02"))]),
            Some("since 2021-01-02".into())
        );
        assert_eq!(
            edge_hint(&[("note".into(), json!("a longer note here"))]),
            None
        );
        assert_eq!(edge_hint(&[]), None);
        assert_eq!(more_label(50, Some(60), 50), "Show 10 more of 10");
        assert_eq!(more_label(50, None, 50), "Show 50 more");
        assert_eq!(more_label(20, Some(418), 20), "Show 20 more of 398");
        assert_eq!(bucket_text(&BucketValue::new(&json!("lead"))), "lead");
        assert_eq!(bucket_text(&BucketValue::new(&json!(null))), "No value");
        assert_eq!(bucket_text(&BucketValue::new(&json!(true))), "true");
        assert_eq!(other_values_label(100, Some(42)), "42 more in other values");
        assert_eq!(other_values_label(100, None), "The 100 most common values");
    }
}
