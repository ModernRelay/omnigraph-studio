//! Deleting rows (spec 004): what a delete takes with it, the mutation that deletes all of it
//! in one commit, and the one that puts it back.
//!
//! A node's relationships go with it; the engine removes them. A node that needs a relationship
//! (`@card(n..)`) to a deleted node and would be left with fewer than `n` is a dependent: the
//! server refuses a delete that leaves it so, so it is deleted too, in the same commit. Its own
//! dependents follow, round by round, until a round finds none. Every read is held at the
//! commit the rows were shown at, and the delete is bound to that commit, so what the dialog
//! lists is exactly what goes.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use gpui::{App, Entity, Task};
use gq_language::builder::{delete_nodes, required_dependents};
use gq_language::{BuiltQuery, SchemaModel};
use graph_client::ReadError;
use serde_json::Value;

use crate::changes::{ChangeFold, NetChange, REVIEW_CHANGE_CAP};
use crate::fixes::{compose, revert_query};
use crate::session::GraphSession;

/// The most dependents a delete from Studio takes; one that needs more is refused, so a click
/// never removes an unbounded part of the graph.
pub const MAX_DEPENDENTS: usize = 1000;
/// Rounds of dependents of dependents before the chain is called too long.
const MAX_ROUNDS: usize = 32;
/// Changes read per page when undoing: the most a page holds.
const UNDO_PAGE: u32 = 8_192;
/// The most changes Undo reads and puts back in one commit, as many as a review folds
/// (constitution IX): a delete that removed more is refused before it is all held.
const UNDO_CAP: usize = REVIEW_CHANGE_CAP;

/// A node deleted because it needs one that is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependent {
    pub type_name: String,
    pub id: String,
    pub label: Option<String>,
    /// The relationship it needs.
    pub edge: String,
}

/// Everything one delete removes, found before it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletePlan {
    /// Every node to delete, by type, the type of the rows asked for first.
    pub nodes: Vec<(String, Vec<String>)>,
    /// How many rows were asked for.
    pub requested: usize,
    pub dependents: Vec<Dependent>,
    /// Why Undo could not restore this delete, when it could not.
    pub undo_unavailable: Option<String>,
}

impl DeletePlan {
    /// The mutation that deletes every node of the plan in one commit.
    pub fn query(&self, schema: &SchemaModel) -> Result<BuiltQuery, gq_language::BuildError> {
        let parts = self
            .nodes
            .iter()
            .map(|(type_name, ids)| delete_nodes(schema, type_name, ids))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(compose("studio_delete", parts))
    }

    /// Dependents per type, in the order found: `[("Person", 2), ("Badge", 1)]`.
    pub fn dependent_counts(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for dependent in &self.dependents {
            match counts.iter_mut().find(|(t, _)| *t == dependent.type_name) {
                Some((_, n)) => *n += 1,
                None => counts.push((dependent.type_name.clone(), 1)),
            }
        }
        counts
    }
}

/// Why a delete could not be planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    Read(String),
    TooMany,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Read(message) => f.write_str(message),
            PlanError::TooMany => write!(
                f,
                "More than {MAX_DEPENDENTS} other nodes need these. Delete them in the query \
                 editor, where the whole delete is written out."
            ),
        }
    }
}

impl From<ReadError> for PlanError {
    fn from(error: ReadError) -> Self {
        PlanError::Read(error.to_string())
    }
}

/// Finds what deleting `ids` of `type_name` takes with it, reading at `at_commit` (the commit
/// the rows were shown at; the head when `None`).
pub fn plan_node_delete(
    session: Entity<GraphSession>,
    schema: Arc<SchemaModel>,
    type_name: String,
    ids: Vec<String>,
    at_commit: Option<String>,
    cx: &mut App,
) -> Task<Result<DeletePlan, PlanError>> {
    cx.spawn(async move |cx| {
        let requested = ids.len();
        let mut nodes: Vec<(String, Vec<String>)> = vec![(type_name.clone(), ids.clone())];
        let mut seen: HashMap<String, BTreeSet<String>> =
            HashMap::from([(type_name.clone(), ids.into_iter().collect())]);
        let mut dependents = Vec::new();
        // The types that gained nodes in the last round: only edges into them can find more.
        let mut grown: BTreeSet<String> = BTreeSet::from([type_name]);
        let required: Vec<_> = schema.edges.iter().filter(|edge| edge.min > 0).collect();
        for _ in 0..MAX_ROUNDS {
            let mut next = BTreeSet::new();
            // A round's reads go together, each from what the rounds before found: a node one
            // of them finds is read past in the next round, and one found twice counts once.
            let mut reads = Vec::new();
            for edge in required.iter().filter(|edge| grown.contains(&edge.to)) {
                let gone: Vec<String> = seen[&edge.to].iter().cloned().collect();
                let done: Vec<String> = seen
                    .get(&edge.from)
                    .map(|ids| ids.iter().cloned().collect())
                    .unwrap_or_default();
                let query = required_dependents(
                    &schema,
                    &edge.name,
                    &gone,
                    &done,
                    (MAX_DEPENDENTS + 1) as u32,
                )
                .map_err(|error| PlanError::Read(error.to_string()))?;
                let read = session.update(cx, |session, cx| {
                    session.read(&query, at_commit.clone(), cx)
                });
                reads.push((edge, read));
            }
            let (edges, reads): (Vec<&gq_language::EdgeType>, Vec<_>) = reads.into_iter().unzip();
            let outputs = futures::future::join_all(reads).await;
            for (edge, output) in edges.into_iter().zip(outputs) {
                let output = output?;
                let rows = output.rows.as_array().cloned().unwrap_or_default();
                for row in rows {
                    let Some(id) = row.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let known = seen.entry(edge.from.clone()).or_default();
                    if !known.insert(id.to_owned()) {
                        continue;
                    }
                    match nodes.iter_mut().find(|(t, _)| *t == edge.from) {
                        Some((_, ids)) => ids.push(id.to_owned()),
                        None => nodes.push((edge.from.clone(), vec![id.to_owned()])),
                    }
                    dependents.push(Dependent {
                        type_name: edge.from.clone(),
                        id: id.to_owned(),
                        label: row.get("label").and_then(Value::as_str).map(str::to_owned),
                        edge: edge.name.clone(),
                    });
                    if dependents.len() > MAX_DEPENDENTS {
                        return Err(PlanError::TooMany);
                    }
                    next.insert(edge.from.clone());
                }
            }
            if next.is_empty() {
                let types: Vec<&str> = nodes.iter().map(|(t, _)| t.as_str()).collect();
                let undo_unavailable = undo_unavailable(&schema, &types);
                return Ok(DeletePlan {
                    nodes,
                    requested,
                    dependents,
                    undo_unavailable,
                });
            }
            grown = next;
        }
        Err(PlanError::TooMany)
    })
}

/// Why Undo could not bring back nodes of `types` with their relationships: an insert cannot
/// give a node without a key its old id, and Blob and Vector values are not in the change
/// feed's images. Judged by the schema, so it is known before the delete is sent.
pub fn undo_unavailable(schema: &SchemaModel, types: &[&str]) -> Option<String> {
    for type_name in types {
        let Some(node) = schema.node(type_name) else {
            return Some(format!("The schema has no type {type_name}."));
        };
        if node.key.is_none() {
            return Some(format!(
                "{} has no key, so Undo could not bring one back with its id.",
                node.name
            ));
        }
        if let Some(property) = node.properties.iter().find(|p| !p.projectable()) {
            return Some(format!(
                "{}.{} holds {} values, which Undo cannot restore.",
                node.name, property.name, property.base_type
            ));
        }
    }
    for edge in &schema.edges {
        if !types.iter().any(|t| *t == edge.from || *t == edge.to) {
            continue;
        }
        if let Some(property) = edge.properties.iter().find(|p| !p.projectable()) {
            return Some(format!(
                "{}.{} holds {} values, which Undo cannot restore.",
                edge.name, property.name, property.base_type
            ));
        }
    }
    None
}

/// The mutation that puts back everything the delete `commit` removed: its nodes, then their
/// relationships, in one commit. Unkeyed relationships come back under new ids. A commit that
/// did anything but remove cannot be undone this way.
pub fn undo_query(
    session: Entity<GraphSession>,
    schema: Arc<SchemaModel>,
    commit: String,
    cx: &mut App,
) -> Task<Result<BuiltQuery, String>> {
    cx.spawn(async move |cx| {
        let mut fold = ChangeFold::new();
        let mut page_token: Option<String> = None;
        let mut read = 0;
        loop {
            let page = session
                .update(cx, |session, cx| {
                    session.commit_changes(&commit, page_token.clone(), UNDO_PAGE, cx)
                })
                .await
                .map_err(|error| error.to_string())?;
            read += page.changes.len();
            if read > UNDO_CAP {
                return Err(format!(
                    "The commit removed more than {} nodes and relationships, more than Undo puts \
                     back in one commit.",
                    studio_ui::group_digits(UNDO_CAP)
                ));
            }
            for change in page.changes {
                fold.add(&page.cause, change);
            }
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => break,
            }
        }
        let folded = fold.finish();
        let mut parts = Vec::new();
        for change in &folded.changes {
            if change.net() != Some(NetChange::Removed) {
                return Err(
                    "The commit did more than delete, so Undo cannot put it back.".to_owned(),
                );
            }
            parts.push(revert_query(&schema, change).map_err(|reason| reason.0)?);
        }
        if parts.is_empty() {
            return Err("The commit removed nothing.".to_owned());
        }
        Ok(compose("studio_undo_delete", parts))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use gpui::{AppContext as _, TestAppContext};
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};

    use super::*;
    use crate::connection::{Connection, ServerEntry, ServerSource};

    const REQUIRED: &str = "node Company {\n  slug: String @key\n  name: String\n}\n\
        node Person {\n  slug: String @key\n  name: String\n}\n\
        node Note {\n  body: String\n}\n\
        node Doc {\n  slug: String @key\n  file: Blob?\n}\n\
        edge WorksAt: Person -> Company @card(1..)\n\
        edge About: Note -> Person @card(1..)\n";

    const CHAIN: &str = "node Company {\n  slug: String @key\n  name: String\n}\n\
        node Person {\n  slug: String @key\n  name: String\n}\n\
        node Badge {\n  slug: String @key\n}\n\
        edge WorksAt: Person -> Company @card(1..)\n\
        edge Holds: Badge -> Person @card(1..1)\n\
        edge Knows: Person -> Person @card(0..)\n";

    /// Undo of a delete that removed more than it puts back in one commit is refused once the
    /// pages read pass the cap, without reading the rest or holding them.
    #[gpui::test]
    async fn undo_refuses_a_delete_larger_than_one_commit_puts_back(cx: &mut TestAppContext) {
        let pages = Arc::new(Mutex::new(0usize));
        let counted = pages.clone();
        let http = FakeHttpClient::create(move |request| {
            let counted = counted.clone();
            async move {
                let path = request.uri().path().to_owned();
                let body = if path.ends_with("/changes") {
                    let page = {
                        let mut pages = counted.lock().unwrap();
                        *pages += 1;
                        *pages
                    };
                    let changes: Vec<Value> = (0..UNDO_PAGE)
                        .map(|n| {
                            serde_json::json!({
                                "kind": "node",
                                "type": {"id": "t1", "name": "Company"},
                                "id": format!("c{page}-{n}"),
                                "op": "delete",
                                "before": {"properties": {"slug": format!("c{page}-{n}"), "name": "x"}},
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "cause": {"graph_commit_id": "c9", "authored_branch": "main", "authored_at": 9},
                        "changes": changes,
                        "next_page_token": format!("p{page}"),
                    })
                } else if path.ends_with("/schema") {
                    serde_json::json!({ "schema_source": REQUIRED })
                } else {
                    serde_json::json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let schema = Arc::new(SchemaModel::parse(REQUIRED).unwrap());
        let undo = cx.update(|cx| undo_query(session.clone(), schema, "c9".into(), cx));
        cx.run_until_parked();
        let refused = futures::executor::block_on(undo).unwrap_err();
        assert!(refused.contains("more than 20,000"), "{refused}");
        assert_eq!(
            *pages.lock().unwrap(),
            UNDO_CAP.div_ceil(UNDO_PAGE as usize),
            "no page past the cap"
        );
    }

    /// Deleting a company finds the person who works only there, then the badge only that
    /// person holds; each read is held at the commit the rows were shown at, and a round that
    /// finds nothing ends the search. An optional relationship (`Knows`) is never read.
    #[gpui::test]
    async fn dependents_follow_the_chain_of_required_relationships(cx: &mut TestAppContext) {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let log = seen.clone();
        let http = FakeHttpClient::create(move |request| {
            let log = log.clone();
            async move {
                let path = request.uri().path().to_owned();
                let mut body = Vec::new();
                use futures::AsyncReadExt as _;
                request.into_body().read_to_end(&mut body).await.ok();
                let body = if path.ends_with("/schema") {
                    serde_json::json!({ "schema_source": CHAIN })
                } else if path.ends_with("/query") {
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    log.lock().unwrap().push(request.clone());
                    let text = request["query"].as_str().unwrap_or_default();
                    let gone = request["params"]["gone"].to_string();
                    let rows = if text.contains("$m: Person") && gone.contains("acme") {
                        serde_json::json!([{"id": "ada", "label": "Ada"}])
                    } else if text.contains("$m: Badge") && gone.contains("ada") {
                        serde_json::json!([{"id": "b-ada"}])
                    } else {
                        serde_json::json!([])
                    };
                    serde_json::json!({
                        "query_name": "q",
                        "target": {"branch": null, "snapshot": "c1"},
                        "row_count": rows.as_array().unwrap().len(),
                        "columns": ["id"],
                        "rows": rows,
                        "graph_commit_id": "c1",
                    })
                } else {
                    serde_json::json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let schema = Arc::new(SchemaModel::parse(CHAIN).unwrap());
        let plan = cx.update(|cx| {
            plan_node_delete(
                session.clone(),
                schema.clone(),
                "Company".into(),
                vec!["acme".into()],
                Some("c1".into()),
                cx,
            )
        });
        cx.run_until_parked();
        let plan = plan.await.unwrap();
        assert_eq!(
            plan.nodes,
            [
                ("Company".to_owned(), vec!["acme".to_owned()]),
                ("Person".to_owned(), vec!["ada".to_owned()]),
                ("Badge".to_owned(), vec!["b-ada".to_owned()]),
            ]
        );
        assert_eq!(plan.dependents[0].label.as_deref(), Some("Ada"));
        assert_eq!(plan.dependents[0].edge, "WorksAt");
        assert_eq!(plan.dependents[1].edge, "Holds");
        assert_eq!(plan.undo_unavailable, None);
        // The session's own reads (its head) are not the plan's.
        let reads: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|read| read["name"] == "studio_required_dependents")
            .cloned()
            .collect();
        // WorksAt from the company, Holds from the person, then nothing grew.
        assert_eq!(reads.len(), 2, "{reads:#?}");
        // Held at c1: pinned to it, or, c1 being the head, sent as a read of the branch whose
        // answer the session accepts only at c1 (spec 015).
        assert!(
            reads.iter().all(|read| read["snapshot"] == "c1"
                || (read["branch"] == "main" && read.get("snapshot").is_none())),
            "{reads:#?}"
        );
        assert!(
            reads
                .iter()
                .all(|read| !read["query"].as_str().unwrap().contains("knows"))
        );
    }

    /// Undo needs a key on every node it brings back and no value the change feed leaves
    /// out, on the nodes or on any relationship they had.
    #[test]
    fn undo_is_known_before_the_delete() {
        let schema = SchemaModel::parse(REQUIRED).unwrap();
        assert_eq!(undo_unavailable(&schema, &["Company"]), None);
        assert_eq!(undo_unavailable(&schema, &["Company", "Person"]), None);
        assert!(
            undo_unavailable(&schema, &["Note"])
                .unwrap()
                .contains("no key")
        );
        assert!(
            undo_unavailable(&schema, &["Doc"])
                .unwrap()
                .contains("Doc.file")
        );
    }

    /// One statement per type, in one mutation, the rows asked for first.
    #[test]
    fn a_plan_deletes_every_type_in_one_mutation() {
        let schema = SchemaModel::parse(REQUIRED).unwrap();
        let plan = DeletePlan {
            nodes: vec![
                ("Company".into(), vec!["acme".into()]),
                ("Person".into(), vec!["ada".into(), "cy".into()]),
            ],
            requested: 1,
            dependents: vec![
                Dependent {
                    type_name: "Person".into(),
                    id: "ada".into(),
                    label: Some("Ada".into()),
                    edge: "WorksAt".into(),
                },
                Dependent {
                    type_name: "Person".into(),
                    id: "cy".into(),
                    label: None,
                    edge: "WorksAt".into(),
                },
            ],
            undo_unavailable: None,
        };
        let query = plan.query(&schema).unwrap();
        let company = query.text.find("delete Company where @id in").unwrap();
        let person = query.text.find("delete Person where @id in").unwrap();
        assert!(company < person, "{}", query.text);
        assert_eq!(plan.dependent_counts(), [("Person".to_owned(), 2)]);
        let check = gq_language::check(&schema, &query.text);
        assert!(check.diagnostics.is_empty(), "{:?}", check.diagnostics);
    }
}
