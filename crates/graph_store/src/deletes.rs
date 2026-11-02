//! Deleting rows (spec 004): what a delete takes with it, the mutation that deletes all of it
//! in one commit, and the one that puts it back.
//!
//! A node's relationships go with it; the engine removes them. A node that needs a relationship
//! (`@card(n..)`) to a deleted node and would be left with fewer than `n` is a dependent: the
//! server refuses a delete that leaves it so, so it is deleted too, in the same commit. Its own
//! dependents follow, round by round, until a round finds none. Every read is held at the
//! commit the rows were shown at, and the delete is bound to that commit, so what the dialog
//! lists is exactly what goes. What it removes is counted there too, so a delete larger than
//! Undo puts back is confirmed by typing before it is sent, not found too large by Undo after.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use gpui::{App, Entity, Task};
use gq_language::builder::{
    EdgeEnd, delete_nodes, edge_count, endpoint_property_key, required_dependents,
};
use gq_language::{BuiltQuery, Filter, FilterOp, SchemaModel};
use graph_client::ReadError;
use serde_json::Value;

use crate::changes::{ChangeFold, NetChange, REVIEW_CHANGE_CAP};
use crate::fixes::{compose, revert_query};
use crate::session::GraphSession;

/// The most dependents a delete from Studio takes; one that needs more is refused, so a click
/// never removes an unbounded part of the graph.
pub const MAX_DEPENDENTS: usize = 1000;
/// Rounds of dependents of dependents before the chain is called too long: each round reads
/// one step further down it.
const MAX_ROUNDS: usize = 32;
/// Changes read per page when undoing: the most a page holds.
const UNDO_PAGE: u32 = 8_192;
/// The most changes Undo reads and puts back in one commit, as many as a review folds
/// (constitution IX): a delete that removed more is refused before it is all held, and a delete
/// that would remove more is confirmed by typing.
pub const UNDO_CAP: usize = REVIEW_CHANGE_CAP;

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
    pub dependents: Vec<Dependent>,
    /// Why Undo could not restore this delete, when it could not: judged from the schema, then
    /// from how much it removes at the commit it is bound to.
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
    /// A read of what depends on the rows failed.
    Read(String),
    /// More than [`MAX_DEPENDENTS`] nodes would go with the rows.
    TooMany,
    /// Every one of [`MAX_ROUNDS`] rounds found nodes that need those found before it: the
    /// chain of dependents goes at least that many steps deep, and may go further.
    TooDeep,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Read(message) => {
                write!(f, "Studio could not read what depends on it. {message}")
            }
            PlanError::TooMany => write!(
                f,
                "More than {} other nodes need these. Delete them in the query editor, where \
                 the whole delete is written out.",
                studio_ui::group_digits(MAX_DEPENDENTS)
            ),
            PlanError::TooDeep => write!(
                f,
                "Nodes that need these are needed by others in turn, at least {MAX_ROUNDS} \
                 steps deep, and Studio follows the chain no further. Delete them in the query \
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
                let mut unavailable = undo_unavailable(&schema, &types);
                // Undo reads the delete's changes and puts at most `UNDO_CAP` back: a delete
                // that removes more is found so here, at the pin, so its dialog asks for the
                // key to be typed rather than offering an Undo that would restore nothing.
                if unavailable.is_none() {
                    let removed = removed_changes(&session, &schema, &nodes, &at_commit, cx).await;
                    unavailable = undo_past_cap(removed);
                }
                return Ok(DeletePlan {
                    nodes,
                    dependents,
                    undo_unavailable: unavailable,
                });
            }
            grown = next;
        }
        Err(PlanError::TooDeep)
    })
}

/// Why Undo could not put back a delete that makes `removed` changes: more than it puts back
/// in one commit, or a count that could not be read, which promises nothing.
fn undo_past_cap(removed: Result<usize, String>) -> Option<String> {
    match removed {
        Ok(removed) if removed > UNDO_CAP => Some(format!(
            "It removes {} nodes and relationships, more than the {} Undo puts back in one \
             commit.",
            studio_ui::group_digits(removed),
            studio_ui::group_digits(UNDO_CAP)
        )),
        Ok(_) => None,
        Err(reason) => Some(format!(
            "Studio could not count what it removes ({reason}), so it cannot say Undo would \
             put it all back."
        )),
    }
}

/// How many changes deleting `nodes` makes, counted at `at_commit`: one per node, and one per
/// relationship any of them has. The rows asked for are counted as they were asked for, so one
/// gone already counts too: the count is never short.
async fn removed_changes(
    session: &Entity<GraphSession>,
    schema: &SchemaModel,
    nodes: &[(String, Vec<String>)],
    at_commit: &Option<String>,
    cx: &mut gpui::AsyncApp,
) -> Result<usize, String> {
    let counts = removal_counts(schema, nodes)?;
    let reads: Vec<_> = counts
        .iter()
        .map(|(_, query)| {
            session.update(cx, |session, cx| session.read(query, at_commit.clone(), cx))
        })
        .collect();
    let outputs = futures::future::join_all(reads).await;
    let mut removed: i64 = nodes.iter().map(|(_, ids)| ids.len() as i64).sum();
    for ((sign, _), output) in counts.iter().zip(outputs) {
        let output = output.map_err(|error| error.to_string())?;
        let n = gq_language::builder::count_of(&output.rows)
            .ok_or_else(|| "the server's count could not be read".to_owned())?;
        removed += sign * n;
    }
    Ok(removed.max(0) as usize)
}

/// The reads that count the relationships deleting `nodes` removes, each with the sign it is
/// summed with. A relationship type with a deleted end is counted by its ends' keys, which are
/// the nodes' ids: those from the deleted sources and those to the deleted targets, less those
/// both, so a relationship between two deleted nodes, or from a node to itself, counts once as
/// it is one change.
fn removal_counts(
    schema: &SchemaModel,
    nodes: &[(String, Vec<String>)],
) -> Result<Vec<(i64, BuiltQuery)>, String> {
    // The filter on one end: its key among the deleted ids of its type; none when no node of
    // its type goes. A node without a key has no value to match its id by.
    let end = |end: EdgeEnd, type_name: &str| -> Result<Option<Filter>, String> {
        let Some((_, ids)) = nodes
            .iter()
            .find(|(t, ids)| t == type_name && !ids.is_empty())
        else {
            return Ok(None);
        };
        let key = schema
            .node(type_name)
            .and_then(|node| node.key.as_deref())
            .ok_or_else(|| format!("{type_name} has no key"))?;
        Ok(Some(Filter {
            property: endpoint_property_key(end, key),
            op: FilterOp::In,
            value: Value::from(ids.clone()),
        }))
    };
    let count = |edge: &str, filters: &[Filter]| {
        edge_count(schema, edge, filters).map_err(|error| error.to_string())
    };
    let mut counts = Vec::new();
    for edge in &schema.edges {
        let from = end(EdgeEnd::From, &edge.from)?;
        let to = end(EdgeEnd::To, &edge.to)?;
        for filter in [&from, &to].into_iter().flatten() {
            counts.push((1, count(&edge.name, std::slice::from_ref(filter))?));
        }
        if let (Some(from), Some(to)) = (from, to) {
            counts.push((-1, count(&edge.name, &[from, to])?));
        }
    }
    Ok(counts)
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
            // The page is shared with the session's memo: its changes are copied for the fold
            // off the main thread.
            let (cause, changes) = (page.cause, page.changes);
            fold = cx
                .background_executor()
                .spawn(async move {
                    for change in Arc::unwrap_or_clone(changes) {
                        fold.add(&cause, change);
                    }
                    fold
                })
                .await;
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => break,
            }
        }
        // The fold's end and the statements, one per change of up to `UNDO_CAP`, off the main
        // thread too: the query they make can be megabytes long.
        cx.background_executor()
            .spawn(async move {
                let folded = fold.finish();
                let mut parts = Vec::new();
                for change in &folded.changes {
                    if change.net() != Some(NetChange::Removed) {
                        return Err(
                            "The commit did more than delete, so Undo cannot put it back."
                                .to_owned(),
                        );
                    }
                    parts.push(revert_query(&schema, change).map_err(|reason| reason.0)?);
                }
                if parts.is_empty() {
                    return Err("The commit removed nothing.".to_owned());
                }
                Ok(compose("studio_undo_delete", parts))
            })
            .await
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
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
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
                    } else if text.contains("count(") {
                        // As the engine answers a count that matches nothing: one row.
                        serde_json::json!([{"n": 0}])
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
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
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

    /// A chain of dependents longer than the rounds Studio follows is refused for its depth,
    /// not for its size: forty links, each needing the one before, stop after 32 rounds with
    /// 32 found, and the refusal says the chain goes at least 32 steps deep. Before, it said
    /// more than 1,000 nodes need them. A chain of 31 ends within the rounds and is planned.
    #[gpui::test]
    async fn a_chain_deeper_than_the_rounds_followed_is_refused_for_its_depth(
        cx: &mut TestAppContext,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        const LINKS: &str = "node Link {\n  slug: String @key\n}\n\
            edge Next: Link -> Link @card(1..)\n";
        // How long the chain is: link `n` needs link `n - 1`, down to `l0`.
        let depth = Arc::new(AtomicUsize::new(40));
        let rounds = Arc::new(AtomicUsize::new(0));
        let (chain, counted) = (depth.clone(), rounds.clone());
        let http = FakeHttpClient::create(move |request| {
            let (chain, counted) = (chain.clone(), counted.clone());
            async move {
                let path = request.uri().path().to_owned();
                let mut body = Vec::new();
                use futures::AsyncReadExt as _;
                request.into_body().read_to_end(&mut body).await.ok();
                let body = if path.ends_with("/schema") {
                    serde_json::json!({ "schema_source": LINKS })
                } else if path.ends_with("/query") {
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    let rows = match request["name"].as_str() {
                        Some("studio_required_dependents") => {
                            counted.fetch_add(1, Ordering::SeqCst);
                            // The link that needs the furthest one gone, while the chain goes on.
                            let furthest = request["params"]["gone"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|id| id.as_str()?.strip_prefix('l')?.parse().ok())
                                .max()
                                .unwrap_or(0usize);
                            let next = furthest + 1;
                            if next <= chain.load(Ordering::SeqCst) {
                                serde_json::json!([{ "id": format!("l{next}") }])
                            } else {
                                serde_json::json!([])
                            }
                        }
                        Some("studio_edge_count") => serde_json::json!([{ "n": 0 }]),
                        _ => serde_json::json!([]),
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
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let schema = Arc::new(SchemaModel::parse(LINKS).unwrap());
        // Each plan on a session of its own: a session answers a read at a commit it has
        // answered before from what it kept.
        let plan = |cx: &mut TestAppContext| {
            let server = ServerEntry {
                name: "dev".into(),
                url: "http://127.0.0.1:8787".into(),
                source: ServerSource::Environment,
                known_graphs: Vec::new(),
            };
            let http = http.clone();
            let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
            let session =
                cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
            cx.run_until_parked();
            let plan = cx.update(|cx| {
                plan_node_delete(
                    session,
                    schema.clone(),
                    "Link".into(),
                    vec!["l0".into()],
                    Some("c1".into()),
                    cx,
                )
            });
            cx.run_until_parked();
            futures::executor::block_on(plan)
        };

        let refused = plan(cx).unwrap_err();
        assert_eq!(refused, PlanError::TooDeep);
        assert_eq!(rounds.swap(0, Ordering::SeqCst), MAX_ROUNDS);
        let message = refused.to_string();
        assert!(message.contains("at least 32 steps deep"), "{message}");
        assert!(!message.contains("1,000"), "{message}");

        depth.store(31, Ordering::SeqCst);
        let planned = plan(cx).unwrap();
        assert_eq!(planned.dependents.len(), 31);
        assert_eq!(
            rounds.load(Ordering::SeqCst),
            32,
            "the last round found none"
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

    const TAGS: &str = "node Tag {\n  slug: String @key\n}\n\
        node Doc {\n  slug: String @key\n}\n\
        node Note {\n  body: String\n}\n\
        edge Tagged: Doc -> Tag\n\
        edge Broader: Tag -> Tag\n\
        edge Cites: Doc -> Doc\n";

    /// A relationship type is counted from each end that loses nodes, less what both ends
    /// lose, so a relationship between two deleted nodes, or from one to itself, counts once;
    /// a type no deleted node takes part in is not read. Every count type-checks.
    #[test]
    fn relationships_between_deleted_nodes_count_once() {
        let schema = SchemaModel::parse(TAGS).unwrap();
        let tags = [("Tag".to_owned(), vec!["t1".to_owned(), "t2".to_owned()])];
        let counts = removal_counts(&schema, &tags).unwrap();
        let shape: Vec<(i64, bool, bool)> = counts
            .iter()
            .map(|(sign, query)| {
                assert!(!query.text.contains("$e:cites"), "{}", query.text);
                let check = gq_language::check(&schema, &query.text);
                assert!(check.diagnostics.is_empty(), "{:?}", check.diagnostics);
                (
                    *sign,
                    query.text.contains("$a.slug in"),
                    query.text.contains("$b.slug in"),
                )
            })
            .collect();
        // Broader from, to, and less both; Tagged by its target.
        assert_eq!(
            shape,
            [
                (1, true, false),
                (1, false, true),
                (-1, true, true),
                (1, false, true)
            ]
        );
        assert!(counts[3].1.text.contains("$e:tagged"));
        // Docs too: Tagged and Cites are read from their sources as well.
        let both = [tags[0].clone(), ("Doc".to_owned(), vec!["d1".to_owned()])];
        let counts = removal_counts(&schema, &both).unwrap();
        let signs: Vec<i64> = counts.iter().map(|(sign, _)| *sign).collect();
        assert_eq!(signs, [1, 1, -1, 1, 1, -1, 1, 1, -1]);
        // A node type without a key has nothing to count its relationships by.
        let unkeyed = SchemaModel::parse(
            "node Note {\n  body: String\n}\nnode Tag {\n  slug: String @key\n}\n\
             edge About: Note -> Tag\n",
        )
        .unwrap();
        assert!(
            removal_counts(&unkeyed, &[("Note".to_owned(), vec!["n1".to_owned()])])
                .unwrap_err()
                .contains("Note has no key")
        );
    }

    /// A delete that removes more than Undo puts back in one commit, a tag on 25,000 documents,
    /// is planned without Undo, so its dialog asks for the key to be typed; the same delete of
    /// a tag on ten offers Undo. Each relationship count is held at the commit the rows were
    /// shown at, and a delete the schema already rules Undo out for counts nothing.
    #[gpui::test]
    async fn a_delete_past_what_undo_puts_back_is_planned_without_undo(cx: &mut TestAppContext) {
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
                    serde_json::json!({ "schema_source": TAGS })
                } else if path.ends_with("/query") {
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    log.lock().unwrap().push(request.clone());
                    let text = request["query"].as_str().unwrap_or_default();
                    let hub = request["params"].to_string().contains("hub");
                    let (from, to) = (text.contains("$a.slug in"), text.contains("$b.slug in"));
                    let n = match (text.contains("$e:tagged"), from, to) {
                        (true, ..) if hub => 25_000,
                        (true, ..) => 10,
                        (false, true, true) => 1,
                        (false, true, false) => 3,
                        (false, false, true) => 2,
                        _ => 0,
                    };
                    let rows = if request["name"] == "studio_edge_count" {
                        serde_json::json!([{ "n": n }])
                    } else {
                        serde_json::json!([])
                    };
                    serde_json::json!({
                        "query_name": "q",
                        "target": {"branch": null, "snapshot": "c1"},
                        "row_count": rows.as_array().unwrap().len(),
                        "columns": ["n"],
                        "rows": rows,
                        "graph_commit_id": "c1",
                    })
                } else {
                    serde_json::json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
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
        let schema = Arc::new(SchemaModel::parse(TAGS).unwrap());
        let plan = |type_name: &str, id: &str| {
            let plan = cx.update(|cx| {
                plan_node_delete(
                    session.clone(),
                    schema.clone(),
                    type_name.into(),
                    vec![id.into()],
                    Some("c1".into()),
                    cx,
                )
            });
            cx.run_until_parked();
            futures::executor::block_on(plan).unwrap()
        };
        let counts = |seen: &Mutex<Vec<Value>>| -> Vec<Value> {
            let counts = seen
                .lock()
                .unwrap()
                .iter()
                .filter(|read| read["name"] == "studio_edge_count")
                .cloned()
                .collect();
            seen.lock().unwrap().clear();
            counts
        };

        let hub = plan("Tag", "hub");
        // The tag, its 25,000 documents, and Broader's 3 + 2 - 1.
        let reason = hub.undo_unavailable.expect("Undo is not offered");
        assert!(
            reason.contains("25,005") && reason.contains("20,000"),
            "{reason}"
        );
        let reads = counts(&seen);
        assert_eq!(reads.len(), 4, "{reads:#?}");
        assert!(
            reads.iter().all(|read| read["snapshot"] == "c1"
                || (read["branch"] == "main" && read.get("snapshot").is_none())),
            "{reads:#?}"
        );

        let small = plan("Tag", "rare");
        assert_eq!(small.undo_unavailable, None);
        assert_eq!(counts(&seen).len(), 4);

        let note = plan("Note", "n1");
        assert!(note.undo_unavailable.unwrap().contains("no key"));
        assert!(counts(&seen).is_empty(), "nothing counted");
    }
}
