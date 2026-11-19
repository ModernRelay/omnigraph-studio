//! One node's history (spec 040): the commits of a branch's recent first-parent history that
//! changed the node or one of its relationships, newest first, each with who made it and what
//! it did.
//!
//! Omnigraph has no read of the commits that changed one entity (E62). Studio reads a window of
//! the branch's newest commits instead: the commit listing names the window's oldest commit,
//! and the branch's change feed from just before it, filtered to the node's type and the
//! relationship types that end at it, answers every commit of the window in one or two
//! requests (each feed page covers up to 128 commits, E38). The feed is read forward and stops
//! at the head the listing resolved, so the window is one snapshot (constitution II). An older
//! window goes on from the listing's page token and stops where the window before it began.
//! Pure functions; the session sends the reads ([`crate::GraphSession::node_history`]).

use gq_language::SchemaModel;
use graph_client::api::{ChangeBlockOutput, ChangeCauseOutput, EntityChangeOutput, EntityKind};

use crate::changes::{FoldedChange, endpoints_of};

/// First-parent commits a window of a node's history covers: about two feed pages, so a window
/// is read in a second or two on a large graph (spec 008's readings), and the honest exit is
/// "Read the 200 commits before".
pub const NODE_HISTORY_COMMITS: u32 = 200;

/// Changes a feed page of a node's history holds at most: the server's largest page, so a
/// window is as few requests as the server allows (E36).
pub const NODE_HISTORY_FEED_LIMIT: u32 = 8_192;

/// Feed pages a window reads at most. A page ends on a whole commit and covers up to 128
/// commits, so a window of [`NODE_HISTORY_COMMITS`] needs two; more are taken only when the
/// node's types changed so often that a page's change limit ended it first.
pub const NODE_HISTORY_FEED_PAGES: usize = 12;

/// What a node's history reads: the node's type and id, and the relationship types that can
/// end at it, with the ends that are its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeScope {
    pub type_name: String,
    pub id: String,
    /// Each relationship type that ends at the type: its name, and whether its source and its
    /// target are of the node's type.
    edges: Vec<(String, bool, bool)>,
}

impl NodeScope {
    /// The node `id` of `type_name`, with every relationship type the schema ends at its type.
    pub fn new(schema: &SchemaModel, type_name: &str, id: &str) -> Self {
        let edges = schema
            .edges
            .iter()
            .filter(|edge| edge.from == type_name || edge.to == type_name)
            .map(|edge| {
                (
                    edge.name.clone(),
                    edge.from == type_name,
                    edge.to == type_name,
                )
            })
            .collect();
        Self {
            type_name: type_name.to_owned(),
            id: id.to_owned(),
            edges,
        }
    }

    /// The types the feed is filtered by: the node's, then its relationships'.
    pub fn types(&self) -> Vec<String> {
        std::iter::once(self.type_name.clone())
            .chain(self.edges.iter().map(|(name, _, _)| name.clone()))
            .collect()
    }

    /// Whether `change` is to the node or to one of its relationships.
    fn holds(&self, change: &EntityChangeOutput) -> bool {
        match change.kind {
            EntityKind::Node => change.entity_type.name == self.type_name && change.id == self.id,
            EntityKind::Edge => {
                let Some((_, from, to)) = self
                    .edges
                    .iter()
                    .find(|(name, _, _)| *name == change.entity_type.name)
                else {
                    return false;
                };
                [change.before.as_ref(), change.after.as_ref()]
                    .into_iter()
                    .flatten()
                    .filter_map(|image| endpoints_of(image.endpoints.as_ref()?))
                    .any(|(source, target)| {
                        (*from && source == self.id) || (*to && target == self.id)
                    })
            }
        }
    }
}

/// One commit's changes to the node and its relationships.
#[derive(Debug, Clone)]
pub struct NodeHistoryEntry {
    pub commit: String,
    pub actor: Option<String>,
    /// When the commit was authored, Unix microseconds.
    pub at: i64,
    /// The merged branch's head, for a merge: the change is the branch's, made by others.
    pub merged_parent: Option<String>,
    /// What the commit did to the node itself.
    pub node: Option<FoldedChange>,
    /// What it did to the node's relationships, in the feed's order.
    pub edges: Vec<FoldedChange>,
}

/// Where an older window of a node's history goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OlderWindow {
    /// The commit listing's continuation, for the head the first window resolved.
    pub page_token: String,
    /// The oldest commit read so far: the older window stops before it.
    pub until: String,
}

/// What a window of a node's history may read: the first, from the branch head, or an older
/// one, below the windows read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeHistoryWindow {
    Newest,
    Older { head: String, window: OlderWindow },
}

/// A window of a node's history, as it was read.
#[derive(Debug, Clone)]
pub struct NodeHistoryPage {
    /// The head the window is read at: the branch's head when the first window was read.
    pub head: String,
    /// The commits that changed the node or its relationships, newest first.
    pub entries: Vec<NodeHistoryEntry>,
    /// The first-parent commits the window covered.
    pub commits: usize,
    /// Where an older window goes on; `None` once the window reached the graph's first commit.
    pub older: Option<OlderWindow>,
    /// Why the window was not read to its end, in words, when it was not: what it holds is
    /// what was read before that.
    pub stopped: Option<String>,
}

/// Folds feed pages into a window's entries, keeping the commits that touched the node.
#[derive(Debug)]
pub struct NodeHistoryFold {
    scope: NodeScope,
    /// The commit the window ends at, inclusive: the head for the first window.
    last: Option<String>,
    /// The commit the window ends before: where the window above it began.
    before: Option<String>,
    /// Entries oldest first, as the feed reads.
    entries: Vec<NodeHistoryEntry>,
    commits: usize,
    done: bool,
}

impl NodeHistoryFold {
    /// A fold that ends at commit `last` (inclusive), or before commit `before`.
    pub fn new(scope: NodeScope, last: Option<String>, before: Option<String>) -> Self {
        Self {
            scope,
            last,
            before,
            entries: Vec::new(),
            commits: 0,
            done: false,
        }
    }

    /// Takes a page's blocks; says whether the window is read to its end.
    pub fn page(&mut self, blocks: Vec<ChangeBlockOutput>) -> bool {
        for block in blocks {
            if self.done {
                break;
            }
            let ChangeBlockOutput { cause, changes } = block;
            if self.before.as_deref() == Some(cause.graph_commit_id.as_str()) {
                self.done = true;
                break;
            }
            self.commits += 1;
            self.take(&cause, changes);
            if self.last.as_deref() == Some(cause.graph_commit_id.as_str()) {
                self.done = true;
            }
        }
        self.done
    }

    fn take(&mut self, cause: &ChangeCauseOutput, changes: Vec<EntityChangeOutput>) {
        let mut entry = NodeHistoryEntry {
            commit: cause.graph_commit_id.clone(),
            actor: cause.actor_id.clone(),
            at: cause.authored_at,
            merged_parent: cause.merged_parent_commit_id.clone(),
            node: None,
            edges: Vec::new(),
        };
        for change in changes {
            if !self.scope.holds(&change) {
                continue;
            }
            let is_edge = matches!(change.kind, EntityKind::Edge);
            let folded = FoldedChange {
                kind: change.kind,
                type_id: change.entity_type.id,
                type_name: change.entity_type.name,
                id: change.id,
                before: change.before,
                after: change.after,
                commits: vec![cause.graph_commit_id.clone()],
                set_by: Default::default(),
            };
            // A change that ends where it began says nothing of the node.
            if folded.net().is_none() {
                continue;
            }
            if is_edge {
                entry.edges.push(folded);
            } else {
                entry.node = Some(folded);
            }
        }
        if entry.node.is_some() || !entry.edges.is_empty() {
            self.entries.push(entry);
        }
    }

    /// Whether the window is read to its end.
    pub fn done(&self) -> bool {
        self.done
    }

    /// Whether no commit was read yet.
    pub fn is_empty(&self) -> bool {
        self.commits == 0
    }

    /// The window's entries, newest first, and the commits it covered.
    pub fn finish(self) -> (Vec<NodeHistoryEntry>, usize) {
        let mut entries = self.entries;
        entries.reverse();
        (entries, self.commits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCHEMA: &str = "node Issue {\n    slug: String @key\n    status: String?\n    \
        priority: String?\n}\n\
        node Comment {\n    slug: String @key\n}\n\
        edge BlockedBy: Issue -> Issue\n\
        edge CommentOn: Comment -> Issue\n";

    fn block(commit: &str, actor: &str, changes: serde_json::Value) -> ChangeBlockOutput {
        serde_json::from_value(json!({
            "cause": {
                "graph_commit_id": commit,
                "authored_branch": "main",
                "authored_at": 1_000,
                "actor_id": actor,
            },
            "changes": changes,
        }))
        .unwrap()
    }

    fn issue(id: &str, before: serde_json::Value, after: serde_json::Value) -> serde_json::Value {
        json!({
            "kind": "node", "type": {"id": "t-issue", "name": "Issue"}, "id": id, "op": "update",
            "before": {"properties": before}, "after": {"properties": after},
        })
    }

    fn edge(name: &str, from: &str, to: &str) -> serde_json::Value {
        json!({
            "kind": "edge", "type": {"id": format!("t-{name}"), "name": name},
            "id": format!("[\"{from}\",\"{to}\"]"), "op": "insert",
            "after": {"properties": {}, "endpoints": {"from": from, "to": to}},
        })
    }

    fn scope() -> NodeScope {
        NodeScope::new(&SchemaModel::parse(SCHEMA).unwrap(), "Issue", "lk-acl")
    }

    #[test]
    fn the_scope_reads_the_type_and_the_relationships_that_end_at_it() {
        assert_eq!(scope().types(), ["Issue", "BlockedBy", "CommentOn"]);
    }

    /// A window keeps the commits that changed the node or a relationship ending at it,
    /// newest first, with who made each; another node's change and a change that ends where
    /// it began are left out.
    #[test]
    fn a_window_keeps_the_commits_that_touched_the_node() {
        let mut fold = NodeHistoryFold::new(scope(), Some("c4".into()), None);
        let done = fold.page(vec![
            block(
                "c1",
                "act-writer",
                json!([issue(
                    "lk-acl",
                    json!({"status": "open"}),
                    json!({"status": "in_progress"})
                )]),
            ),
            block(
                "c2",
                "act-writer",
                json!([issue(
                    "lk-share",
                    json!({"status": "open"}),
                    json!({"status": "closed"})
                )]),
            ),
            block(
                "c3",
                "act-writer",
                json!([
                    edge("CommentOn", "c-acl-1", "lk-acl"),
                    edge("BlockedBy", "lk-acl", "x")
                ]),
            ),
        ]);
        assert!(!done, "the head is not reached yet");
        let done = fold.page(vec![
            block(
                "c4",
                "act-admin",
                json!([issue(
                    "lk-acl",
                    json!({"priority": "1"}),
                    json!({"priority": "1"})
                )]),
            ),
            block(
                "c5",
                "act-admin",
                json!([issue(
                    "lk-acl",
                    json!({"priority": "1"}),
                    json!({"priority": "0"})
                )]),
            ),
        ]);
        assert!(done, "the window ends at its head");
        let (entries, commits) = fold.finish();
        assert_eq!(commits, 4, "c5 is past the head the window was read at");
        let read: Vec<(&str, Option<&str>, bool, usize)> = entries
            .iter()
            .map(|e| {
                (
                    e.commit.as_str(),
                    e.actor.as_deref(),
                    e.node.is_some(),
                    e.edges.len(),
                )
            })
            .collect();
        assert_eq!(
            read,
            [
                ("c3", Some("act-writer"), false, 2),
                ("c1", Some("act-writer"), true, 0)
            ]
        );
    }

    /// An older window ends where the window above it began.
    #[test]
    fn an_older_window_stops_before_the_window_above() {
        let mut fold = NodeHistoryFold::new(scope(), None, Some("c2".into()));
        let done = fold.page(vec![
            block(
                "c1",
                "act-writer",
                json!([issue(
                    "lk-acl",
                    json!({"status": "open"}),
                    json!({"status": "closed"})
                )]),
            ),
            block(
                "c2",
                "act-writer",
                json!([issue(
                    "lk-acl",
                    json!({"status": "closed"}),
                    json!({"status": "open"})
                )]),
            ),
        ]);
        assert!(done);
        let (entries, commits) = fold.finish();
        assert_eq!(commits, 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].commit, "c1");
    }
}
