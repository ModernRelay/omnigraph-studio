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
            "graph_manifest_version": 2, "kind": "published",
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

/// As [`session_followed`], every write matching nothing, as one bound to a value the other
/// writer changed does: it commits nothing.
pub(crate) fn session_routing_unmatched(
    schema: &str,
    answer: impl Fn(&Value) -> Value + Send + Sync + 'static,
    cx: &mut TestAppContext,
) -> (Entity<GraphSession>, Arc<Writer>) {
    let unmatched = json!({
        "branch": "main",
        "query_name": "q",
        "affected_nodes": 0,
        "affected_edges": 0,
        "commit": null,
    });
    let writer = Arc::new(Writer::default());
    let session = serve_with(
        schema,
        Arc::new(answer),
        always(200, unmatched),
        Some(writer.clone()),
        cx,
    );
    (session, writer)
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

    /// The `__manifest` version holding the newest commit: `c1`'s is 1, and each commit
    /// publishes the next.
    fn version(&self) -> u64 {
        1 + self.commits.lock().unwrap().len() as u64
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

/// What a fake server is sent and answers in steps: every read's request body in order, as
/// the rows see it (with `at`, the commit it is answered at), reads held until a test lets them
/// go, and the row counts `/snapshot` answers with. A read of a branch's commits (`/commits`)
/// is noted as `{"route": "commits", "query": …}`, and answers the branch's newest commit.
#[derive(Default)]
pub(crate) struct Served {
    reads: Mutex<Vec<Value>>,
    holds: Mutex<Vec<Hold>>,
    /// Node types' row counts by name; `/snapshot` answers as before while there are none.
    counts: Mutex<Vec<(String, i64)>>,
}

/// The first read `which` matches waits until `until` is sent or dropped.
struct Hold {
    which: Box<dyn Fn(&Value) -> bool + Send>,
    until: futures::channel::oneshot::Receiver<()>,
}

impl Served {
    /// Holds the next read `which` matches until the sender returned sends, or is dropped.
    pub(crate) fn hold(
        &self,
        which: impl Fn(&Value) -> bool + Send + 'static,
    ) -> futures::channel::oneshot::Sender<()> {
        let (send, until) = futures::channel::oneshot::channel();
        self.holds.lock().unwrap().push(Hold {
            which: Box::new(which),
            until,
        });
        send
    }

    /// The reads named `name` sent so far, in order.
    pub(crate) fn sent(&self, name: &str) -> Vec<Value> {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|read| read["name"] == name)
            .cloned()
            .collect()
    }

    /// The reads of a branch's commits sent so far, in order: each one's query string.
    pub(crate) fn commit_pages(&self) -> Vec<String> {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|read| read["route"] == "commits")
            .map(|read| read["query"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// Has the snapshot count `count` rows of node type `type_name`, at every branch's head.
    pub(crate) fn count_nodes(&self, type_name: &str, count: i64) {
        let mut counts = self.counts.lock().unwrap();
        counts.retain(|(name, _)| name != type_name);
        counts.push((type_name.to_owned(), count));
    }

    /// Notes a read, and what it waits for before it is answered.
    fn take(&self, read: &Value) -> Option<futures::channel::oneshot::Receiver<()>> {
        self.reads.lock().unwrap().push(read.clone());
        let mut holds = self.holds.lock().unwrap();
        let at = holds.iter().position(|hold| (hold.which)(read))?;
        Some(holds.remove(at).until)
    }

    /// What `/snapshot` answers, once a count is set: the counts at manifest `version`.
    fn snapshot(&self, branch: &str, version: u64) -> Option<Value> {
        let counts = self.counts.lock().unwrap();
        if counts.is_empty() {
            return None;
        }
        let datasets: Vec<Value> = counts
            .iter()
            .map(|(name, count)| {
                json!({
                    "entity_kind": "node", "type_name": name, "dataset_path": format!("nodes/{name}"),
                    "published_dataset_version": 1, "entity_count": count,
                })
            })
            .collect();
        Some(json!({
            "graph_branch": branch, "graph_manifest_version": version, "internal_schema_version": 16,
            "datasets": datasets,
        }))
    }
}

/// As [`session_followed`], with what the server is sent and answers in steps in a [`Served`].
pub(crate) fn session_served(
    schema: &str,
    answer: impl Fn(&Value) -> Value + Send + Sync + 'static,
    cx: &mut TestAppContext,
) -> (Entity<GraphSession>, Arc<Writer>, Arc<Served>) {
    let writer = Arc::new(Writer::default());
    let served = Arc::new(Served::default());
    let routes = Routes {
        served: Some(served.clone()),
        ..Routes::default()
    };
    let session = serve_blobs(
        schema,
        Arc::new(answer),
        always(200, committed("c2", "c1")),
        Some(writer.clone()),
        routes,
        cx,
    );
    (session, writer, served)
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

/// Rows answered as whole nodes (`{"n": {…}}`) shaped as a read of their fields gives them:
/// `id` and each property in its field column. Rows already shaped so stay as they are.
fn as_fields(rows: Value) -> Value {
    match rows {
        Value::Array(rows) => Value::Array(rows.into_iter().map(as_field_row).collect()),
        rows => rows,
    }
}

/// One row as [`as_fields`] shapes it.
fn as_field_row(row: Value) -> Value {
    let fields = row.get("n").and_then(Value::as_object).map(|node| {
        node.iter()
            .map(|(property, value)| {
                let column = match property.as_str() {
                    "@id" => "id".to_owned(),
                    _ => gq_language::builder::field_column(property),
                };
                (column, value.clone())
            })
            .collect::<serde_json::Map<String, Value>>()
    });
    fields.map_or(row, Value::Object)
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

/// As [`session`], with Blob cells the server holds and writes: their values, and every Blob
/// write sent, in order.
pub(crate) fn session_with_blobs(
    schema: &str,
    cx: &mut TestAppContext,
) -> (Entity<GraphSession>, Arc<Blobs>) {
    let blobs = Arc::new(Blobs::default());
    let rows = Arc::new(people());
    let routes = Routes {
        blobs: blobs.clone(),
        ..Routes::default()
    };
    let session = serve_blobs(
        schema,
        Arc::new(move |_: &Value| (*rows).clone()),
        always(200, committed("c2", "c1")),
        None,
        routes,
        cx,
    );
    (session, blobs)
}

/// What the access route answers for a credential that reads every branch and changes none.
pub(crate) fn read_only_access() -> Value {
    json!({ "graph_id": "knowledge", "actor": "act-reader", "graph_wide": ["read"], "actions": ["read"] })
}

/// As [`session_with_blobs`], the access route answering `access` for every branch (the other
/// sessions' routes answer nothing it can read, so what the credential may do stays unknown),
/// and every write's request body recorded, in order.
pub(crate) fn session_with_access(
    schema: &str,
    access: Value,
    cx: &mut TestAppContext,
) -> (Entity<GraphSession>, Arc<Mutex<Vec<Value>>>, Arc<Blobs>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let log = sent.clone();
    let writes: Writes = Arc::new(move |request: &Value| {
        log.lock().unwrap().push(request.clone());
        (200, committed("c2", "c1"))
    });
    let blobs = Arc::new(Blobs::default());
    let rows = Arc::new(people());
    let routes = Routes {
        blobs: blobs.clone(),
        access: Some(access),
        served: None,
    };
    let session = serve_blobs(
        schema,
        Arc::new(move |_: &Value| (*rows).clone()),
        writes,
        None,
        routes,
        cx,
    );
    (session, sent, blobs)
}

/// A file of `bytes` named `name` in a folder of this test process's own, for an upload to
/// read; `len` makes it that long instead, sparse, without writing its bytes.
pub(crate) fn upload_file(name: &str, bytes: &[u8], len: Option<u64>) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("studio-blob-uploads-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    if let Some(len) = len {
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len)
            .unwrap();
    }
    path
}

/// One Blob write the fake server was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlobWriteSent {
    pub method: String,
    /// `type/id/property`.
    pub cell: String,
    pub branch: String,
    pub if_match: Option<String>,
    pub bytes: usize,
}

/// Blob cells by `type/id/property`, each with its bytes and ETag.
type BlobCells = std::collections::HashMap<String, (Vec<u8>, String)>;

/// The fake server's Blob cells by `type/id/property`, each with its bytes and ETag; a cell
/// absent is null. It keeps no history: a read at any commit answers what a cell holds now,
/// so a view's read at its commit is what the session's memo kept of it. A write that lands
/// publishes the next commit (`c2`, then `c3`, …) on the one before.
///
/// With [`Blobs::versioned`] set it answers as the server does: an ETag names the type's table
/// version, so a change to any cell of a type gives every cell of it a new ETag, and a read at
/// a commit a write published answers what the cells held there.
#[derive(Default)]
pub(crate) struct Blobs {
    cells: Mutex<BlobCells>,
    pub(crate) versioned: std::sync::atomic::AtomicBool,
    /// The cells as each commit a write published left them, by commit, while `versioned`.
    history: Mutex<std::collections::HashMap<String, BlobCells>>,
    pub(crate) writes: Mutex<Vec<BlobWriteSent>>,
    /// Writes are applied, but their answers lost on the way back: a gateway's 504 comes back
    /// instead, without the server's contract header.
    pub(crate) lose_answers: std::sync::atomic::AtomicBool,
    /// How many commits Blob writes published.
    published: Mutex<u64>,
    tags: std::sync::atomic::AtomicU64,
    /// A gate the next Blob write waits at before it is answered, once one is set.
    pub(crate) hold: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
}

impl Blobs {
    /// Sets a cell's value, as another writer would: it gets an ETag of its own, and while
    /// `versioned` so does every other cell of its type.
    pub(crate) fn set(&self, cell: &str, bytes: &[u8]) -> String {
        let etag = self.next_tag();
        let mut cells = self.cells.lock().unwrap();
        cells.insert(cell.to_owned(), (bytes.to_vec(), etag.clone()));
        self.retag_type_of(cell, &mut cells);
        etag
    }

    fn next_tag(&self) -> String {
        let n = self.tags.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        format!("\"e{n}\"")
    }

    /// While `versioned`, gives every cell of `cell`'s type but `cell` a new ETag: the table
    /// version they name moved.
    fn retag_type_of(&self, cell: &str, cells: &mut BlobCells) {
        if !self.versioned.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let of_type = |other: &str| other.split('/').next() == cell.split('/').next();
        let others: Vec<String> = cells
            .keys()
            .filter(|other| *other != cell && of_type(other))
            .cloned()
            .collect();
        for other in others {
            let etag = self.next_tag();
            if let Some(held) = cells.get_mut(&other) {
                held.1 = etag;
            }
        }
    }

    /// What a cell holds at `snapshot`, a commit a versioned write published, else now.
    fn get_at(&self, cell: &str, snapshot: &str) -> Option<(Vec<u8>, String)> {
        let history = self.history.lock().unwrap();
        match history.get(snapshot) {
            Some(cells) => cells.get(cell).cloned(),
            None => self.get(cell),
        }
    }

    /// What a cell holds: its bytes and ETag, or `None` for null.
    pub(crate) fn get(&self, cell: &str) -> Option<(Vec<u8>, String)> {
        self.cells.lock().unwrap().get(cell).cloned()
    }

    /// The answer to one request to `/blob`: its status, headers and body.
    fn answer(
        &self,
        method: &str,
        query: &str,
        range: Option<&str>,
        if_match: Option<&str>,
        body: Vec<u8>,
    ) -> (u16, Vec<(&'static str, String)>, Vec<u8>) {
        let param = |name: &str| {
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|(key, _)| *key == name)
                .map(|(_, value)| decoded(value))
                .unwrap_or_default()
        };
        let cell = format!("{}/{}/{}", param("type"), param("id"), param("property"));
        let held = match method {
            "HEAD" | "GET" => self.get_at(&cell, &param("snapshot")),
            _ => self.get(&cell),
        };
        match method {
            "HEAD" | "GET" => {
                let Some((bytes, etag)) = held else {
                    return (404, Vec::new(), Vec::new());
                };
                let length = bytes.len();
                let range = range
                    .and_then(|range| range.strip_prefix("bytes="))
                    .and_then(|range| range.split_once('-'))
                    .and_then(|(from, to)| {
                        Some((from.parse::<usize>().ok()?, to.parse::<usize>().ok()?))
                    });
                let mut headers = vec![("etag", etag)];
                match (method, range) {
                    ("HEAD", _) => {
                        headers.push(("content-length", length.to_string()));
                        (200, headers, Vec::new())
                    }
                    (_, Some((from, to))) => {
                        let to = to.min(length.saturating_sub(1));
                        headers.push(("content-range", format!("bytes {from}-{to}/{length}")));
                        (206, headers, bytes[from.min(length)..=to].to_vec())
                    }
                    _ => (200, headers, bytes),
                }
            }
            _ => {
                self.writes.lock().unwrap().push(BlobWriteSent {
                    method: method.to_owned(),
                    cell: cell.clone(),
                    branch: param("branch"),
                    if_match: if_match.map(str::to_owned),
                    bytes: body.len(),
                });
                let holds = match (if_match, &held) {
                    (None, _) => true,
                    (Some("*"), held) => held.is_some(),
                    (Some(tags), Some((_, etag))) => tags.split(',').any(|tag| tag.trim() == etag),
                    (Some(_), None) => false,
                };
                if !holds {
                    let failure = json!({
                        "error": format!("If-Match did not hold for {cell}"),
                        "blob_precondition_failure": { "current_etag": held.map(|(_, etag)| etag) },
                    });
                    return (412, Vec::new(), failure.to_string().into_bytes());
                }
                let (id, ty, property) = (param("id"), param("type"), param("property"));
                let selector =
                    json!({ "entity": "node", "type": ty, "id": id, "property": property });
                // A clear of a cell already null publishes nothing.
                if method == "DELETE" && held.is_none() {
                    let receipt = json!({ "selector": selector, "branch": param("branch"), "kind": "null", "commit": null });
                    return (200, Vec::new(), receipt.to_string().into_bytes());
                }
                let (kind, size, etag) = if method == "PUT" {
                    let size = body.len();
                    (json!("managed"), json!(size), json!(self.set(&cell, &body)))
                } else {
                    let mut cells = self.cells.lock().unwrap();
                    cells.remove(&cell);
                    self.retag_type_of(&cell, &mut cells);
                    (json!("null"), Value::Null, Value::Null)
                };
                let mut published = self.published.lock().unwrap();
                *published += 1;
                if self.versioned.load(std::sync::atomic::Ordering::SeqCst) {
                    let cells = self.cells.lock().unwrap().clone();
                    self.history
                        .lock()
                        .unwrap()
                        .insert(format!("c{}", *published + 1), cells);
                }
                let commit = json!({
                    "graph_commit_id": format!("c{}", *published + 1),
                    "graph_manifest_version": 2, "kind": "published",
                    "created_at": 0,
                    "parent_commit_id": format!("c{}", *published),
                });
                let receipt = json!({
                    "selector": selector, "branch": param("branch"), "kind": kind,
                    "size": size, "etag": etag, "commit": commit,
                });
                (200, Vec::new(), receipt.to_string().into_bytes())
            }
        }
    }
}

/// A query value with its `%XX` escapes read.
fn decoded(value: &str) -> String {
    let mut bytes = Vec::new();
    let mut rest = value.as_bytes();
    while let Some((&first, tail)) = rest.split_first() {
        match (first, tail) {
            (b'%', [high, low, tail @ ..]) => {
                let hex = std::str::from_utf8(&[*high, *low]).ok().map(str::to_owned);
                match hex.and_then(|hex| u8::from_str_radix(&hex, 16).ok()) {
                    Some(byte) => bytes.push(byte),
                    None => bytes.extend_from_slice(&[b'%', *high, *low]),
                }
                rest = tail;
            }
            (b'+', _) => {
                bytes.push(b' ');
                rest = tail;
            }
            _ => {
                bytes.push(first);
                rest = tail;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn serve_with(
    schema: &str,
    rows: Rows,
    writes: Writes,
    writer: Option<Arc<Writer>>,
    cx: &mut TestAppContext,
) -> Entity<GraphSession> {
    serve_blobs(schema, rows, writes, writer, Routes::default(), cx)
}

/// The fake server's routes past the schema, the reads and the writes: its Blob cells, what the
/// access route answers (nothing it can read when `None`), and what it is sent and answers in
/// steps ([`Served`]).
#[derive(Default)]
struct Routes {
    blobs: Arc<Blobs>,
    access: Option<Value>,
    served: Option<Arc<Served>>,
}

fn serve_blobs(
    schema: &str,
    rows: Rows,
    writes: Writes,
    writer: Option<Arc<Writer>>,
    routes: Routes,
    cx: &mut TestAppContext,
) -> Entity<GraphSession> {
    use futures::AsyncReadExt as _;
    let schema: Arc<str> = schema.into();
    let Routes {
        blobs,
        access,
        served,
    } = routes;
    let http = FakeHttpClient::create(move |request| {
        let (schema, rows, writes) = (schema.clone(), rows.clone(), writes.clone());
        let (writer, blobs, access) = (writer.clone(), blobs.clone(), access.clone());
        let served = served.clone();
        async move {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            if path.ends_with("/blob") {
                let header = |name: &str| {
                    request
                        .headers()
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned)
                };
                let (range, if_match) = (header("range"), header("if-match"));
                let method = request.method().to_string();
                let mut body = Vec::new();
                request.into_body().read_to_end(&mut body).await.ok();
                let write = !matches!(method.as_str(), "HEAD" | "GET");
                let gate = write.then(|| blobs.hold.lock().unwrap().take()).flatten();
                if let Some(gate) = gate {
                    gate.await.ok();
                }
                let (status, headers, body) =
                    blobs.answer(&method, &query, range.as_deref(), if_match.as_deref(), body);
                let lost = write && blobs.lose_answers.load(std::sync::atomic::Ordering::SeqCst);
                if lost {
                    return Ok(Response::builder()
                        .status(504)
                        .body(AsyncBody::from("gateway timeout"))
                        .unwrap());
                }
                let mut response = Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT);
                for (name, value) in headers {
                    response = response.header(name, value);
                }
                return Ok(response.body(AsyncBody::from(body)).unwrap());
            }
            let (status, body) = if path.ends_with("/schema") {
                let changed = writer
                    .as_ref()
                    .and_then(|writer| writer.schema.lock().unwrap().clone());
                (
                    200,
                    json!({ "schema_source": changed.unwrap_or_else(|| schema.to_string()) }),
                )
            } else if let Some(snapshot) = served
                .as_ref()
                .filter(|_| path.ends_with("/snapshot"))
                .and_then(|served| {
                    let branch = query
                        .split('&')
                        .find_map(|pair| pair.strip_prefix("branch="))
                        .unwrap_or("main");
                    let version = writer.as_ref().map_or(1, |writer| writer.version());
                    served.snapshot(branch, version)
                })
            {
                (200, snapshot)
            } else if let Some(served) = served.as_ref().filter(|_| path.ends_with("/commits")) {
                // The branch's newest commit, as the server has it once a held read goes.
                let asked = json!({ "route": "commits", "query": query });
                if let Some(held) = served.take(&asked) {
                    held.await.ok();
                }
                let head = writer.as_ref().and_then(|writer| writer.head());
                let version = writer.as_ref().map_or(1, |writer| writer.version());
                let commit = json!({
                    "graph_commit_id": head.unwrap_or_else(|| "c1".to_owned()),
                    "graph_manifest_version": version, "kind": "published", "created_at": 0,
                });
                (200, json!({ "commits": [commit] }))
            } else if path.ends_with("/query") {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let mut asked: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                // A read at a commit is answered there; a read of the branch at the other
                // writer's newest commit, or c1. The rows see it as `at`.
                let at_now = |asked: &Value| {
                    asked["snapshot"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| writer.as_ref().and_then(|writer| writer.head()))
                        .unwrap_or_else(|| "c1".to_owned())
                };
                let mut at = at_now(&asked);
                if let Some(asked) = asked.as_object_mut() {
                    asked.insert("at".into(), Value::String(at.clone()));
                }
                // A read held waits for the test, then is answered as the server would answer
                // it then: a read of the branch at the head it has reached meanwhile.
                let held = served.as_ref().and_then(|served| served.take(&asked));
                if let Some(held) = held {
                    held.await.ok();
                    at = at_now(&asked);
                    if let Some(asked) = asked.as_object_mut() {
                        asked.insert("at".into(), Value::String(at.clone()));
                    }
                }
                let rows = rows(&asked);
                // A read of nodes' fields gets the nodes answered in its shape.
                let rows = if asked["name"] == "studio_nodes_fields" {
                    as_fields(rows)
                } else {
                    rows
                };
                // An answer `{"refuse": message}` is the server refusing the read.
                if let Some(message) = rows.get("refuse").and_then(Value::as_str) {
                    return Ok(Response::builder()
                        .status(400)
                        .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                        .body(AsyncBody::from(
                            json!({ "error": message, "code": "bad_request" }).to_string(),
                        ))
                        .unwrap());
                }
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
            } else if let Some(access) = access.filter(|_| path.ends_with("/access")) {
                (200, access)
            } else {
                // A second branch, for views that follow the window to another one.
                (200, json!({ "branches": ["main", "fix"] }))
            };
            Ok(Response::builder()
                .status(status)
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
    cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx))
}
