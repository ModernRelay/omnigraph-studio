//! A fake server for tests whose branch another writer commits to: the head probe answers the
//! newest commit, or is refused when it matches a type the schema no longer declares, and the
//! change feed the commits after its `start`, with only the changes to the types it names.
//! Other crates' tests reach it through the `test-support` feature.

use std::sync::{Arc, Mutex};

use gpui::{AppContext as _, Entity, TestAppContext};
use http_client::http::{Method, Response};
use http_client::{AsyncBody, FakeHttpClient};
use serde_json::{Value, json};

use crate::connection::{Connection, ServerEntry, ServerSource};
use crate::session::GraphSession;

/// The rows a read other than the head probe answers with, from its request body.
pub type Rows = Box<dyn Fn(&Value) -> Value + Send>;

/// Which reads, by their request body, are never answered, as a slow server's are not
/// before their view stops waiting.
pub type Hold = Box<dyn Fn(&Value) -> bool + Send>;

pub struct Served {
    pub head: String,
    pub schema: String,
    /// The schema read fails, as a server that cannot answer it.
    pub schema_fails: bool,
    /// How many times the schema was asked for.
    pub schema_reads: usize,
    /// How many times a branch's storage snapshot was asked for.
    pub snapshot_reads: usize,
    /// Each commit with its parent and changes, oldest first.
    pub commits: Vec<(String, String, Vec<Value>)>,
    /// The query string of every feed read.
    pub feeds: Vec<String>,
    /// The branch list, and what every other route but a branch delete answers: `main` alone
    /// at first.
    pub branches: Vec<String>,
    /// Reads never answered: none at first.
    pub hold: Hold,
    rows: Rows,
}

impl Served {
    /// Commits `id` on the head with `changes`.
    pub fn commit(&mut self, id: &str, changes: Vec<Value>) {
        let parent = std::mem::replace(&mut self.head, id.to_owned());
        self.commits.push((id.to_owned(), parent, changes));
    }

    /// Whether the served schema declares the node type head probe `text` matches: a server
    /// refuses a probe of a type renamed or dropped since the probe was built.
    fn probes_a_declared_type(&self, text: &str) -> bool {
        let probed = text
            .split("$n: ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next());
        match (probed, gq_language::SchemaModel::parse(&self.schema)) {
            (Some(ty), Ok(schema)) => schema.node(ty).is_some(),
            _ => true,
        }
    }

    fn feed(&mut self, query: &str) -> Value {
        self.feeds.push(query.to_owned());
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
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
        let start = self
            .commits
            .iter()
            .position(|(commit, ..)| commit == after)
            .map_or(0, |at| at + 1);
        let blocks: Vec<Value> = self.commits[start..]
            .iter()
            .map(|(commit, parent, changes)| {
                let changes: Vec<&Value> = changes
                    .iter()
                    .filter(|change| {
                        types.is_empty()
                            || types.contains(&change["type"]["name"].as_str().unwrap())
                    })
                    .collect();
                json!({
                    "cause": {
                        "graph_commit_id": commit,
                        "parent_commit_id": parent,
                        "authored_branch": "main",
                        "authored_at": 0,
                    },
                    "changes": changes,
                })
            })
            .collect();
        json!({ "blocks": blocks, "cursor": "cursor", "caught_up": true })
    }
}

/// One update to a node, as the change feed reports it.
pub fn node_change(ty: &str, id: &str) -> Value {
    json!({
        "kind": "node",
        "type": { "id": ty, "name": ty },
        "id": id,
        "op": "update",
        "before": { "properties": {} },
        "after": { "properties": {} },
    })
}

/// A session on a server serving `schema` at head `c1`; reads other than the head probe are
/// answered at the head with the rows `rows` gives.
pub fn followed_session(
    schema: &str,
    rows: impl Fn(&Value) -> Value + Send + 'static,
    cx: &mut TestAppContext,
) -> (Entity<GraphSession>, Arc<Mutex<Served>>) {
    use futures::AsyncReadExt as _;
    let served = Arc::new(Mutex::new(Served {
        head: "c1".into(),
        schema: schema.into(),
        schema_fails: false,
        schema_reads: 0,
        snapshot_reads: 0,
        commits: Vec::new(),
        feeds: Vec::new(),
        branches: vec!["main".into()],
        hold: Box::new(|_| false),
        rows: Box::new(rows),
    }));
    let state = served.clone();
    let http = FakeHttpClient::create(move |request| {
        let path = request.uri().path().to_owned();
        let query = request.uri().query().unwrap_or_default().to_owned();
        let deletes = request.method() == Method::DELETE;
        let state = state.clone();
        async move {
            let mut bytes = Vec::new();
            request.into_body().read_to_end(&mut bytes).await.ok();
            if path.ends_with("/query") {
                let asked: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let held = (state.lock().unwrap().hold)(&asked);
                if held {
                    futures::future::pending::<()>().await;
                }
            }
            let mut served = state.lock().unwrap();
            if path.ends_with("/snapshot") {
                served.snapshot_reads += 1;
            }
            if path.ends_with("/schema") {
                served.schema_reads += 1;
            }
            if path.ends_with("/schema") && served.schema_fails {
                return Ok(Response::builder()
                    .status(500)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(
                        r#"{"error":"the schema could not be read"}"#,
                    ))
                    .unwrap());
            }
            let body = if path.ends_with("/schema") {
                json!({ "schema_source": served.schema })
            } else if path.ends_with("/changes") {
                served.feed(&query)
            } else if path.ends_with("/query") {
                let asked: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                if asked["name"] == "studio_head"
                    && !served.probes_a_declared_type(asked["query"].as_str().unwrap_or_default())
                {
                    return Ok(Response::builder()
                        .status(400)
                        .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                        .body(AsyncBody::from(
                            r#"{"error":"unknown node type","code":"bad_request"}"#,
                        ))
                        .unwrap());
                }
                let rows = if asked["name"] == "studio_head" {
                    json!([])
                } else {
                    (served.rows)(&asked)
                };
                json!({
                    "query_name": asked["name"],
                    "target": { "branch": "main" },
                    "row_count": rows.as_array().map_or(0, Vec::len),
                    "columns": [],
                    "rows": rows,
                    "graph_commit_id": served.head,
                })
            } else if deletes && let Some((_, branch)) = path.split_once("/branches/") {
                // A branch delete's receipt, as the server answers it.
                json!({ "uri": "memory://knowledge", "name": branch, "actor_id": null })
            } else {
                json!({ "branches": served.branches })
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
    let session = cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
    (session, served)
}
