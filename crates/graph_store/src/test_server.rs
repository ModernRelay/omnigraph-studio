//! A fake server for this crate's tests whose branch another writer commits to: the head probe
//! answers the newest commit, and the change feed the commits after its `start`, with only the
//! changes to the types it names.

use std::sync::{Arc, Mutex};

use gpui::{AppContext as _, Entity, TestAppContext};
use http_client::http::Response;
use http_client::{AsyncBody, FakeHttpClient};
use serde_json::{Value, json};

use crate::connection::{Connection, ServerEntry, ServerSource};
use crate::session::GraphSession;

/// The rows a read other than the head probe answers with, from its request body.
pub(crate) type Rows = Box<dyn Fn(&Value) -> Value + Send>;

pub(crate) struct Served {
    pub head: String,
    pub schema: String,
    /// The schema read fails, as a server that cannot answer it.
    pub schema_fails: bool,
    /// Each commit with its parent and changes, oldest first.
    pub commits: Vec<(String, String, Vec<Value>)>,
    /// The query string of every feed read.
    pub feeds: Vec<String>,
    rows: Rows,
}

impl Served {
    /// Commits `id` on the head with `changes`.
    pub fn commit(&mut self, id: &str, changes: Vec<Value>) {
        let parent = std::mem::replace(&mut self.head, id.to_owned());
        self.commits.push((id.to_owned(), parent, changes));
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
pub(crate) fn node_change(ty: &str, id: &str) -> Value {
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
pub(crate) fn followed_session(
    schema: &str,
    rows: impl Fn(&Value) -> Value + Send + 'static,
    cx: &mut TestAppContext,
) -> (Entity<GraphSession>, Arc<Mutex<Served>>) {
    use futures::AsyncReadExt as _;
    let served = Arc::new(Mutex::new(Served {
        head: "c1".into(),
        schema: schema.into(),
        schema_fails: false,
        commits: Vec::new(),
        feeds: Vec::new(),
        rows: Box::new(rows),
    }));
    let state = served.clone();
    let http = FakeHttpClient::create(move |request| {
        let path = request.uri().path().to_owned();
        let query = request.uri().query().unwrap_or_default().to_owned();
        let state = state.clone();
        async move {
            let mut bytes = Vec::new();
            request.into_body().read_to_end(&mut bytes).await.ok();
            let mut served = state.lock().unwrap();
            if path.ends_with("/schema") && served.schema_fails {
                return Ok(Response::builder()
                    .status(500)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
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
            } else {
                json!({ "branches": ["main"] })
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
    let session = cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
    (session, served)
}
