//! What a branch changed in one type's rows, against its target (spec 004, "Reading on a
//! branch"): the net diff from the merge base, folded per entity (`graph_store::changes`), kept
//! by row id. The table washes added and changed rows in their kind's colour, marks the cells
//! that changed, and shows the rows the branch removed first, struck through. Pure; no GPUI.

use std::collections::{HashMap, HashSet};

use graph_store::{EntityKind, FoldedChanges, NetChange};
use serde_json::{Map, Value};

use crate::model::{FROM_COLUMN, Row, TO_COLUMN};

/// The branch every other branch is read against, as review merges into it.
pub const TARGET: &str = "main";

/// How a branch changed one row, and for a changed row which of its columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowMark {
    pub net: NetChange,
    /// The columns whose value differs from the target's, for a changed row; empty otherwise.
    pub changed: HashSet<String>,
}

/// One type's rows as a branch changed them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BranchDiff {
    marks: HashMap<String, RowMark>,
    /// The rows the branch removed, with the values they last had, in the diff's order.
    removed: Vec<Row>,
}

impl BranchDiff {
    /// The changes to `type_name`'s rows (nodes or edges, by `kind`) among `folded`.
    pub fn of(folded: &FoldedChanges, kind: EntityKind, type_name: &str) -> Self {
        let mut diff = Self::default();
        let is_edge = matches!(kind, EntityKind::Edge);
        for change in &folded.changes {
            if change.is_edge() != is_edge || change.type_name != type_name {
                continue;
            }
            let Some(net) = change.net() else {
                continue;
            };
            let changed = match net {
                NetChange::Changed => change
                    .property_diffs(&[])
                    .into_iter()
                    .map(|diff| diff.name)
                    .chain(
                        change
                            .moved_ends()
                            .into_iter()
                            .flat_map(|_| [FROM_COLUMN.to_owned(), TO_COLUMN.to_owned()]),
                    )
                    .collect(),
                _ => HashSet::new(),
            };
            if net == NetChange::Removed
                && let Some(before) = &change.before
            {
                diff.removed.push(Row {
                    id: change.id.clone(),
                    values: row_values(&before.properties, before.endpoints.as_ref()),
                    dimmed: false,
                });
            }
            diff.marks
                .insert(change.id.clone(), RowMark { net, changed });
        }
        diff
    }

    /// How the branch changed the row `id`, if it did.
    pub fn mark(&self, id: &str) -> Option<&RowMark> {
        self.marks.get(id)
    }

    /// The rows the branch removed, with their last values.
    pub fn removed(&self) -> &[Row] {
        &self.removed
    }

    /// How many rows the branch added, changed and removed, in that order.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for mark in self.marks.values() {
            match mark.net {
                NetChange::Added => counts.0 += 1,
                NetChange::Changed => counts.1 += 1,
                NetChange::Removed => counts.2 += 1,
            }
        }
        counts
    }
}

/// A row's values from a change image: its properties without nulls, and an edge's ends under
/// the table's end columns.
fn row_values(properties: &Map<String, Value>, endpoints: Option<&Value>) -> Map<String, Value> {
    let mut values: Map<String, Value> = properties
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(ends) = endpoints.and_then(Value::as_object) {
        for (end, column) in [("from", FROM_COLUMN), ("to", TO_COLUMN)] {
            if let Some(value) = ends.get(end) {
                values.insert(column.to_owned(), value.clone());
            }
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use graph_client::api::{ChangeBlockOutput, ChangeCauseOutput, EntityChangeOutput};
    use graph_store::ChangeFold;
    use serde_json::json;

    use super::*;

    fn fold(changes: Vec<Value>) -> FoldedChanges {
        let block: ChangeBlockOutput = serde_json::from_value(json!({
            "cause": {
                "graph_commit_id": "c2",
                "authored_branch": "fix",
                "authored_at": 0,
            },
            "changes": changes,
        }))
        .expect("a block");
        let _: (&ChangeCauseOutput, &Vec<EntityChangeOutput>) = (&block.cause, &block.changes);
        let mut fold = ChangeFold::new();
        fold.add_block(block);
        fold.finish()
    }

    fn change(
        kind: &str,
        ty: &str,
        id: &str,
        before: Option<Value>,
        after: Option<Value>,
    ) -> Value {
        json!({
            "kind": kind,
            "type": {"id": format!("t-{ty}"), "name": ty},
            "id": id,
            "op": if before.is_none() { "insert" } else if after.is_none() { "delete" } else { "update" },
            "before": before,
            "after": after,
        })
    }

    /// Only the type's own rows are marked; a changed row names its changed columns, and a
    /// removed one keeps its last values.
    #[test]
    fn marks_the_types_rows_by_what_the_branch_did() {
        let folded = fold(vec![
            change(
                "node",
                "Person",
                "p1",
                None,
                Some(json!({"properties": {"name": "Ada"}})),
            ),
            change(
                "node",
                "Person",
                "p2",
                Some(json!({"properties": {"name": "Grace", "born": 1906}})),
                Some(json!({"properties": {"name": "Grace Hopper", "born": 1906}})),
            ),
            change(
                "node",
                "Person",
                "p3",
                Some(json!({"properties": {"name": "Linus", "bio": null}})),
                None,
            ),
            change(
                "node",
                "Note",
                "n1",
                None,
                Some(json!({"properties": {"text": "x"}})),
            ),
            change(
                "edge",
                "Person",
                "e1",
                None,
                Some(json!({"properties": {}, "endpoints": {"from": "p1", "to": "p2"}})),
            ),
        ]);
        let diff = BranchDiff::of(&folded, EntityKind::Node, "Person");
        assert_eq!(diff.mark("p1").map(|m| m.net), Some(NetChange::Added));
        let changed = diff.mark("p2").expect("changed");
        assert_eq!(changed.net, NetChange::Changed);
        assert_eq!(changed.changed, HashSet::from(["name".to_owned()]));
        assert_eq!(diff.mark("p3").map(|m| m.net), Some(NetChange::Removed));
        assert!(diff.mark("n1").is_none(), "another type");
        assert!(diff.mark("e1").is_none(), "an edge of the same name");
        assert_eq!(diff.removed().len(), 1);
        assert_eq!(diff.removed()[0].id, "p3");
        assert_eq!(
            diff.removed()[0].values,
            json!({"name": "Linus"}).as_object().cloned().unwrap()
        );
        assert_eq!(diff.counts(), (1, 1, 1));
    }

    /// A removed edge's ends sit under the table's end columns.
    #[test]
    fn a_removed_edge_keeps_its_ends() {
        let folded = fold(vec![change(
            "edge",
            "Knows",
            "e1",
            Some(json!({"properties": {"since": 2001}, "endpoints": {"from": "p1", "to": "p2"}})),
            None,
        )]);
        let diff = BranchDiff::of(&folded, EntityKind::Edge, "Knows");
        let row = &diff.removed()[0];
        assert_eq!(row.value(FROM_COLUMN), "p1");
        assert_eq!(row.value(TO_COLUMN), "p2");
        assert_eq!(row.value("since"), 2001);
    }

    /// A row inserted then deleted on the branch is no change to the target.
    #[test]
    fn a_row_that_ends_where_it_began_is_not_marked() {
        let folded = fold(vec![
            change(
                "node",
                "Person",
                "p9",
                None,
                Some(json!({"properties": {"name": "x"}})),
            ),
            change(
                "node",
                "Person",
                "p9",
                Some(json!({"properties": {"name": "x"}})),
                None,
            ),
        ]);
        let diff = BranchDiff::of(&folded, EntityKind::Node, "Person");
        assert_eq!(diff.counts(), (0, 0, 0));
        assert!(diff.mark("p9").is_none());
    }
}
