//! What the tree shows, without GPUI: row identity, the text a filter matches, filtering that
//! keeps ancestors, and the moves the keys make.

use graph_store::{Entry, EntryKind, ExplorePath, ExploreState, NEIGHBOUR_PAGE, NodeRef};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowTag {
    Node,
    Cycle,
    Field,
    Group,
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
    let tag = match entry.kind {
        EntryKind::Node { .. } => RowTag::Node,
        EntryKind::Cycle { .. } => RowTag::Cycle,
        EntryKind::Field { .. } => RowTag::Field,
        EntryKind::Group { .. } => RowTag::Group,
        EntryKind::More { .. } => RowTag::More,
        EntryKind::Loading { .. } => RowTag::Loading,
        EntryKind::Error { .. } => RowTag::Error,
    };
    RowId {
        path: entry.path.clone(),
        tag,
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
        EntryKind::More { .. } | EntryKind::Loading { .. } | EntryKind::Error { .. } => {
            String::new()
        }
    }
}

/// The entries to show for `query`: every match with its ancestors, in tree order. An empty
/// query shows everything. Paging, loading and error rows hide while filtering.
pub fn filter(entries: &[Entry], state: &ExploreState, query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return (0..entries.len()).collect();
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
    (0..entries.len()).filter(|ix| keep[*ix]).collect()
}

/// The visible row with this identity.
pub fn find(visible: &[usize], entries: &[Entry], id: &RowId) -> Option<usize> {
    visible.iter().position(|ix| row_id(&entries[*ix]) == *id)
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
            matches!(entry.kind, EntryKind::Node { .. } | EntryKind::Group { .. })
                && id.path.starts_with(&entry.path)
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

/// The visible row of a visible row's parent.
pub fn parent_row(visible: &[usize], entries: &[Entry], row: usize) -> Option<usize> {
    let parent = entries[visible[row]].parent?;
    visible.iter().position(|ix| *ix == parent)
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

/// Edge properties as a muted line: `role lead · since 2021-01-02`.
pub fn edge_summary(edge: &[(String, Value)]) -> String {
    edge.iter()
        .map(|(name, value)| {
            let value = match value {
                Value::String(text) => text.clone(),
                other => studio_ui::format_value(other),
            };
            format!("{name} {value}")
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// "Show 50 more of 418": the next page size and how many are not shown yet.
pub fn more_label(shown: usize, total: Option<u64>) -> String {
    match total {
        Some(total) => {
            let remaining = total.saturating_sub(shown as u64);
            let next = remaining.min(u64::from(NEIGHBOUR_PAGE));
            format!("Show {next} more of {remaining}")
        }
        None => format!("Show {NEIGHBOUR_PAGE} more"),
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
        let ada_knows = GroupKey {
            node: person("ada"),
            group: knows(),
        };
        let bob_knows = GroupKey {
            node: person("bob"),
            group: knows(),
        };
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
                    EntryKind::More { shown, total, .. } => more_label(*shown, *total),
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
        let all = filter(&entries, &state, "");
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
        let visible = filter(&entries, &state, "young");
        assert_eq!(
            texts(&entries, &state, &visible),
            ["Ada Lovelace ada", "  Knows Person", "    Cy Young cy"]
        );
        // The root always stays, so the filter never empties the tree.
        assert_eq!(filter(&entries, &state, "zzz"), vec![0]);
    }

    #[test]
    fn moves_follow_parents_and_children() {
        let (_, state, entries) = fixture();
        let visible = filter(&entries, &state, "");
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
        assert_eq!(
            edge_summary(&[("role".into(), json!("lead")), ("hours".into(), json!(12))]),
            "role lead · hours 12"
        );
        assert_eq!(more_label(50, Some(418)), "Show 50 more of 368");
        assert_eq!(more_label(50, Some(60)), "Show 10 more of 10");
        assert_eq!(more_label(50, None), "Show 50 more");
    }
}
