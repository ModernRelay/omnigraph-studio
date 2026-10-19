//! The fake server this crate's tests read from: a schema, two people on every read, and one
//! answer for every write.

use std::sync::{Arc, Mutex};

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

/// As [`session`], each read answered with the rows `answer` gives for its request body
/// (`query`, `name`, `params`, …); every read's query name is recorded, in order.
pub(crate) fn session_routing(
    schema: &str,
    answer: impl Fn(&Value) -> Value + Send + Sync + 'static,
    cx: &mut TestAppContext,
) -> (Entity<GraphSession>, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    let rows: Rows = Arc::new(move |request: &Value| {
        let name = request["name"].as_str().unwrap_or_default().to_owned();
        seen.lock().unwrap().push(name);
        answer(request)
    });
    let session = serve_with(schema, rows, always(200, committed("c2", "c1")), None, cx);
    (session, asked)
}

/// Another writer's commits, which the fake server's head and change feed follow: the head
/// probe answers the newest, and the feed the commits after its `start`, with only the changes
/// to the types it names.
#[derive(Default)]
pub(crate) struct Writer {
    commits: Mutex<Vec<(String, String, Vec<Value>)>>,
    /// The schema the server answers with once the writer changed it.
    schema: Mutex<Option<String>>,
    /// The query string of every change-feed read.
    pub(crate) feeds: Mutex<Vec<String>>,
}

impl Writer {
    /// Changes the schema the server answers with.
    pub(crate) fn set_schema(&self, schema: &str) {
        *self.schema.lock().unwrap() = Some(schema.to_owned());
    }

    /// Commits `commit` on the newest one (or `c1`) with `changes`.
    pub(crate) fn commit(&self, commit: &str, changes: Vec<Value>) {
        let mut commits = self.commits.lock().unwrap();
        let parent = commits
            .last()
            .map_or("c1".to_owned(), |(id, ..)| id.clone());
        commits.push((commit.to_owned(), parent, changes));
    }

    fn head(&self) -> Option<String> {
        self.commits
            .lock()
            .unwrap()
            .last()
            .map(|(id, ..)| id.clone())
    }

    fn feed(&self, query: &str) -> Value {
        self.feeds.lock().unwrap().push(query.to_owned());
        // Type names and commit ids need no decoding past the `:` of `after:`.
        let pairs: Vec<(String, String)> = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.replace("%3A", ":")))
            .collect();
        let types: Vec<&str> = pairs
            .iter()
            .filter(|(key, _)| key == "type")
            .map(|(_, value)| value.as_str())
            .collect();
        let after = pairs
            .iter()
            .find(|(key, _)| key == "start")
            .and_then(|(_, value)| value.strip_prefix("after:"))
            .unwrap_or_default();
        let commits = self.commits.lock().unwrap();
        let start = commits
            .iter()
            .position(|(id, ..)| id == after)
            .map_or(0, |at| at + 1);
        let blocks: Vec<Value> = commits[start..]
            .iter()
            .map(|(id, parent, changes)| {
                let changes: Vec<&Value> = changes
                    .iter()
                    .filter(|change| {
                        types.is_empty() || types.contains(&change["type"]["name"].as_str().unwrap())
                    })
                    .collect();
                json!({
                    "cause": { "graph_commit_id": id, "parent_commit_id": parent, "authored_branch": "main", "authored_at": 0 },
                    "changes": changes,
                })
            })
            .collect();
        json!({ "blocks": blocks, "cursor": "cursor", "caught_up": true })
    }
}

/// One change to a node or an edge (with its ends), as the change feed reports it.
pub(crate) fn changed(kind: &str, ty: &str, id: &str, ends: Option<(&str, &str)>) -> Value {
    let mut image = json!({ "properties": {} });
    if let Some((from, to)) = ends {
        image["endpoints"] = json!({ "from": from, "to": to });
    }
    json!({ "kind": kind, "type": { "id": ty, "name": ty }, "id": id, "op": "update", "before": image, "after": image })
}

/// As [`session_routing`], with another writer committing on the branch.
pub(crate) fn session_followed(
    schema: &str,
    answer: impl Fn(&Value) -> Value + Send + Sync + 'static,
    cx: &mut TestAppContext,
) -> (Entity<GraphSession>, Arc<Writer>) {
    let writer = Arc::new(Writer::default());
    let rows: Rows = Arc::new(answer);
    let session = serve_with(
        schema,
        rows,
        always(200, committed("c2", "c1")),
        Some(writer.clone()),
        cx,
    );
    (session, writer)
}

/// The rows a read answers with, from its request body.
type Rows = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

/// How the next write, its request body given, is answered: its status and body.
type Writes = Arc<dyn Fn(&Value) -> (u16, Value) + Send + Sync>;

fn always(status: u16, body: Value) -> Writes {
    Arc::new(move |_| (status, body.clone()))
}

/// As [`session`], every write's request body recorded, in order, with `if_graph_commit` added
/// for a write bound to a commit.
pub(crate) fn session_recording_writes(
    schema: &str,
    cx: &mut TestAppContext,
) -> (Entity<GraphSession>, Arc<Mutex<Vec<Value>>>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let log = sent.clone();
    let writes: Writes = Arc::new(move |request: &Value| {
        log.lock().unwrap().push(request.clone());
        (200, committed("c2", "c1"))
    });
    let rows = Arc::new(people());
    let session = serve_with(
        schema,
        Arc::new(move |_: &Value| (*rows).clone()),
        writes,
        None,
        cx,
    );
    (session, sent)
}

/// As [`session`], the writes answered in turn by `answers`; any after the last get the last.
pub(crate) fn session_writing(
    schema: &str,
    answers: Vec<(u16, Value)>,
    cx: &mut TestAppContext,
) -> Entity<GraphSession> {
    let answers = Mutex::new(
        answers
            .into_iter()
            .collect::<std::collections::VecDeque<_>>(),
    );
    let writes: Writes = Arc::new(move |_| {
        let mut answers = answers.lock().unwrap();
        match answers.len() {
            0 => (500, json!({})),
            1 => answers[0].clone(),
            _ => answers.pop_front().unwrap(),
        }
    });
    let rows = Arc::new(people());
    serve_with(
        schema,
        Arc::new(move |_: &Value| (*rows).clone()),
        writes,
        None,
        cx,
    )
}

fn serve(
    schema: &str,
    rows: Value,
    write_status: u16,
    write_body: Value,
    cx: &mut TestAppContext,
) -> Entity<GraphSession> {
    let rows = Arc::new(rows);
    serve_with(
        schema,
        Arc::new(move |_: &Value| (*rows).clone()),
        always(write_status, write_body),
        None,
        cx,
    )
}

fn serve_with(
    schema: &str,
    rows: Rows,
    writes: Writes,
    writer: Option<Arc<Writer>>,
    cx: &mut TestAppContext,
) -> Entity<GraphSession> {
    use futures::AsyncReadExt as _;
    let schema: Arc<str> = schema.into();
    let http = FakeHttpClient::create(move |request| {
        let (schema, rows, writes) = (schema.clone(), rows.clone(), writes.clone());
        let writer = writer.clone();
        async move {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let (status, body) = if path.ends_with("/schema") {
                let changed = writer
                    .as_ref()
                    .and_then(|writer| writer.schema.lock().unwrap().clone());
                (
                    200,
                    json!({ "schema_source": changed.unwrap_or_else(|| schema.to_string()) }),
                )
            } else if path.ends_with("/query") {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let asked: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let rows = rows(&asked);
                // An answer `{"refuse": message}` is the server refusing the read.
                if let Some(message) = rows.get("refuse").and_then(Value::as_str) {
                    return Ok(Response::builder()
                        .status(400)
                        .header(graph_client::CONTRACT_HEADER, "0.13")
                        .body(AsyncBody::from(
                            json!({ "error": message, "code": "bad_request" }).to_string(),
                        ))
                        .unwrap());
                }
                // A read at a commit is answered there; a read of the branch at the other
                // writer's newest commit, or c1.
                let at = asked["snapshot"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| writer.as_ref().and_then(|writer| writer.head()))
                    .unwrap_or_else(|| "c1".to_owned());
                (
                    200,
                    json!({
                        "query_name": "q",
                        "target": {"branch": "main", "snapshot": null},
                        "row_count": rows.as_array().map_or(0, Vec::len),
                        "columns": ["n"],
                        "rows": rows,
                        "graph_commit_id": at,
                    }),
                )
            } else if let Some(writer) = writer.as_ref().filter(|_| path.ends_with("/changes")) {
                (200, writer.feed(&query))
            } else if path.contains("/mutate") {
                // A write bound to a commit carries it in the body the writes see.
                let bound = request
                    .headers()
                    .get(graph_client::IF_GRAPH_COMMIT_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let mut body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                if let (Some(bound), Some(body)) = (bound, body.as_object_mut()) {
                    body.insert("if_graph_commit".into(), Value::String(bound));
                }
                writes(&body)
            } else {
                // A second branch, for views that follow the window to another one.
                (200, json!({ "branches": ["main", "fix"] }))
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
