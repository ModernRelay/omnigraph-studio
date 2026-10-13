//! Relationship counts asked for one node at a time, read for many at once. The tree, the
//! canvas and the peek each ask how many neighbours a node has through a relationship; when an
//! exploration expands several nodes, reloads after Refresh, or comes back after a restart,
//! that is one `builder::degree` read per node and relationship. Counts asked for in the same
//! turn of the app for one node type, relationship and commit go out together as
//! `builder::page_degrees` reads of at most [`DEGREE_BATCH`] nodes, sorted, so the same batch
//! asked again at a commit is answered by the session's memo.

use std::collections::{BTreeMap, HashMap};

use futures::channel::oneshot;
use gq_language::RelationshipGroup;
use serde_json::Value;

/// Nodes per `page_degrees` read, as the table reads a page's relationship columns.
pub const DEGREE_BATCH: usize = 100;

/// Where one count goes: the asker, until it stops waiting.
pub(crate) type Waiter = oneshot::Sender<Result<u64, String>>;

/// The counts read together: one node type, one relationship, one commit (`None`, the branch
/// head).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DegreeBatchKey {
    pub type_name: String,
    pub group: RelationshipGroup,
    pub snapshot: Option<String>,
}

/// One read to send: its key, and its nodes in id order with everyone waiting on each.
pub(crate) type DegreeBatch = (DegreeBatchKey, Vec<(String, Vec<Waiter>)>);

/// The counts asked for since the last flush.
#[derive(Default)]
pub(crate) struct DegreeBatches {
    pending: HashMap<DegreeBatchKey, BTreeMap<String, Vec<Waiter>>>,
    scheduled: bool,
}

impl DegreeBatches {
    /// Adds an asker for `id`'s count; true when no flush is scheduled yet, so the caller
    /// schedules one.
    pub(crate) fn add(&mut self, key: DegreeBatchKey, id: String, waiter: Waiter) -> bool {
        self.pending
            .entry(key)
            .or_default()
            .entry(id)
            .or_default()
            .push(waiter);
        !std::mem::replace(&mut self.scheduled, true)
    }

    /// Everything asked for, as reads of at most `chunk` nodes each.
    pub(crate) fn take(&mut self, chunk: usize) -> Vec<DegreeBatch> {
        self.scheduled = false;
        let mut batches = Vec::new();
        for (key, nodes) in self.pending.drain() {
            let mut nodes = nodes.into_iter().peekable();
            while nodes.peek().is_some() {
                let part: Vec<_> = nodes.by_ref().take(chunk.max(1)).collect();
                batches.push((key.clone(), part));
            }
        }
        batches
    }
}

/// `id → n` from a `builder::page_degrees` answer; a node with no neighbours has no row.
pub(crate) fn page_counts(rows: &Value) -> HashMap<String, u64> {
    rows.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((row.get("id")?.as_str()?.to_owned(), row.get("n")?.as_u64()?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gq_language::Direction;
    use serde_json::json;

    fn key(type_name: &str, edge: &str) -> DegreeBatchKey {
        DegreeBatchKey {
            type_name: type_name.into(),
            group: RelationshipGroup {
                edge: edge.into(),
                direction: Direction::Outgoing,
                other_type: "Person".into(),
            },
            snapshot: Some("c1".into()),
        }
    }

    #[test]
    fn askers_gather_by_type_relationship_and_commit_in_id_order() {
        let mut batches = DegreeBatches::default();
        let mut receivers = Vec::new();
        let mut ask = |batches: &mut DegreeBatches, key: DegreeBatchKey, id: &str| {
            let (tx, rx) = oneshot::channel();
            receivers.push(rx);
            batches.add(key, id.into(), tx)
        };
        assert!(
            ask(&mut batches, key("Person", "Knows"), "p-3"),
            "the first schedules"
        );
        assert!(!ask(&mut batches, key("Person", "Knows"), "p-1"));
        assert!(
            !ask(&mut batches, key("Person", "Knows"), "p-3"),
            "a second asker"
        );
        assert!(!ask(&mut batches, key("Person", "WorksAt"), "p-1"));
        assert!(!ask(&mut batches, key("Person", "Knows"), "p-2"));

        let mut taken = batches.take(2);
        taken.sort_by(|a, b| {
            (a.0.group.edge.as_str(), &a.1[0].0).cmp(&(b.0.group.edge.as_str(), &b.1[0].0))
        });
        let shape: Vec<(String, Vec<(String, usize)>)> = taken
            .iter()
            .map(|(key, nodes)| {
                (
                    key.group.edge.clone(),
                    nodes
                        .iter()
                        .map(|(id, waiters)| (id.clone(), waiters.len()))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("Knows".into(), vec![("p-1".into(), 1), ("p-2".into(), 1)]),
                ("Knows".into(), vec![("p-3".into(), 2)]),
                ("WorksAt".into(), vec![("p-1".into(), 1)]),
            ]
        );
        assert!(batches.take(2).is_empty());
        let (tx, _rx) = oneshot::channel();
        assert!(
            batches.add(key("Person", "Knows"), "p-9".into(), tx),
            "after a flush the next asker schedules again"
        );
    }

    #[test]
    fn counts_read_by_id_and_absent_nodes_have_none() {
        let counts = page_counts(&json!([{"id": "p-1", "n": 3}, {"id": "p-2", "n": 0}, {"n": 4}]));
        assert_eq!(counts.get("p-1"), Some(&3));
        assert_eq!(counts.get("p-2"), Some(&0));
        assert_eq!(counts.len(), 2);
    }
}
