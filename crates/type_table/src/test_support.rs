//! The fake server this crate's tests read from: a schema, two people on every read, and one
//! answer for every write.

use std::sync::Arc;

use gpui::{AppContext as _, Entity, TestAppContext};
use graph_store::{Connection, GraphSession, ServerEntry, ServerSource};
use http_client::{AsyncBody, FakeHttpClient, Response};
use serde_json::{Value, json};

pub(crate) const PEOPLE: &str = "node Person {\n  slug: String @key\n  name: String\n}\n";

/// A server with `schema` and two people read at commit `c1`; a write commits `c2` on it.
pub(crate) fn session(schema: &str, cx: &mut TestAppContext) -> Entity<GraphSession> {
    session_answering(schema, 200, committed("c2", "c1"), cx)
}

/// What a write that changed one node and committed `commit` on `parent` answers.
pub(crate) fn committed(commit: &str, parent: &str) -> Value {
    json!({
        "branch": "main",
        "query_name": "q",
        "affected_nodes": 1,
        "affected_edges": 0,
        "commit": {
            "graph_commit_id": commit,
            "graph_manifest_version": 2,
            "created_at": 0,
            "parent_commit_id": parent,
        },
    })
}

/// The two people every read answers with.
fn people() -> Value {
    json!([
        {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
        {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
    ])
}

/// As [`session`], with every write answered by `write_status` and `write_body`.
pub(crate) fn session_answering(
    schema: &str,
    write_status: u16,
    write_body: Value,
    cx: &mut TestAppContext,
) -> Entity<GraphSession> {
    serve(schema, people(), write_status, write_body, cx)
}

/// As [`session`], every read answering with `rows` (each `{"n": {…}}`).
pub(crate) fn session_reading(
    schema: &str,
    rows: Value,
    cx: &mut TestAppContext,
) -> Entity<GraphSession> {
    serve(schema, rows, 200, committed("c2", "c1"), cx)
}

fn serve(
    schema: &str,
    rows: Value,
    write_status: u16,
    write_body: Value,
    cx: &mut TestAppContext,
) -> Entity<GraphSession> {
    let schema: Arc<str> = schema.into();
    let rows: Arc<Value> = Arc::new(rows);
    let write_body: Arc<Value> = Arc::new(write_body);
    let http = FakeHttpClient::create(move |request| {
        let (schema, rows, write_body) = (schema.clone(), rows.clone(), write_body.clone());
        async move {
            let path = request.uri().path().to_owned();
            let (status, body) = if path.ends_with("/schema") {
                (200, json!({ "schema_source": &*schema }))
            } else if path.ends_with("/query") {
                (
                    200,
                    json!({
                        "query_name": "q",
                        "target": {"branch": "main", "snapshot": null},
                        "row_count": rows.as_array().map_or(0, Vec::len),
                        "columns": ["n"],
                        "rows": &*rows,
                        "graph_commit_id": "c1",
                    }),
                )
            } else if path.contains("/mutate") {
                (write_status, (*write_body).clone())
            } else {
                (200, json!({ "branches": ["main"] }))
            };
            Ok(Response::builder()
                .status(status)
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
    cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx))
}
