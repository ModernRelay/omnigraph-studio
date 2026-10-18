//! One open graph on one server and branch: schema, branches, type counts, and every read and
//! write Studio sends for it (spec 003, 004, 008).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{Either, Shared, select};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task};
use gq_language::builder::Depends;
use gq_language::{BuiltQuery, RelationshipGroup, SchemaModel, builder};
use graph_client::api::{
    BranchCreateOutput, BranchCreateRequest, BranchMergeOutput, BranchMergeRequest,
    ChangeFeedOutput, ChangeOutput, ChangeRequest, CommitOutput, GraphListResponse,
    InvokeStoredQueryRequest, MergeBaseOutput, QueryCatalogEntry, QueryRequest, ReadOutput,
    ReadinessOutput, SnapshotDatasetOutput, SnapshotOutput,
};
use graph_client::{
    ApiErrorSummary, BlobAt, BlobCell, BlobMeta, CommitChanges, FeedAt, GraphClient, ReadError,
    ServerHealth, WriteOutcome,
};
use serde_json::Value;

use crate::carry::{ChangeLog, FeedFold, Since, TypeRef, answer_unchanged};
use crate::connection::Connection;
use crate::degrees::{DEGREE_BATCH, DegreeBatchKey, DegreeBatches, page_counts};
use crate::memo::{Held, Memo};

/// A value that loads from the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Loadable<T> {
    NotLoaded,
    Loading,
    Loaded(T),
    Failed(String),
}

impl<T> Loadable<T> {
    pub fn loaded(&self) -> Option<&T> {
        match self {
            Loadable::Loaded(value) => Some(value),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Loadable::Loading)
    }
}

/// How one write ended, kept for the activity log (constitution III).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeKind {
    Committed { commit: Option<String> },
    Refused { message: String },
    Unknown { reason: String },
}

/// Which property a draft is for: the node's type, its id and the property's name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DraftKey {
    pub type_name: String,
    pub id: String,
    pub property: String,
}

/// A property's unsaved value (spec 014): the value it was read at, which its save is bound to,
/// and the text typed for it.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftEntry {
    pub old: Value,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct OutcomeEntry {
    pub at: SystemTime,
    pub branch: String,
    pub description: String,
    pub outcome: OutcomeKind,
    /// What a committed write's toast offers instead of opening the commit.
    pub follow_up: Option<FollowUp>,
}

/// A follow-up a write's outcome offers once it commits (spec 002, toasts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUp {
    /// The commit only removed rows, and Studio can put them back (spec 004).
    UndoDelete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    SchemaChanged,
    BranchChanged,
    BranchesChanged,
    CountsChanged,
    /// A commit landed on `branch`: one of Studio's writes, or another writer's that the head
    /// watch saw. The branch's commit list is read again.
    Committed {
        branch: String,
    },
    /// `branch`'s head is now `head` ([`GraphSession::head`]), seen for the first time or
    /// moved. A view pinned to another commit of the branch may offer Refresh.
    HeadMoved {
        branch: String,
        head: String,
    },
    ActivityChanged,
    /// The graph became unavailable (503) or answered again.
    AvailabilityChanged,
    /// The graph's stored queries were read ([`GraphSession::stored_queries`]).
    StoredQueriesChanged,
    /// A draft was kept, changed or let go ([`GraphSession::set_draft`]).
    DraftsChanged,
    /// `branch`'s change feed was read up to its head, or the read stopped: a view waiting on
    /// [`GraphSession::changes_since`] asks again.
    ChangesRead {
        branch: String,
    },
}

/// How many answers to reads pinned to a commit, and Blob cells at a commit, a session keeps. A
/// node page holds one per relationship group, up to about a hundred; the answers together are
/// kept within [`PINNED_READ_BYTES`], least recently used first out.
const PINNED_READS: usize = 4_096;
const PINNED_READ_BYTES: usize = 64 * 1024 * 1024;
const PINNED_BLOBS: usize = 2_048;

/// How many branches' commit lists a session keeps.
const COMMIT_LISTS: usize = 64;

/// How often a session asks after the heads of the branches its views show, while a window of
/// Studio's is active.
pub const HEAD_POLL: Duration = Duration::from_secs(30);

/// Changes asked for per page of a change-feed read, and the most pages one read takes. A read
/// that stops sooner leaves its remaining commits unknown: the views over them read again.
const FEED_PAGE: u32 = 1_000;
const FEED_PAGES: usize = 8;

/// What a view reads, by type, while the watch is held: when its branch's head moves, the
/// change feed is asked about these types, so a view none of them changed for moves to the new
/// head without reading again (spec 015, carry-forward).
pub struct ReadWatch {
    _alive: Rc<()>,
}

/// A view's interest in a branch's head: the session asks after the branch while one is held.
/// The session's own branch is always asked after.
pub struct HeadWatch {
    _alive: Rc<()>,
}

/// A branch's commits, newest first, shared by the views that show them.
pub type CommitList = Arc<Vec<CommitOutput>>;

/// The server as the storage inspector shows it ([`GraphSession::server_state`]): each read's
/// own answer, so one failing leaves the others.
pub struct ServerState {
    pub health: Result<ServerHealth, ReadError>,
    pub readiness: Result<ReadinessOutput, ReadError>,
    pub graphs: Result<GraphListResponse, ReadError>,
    pub snapshot: Result<SnapshotOutput, ReadError>,
}

/// A read at a commit: the commit, the query text and its parameters, and the schema epoch it
/// was read under ([`GraphSession::schema_epoch`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PinnedRead {
    commit: String,
    query: String,
    params: String,
    schema: u64,
}

/// The first wait before a graph that answered 503 is asked again; it doubles up to the cap.
const RECHECK_FIRST: Duration = Duration::from_secs(5);
const RECHECK_CAP: Duration = Duration::from_secs(60);

/// About what an answer takes in memory: its rows' text and numbers, and their containers.
fn read_bytes(output: &ReadOutput) -> usize {
    fn weight(value: &Value) -> usize {
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => 16,
            Value::String(text) => 24 + text.len(),
            Value::Array(items) => 24 + items.iter().map(weight).sum::<usize>(),
            Value::Object(map) => {
                48 + map
                    .iter()
                    .map(|(key, item)| 24 + key.len() + weight(item))
                    .sum::<usize>()
            }
        }
    }
    256 + weight(&output.rows) + output.columns.iter().map(|c| 24 + c.len()).sum::<usize>()
}

fn parse_schema(source: &str) -> Loadable<Arc<SchemaModel>> {
    match SchemaModel::parse(source) {
        Ok(model) => Loadable::Loaded(Arc::new(model)),
        Err(error) => Loadable::Failed(format!("Studio could not read the schema: {error}")),
    }
}

/// Where the next page of a change-feed read starts.
enum FeedNext {
    Start(String),
    Cursor(String),
    Page(String),
}

impl FeedNext {
    fn at(&self) -> FeedAt<'_> {
        match self {
            Self::Start(start) => FeedAt::Start(start),
            Self::Cursor(cursor) => FeedAt::Cursor(cursor),
            Self::Page(token) => FeedAt::Page(token),
        }
    }
}

/// Answers kept by key, with the read under way shared by everyone who asks meanwhile.
type ReadMemo<K, V> = Memo<K, V, Shared<Task<Result<V, ReadError>>>>;

pub struct GraphSession {
    pub connection: Entity<Connection>,
    pub graph_id: String,
    pub branch: String,
    pub schema: Loadable<Arc<SchemaModel>>,
    pub branches: Loadable<Vec<String>>,
    pub datasets: Loadable<Vec<SnapshotDatasetOutput>>,
    /// The queries the server serves by name for this graph (spec 007), read with the schema.
    pub stored_queries: Loadable<Arc<Vec<QueryCatalogEntry>>>,
    pub outcomes: Vec<OutcomeEntry>,
    pub reads_in_flight: usize,
    pub writes_in_flight: usize,
    /// Why the graph is unavailable, when it answered 503 (spec 003); reads resume, and this
    /// clears, when it answers again.
    pub unavailable: Option<String>,
    recheck_after: Duration,
    /// Answers to reads pinned to a commit, and those under way (they never change).
    pinned_reads: ReadMemo<PinnedRead, ReadOutput>,
    pinned_blobs: ReadMemo<(BlobCell, String, u64), BlobMeta>,
    /// Which schema the session reads under: a new number each time the schema's text changes,
    /// the first one included. A read at an old commit uses the current schema, so the same
    /// query at the same commit can mean something else after a rename, a drop or an addition;
    /// every answer kept is kept under its epoch, and nothing crosses one.
    schema_epoch: u64,
    schema_text: Option<Arc<str>>,
    /// Each branch's commit list, read once and shared until a commit lands on that branch.
    commit_lists: ReadMemo<String, CommitList>,
    /// The branch list has been read once; reading it again forgets the commit lists.
    branches_read: bool,
    /// Relationship counts asked for this turn, read together when it ends.
    degrees: DegreeBatches,
    /// The commit each watched branch's head was last seen at, by the head watch or as one of
    /// Studio's own commits.
    heads: HashMap<String, String>,
    /// Unsaved property values by branch (spec 014): every table, page and peek of the graph
    /// reads and writes these, and a branch's drafts wait for it while another is shown.
    drafts: HashMap<String, BTreeMap<DraftKey, DraftEntry>>,
    /// Branches views asked after besides the session's own, while their watch is held.
    head_watches: Vec<(String, Weak<()>)>,
    /// Head reads under way, by branch.
    head_reads: HashMap<String, Task<()>>,
    /// The types views read, by branch, while their watch is held.
    read_watches: Vec<(String, BTreeSet<TypeRef>, Weak<()>)>,
    /// Each followed branch's commits as the change feed described them.
    change_logs: HashMap<String, ChangeLog>,
    /// Change-feed reads under way, by branch, and the branches whose head moved meanwhile.
    change_reads: HashMap<String, Task<()>>,
    change_reads_again: HashSet<String>,
    /// The branch list read again by the head watch, while under way.
    branch_recheck: Option<Task<()>>,
    /// A poll came while no window was active; the next activation asks at once.
    head_check_missed: bool,
    _head_poll: Option<Task<()>>,
    _recheck: Option<Task<()>>,
    _schema_task: Option<Task<()>>,
    _branches_task: Option<Task<()>>,
    _datasets_task: Option<Task<()>>,
    _stored_queries_task: Option<Task<()>>,
}

impl EventEmitter<SessionEvent> for GraphSession {}

impl GraphSession {
    pub fn new(
        connection: Entity<Connection>,
        graph_id: String,
        branch: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut session = Self {
            connection,
            graph_id,
            branch,
            schema: Loadable::NotLoaded,
            branches: Loadable::NotLoaded,
            datasets: Loadable::NotLoaded,
            stored_queries: Loadable::NotLoaded,
            outcomes: Vec::new(),
            reads_in_flight: 0,
            writes_in_flight: 0,
            unavailable: None,
            recheck_after: RECHECK_FIRST,
            pinned_reads: Memo::weighed(PINNED_READS, PINNED_READ_BYTES, read_bytes),
            pinned_blobs: Memo::new(PINNED_BLOBS),
            schema_epoch: 0,
            schema_text: None,
            commit_lists: Memo::new(COMMIT_LISTS),
            branches_read: false,
            degrees: DegreeBatches::default(),
            heads: HashMap::new(),
            drafts: HashMap::new(),
            head_watches: Vec::new(),
            head_reads: HashMap::new(),
            read_watches: Vec::new(),
            change_logs: HashMap::new(),
            change_reads: HashMap::new(),
            change_reads_again: HashSet::new(),
            branch_recheck: None,
            head_check_missed: false,
            _head_poll: None,
            _recheck: None,
            _schema_task: None,
            _branches_task: None,
            _datasets_task: None,
            _stored_queries_task: None,
        };
        session._head_poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HEAD_POLL).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if cx.active_window().is_some() {
                            this.check_heads(cx);
                        } else {
                            this.head_check_missed = true;
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
        session.reload(cx);
        session
    }

    /// The client every read and write of this session goes through. Views never hold one
    /// (`docs/architecture.md`): they ask the session, which counts the read and notes what
    /// it says about the server.
    pub(crate) fn client(&self, cx: &App) -> GraphClient {
        self.connection.read(cx).client().clone()
    }

    /// The server's address, for a view that shows it.
    pub fn server_url(&self, cx: &App) -> String {
        self.connection.read(cx).client().base_url().to_owned()
    }

    /// The client the agent gateway forwards the agent CLI's own requests through, inside the
    /// sandbox the gateway enforces. The one client a crate outside `graph_store` holds: the
    /// CLI's reads are its own, not the session's.
    pub fn gateway_client(&self, cx: &App) -> GraphClient {
        self.client(cx)
    }

    pub fn server_name(&self, cx: &App) -> String {
        self.connection.read(cx).server.name.clone()
    }

    pub fn supports(&self, operation_id: &str, cx: &App) -> bool {
        self.connection.read(cx).supports(operation_id)
    }

    pub fn schema_model(&self) -> Option<Arc<SchemaModel>> {
        self.schema.loaded().cloned()
    }

    /// Notes what a read says about the graph's availability: a 503 marks it unavailable and
    /// asks again later, with a growing wait; any answer clears the mark.
    fn note_read<T>(&mut self, result: &Result<T, ReadError>, cx: &mut Context<Self>) {
        match result {
            Err(ReadError::Refused(error)) if error.status == 503 => {
                let first = self.unavailable.is_none();
                self.unavailable = Some(error.message.clone());
                if first || self._recheck.is_none() {
                    let wait = self.recheck_after;
                    self.recheck_after = (wait * 2).min(RECHECK_CAP);
                    self._recheck = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(wait).await;
                        this.update(cx, |this, cx| {
                            this._recheck = None;
                            this.reload(cx);
                        })
                        .ok();
                    }));
                }
                if first {
                    cx.emit(SessionEvent::AvailabilityChanged);
                }
            }
            Err(ReadError::Transport(_)) => {}
            _ => {
                if self.unavailable.take().is_some() {
                    self.recheck_after = RECHECK_FIRST;
                    self._recheck = None;
                    cx.emit(SessionEvent::AvailabilityChanged);
                }
            }
        }
    }

    /// Reloads the schema, the branch list, the type counts and the stored queries.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_schema(cx);
        self.reload_branches(cx);
        self.reload_counts(cx);
        self.reload_stored_queries(cx);
    }

    /// Reads the graph's stored-query catalog. A server that serves none for the graph (a 404)
    /// has an empty catalog, not a failed one.
    pub fn reload_stored_queries(&mut self, cx: &mut Context<Self>) {
        self.stored_queries = Loadable::Loading;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        self._stored_queries_task = Some(cx.spawn(async move |this, cx| {
            let read = client.stored_queries(&graph).await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match read {
                Ok(output) => Loadable::Loaded(Arc::new(output.queries)),
                Err(ReadError::Refused(error)) if error.status == 404 => {
                    Loadable::Loaded(Arc::new(Vec::new()))
                }
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.stored_queries = result;
                cx.emit(SessionEvent::StoredQueriesChanged);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The stored query named `name`, once the catalog is read.
    pub fn stored_query(&self, name: &str) -> Option<QueryCatalogEntry> {
        self.stored_queries
            .loaded()?
            .iter()
            .find(|query| query.name == name)
            .cloned()
    }

    /// Runs a stored read at `snapshot`, else at the session's branch head. The server refuses a
    /// stored query that writes, since the request says it expects a read.
    pub fn invoke_stored_read(
        &mut self,
        name: &str,
        params: Option<Value>,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let request = InvokeStoredQueryRequest {
            params,
            branch: snapshot.is_none().then(|| self.branch.clone()),
            snapshot,
            expect_mutation: Some(false),
        };
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let name = name.to_owned();
        self.counted_read(
            async move { client.invoke_stored_read(&graph, &name, &request).await },
            cx,
        )
    }

    /// Runs a stored mutation on `branch`, sent once and logged like every write; with
    /// `if_commit` it lands only while that commit is the branch head.
    pub fn invoke_stored_write(
        &mut self,
        name: &str,
        params: Option<Value>,
        branch: String,
        if_commit: Option<String>,
        description: String,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<ChangeOutput>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let name = name.to_owned();
        let request = InvokeStoredQueryRequest {
            params,
            branch: Some(branch.clone()),
            snapshot: None,
            expect_mutation: Some(true),
        };
        self.writes_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let work = cx.spawn(async move |this, cx| {
            let outcome = client
                .invoke_stored_write(&graph, &name, &request, if_commit.as_deref())
                .await;
            let kind = outcome_kind(&outcome, |output: &ChangeOutput| {
                output.commit.as_ref().map(|c| c.graph_commit_id.clone())
            });
            this.update(cx, |this, cx| {
                this.finish_write(branch, description, kind, None, cx);
            })
            .ok();
            outcome
        });
        Self::to_outcome(work, cx)
    }

    pub fn reload_schema(&mut self, cx: &mut Context<Self>) {
        self.schema = Loadable::Loading;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        self._schema_task = Some(cx.spawn(async move |this, cx| {
            let read = client.schema(&graph).await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match read {
                Ok(output) => parse_schema(&output.schema_source),
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| this.install_schema(result, cx))
                .ok();
        }));
    }

    /// Takes a schema read from the server. A text other than the one held starts a new schema
    /// epoch: what was kept under the old one, answers and change logs, is let go.
    fn install_schema(&mut self, schema: Loadable<Arc<SchemaModel>>, cx: &mut Context<Self>) {
        if let Loadable::Loaded(model) = &schema
            && self.schema_text.as_deref() != Some(&*model.source)
        {
            self.schema_epoch += 1;
            self.schema_text = Some(model.source.clone());
            self.pinned_reads.clear();
            self.pinned_blobs.clear();
            // Each log starts again at its branch's head, so the next move is read.
            self.change_logs = self
                .heads
                .iter()
                .map(|(branch, head)| (branch.clone(), ChangeLog::starting_at(head.clone())))
                .collect();
        }
        self.schema = schema;
        cx.emit(SessionEvent::SchemaChanged);
        cx.notify();
        self.check_heads(cx);
    }

    /// Reads the schema again while keeping the one held, and takes it if its text changed: a
    /// head move showed another writer may have changed it.
    fn recheck_schema(&mut self, cx: &mut Context<Self>) {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        self._schema_task = Some(cx.spawn(async move |this, cx| {
            let read = client.schema(&graph).await;
            this.update(cx, |this, cx| {
                this.note_read(&read, cx);
                if let Ok(output) = read
                    && this.schema_text.as_deref() != Some(output.schema_source.as_str())
                {
                    this.install_schema(parse_schema(&output.schema_source), cx);
                }
            })
            .ok();
        }));
    }

    pub fn reload_branches(&mut self, cx: &mut Context<Self>) {
        self.branches = Loadable::Loading;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        self._branches_task = Some(cx.spawn(async move |this, cx| {
            let read = client.branches(&graph).await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match read {
                Ok(output) => Loadable::Loaded(branch_order(output.branches)),
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                // A branch deleted and made again under its name has another history. The first
                // list changes nothing read before it: lists asked for meanwhile (main's, as the
                // window opens) stay, rather than being read twice.
                if this.branches_read {
                    this.commit_lists.clear();
                }
                this.branches_read |= matches!(result, Loadable::Loaded(_));
                this.branches = result;
                cx.emit(SessionEvent::BranchesChanged);
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn reload_counts(&mut self, cx: &mut Context<Self>) {
        self.datasets = Loadable::Loading;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let branch = self.branch.clone();
        self._datasets_task = Some(cx.spawn(async move |this, cx| {
            let read = client.snapshot(&graph, &branch).await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match read {
                Ok(output) => Loadable::Loaded(output.datasets),
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.datasets = result;
                cx.emit(SessionEvent::CountsChanged);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The `graphs` key of this graph in the settings (spec 002).
    pub fn settings_key(&self, cx: &App) -> String {
        studio_settings::graph_key(&self.connection.read(cx).server.name, &self.graph_id)
    }

    /// The browsing limits in effect for this graph: its overrides in the settings, else the
    /// shared ones.
    pub fn options(&self, cx: &App) -> studio_settings::GraphOptions {
        let server = &self.connection.read(cx).server.name;
        match cx.try_global::<studio_settings::StudioSettings>() {
            Some(settings) => settings.for_graph(server, &self.graph_id),
            None => studio_settings::StudioSettings::default().for_graph(server, &self.graph_id),
        }
    }

    /// The row count of a type at the branch head, from the snapshot.
    pub fn count_of(&self, type_name: &str) -> Option<i64> {
        self.datasets
            .loaded()?
            .iter()
            .find(|d| d.type_name == type_name)
            .map(|d| d.entity_count)
    }

    pub fn set_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        if self.branch == branch {
            return;
        }
        self.branch = branch;
        self.reload_counts(cx);
        cx.emit(SessionEvent::BranchChanged);
        cx.notify();
        self.check_heads(cx);
    }

    /// The unsaved value of one property on the session's branch.
    pub fn draft(&self, key: &DraftKey) -> Option<&DraftEntry> {
        self.drafts.get(&self.branch)?.get(key)
    }

    /// Every unsaved value on the session's branch, by type, id and property.
    pub fn drafts(&self) -> impl Iterator<Item = (&DraftKey, &DraftEntry)> {
        self.drafts.get(&self.branch).into_iter().flatten()
    }

    /// Keeps a property's unsaved value on the session's branch, replacing the one there.
    pub fn set_draft(&mut self, key: DraftKey, entry: DraftEntry, cx: &mut Context<Self>) {
        let drafts = self.drafts.entry(self.branch.clone()).or_default();
        if drafts.get(&key) != Some(&entry) {
            drafts.insert(key, entry);
            cx.emit(SessionEvent::DraftsChanged);
            cx.notify();
        }
    }

    /// Lets a property's unsaved value go: saved, discarded or put back.
    pub fn remove_draft(&mut self, key: &DraftKey, cx: &mut Context<Self>) -> Option<DraftEntry> {
        let removed = self.drafts.get_mut(&self.branch)?.remove(key);
        if removed.is_some() {
            cx.emit(SessionEvent::DraftsChanged);
            cx.notify();
        }
        removed
    }

    /// The commit `branch`'s head was last seen at, once a watched branch has been asked after
    /// or Studio committed to it.
    pub fn head(&self, branch: &str) -> Option<&str> {
        self.heads.get(branch).map(String::as_str)
    }

    /// Asks after `branch`'s head while the returned watch is held.
    pub fn watch_head(&mut self, branch: &str, cx: &mut Context<Self>) -> HeadWatch {
        let alive = Rc::new(());
        let new = !self
            .watched_branches()
            .iter()
            .any(|watched| watched == branch);
        self.head_watches
            .push((branch.to_owned(), Rc::downgrade(&alive)));
        if new {
            self.check_heads(cx);
        }
        HeadWatch { _alive: alive }
    }

    /// Asks the change feed about `types` on `branch` while the returned watch is held.
    pub fn watch_reads(&mut self, branch: &str, types: BTreeSet<TypeRef>) -> ReadWatch {
        let alive = Rc::new(());
        self.read_watches
            .push((branch.to_owned(), types, Rc::downgrade(&alive)));
        ReadWatch { _alive: alive }
    }

    /// What `branch`'s commits after `from`, up to and including `to`, changed among the types
    /// the change feed was asked about: [`Since::Pending`] while the feed is being read.
    pub fn changes_since(&self, branch: &str, from: &str, to: &str) -> Since {
        if let Some(since) = self
            .change_logs
            .get(branch)
            .and_then(|log| log.since(from, to))
        {
            return Since::Known(since);
        }
        if self.change_reads.contains_key(branch) {
            Since::Pending
        } else {
            Since::Unknown
        }
    }

    /// The type names the views on `branch` read.
    fn read_names(&mut self, branch: &str) -> BTreeSet<String> {
        self.read_watches
            .retain(|(_, _, alive)| alive.strong_count() > 0);
        self.read_watches
            .iter()
            .filter(|(watched, ..)| watched == branch)
            .flat_map(|(_, types, _)| types.iter().map(|ty| ty.name().to_owned()))
            .collect()
    }

    /// Reads `branch`'s change feed after the newest commit its log holds, up to the feed's
    /// head, asked only about the types views read: every change carries its images, so an
    /// unfiltered read costs what every type wrote.
    fn read_changes(&mut self, branch: &str, cx: &mut Context<Self>) {
        if self.change_reads.contains_key(branch) {
            self.change_reads_again.insert(branch.to_owned());
            return;
        }
        let Some(head) = self.heads.get(branch).cloned() else {
            return;
        };
        let names = self.read_names(branch);
        let schema = self.schema_model().map(|schema| schema.source.clone());
        let log = self.change_logs.entry(branch.to_owned()).or_default();
        if log.holds(&head) {
            return;
        }
        let (Some(from), Some(schema)) = (log.last.clone(), schema) else {
            log.last = Some(head);
            return;
        };
        if names.is_empty() {
            // Nobody reads the branch: the log starts again at its head.
            log.last = Some(head);
            return;
        }
        let (client, graph, epoch) = (self.client(cx), self.graph_id.clone(), self.schema_epoch);
        let types: Vec<String> = names.iter().cloned().collect();
        let mut fold = FeedFold::new(Arc::new(names));
        let key = branch.to_owned();
        let branch = branch.to_owned();
        let read = cx.spawn(async move |this, cx| {
            let mut next = Some(FeedNext::Start(format!("after:{from}")));
            let mut failed = None;
            for _ in 0..FEED_PAGES {
                let Some(at) = next.take() else {
                    break;
                };
                match client
                    .changes(&graph, &branch, at.at(), &types, FEED_PAGE)
                    .await
                {
                    Ok(page) => {
                        fold.page(&page);
                        next = match (page.next_page_token, page.cursor, page.caught_up) {
                            (Some(token), ..) => Some(FeedNext::Page(token)),
                            (None, _, Some(true)) | (None, None, _) => None,
                            // More complete commits wait past the page's limit.
                            (None, Some(cursor), _) => Some(FeedNext::Cursor(cursor)),
                        };
                    }
                    // A schema boundary (409), reclaimed history (410) or a failed read: the
                    // commits read before it stand.
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            let commits = fold.complete();
            // The feed filters by type name, so a schema change in between (a rename) could
            // hide a view's changes: what it says counts only under the schema views read with.
            let schema_read = if commits.is_empty() {
                None
            } else {
                Some(client.schema(&graph).await)
            };
            this.update(cx, |this, cx| {
                this.change_reads.remove(&branch);
                if let Some(error) = failed.clone() {
                    this.note_read::<()>(&Err(error), cx);
                }
                let same_schema = this.schema_epoch == epoch
                    && match &schema_read {
                        None => true,
                        Some(Ok(output)) => *output.schema_source == *schema,
                        Some(Err(_)) => false,
                    };
                // Another writer changed the schema: a feed that crossed it refuses (409), or
                // the schema read after it says so.
                let schema_moved = matches!(&failed, Some(ReadError::Refused(refusal)) if refusal.status == 409)
                    || matches!(&schema_read, Some(Ok(output)) if *output.schema_source != *schema);
                let stopped = failed.is_some() || next.is_some() || !same_schema;
                let head = this.heads.get(&branch).cloned();
                let log = this.change_logs.entry(branch.clone()).or_default();
                if same_schema {
                    for (commit, logged) in commits {
                        log.record(commit, logged);
                    }
                }
                // The commits a read stopped short of stay unknown; the next read starts at the
                // head.
                if let Some(head) = head
                    && stopped
                    && !log.holds(&head)
                {
                    log.last = Some(head);
                }
                cx.emit(SessionEvent::ChangesRead {
                    branch: branch.clone(),
                });
                if schema_moved && this.schema_epoch == epoch {
                    this.recheck_schema(cx);
                }
                if this.change_reads_again.remove(&branch) {
                    this.read_changes(&branch, cx);
                }
            })
            .ok();
        });
        self.change_reads.insert(key, read);
    }

    /// A window became active: asks after the heads at once when a poll came while none was.
    pub fn window_activated(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.head_check_missed) {
            self.check_heads(cx);
        }
    }

    /// The session's branch, then every branch a held watch names.
    fn watched_branches(&self) -> Vec<String> {
        let mut branches = vec![self.branch.clone()];
        for (branch, alive) in &self.head_watches {
            if alive.strong_count() > 0 && !branches.contains(branch) {
                branches.push(branch.clone());
            }
        }
        branches
    }

    /// Reads each watched branch's head with a read that returns no rows, unless one is under
    /// way, and the branch list. Heads of branches no longer watched are forgotten.
    pub fn check_heads(&mut self, cx: &mut Context<Self>) {
        self.head_watches
            .retain(|(_, alive)| alive.strong_count() > 0);
        let watched = self.watched_branches();
        self.heads.retain(|branch, _| watched.contains(branch));
        self.change_logs
            .retain(|branch, _| watched.contains(branch));
        if self.unavailable.is_some() {
            return;
        }
        self.recheck_branches(cx);
        let Some(probe) = self
            .schema_model()
            .and_then(|schema| builder::head_probe(&schema))
        else {
            return;
        };
        for branch in watched {
            if self.head_reads.contains_key(&branch) {
                continue;
            }
            let request = QueryRequest {
                query: probe.text.clone(),
                name: Some(probe.name.clone()),
                params: None,
                branch: Some(branch.clone()),
                snapshot: None,
            };
            let client = self.client(cx);
            let graph = self.graph_id.clone();
            let key = branch.clone();
            let read = cx.spawn(async move |this, cx| {
                let read = client.query(&graph, &request).await;
                this.update(cx, |this, cx| {
                    this.head_reads.remove(&branch);
                    this.note_read(&read, cx);
                    if let Ok(ReadOutput {
                        graph_commit_id: Some(head),
                        ..
                    }) = read
                    {
                        this.saw_head(branch, head, false, cx);
                    }
                })
                .ok();
            });
            self.head_reads.insert(key, read);
        }
    }

    /// Reads the branch list again once it has been read, so a branch made or deleted elsewhere
    /// (the CLI, an agent, another window) appears; an unchanged list changes nothing.
    fn recheck_branches(&mut self, cx: &mut Context<Self>) {
        if self.branch_recheck.is_some() || !matches!(self.branches, Loadable::Loaded(_)) {
            return;
        }
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        self.branch_recheck = Some(cx.spawn(async move |this, cx| {
            let read = client.branches(&graph).await;
            this.update(cx, |this, cx| {
                this.branch_recheck = None;
                this.note_read(&read, cx);
                let Ok(output) = read else {
                    return;
                };
                let branches = branch_order(output.branches);
                if this.branches.loaded() == Some(&branches) {
                    return;
                }
                // As after any branch list read: a branch deleted and made again under its
                // name has another history.
                this.commit_lists.clear();
                this.branches = Loadable::Loaded(branches);
                cx.emit(SessionEvent::BranchesChanged);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Records `branch`'s head. A move from a head already seen that is not one of Studio's own
    /// commits (`own`) is another writer's commit: the branch's commit list and counts are read
    /// again, as after Studio's own.
    fn saw_head(&mut self, branch: String, head: String, own: bool, cx: &mut Context<Self>) {
        if own {
            // A head read sent before this commit could answer with the head before it.
            self.head_reads.remove(&branch);
        }
        let previous = self.heads.insert(branch.clone(), head.clone());
        if previous.as_deref() == Some(head.as_str()) {
            return;
        }
        // Before views hear of the move, so they find the read under way.
        self.read_changes(&branch, cx);
        if previous.is_some() && !own {
            self.commit_lists.forget(&branch);
            if branch == self.branch {
                self.reload_counts(cx);
            }
            cx.emit(SessionEvent::Committed {
                branch: branch.clone(),
            });
        }
        cx.emit(SessionEvent::HeadMoved { branch, head });
    }

    /// An answer to the same query and parameters read at an earlier commit of the session's
    /// branch, standing for `key`'s commit because no commit in between changed what it
    /// depends on (spec 015, carry-forward). Answered as read at `key`'s commit.
    fn carried(&self, key: &PinnedRead, depends: &Depends) -> Option<ReadOutput> {
        if *depends == Depends::Unknown {
            return None;
        }
        self.pinned_reads
            .answers()
            .filter(|(read, _)| {
                read.query == key.query
                    && read.params == key.params
                    && read.schema == key.schema
                    && read.commit != key.commit
            })
            .find_map(|(read, output)| {
                match self.changes_since(&self.branch, &read.commit, &key.commit) {
                    Since::Known(since) if answer_unchanged(&since, depends, &output.rows) => {
                        let mut output = output.clone();
                        output.graph_commit_id = Some(key.commit.clone());
                        output.target.branch = None;
                        output.target.snapshot = Some(key.commit.clone());
                        Some(output)
                    }
                    _ => None,
                }
            })
    }

    /// Runs a read on the session's branch, or at `snapshot` when given (constitution II). A
    /// read at a commit is answered from what was read at that commit before, or shares an
    /// identical read under way: a commit never changes.
    pub fn read(
        &mut self,
        query: &BuiltQuery,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let request = QueryRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: snapshot.is_none().then(|| self.branch.clone()),
            snapshot,
        };
        let Some(commit) = request.snapshot.clone() else {
            return self.read_request(request, cx);
        };
        let key = PinnedRead {
            commit,
            query: request.query.clone(),
            params: request
                .params
                .as_ref()
                .map(|params| params.to_string())
                .unwrap_or_default(),
            schema: self.schema_epoch,
        };
        let shared = match self.pinned_reads.get(&key) {
            Some(Held::Done(output)) => return Task::ready(Ok(output)),
            Some(Held::Pending(shared)) => shared,
            None if let Some(output) = self.carried(&key, &query.depends) => {
                self.pinned_reads.insert(key, output.clone());
                return Task::ready(Ok(output));
            }
            None => {
                let shared = self.read_at_commit(request, cx).shared();
                self.pinned_reads.start(key.clone(), shared.clone());
                shared
            }
        };
        cx.spawn(async move |this, cx| {
            let result = shared.await;
            this.update(cx, |this, _| this.pinned_reads.finish(&key, &result))
                .ok();
            result
        })
    }

    /// A read at `request`'s commit. When that commit is the session branch's head, it is sent
    /// as a read of the branch, which the server answers from its warm caches where a read
    /// pinned to a commit resolves the commit and opens the graph at it, 0.5 s or more slower
    /// (spec 015). Its answer counts only if it comes back at that commit, so the read still
    /// sees exactly the commit asked for; one from a newer head is read again, pinned. Only the
    /// head watch moves a head, since reads can finish out of order.
    fn read_at_commit(
        &mut self,
        request: QueryRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let Some(commit) = request.snapshot.clone() else {
            return self.read_request(request, cx);
        };
        if self.heads.get(&self.branch) != Some(&commit) {
            return self.read_request(request, cx);
        }
        let at_branch = QueryRequest {
            branch: Some(self.branch.clone()),
            snapshot: None,
            ..request.clone()
        };
        let branch_read = self.read_request(at_branch, cx);
        cx.spawn(async move |this, cx| {
            match branch_read.await {
                Ok(mut output) if output.graph_commit_id.as_deref() == Some(commit.as_str()) => {
                    // Answered as the pinned read would be.
                    output.target.branch = None;
                    output.target.snapshot = Some(commit);
                    return Ok(output);
                }
                // The branch moved past the commit, or went: read at the commit itself.
                Ok(_)
                | Err(ReadError::Refused(ApiErrorSummary {
                    status: 404 | 409, ..
                })) => {}
                Err(error) => return Err(error),
            }
            let pinned = this.update(cx, |this, cx| this.read_request(request, cx));
            match pinned {
                Ok(pinned) => pinned.await,
                Err(_) => Err(ReadError::Transport(
                    "Studio stopped before the server answered".into(),
                )),
            }
        })
    }

    pub fn read_request(
        &mut self,
        request: QueryRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        self.counted_read(async move { client.query(&graph, &request).await }, cx)
    }

    /// Runs a read counted in [`Self::reads_in_flight`] for exactly as long as it runs. A read
    /// nobody waits for any more (its task dropped, as a search does when the text changes)
    /// stops, and is counted out either way: a count that only came down on an answer would
    /// leave "queries running" up for reads that no longer exist.
    fn counted_read<T: 'static>(
        &mut self,
        request: impl Future<Output = Result<T, ReadError>> + 'static,
        cx: &mut Context<Self>,
    ) -> Task<Result<T, ReadError>> {
        self.reads_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let (answer_tx, answer_rx) = oneshot::channel();
        // Held by the caller's task: dropping that task drops this, which stops the read.
        let (waiting_tx, waiting_rx) = oneshot::channel::<std::convert::Infallible>();
        cx.spawn(async move |this, cx| {
            let result = match select(std::pin::pin!(request), waiting_rx).await {
                Either::Left((result, _)) => Some(result),
                Either::Right(_) => None,
            };
            this.update(cx, |this, cx| {
                if let Some(result) = &result {
                    this.note_read(result, cx);
                }
                this.reads_in_flight = this.reads_in_flight.saturating_sub(1);
                cx.emit(SessionEvent::ActivityChanged);
                cx.notify();
            })
            .ok();
            if let Some(result) = result {
                answer_tx.send(result).ok();
            }
        })
        .detach();
        cx.spawn(async move |_, _| {
            let _waiting = waiting_tx;
            answer_rx.await.unwrap_or_else(|_| {
                Err(ReadError::Transport(
                    "Studio stopped before the server answered".into(),
                ))
            })
        })
    }

    /// Runs a write to its outcome whether or not anyone still waits for it. Dropping the
    /// caller's task must not cut a mutation off mid-flight: its outcome would go unlogged
    /// and its count would never come down, and nothing may resend it.
    fn to_outcome<T: 'static>(
        work: Task<WriteOutcome<T>>,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<T>> {
        let (outcome_tx, outcome_rx) = oneshot::channel();
        cx.spawn(async move |_, _| {
            outcome_tx.send(work.await).ok();
        })
        .detach();
        cx.spawn(async move |_, _| {
            outcome_rx.await.unwrap_or_else(|_| WriteOutcome::Unknown {
                reason: "Studio stopped before the server answered".into(),
            })
        })
    }

    /// What a Blob cell holds (spec 004): its size, or the reference it names, or null. Read at
    /// `snapshot`, else the session's branch head.
    pub fn blob_meta(
        &mut self,
        cell: BlobCell,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<BlobMeta, ReadError>> {
        // At a commit a cell never changes: the table, the peek and the Blob tab share one read.
        if let Some(commit) = snapshot.clone() {
            let key = (cell.clone(), commit, self.schema_epoch);
            let shared = match self.pinned_blobs.get(&key) {
                Some(Held::Done(meta)) => return Task::ready(Ok(meta)),
                Some(Held::Pending(shared)) => shared,
                None => {
                    let shared = self.blob_meta_unpinned(cell, snapshot, cx).shared();
                    self.pinned_blobs.start(key.clone(), shared.clone());
                    shared
                }
            };
            return cx.spawn(async move |this, cx| {
                let result = shared.await;
                this.update(cx, |this, _| this.pinned_blobs.finish(&key, &result))
                    .ok();
                result
            });
        }
        self.blob_meta_unpinned(cell, snapshot, cx)
    }

    fn blob_meta_unpinned(
        &mut self,
        cell: BlobCell,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<BlobMeta, ReadError>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let branch = self.branch.clone();
        cx.spawn(async move |this, cx| {
            let at = match &snapshot {
                Some(commit) => BlobAt::Snapshot(commit),
                None => BlobAt::Branch(&branch),
            };
            let result = client.blob_head(&graph, &cell, at).await;
            this.update(cx, |this, cx| this.note_read(&result, cx)).ok();
            result
        })
    }

    /// A branch's commits as the server lists them. Read once and shared by every view that
    /// asks (the history, the branches panel), and read again only after a commit Studio makes
    /// on that branch or a change to the branch list; `fresh` reads it again now, as a Refresh
    /// does, to see commits made elsewhere.
    pub fn commits(
        &mut self,
        branch: &str,
        fresh: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<CommitList, ReadError>> {
        let key = branch.to_owned();
        if fresh {
            self.commit_lists.forget(&key);
        }
        let shared = match self.commit_lists.get(&key) {
            Some(Held::Done(list)) => return Task::ready(Ok(list)),
            Some(Held::Pending(shared)) => shared,
            None => {
                let client = self.client(cx);
                let graph = self.graph_id.clone();
                let branch = key.clone();
                let shared = self
                    .counted_read(
                        async move {
                            client
                                .commits(&graph, &branch)
                                .await
                                .map(|list| Arc::new(list.commits))
                        },
                        cx,
                    )
                    .shared();
                self.commit_lists.start(key.clone(), shared.clone());
                shared
            }
        };
        cx.spawn(async move |this, cx| {
            let result = shared.await;
            this.update(cx, |this, _| this.commit_lists.finish(&key, &result))
                .ok();
            result
        })
    }

    /// One page of a commit's changes against its first parent, from `page_token` (the first
    /// page when `None`).
    pub fn commit_changes(
        &mut self,
        commit_id: &str,
        page_token: Option<String>,
        limit: u32,
        cx: &mut Context<Self>,
    ) -> Task<Result<CommitChanges, ReadError>> {
        let (client, graph, commit) =
            (self.client(cx), self.graph_id.clone(), commit_id.to_owned());
        self.counted_read(
            async move {
                client
                    .commit_changes(&graph, &commit, page_token.as_deref(), limit)
                    .await
            },
            cx,
        )
    }

    /// The commit a merge of `source` into `target` would classify against, and both heads.
    pub fn merge_base(
        &mut self,
        source: &str,
        target: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<MergeBaseOutput, ReadError>> {
        let (client, graph) = (self.client(cx), self.graph_id.clone());
        let (source, target) = (source.to_owned(), target.to_owned());
        self.counted_read(
            async move { client.merge_base(&graph, &source, &target).await },
            cx,
        )
    }

    /// One page of `branch`'s change feed: from `start` (`now`, `beginning` or
    /// `after:<commit_id>`), or from `page_token` to continue.
    pub fn changes(
        &mut self,
        branch: &str,
        start: Option<String>,
        page_token: Option<String>,
        limit: u32,
        cx: &mut Context<Self>,
    ) -> Task<Result<ChangeFeedOutput, ReadError>> {
        let (client, graph, branch) = (self.client(cx), self.graph_id.clone(), branch.to_owned());
        self.counted_read(
            async move {
                let at = match (&page_token, &start) {
                    (Some(token), _) => FeedAt::Page(token),
                    (None, Some(start)) => FeedAt::Start(start),
                    (None, None) => FeedAt::Start("now"),
                };
                client.changes(&graph, &branch, at, &[], limit).await
            },
            cx,
        )
    }

    /// The server's health and readiness, the graphs it serves, and `branch`'s snapshot: what
    /// the storage inspector shows, read together.
    pub fn server_state(&mut self, branch: &str, cx: &mut Context<Self>) -> Task<ServerState> {
        let client = self.client(cx);
        let (graph, branch) = (self.graph_id.clone(), branch.to_owned());
        // Each is a read of its own, counted and noted as one.
        let health = {
            let client = client.clone();
            self.counted_read(async move { client.health().await }, cx)
        };
        let readiness = {
            let client = client.clone();
            self.counted_read(async move { client.readiness().await }, cx)
        };
        let graphs = {
            let client = client.clone();
            self.counted_read(async move { client.graphs().await }, cx)
        };
        let snapshot = self.counted_read(async move { client.snapshot(&graph, &branch).await }, cx);
        cx.background_spawn(async move {
            ServerState {
                health: health.await,
                readiness: readiness.await,
                graphs: graphs.await,
                snapshot: snapshot.await,
            }
        })
    }

    /// How many neighbours `id` has through `group`, at `snapshot` (the branch head when
    /// `None`). Counts asked for in the same turn for one node type, relationship and commit
    /// are read together, as `builder::page_degrees` reads of up to a hundred nodes in id
    /// order (`crate::degrees`); an asker that stops waiting is left out, and a read nobody
    /// waits for any more is stopped.
    pub fn degree(
        &mut self,
        type_name: &str,
        group: &RelationshipGroup,
        id: &str,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<u64, String>> {
        let (tx, rx) = oneshot::channel();
        let key = DegreeBatchKey {
            type_name: type_name.to_owned(),
            group: group.clone(),
            snapshot,
        };
        if self.degrees.add(key, id.to_owned(), tx) {
            let session = cx.entity().downgrade();
            cx.defer(move |cx| {
                session
                    .update(cx, |session, cx| session.flush_degrees(cx))
                    .ok();
            });
        }
        cx.background_spawn(async move {
            rx.await
                .unwrap_or_else(|_| Err("The count was not read.".to_owned()))
        })
    }

    fn flush_degrees(&mut self, cx: &mut Context<Self>) {
        let schema = self.schema_model();
        for (key, mut nodes) in self.degrees.take(DEGREE_BATCH) {
            nodes.retain_mut(|(_, waiters)| {
                waiters.retain(|waiter| !waiter.is_canceled());
                !waiters.is_empty()
            });
            if nodes.is_empty() {
                continue;
            }
            let ids: Vec<String> = nodes.iter().map(|(id, _)| id.clone()).collect();
            let query = match &schema {
                Some(schema) => builder::page_degrees(schema, &key.type_name, &key.group, &ids)
                    .map_err(|error| error.to_string()),
                None => Err("The schema is still loading.".to_owned()),
            };
            let query = match query {
                Ok(query) => query,
                Err(error) => {
                    for (_, waiters) in nodes {
                        for waiter in waiters {
                            waiter.send(Err(error.clone())).ok();
                        }
                    }
                    continue;
                }
            };
            let read = self.read(&query, key.snapshot.clone(), cx);
            cx.background_spawn(async move {
                // Dropping the read when every asker has stopped waiting stops it.
                let result = {
                    let gone = futures::future::join_all(
                        nodes
                            .iter_mut()
                            .flat_map(|(_, waiters)| waiters.iter_mut())
                            .map(|waiter| waiter.cancellation()),
                    );
                    match select(read, std::pin::pin!(gone)).await {
                        Either::Left((result, _)) => result,
                        Either::Right(_) => return,
                    }
                };
                let answer = result
                    .map(|output| page_counts(&output.rows))
                    .map_err(|error| error.to_string());
                for (id, waiters) in nodes {
                    let count = answer
                        .as_ref()
                        .map(|counts| counts.get(&id).copied().unwrap_or(0))
                        .map_err(Clone::clone);
                    for waiter in waiters {
                        waiter.send(count.clone()).ok();
                    }
                }
            })
            .detach();
        }
    }

    /// Bytes `range` of a managed Blob value, for a preview.
    pub fn blob_bytes(
        &mut self,
        cell: BlobCell,
        snapshot: Option<String>,
        range: std::ops::Range<u64>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<u8>, ReadError>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let branch = self.branch.clone();
        self.counted_read(
            async move {
                let at = match &snapshot {
                    Some(commit) => BlobAt::Snapshot(commit),
                    None => BlobAt::Branch(&branch),
                };
                client.blob_bytes(&graph, &cell, at, range).await
            },
            cx,
        )
    }

    /// Runs a mutation on the session's branch. `if_commit` binds it to the commit it was read
    /// at. The outcome is logged; nothing is retried.
    pub fn write(
        &mut self,
        query: &BuiltQuery,
        if_commit: Option<String>,
        description: String,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<ChangeOutput>> {
        self.write_with(query, if_commit, description, None, cx)
    }

    /// [`Self::write`], whose outcome offers `follow_up` once it commits.
    pub fn write_with(
        &mut self,
        query: &BuiltQuery,
        if_commit: Option<String>,
        description: String,
        follow_up: Option<FollowUp>,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<ChangeOutput>> {
        let request = ChangeRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: Some(self.branch.clone()),
        };
        self.send_write(request, if_commit, description, follow_up, cx)
    }

    pub fn write_request(
        &mut self,
        request: ChangeRequest,
        if_commit: Option<String>,
        description: String,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<ChangeOutput>> {
        self.send_write(request, if_commit, description, None, cx)
    }

    fn send_write(
        &mut self,
        request: ChangeRequest,
        if_commit: Option<String>,
        description: String,
        follow_up: Option<FollowUp>,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<ChangeOutput>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let branch = request
            .branch
            .clone()
            .unwrap_or_else(|| self.branch.clone());
        self.writes_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let work = cx.spawn(async move |this, cx| {
            let outcome = client.mutate(&graph, &request, if_commit.as_deref()).await;
            let kind = match &outcome {
                WriteOutcome::Committed(output) => OutcomeKind::Committed {
                    commit: output.commit.as_ref().map(|c| c.graph_commit_id.clone()),
                },
                WriteOutcome::Refused(error) => OutcomeKind::Refused {
                    message: error.body.error.clone(),
                },
                WriteOutcome::Unknown { reason } => OutcomeKind::Unknown {
                    reason: reason.clone(),
                },
            };
            this.update(cx, |this, cx| {
                this.finish_write(branch, description, kind, follow_up, cx);
            })
            .ok();
            outcome
        });
        Self::to_outcome(work, cx)
    }

    /// Records a write sent on the person's behalf outside this session's own calls, as the
    /// agent's CLI writes through Studio's gateway (spec 013): logged with Studio's own
    /// writes, and views of the branch learn of the commit. `branch_created` reads the branch
    /// list again.
    pub fn note_write(
        &mut self,
        branch: String,
        description: String,
        outcome: OutcomeKind,
        branch_created: bool,
        cx: &mut Context<Self>,
    ) {
        let commit = match &outcome {
            OutcomeKind::Committed { commit } => commit.clone(),
            _ => None,
        };
        self.outcomes.push(OutcomeEntry {
            at: SystemTime::now(),
            branch: branch.clone(),
            description,
            outcome,
            follow_up: None,
        });
        if let Some(commit) = commit {
            self.committed(branch, commit, cx);
        }
        if branch_created {
            self.reload_branches(cx);
        }
        cx.emit(SessionEvent::ActivityChanged);
        cx.notify();
    }

    fn finish_write(
        &mut self,
        branch: String,
        description: String,
        outcome: OutcomeKind,
        follow_up: Option<FollowUp>,
        cx: &mut Context<Self>,
    ) {
        self.writes_in_flight = self.writes_in_flight.saturating_sub(1);
        let commit = match &outcome {
            OutcomeKind::Committed { commit } => commit.clone(),
            _ => None,
        };
        self.outcomes.push(OutcomeEntry {
            at: SystemTime::now(),
            branch: branch.clone(),
            description,
            outcome,
            follow_up,
        });
        if let Some(commit) = commit {
            self.committed(branch, commit, cx);
        }
        cx.emit(SessionEvent::ActivityChanged);
        cx.notify();
    }

    /// One of Studio's writes committed `commit` on `branch`: it is the branch's head.
    fn committed(&mut self, branch: String, commit: String, cx: &mut Context<Self>) {
        self.reload_counts(cx);
        self.commit_lists.forget(&branch);
        cx.emit(SessionEvent::Committed {
            branch: branch.clone(),
        });
        self.saw_head(branch, commit, true, cx);
    }

    pub fn create_branch(
        &mut self,
        name: String,
        from: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<BranchCreateOutput>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let description = format!("Create branch {name}");
        let branch = name.clone();
        self.writes_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let work = cx.spawn(async move |this, cx| {
            let outcome = client
                .create_branch(&graph, &BranchCreateRequest { name, from })
                .await;
            let kind = outcome_kind(&outcome, |_| None);
            this.update(cx, |this, cx| {
                this.finish_write(branch, description, kind, None, cx);
                this.reload_branches(cx);
            })
            .ok();
            outcome
        });
        Self::to_outcome(work, cx)
    }

    pub fn delete_branch(
        &mut self,
        name: String,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<serde_json::Value>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let description = format!("Delete branch {name}");
        self.writes_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let work = cx.spawn(async move |this, cx| {
            let outcome = client.delete_branch(&graph, &name).await;
            let kind = outcome_kind(&outcome, |_| None);
            this.update(cx, |this, cx| {
                if this.branch == name {
                    this.set_branch("main".into(), cx);
                }
                this.finish_write(name, description, kind, None, cx);
                this.reload_branches(cx);
            })
            .ok();
            outcome
        });
        Self::to_outcome(work, cx)
    }

    pub fn merge(
        &mut self,
        source: String,
        target: String,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<BranchMergeOutput>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let description = format!("Merge {source} into {target}");
        let request = BranchMergeRequest {
            source,
            target: Some(target.clone()),
            delete_branch: false,
        };
        self.writes_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let work = cx.spawn(async move |this, cx| {
            let outcome = client.merge(&graph, &request).await;
            let kind = outcome_kind(&outcome, |output: &BranchMergeOutput| {
                output.commit.as_ref().map(|c| c.graph_commit_id.clone())
            });
            this.update(cx, |this, cx| {
                this.finish_write(target, description, kind, None, cx);
            })
            .ok();
            outcome
        });
        Self::to_outcome(work, cx)
    }
}

fn outcome_kind<T>(
    outcome: &WriteOutcome<T>,
    commit: impl Fn(&T) -> Option<String>,
) -> OutcomeKind {
    match outcome {
        WriteOutcome::Committed(value) => OutcomeKind::Committed {
            commit: commit(value).or_else(|| Some(String::new())),
        },
        WriteOutcome::Refused(error) => OutcomeKind::Refused {
            message: error.body.error.clone(),
        },
        WriteOutcome::Unknown { reason } => OutcomeKind::Unknown {
            reason: reason.clone(),
        },
    }
}

/// Branches as Studio lists them: `main` first, then by name.
fn branch_order(mut branches: Vec<String>) -> Vec<String> {
    branches.sort_by(|a, b| (a != "main").cmp(&(b != "main")).then(a.cmp(b)));
    branches
}

/// Opens a session on a graph.
pub fn open_session(
    connection: Entity<Connection>,
    graph_id: String,
    branch: String,
    cx: &mut App,
) -> Entity<GraphSession> {
    connection.update(cx, |connection, _| connection.remember_graph(&graph_id));
    cx.new(|cx| GraphSession::new(connection, graph_id, branch, cx))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use gpui::TestAppContext;
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};

    use super::*;
    use crate::connection::{ServerEntry, ServerSource};

    #[gpui::test]
    async fn a_graph_that_answers_503_is_unavailable_until_it_answers_again(
        cx: &mut TestAppContext,
    ) {
        let ready = Arc::new(AtomicBool::new(false));
        let flag = ready.clone();
        let http = FakeHttpClient::create(move |_request| {
            let ready = flag.load(Ordering::SeqCst);
            async move {
                let (status, body) = if ready {
                    (200, r#"{"branches":["main"]}"#)
                } else {
                    (503, r#"{"error":"graph knowledge is loading"}"#)
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
                    .body(AsyncBody::from(body))
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
        session.read_with(cx, |session, _| {
            assert_eq!(
                session.unavailable.as_deref(),
                Some("graph knowledge is loading")
            )
        });

        // Nothing is asked again before the wait; after it, the answer clears the mark.
        ready.store(true, Ordering::SeqCst);
        cx.executor().advance_clock(RECHECK_FIRST / 2);
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert!(session.unavailable.is_some()));
        cx.executor().advance_clock(RECHECK_FIRST);
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.unavailable, None);
            assert_eq!(
                session.branches.loaded().cloned(),
                Some(vec!["main".to_owned()])
            );
        });
    }

    /// Drafts belong to the branch they were typed on: another branch shows its own, and the
    /// first one's wait until it is shown again (spec 014).
    #[gpui::test]
    async fn drafts_wait_with_their_branch(cx: &mut TestAppContext) {
        let http = FakeHttpClient::create(|_request| async {
            Ok(Response::builder()
                .status(200)
                .header(graph_client::CONTRACT_HEADER, "0.13")
                .body(AsyncBody::from(r#"{"branches":["main","fix"]}"#))
                .unwrap())
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
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        let entry = DraftEntry {
            old: Value::from("Ada"),
            text: "Ada Lovelace".into(),
        };
        session.update(cx, |session, cx| {
            session.set_draft(key.clone(), entry.clone(), cx);
            assert_eq!(session.draft(&key), Some(&entry));
            session.set_branch("fix".into(), cx);
            assert_eq!(session.draft(&key), None, "another branch shows its own");
            assert_eq!(session.drafts().count(), 0);
            session.set_branch("main".into(), cx);
            assert_eq!(session.draft(&key), Some(&entry), "it waited");
            assert_eq!(session.remove_draft(&key, cx), Some(entry));
            assert_eq!(session.drafts().count(), 0);
        });
    }

    /// A read at a commit is sent once: an identical read under way is shared, a later one is
    /// answered from memory. A read at the branch head is always sent.
    #[gpui::test]
    async fn reads_at_a_commit_are_sent_once(cx: &mut TestAppContext) {
        let queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let queries_seen = queries.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let queries = queries_seen.clone();
            async move {
                let body = if path.ends_with("/query") {
                    queries.fetch_add(1, Ordering::SeqCst);
                    r#"{"query_name":"q","target":{"snapshot":"c1"},"row_count":1,"columns":["n"],"rows":[{"n":3}],"graph_commit_id":"c1"}"#
                } else {
                    r#"{"branches":["main"]}"#
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
                    .body(AsyncBody::from(body))
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
        let at_start = queries.load(Ordering::SeqCst);

        let query = BuiltQuery {
            name: "q".into(),
            text: "query q() { match { $p: Person } return { count($p) as n } }".into(),
            params: Default::default(),
            ..Default::default()
        };
        let (first, second) = session.update(cx, |session, cx| {
            (
                session.read(&query, Some("c1".into()), cx),
                session.read(&query, Some("c1".into()), cx),
            )
        });
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        assert_eq!(first.rows, second.rows);
        assert_eq!(
            queries.load(Ordering::SeqCst) - at_start,
            1,
            "shared while under way"
        );

        let later = session.update(cx, |session, cx| {
            session.read(&query, Some("c1".into()), cx)
        });
        assert_eq!(later.await.unwrap().rows, first.rows);
        assert_eq!(
            queries.load(Ordering::SeqCst) - at_start,
            1,
            "answered from memory"
        );

        let other = session.update(cx, |session, cx| {
            session.read(&query, Some("c2".into()), cx)
        });
        other.await.unwrap();
        assert_eq!(
            queries.load(Ordering::SeqCst) - at_start,
            2,
            "another commit is read"
        );

        for _ in 0..2 {
            let head = session.update(cx, |session, cx| session.read(&query, None, cx));
            head.await.unwrap();
        }
        assert_eq!(
            queries.load(Ordering::SeqCst) - at_start,
            4,
            "the branch head moves: every read is sent"
        );
    }

    /// A read at the commit the branch's head is at goes as a read of the branch, which the
    /// server answers from its warm caches, and counts only if it comes back at that commit.
    /// Once the branch has moved past it, the read is sent again pinned to the commit; the head
    /// moves only by the head watch.
    #[gpui::test]
    async fn a_read_at_the_head_goes_to_the_branch_and_checks_its_commit(cx: &mut TestAppContext) {
        use futures::AsyncReadExt as _;
        let served_head = Arc::new(std::sync::Mutex::new("c1"));
        let sent = Arc::new(std::sync::Mutex::new(
            Vec::<(Option<String>, Option<String>)>::new(),
        ));
        let (head_seen, sent_seen) = (served_head.clone(), sent.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (head, sent) = (head_seen.clone(), sent_seen.clone());
            async move {
                let body = if path.ends_with("/query") {
                    let mut bytes = Vec::new();
                    request.into_body().read_to_end(&mut bytes).await.ok();
                    let asked: Value = serde_json::from_slice(&bytes).unwrap();
                    let branch = asked["branch"].as_str().map(str::to_owned);
                    let snapshot = asked["snapshot"].as_str().map(str::to_owned);
                    sent.lock()
                        .unwrap()
                        .push((branch.clone(), snapshot.clone()));
                    let at = snapshot
                        .clone()
                        .unwrap_or_else(|| head.lock().unwrap().to_string());
                    serde_json::json!({
                        "query_name": "q",
                        "target": { "branch": branch, "snapshot": snapshot },
                        "row_count": 1,
                        "columns": ["at"],
                        "rows": [{ "at": at }],
                        "graph_commit_id": at,
                    })
                    .to_string()
                } else {
                    r#"{"branches":["main"]}"#.to_owned()
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
                    .body(AsyncBody::from(body))
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
        session.update(cx, |session, _| {
            session.heads.insert("main".into(), "c1".into());
        });
        sent.lock().unwrap().clear();
        let query = |n: i64| BuiltQuery {
            name: "q".into(),
            text: "query q($n: I64) { match { $p: Person } return { count($p) as n } }".into(),
            params: [("n".to_owned(), Value::from(n))].into_iter().collect(),
            ..Default::default()
        };

        let read = session.update(cx, |session, cx| {
            session.read(&query(1), Some("c1".into()), cx)
        });
        let output = read.await.unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            vec![(Some("main".to_owned()), None)],
            "sent as a read of the branch"
        );
        assert_eq!(output.graph_commit_id.as_deref(), Some("c1"));
        assert_eq!(
            output.target.snapshot.as_deref(),
            Some("c1"),
            "answered as pinned"
        );
        assert_eq!(output.target.branch, None);

        *served_head.lock().unwrap() = "c2";
        sent.lock().unwrap().clear();
        let read = session.update(cx, |session, cx| {
            session.read(&query(2), Some("c1".into()), cx)
        });
        let output = read.await.unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            vec![
                (Some("main".to_owned()), None),
                (None, Some("c1".to_owned()))
            ],
            "the branch moved: read again at the commit"
        );
        assert_eq!(
            output.rows,
            serde_json::json!([{ "at": "c1" }]),
            "the rows are the commit's"
        );
        session.read_with(cx, |session, _| {
            assert_eq!(
                session.head("main"),
                Some("c1"),
                "a read does not move the head"
            );
        });
    }

    /// Counts asked for one at a time in the same turn go out as one read per node type and
    /// relationship; asked again at the commit, they are answered from memory.
    #[gpui::test]
    async fn counts_asked_together_are_read_together(cx: &mut TestAppContext) {
        use futures::AsyncReadExt as _;
        use serde_json::Value;
        let reads: Arc<std::sync::Mutex<Vec<(String, Value)>>> = Arc::default();
        let seen = reads.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let seen = seen.clone();
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let body = if path.ends_with("/schema") {
                    serde_json::json!({ "schema_source": "node Person {\n  slug: String @key\n}\nnode Company {\n  slug: String @key\n}\nedge Knows: Person -> Person\nedge WorksAt: Person -> Company\n" })
                } else if path.ends_with("/query") {
                    let request: Value = serde_json::from_slice(&bytes).unwrap();
                    let name = request["name"].as_str().unwrap_or_default().to_owned();
                    let ids = request["params"]["ids"].clone();
                    if name != "studio_head" {
                        seen.lock().unwrap().push((name, ids.clone()));
                    }
                    // Every node but p-2 has as many neighbours as its id's last digit.
                    let rows: Vec<Value> = ids
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter(|id| *id != "p-2")
                        .map(|id| {
                            let n = id[2..].parse::<u64>().unwrap();
                            serde_json::json!({ "id": id, "n": n })
                        })
                        .collect();
                    serde_json::json!({ "query_name": "studio_page_degrees", "target": { "snapshot": "c1" }, "row_count": rows.len(), "columns": ["id", "n"], "rows": rows, "graph_commit_id": "c1" })
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
        assert!(session.read_with(cx, |session, _| session.schema_model().is_some()));
        let group = |edge: &str, other: &str| RelationshipGroup {
            edge: edge.into(),
            direction: gq_language::Direction::Outgoing,
            other_type: other.into(),
        };
        let ask = |cx: &mut TestAppContext, edge: &str, other: &str, ids: &[&str]| {
            let group = group(edge, other);
            session.update(cx, |session, cx| {
                ids.iter()
                    .map(|id| session.degree("Person", &group, id, Some("c1".into()), cx))
                    .collect::<Vec<_>>()
            })
        };

        // Three nodes' Knows and two nodes' WorksAt, asked one by one: two reads.
        let knows = ask(cx, "Knows", "Person", &["p-3", "p-1", "p-2"]);
        let works = ask(cx, "WorksAt", "Company", &["p-1", "p-3"]);
        let mut counts = Vec::new();
        for count in knows.into_iter().chain(works) {
            counts.push(count.await.unwrap());
        }
        assert_eq!(counts, vec![3, 1, 0, 1, 3], "a node without a row has none");
        let mut sent = reads.lock().unwrap().clone();
        sent.sort_by_key(|(_, ids)| ids.to_string());
        assert_eq!(
            sent,
            vec![
                (
                    "studio_page_degrees".into(),
                    serde_json::json!(["p-1", "p-2", "p-3"])
                ),
                (
                    "studio_page_degrees".into(),
                    serde_json::json!(["p-1", "p-3"])
                ),
            ]
        );

        // The same counts at the same commit: from memory.
        let again = ask(cx, "Knows", "Person", &["p-2", "p-1", "p-3"]);
        for count in again {
            count.await.unwrap();
        }
        assert_eq!(reads.lock().unwrap().len(), 2);
    }

    /// The session asks after its branch's head, and each watched branch's, once the schema
    /// loads, at every poll while a window is active, and when a window becomes active after a
    /// poll came while none was. Another writer's commit lands like Studio's own; Studio's own
    /// commit is the head without asking. Each poll reads the branch list too, so a branch made
    /// elsewhere appears.
    #[gpui::test]
    async fn the_head_watch_sees_other_writers_commits_while_a_window_is_active(
        cx: &mut TestAppContext,
    ) {
        use futures::AsyncReadExt as _;
        use serde_json::Value;
        let heads: Arc<std::sync::Mutex<HashMap<String, String>>> = Arc::new(
            [("main", "c1"), ("feature", "f1")]
                .map(|(branch, head)| (branch.to_owned(), head.to_owned()))
                .into_iter()
                .collect::<HashMap<_, _>>()
                .into(),
        );
        let probes: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let listed: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(vec!["main", "feature"].into());
        let (known, asked, branch_list) = (heads.clone(), probes.clone(), listed.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (known, asked, branch_list) = (known.clone(), asked.clone(), branch_list.clone());
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let body = if path.ends_with("/schema") {
                    serde_json::json!({ "schema_source": "node Person {\n  slug: String @key\n}\n" })
                } else if path.ends_with("/query") {
                    let request: Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(request["name"], "studio_head");
                    let branch = request["branch"].as_str().unwrap().to_owned();
                    let head = known.lock().unwrap()[&branch].clone();
                    asked.lock().unwrap().push(branch);
                    serde_json::json!({ "query_name": "studio_head", "target": { "branch": "main" }, "row_count": 0, "rows": [], "graph_commit_id": head })
                } else if path.ends_with("/mutate") {
                    serde_json::json!({ "branch": "main", "query_name": "m", "affected_nodes": 1, "affected_edges": 0, "commit": { "graph_commit_id": "c9", "graph_branch": "main", "graph_manifest_version": 9, "created_at": 9, "parent_commit_id": "c2" } })
                } else {
                    serde_json::json!({ "branches": *branch_list.lock().unwrap() })
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
        let events: Rc<std::cell::RefCell<Vec<SessionEvent>>> = Rc::default();
        let seen = events.clone();
        let _events = cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| {
                if matches!(
                    event,
                    SessionEvent::Committed { .. }
                        | SessionEvent::HeadMoved { .. }
                        | SessionEvent::BranchesChanged
                ) {
                    seen.borrow_mut().push(event.clone());
                }
            })
        });
        let head_of = |cx: &mut TestAppContext, branch: &str| {
            session.read_with(cx, |session, _| session.head(branch).map(str::to_owned))
        };
        let moved = |branch: &str, head: &str| SessionEvent::HeadMoved {
            branch: branch.into(),
            head: head.into(),
        };
        let landed = |branch: &str| SessionEvent::Committed {
            branch: branch.into(),
        };
        cx.run_until_parked();
        assert_eq!(head_of(cx, "main").as_deref(), Some("c1"));
        events
            .borrow_mut()
            .retain(|event| *event != SessionEvent::BranchesChanged);
        assert_eq!(
            events.take(),
            vec![moved("main", "c1")],
            "a head seen first is no commit"
        );

        // Another writer commits while no window is active: the poll asks nothing.
        heads.lock().unwrap().insert("main".into(), "c2".into());
        cx.executor().advance_clock(HEAD_POLL);
        cx.run_until_parked();
        assert_eq!(probes.lock().unwrap().len(), 1);
        assert_eq!(head_of(cx, "main").as_deref(), Some("c1"));

        // A window becomes active: asked at once; a second activation asks nothing more. The
        // branch list read again is unchanged: nothing is said about it.
        for _ in 0..2 {
            session.update(cx, |session, cx| session.window_activated(cx));
            cx.run_until_parked();
        }
        assert_eq!(probes.lock().unwrap().len(), 2);
        assert_eq!(events.take(), vec![landed("main"), moved("main", "c2")]);

        // A watched branch is asked after at once, then with the session's own at each poll. A
        // branch made elsewhere appears at the poll.
        let watch = session.update(cx, |session, cx| session.watch_head("feature", cx));
        cx.run_until_parked();
        assert_eq!(head_of(cx, "feature").as_deref(), Some("f1"));
        cx.add_empty_window()
            .update(|window, _| window.activate_window());
        heads.lock().unwrap().insert("feature".into(), "f2".into());
        listed.lock().unwrap().push("spike");
        probes.lock().unwrap().clear();
        cx.executor().advance_clock(HEAD_POLL);
        cx.run_until_parked();
        let mut asked = probes.lock().unwrap().clone();
        asked.sort();
        assert_eq!(asked, vec!["feature", "main"]);
        assert_eq!(head_of(cx, "feature").as_deref(), Some("f2"));
        assert!(events.take().contains(&SessionEvent::BranchesChanged));
        assert_eq!(
            session.read_with(cx, |session, _| session.branches.loaded().cloned()),
            Some(vec!["main".into(), "feature".into(), "spike".into()])
        );

        // The watch dropped: the branch is no longer asked after, and its head is forgotten.
        drop(watch);
        probes.lock().unwrap().clear();
        cx.executor().advance_clock(HEAD_POLL);
        cx.run_until_parked();
        assert_eq!(*probes.lock().unwrap(), vec!["main"]);
        assert_eq!(head_of(cx, "feature"), None);

        // Studio's own commit: the head without asking, landing once.
        let query = BuiltQuery {
            name: "m".into(),
            text: "query m() { update Person set { slug: \"x\" } where @id = \"p\" }".into(),
            params: Default::default(),
            ..Default::default()
        };
        probes.lock().unwrap().clear();
        let write = session.update(cx, |session, cx| {
            session.write(&query, None, "Set slug".into(), cx)
        });
        write.await;
        cx.run_until_parked();
        assert!(probes.lock().unwrap().is_empty());
        assert_eq!(head_of(cx, "main").as_deref(), Some("c9"));
        assert_eq!(events.take(), vec![landed("main"), moved("main", "c9")]);
    }

    /// The stored-query catalog is read with the graph, and a server without one has an empty
    /// catalog. A stored read goes to the session's branch and says it expects a read; a stored
    /// write goes to the branch it names, says it expects a write, and is logged like any write,
    /// its commit the branch head.
    #[gpui::test]
    async fn stored_queries_are_read_with_the_graph_and_run_by_name(cx: &mut TestAppContext) {
        use futures::AsyncReadExt as _;
        let sent: Arc<std::sync::Mutex<Vec<(String, Value)>>> = Arc::default();
        let log = sent.clone();
        let serves = Arc::new(AtomicBool::new(true));
        let serving = serves.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (log, serving) = (log.clone(), serving.clone());
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let (status, body) = if path.ends_with("/queries") {
                    if serving.load(Ordering::SeqCst) {
                        (
                            200,
                            serde_json::json!({ "queries": [
                            { "name": "find", "tool_name": "find", "mutation": false,
                              "params": [{ "name": "name", "kind": "string", "nullable": false }] },
                            { "name": "add", "tool_name": "add", "mutation": true, "params": [] }
                        ] }),
                        )
                    } else {
                        (404, serde_json::json!({ "error": "not found" }))
                    }
                } else if path.contains("/queries/") {
                    log.lock()
                        .unwrap()
                        .push((path.clone(), serde_json::from_slice(&bytes).unwrap()));
                    if path.ends_with("/find") {
                        (
                            200,
                            serde_json::json!({ "query_name": "find", "target": { "branch": "main" }, "row_count": 0, "rows": [], "graph_commit_id": "c4" }),
                        )
                    } else {
                        (
                            200,
                            serde_json::json!({ "branch": "main", "query_name": "add", "affected_nodes": 1, "affected_edges": 0, "commit": { "graph_commit_id": "c5", "graph_branch": "main", "graph_manifest_version": 5, "created_at": 5, "parent_commit_id": "c4" } }),
                        )
                    }
                } else {
                    (200, serde_json::json!({ "branches": ["main"] }))
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
        let session = cx
            .new(|cx| GraphSession::new(connection.clone(), "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            let names: Vec<_> = session
                .stored_queries
                .loaded()
                .unwrap()
                .iter()
                .map(|query| query.name.clone())
                .collect();
            assert_eq!(names, ["find", "add"]);
            assert!(session.stored_query("add").unwrap().mutation);
        });

        let read = session.update(cx, |session, cx| {
            session.invoke_stored_read("find", Some(serde_json::json!({ "name": "Ada" })), None, cx)
        });
        assert_eq!(read.await.unwrap().graph_commit_id.as_deref(), Some("c4"));
        let write = session.update(cx, |session, cx| {
            session.invoke_stored_write("add", None, "main".into(), None, "Run add".into(), cx)
        });
        assert!(matches!(write.await, WriteOutcome::Committed(_)));
        cx.run_until_parked();
        assert_eq!(
            *sent.lock().unwrap(),
            vec![
                (
                    "/graphs/knowledge/queries/find".to_owned(),
                    serde_json::json!({ "params": { "name": "Ada" }, "branch": "main", "expect_mutation": false })
                ),
                (
                    "/graphs/knowledge/queries/add".to_owned(),
                    serde_json::json!({ "branch": "main", "expect_mutation": true })
                ),
            ]
        );
        session.read_with(cx, |session, _| {
            let last = session.outcomes.last().unwrap();
            assert_eq!(last.description, "Run add");
            assert_eq!(
                last.outcome,
                OutcomeKind::Committed {
                    commit: Some("c5".into())
                }
            );
            assert_eq!(session.head("main"), Some("c5"));
        });

        // A server that serves no catalog for the graph: an empty one, not a failure.
        serves.store(false, Ordering::SeqCst);
        session.update(cx, |session, cx| session.reload_stored_queries(cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(
                session.stored_queries.loaded().map(|queries| queries.len()),
                Some(0)
            )
        });
    }

    /// A branch's commit list is read once for every view that shows it, and again only after
    /// a commit lands on that branch or the branch list is read again.
    #[gpui::test]
    async fn commit_lists_are_read_again_only_for_the_branch_a_commit_lands_on(
        cx: &mut TestAppContext,
    ) {
        let lists: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let seen = lists.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let seen = seen.clone();
            async move {
                let body = if path.ends_with("/commits") {
                    seen.lock().unwrap().push(query);
                    r#"{"commits":[{"graph_commit_id":"c1","created_at":1,"graph_manifest_version":1,"published":true}]}"#
                } else if path.ends_with("/mutate") {
                    r#"{"branch":"main","query_name":"m","affected_nodes":1,"affected_edges":0,"commit":{"graph_commit_id":"c2","graph_branch":"main","graph_manifest_version":2,"created_at":2,"parent_commit_id":"c1"}}"#
                } else {
                    r#"{"branches":["main","feature"]}"#
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
                    .body(AsyncBody::from(body))
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
        // As the window opens: main's list is asked for while the branch list is read, and the
        // branches panel asks again when the branches arrive.
        let _panel = cx.update(|cx| {
            cx.subscribe(&session, |session, event: &SessionEvent, cx| {
                if matches!(event, SessionEvent::BranchesChanged) {
                    session
                        .update(cx, |session, cx| session.commits("main", false, cx))
                        .detach();
                }
            })
        });
        let early = session.update(cx, |session, cx| session.commits("main", false, cx));
        cx.run_until_parked();
        early.await.unwrap();
        let reads_of = |branch: &str| {
            lists
                .lock()
                .unwrap()
                .iter()
                .filter(|query| query.contains(&format!("branch={branch}")))
                .count()
        };

        assert_eq!(
            reads_of("main"),
            1,
            "the first branch list keeps what was asked for meanwhile"
        );

        // The history and the branches panel ask together: one read.
        let (a, b) = session.update(cx, |session, cx| {
            (
                session.commits("main", false, cx),
                session.commits("main", false, cx),
            )
        });
        assert_eq!(a.await.unwrap().len(), 1);
        assert_eq!(b.await.unwrap().len(), 1);
        let feature = session.update(cx, |session, cx| session.commits("feature", false, cx));
        feature.await.unwrap();
        assert_eq!((reads_of("main"), reads_of("feature")), (1, 1));

        // Asked again: nothing is read.
        let again = session.update(cx, |session, cx| session.commits("main", false, cx));
        again.await.unwrap();
        assert_eq!(reads_of("main"), 1);

        // A commit on main: main is read again, feature is not.
        let query = BuiltQuery {
            name: "m".into(),
            text: "query m() { update Person set { title: \"x\" } where @id = \"p\" }".into(),
            params: Default::default(),
            ..Default::default()
        };
        let write = session.update(cx, |session, cx| {
            session.write(&query, None, "Set title".into(), cx)
        });
        write.await;
        cx.run_until_parked();
        for branch in ["main", "feature"] {
            let read = session.update(cx, |session, cx| session.commits(branch, false, cx));
            read.await.unwrap();
        }
        assert_eq!((reads_of("main"), reads_of("feature")), (2, 1));

        // Refresh reads it again whatever was kept.
        let fresh = session.update(cx, |session, cx| session.commits("feature", true, cx));
        fresh.await.unwrap();
        assert_eq!(reads_of("feature"), 2);

        // The branch list read again (a branch made or deleted): every list is read again.
        drop(_panel);
        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        let read = session.update(cx, |session, cx| session.commits("feature", false, cx));
        read.await.unwrap();
        assert_eq!(reads_of("feature"), 3);
    }

    /// The reads the history, the commit view, the review and the storage inspector ask for
    /// go through the session, so they count as running while they run and stop counting
    /// when dropped.
    #[gpui::test]
    async fn feature_reads_are_counted_while_they_run(cx: &mut TestAppContext) {
        let held = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = held.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let held = seen.clone();
            async move {
                let feature = path.ends_with("/changes")
                    || path.ends_with("/healthz")
                    || path.ends_with("/readyz")
                    || path.ends_with("/snapshot")
                    || path == "/graphs";
                if feature {
                    // A read that never answers, held until dropped.
                    held.fetch_add(1, Ordering::SeqCst);
                    futures::future::pending::<()>().await;
                }
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
                    .body(AsyncBody::from(r#"{"branches":["main"]}"#))
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
        // Opening the session reads some of these routes itself.
        let before = session.read_with(cx, |session, _| session.reads_in_flight);
        let held_before = held.load(Ordering::SeqCst);

        let reads = session.update(cx, |session, cx| {
            (
                session.commit_changes("c1", None, 10, cx),
                session.changes("main", Some("beginning".into()), None, 10, cx),
                session.server_state("main", cx),
            )
        });
        cx.run_until_parked();
        assert_eq!(
            held.load(Ordering::SeqCst) - held_before,
            6,
            "two change reads and four server reads"
        );
        session.read_with(cx, |session, _| {
            assert_eq!(session.reads_in_flight, before + 6)
        });
        drop(reads);
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert_eq!(session.reads_in_flight, before));
    }

    /// The command palette's search drops its reads when the text changes; the count must come
    /// down with them. A write whose task is dropped still reaches its outcome and is logged.
    #[gpui::test]
    async fn dropped_reads_are_counted_out_and_dropped_writes_still_finish(
        cx: &mut TestAppContext,
    ) {
        let queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (queries_seen, mutations_seen) = (queries.clone(), mutations.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let queries = queries_seen.clone();
            let mutations = mutations_seen.clone();
            async move {
                if path.ends_with("/query") {
                    // A read that never answers, like a slow search.
                    queries.fetch_add(1, Ordering::SeqCst);
                    futures::future::pending::<()>().await;
                }
                let body = if path.ends_with("/mutate") {
                    mutations.fetch_add(1, Ordering::SeqCst);
                    r#"{"error":"refused for the test"}"#
                } else {
                    r#"{"branches":["main"]}"#
                };
                let status = if path.ends_with("/mutate") { 400 } else { 200 };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
                    .body(AsyncBody::from(body))
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

        let request = || QueryRequest {
            query: "query q() { match { $p: Person } return { $p } }".into(),
            name: Some("q".into()),
            params: None,
            branch: Some("main".into()),
            snapshot: None,
        };
        let reads: Vec<_> = session.update(cx, |session, cx| {
            (0..3)
                .map(|_| session.read_request(request(), cx))
                .collect()
        });
        cx.run_until_parked();
        assert_eq!(queries.load(Ordering::SeqCst), 3);
        session.read_with(cx, |session, _| assert_eq!(session.reads_in_flight, 3));
        drop(reads);
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert_eq!(session.reads_in_flight, 0));

        let write = session.update(cx, |session, cx| {
            session.write_request(
                ChangeRequest {
                    query: "query w() { delete Person where @id = \"p\" }".into(),
                    name: Some("w".into()),
                    params: None,
                    branch: Some("main".into()),
                },
                None,
                "Delete p".into(),
                cx,
            )
        });
        drop(write);
        cx.run_until_parked();
        assert_eq!(mutations.load(Ordering::SeqCst), 1);
        session.read_with(cx, |session, _| {
            assert_eq!(session.writes_in_flight, 0);
            assert_eq!(session.outcomes.len(), 1);
            assert!(matches!(
                session.outcomes[0].outcome,
                OutcomeKind::Refused { .. }
            ));
        });
    }

    /// A head move reads the change feed after the commit the log last holds, asked only about
    /// the types views read; a range answers once that read is in, and is unknown when nobody
    /// read the branch or the schema changed meanwhile.
    #[gpui::test]
    async fn a_head_move_reads_the_change_feed_for_the_types_views_read(cx: &mut TestAppContext) {
        use crate::carry::{Since, TypeRef};
        use crate::test_server::{followed_session, node_change};
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n}\nnode Company {\n  slug: String @key\n}\nedge WorksAt: Person -> Company\n";
        let (session, served) = followed_session(SCHEMA, |_| json!([]), cx);
        cx.run_until_parked();
        assert_eq!(
            session.read_with(cx, |session, _| session.head("main").map(str::to_owned)),
            Some("c1".into())
        );
        let commit = |cx: &mut TestAppContext, id: &str, changes| {
            served.lock().unwrap().commit(id, changes);
            session.update(cx, |session, cx| {
                session.saw_head("main".into(), id.into(), false, cx)
            });
        };
        let since = |cx: &mut TestAppContext, from: &str, to: &str| {
            session.read_with(cx, |session, _| session.changes_since("main", from, to))
        };
        let feeds = || served.lock().unwrap().feeds.clone();
        let person = TypeRef::Node("Person".into());
        let company = TypeRef::Node("Company".into());

        let watch = session.update(cx, |session, _| {
            session.watch_reads("main", BTreeSet::from([person.clone()]))
        });
        commit(cx, "c2", vec![node_change("Company", "co1")]);
        assert!(
            matches!(since(cx, "c1", "c2"), Since::Pending),
            "the read is under way"
        );
        cx.run_until_parked();
        assert_eq!(feeds().len(), 1);
        assert!(
            feeds()[0].contains("start=after%3Ac1") && feeds()[0].contains("type=Person"),
            "{}",
            feeds()[0]
        );
        let Since::Known(range) = since(cx, "c1", "c2") else {
            panic!("known")
        };
        assert!(
            !range.type_changed(&person),
            "Company's change is no Person's"
        );
        assert!(
            range.type_changed(&company),
            "the feed was not asked about Company"
        );

        commit(cx, "c3", vec![node_change("Person", "p1")]);
        cx.run_until_parked();
        assert!(feeds()[1].contains("start=after%3Ac2"));
        let Since::Known(range) = since(cx, "c1", "c3") else {
            panic!("known")
        };
        assert!(range.entity_changed(&person, "p1"));
        assert!(!range.entity_changed(&person, "p2"));

        // Nobody reads the branch: no feed read, and the range is not known.
        drop(watch);
        commit(cx, "c4", vec![node_change("Person", "p2")]);
        cx.run_until_parked();
        assert_eq!(feeds().len(), 2);
        assert!(matches!(since(cx, "c3", "c4"), Since::Unknown));

        // A schema changed in between: what the feed said does not count.
        let _watch = session.update(cx, |session, _| {
            session.watch_reads("main", BTreeSet::from([person.clone()]))
        });
        served.lock().unwrap().schema =
            format!("{SCHEMA}node Project {{\n  slug: String @key\n}}\n");
        commit(cx, "c5", vec![node_change("Company", "co2")]);
        cx.run_until_parked();
        assert_eq!(feeds().len(), 3);
        assert!(matches!(since(cx, "c4", "c5"), Since::Unknown));
    }

    /// A read at a commit that misses is answered by the same read at an earlier commit of the
    /// branch when no commit in between changed what it depends on: for a node's neighbours,
    /// the edges at the node and the neighbours listed (spec 015).
    #[gpui::test]
    async fn a_read_at_a_new_head_is_answered_from_an_earlier_commit_it_still_holds_for(
        cx: &mut TestAppContext,
    ) {
        use crate::carry::node_reads;
        use crate::test_server::{followed_session, node_change};
        use serde_json::json;

        const SCHEMA: &str =
            "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\n";
        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = sent.clone();
        let (session, served) = followed_session(
            SCHEMA,
            move |request| {
                seen.lock()
                    .unwrap()
                    .push(request["name"].as_str().unwrap_or_default().to_owned());
                json!([{ "id": "p2", "edge_id": "e1", "label": "Grace" }])
            },
            cx,
        );
        cx.run_until_parked();
        let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
        let _watch = session.update(cx, |session, _| {
            session.watch_reads("main", node_reads(&schema, "Person"))
        });
        let group = schema.relationships_of("Person")[0].clone();
        let neighbours = builder::neighbours(
            &schema,
            "Person",
            &group,
            "p1",
            None,
            builder::Narrow::default(),
            51,
        )
        .unwrap();
        let sent_now = |name: &str| sent.lock().unwrap().iter().filter(|n| *n == name).count();
        let read_at = |cx: &mut TestAppContext, commit: &str| {
            let read = session.update(cx, |session, cx| {
                session.read(&neighbours, Some(commit.into()), cx)
            });
            cx.run_until_parked();
            read
        };
        let commit = |cx: &mut TestAppContext, id: &str, changes| {
            served.lock().unwrap().commit(id, changes);
            session.update(cx, |session, cx| session.check_heads(cx));
            cx.run_until_parked();
        };
        let edge = |id: &str, from: &str, to: &str| json!({ "kind": "edge", "type": { "id": "Knows", "name": "Knows" }, "id": id, "op": "insert", "after": { "properties": {}, "endpoints": { "from": from, "to": to } } });

        read_at(cx, "c1").await.unwrap();
        assert_eq!(sent_now("studio_neighbours"), 1);

        // Another person, and an edge between two others.
        commit(
            cx,
            "c2",
            vec![node_change("Person", "p9"), edge("e9", "p7", "p8")],
        );
        let carried = read_at(cx, "c2").await.unwrap();
        assert_eq!(sent_now("studio_neighbours"), 1, "answered from c1");
        assert_eq!(carried.graph_commit_id.as_deref(), Some("c2"));
        assert_eq!(carried.target.snapshot.as_deref(), Some("c2"));

        // A neighbour it lists changed.
        commit(cx, "c3", vec![node_change("Person", "p2")]);
        read_at(cx, "c3").await.unwrap();
        assert_eq!(sent_now("studio_neighbours"), 2, "read again");

        // An edge at the node.
        commit(cx, "c4", vec![edge("e10", "p3", "p1")]);
        read_at(cx, "c4").await.unwrap();
        assert_eq!(sent_now("studio_neighbours"), 3, "read again");
    }

    /// An answer kept at a commit is kept under the schema it was read with: a read at an old
    /// commit uses the current schema, so after the schema changes the same query is read
    /// again. A head move whose feed read finds another schema takes it (spec 015).
    #[gpui::test]
    async fn answers_kept_under_one_schema_are_not_given_under_another(cx: &mut TestAppContext) {
        use crate::carry::{Since, node_reads};
        use crate::test_server::{followed_session, node_change};
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n}\n";
        let sent = Arc::new(std::sync::Mutex::new(0usize));
        let counted = sent.clone();
        let (session, served) = followed_session(
            SCHEMA,
            move |request| {
                if request["name"] == "studio_node" {
                    *counted.lock().unwrap() += 1;
                }
                json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
            },
            cx,
        );
        cx.run_until_parked();
        let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
        let node = builder::node_by_id(&schema, "Person", "p1").unwrap();
        let read_at_c1 = |cx: &mut TestAppContext| {
            let read = session.update(cx, |session, cx| session.read(&node, Some("c1".into()), cx));
            cx.run_until_parked();
            read
        };
        let reads = || *sent.lock().unwrap();

        read_at_c1(cx).await.unwrap();
        read_at_c1(cx).await.unwrap();
        assert_eq!(reads(), 1, "kept at the commit");

        // The schema is read again with another text: the same read at the same commit is sent.
        served.lock().unwrap().schema = format!("{SCHEMA}node Note {{\n  slug: String @key\n}}\n");
        session.update(cx, |session, cx| session.reload_schema(cx));
        cx.run_until_parked();
        read_at_c1(cx).await.unwrap();
        assert_eq!(reads(), 2, "read again under the new schema");

        // Another writer changes the schema; the head move's feed read finds it and the session
        // takes it.
        let _watch = session.update(cx, |session, _| {
            session.watch_reads("main", node_reads(&schema, "Person"))
        });
        let renamed = "node Person {\n  slug: String @key\n  full_name: String @rename_from(\"name\")\n}\nnode Note {\n  slug: String @key\n}\n";
        served.lock().unwrap().schema = renamed.into();
        served
            .lock()
            .unwrap()
            .commit("c2", vec![node_change("Note", "n1")]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        let held = session.read_with(cx, |session, _| {
            session.schema_model().unwrap().source.clone()
        });
        assert_eq!(&*held, renamed, "the new schema is taken");
        assert!(
            matches!(
                session.read_with(cx, |session, _| session.changes_since("main", "c1", "c2")),
                Since::Unknown
            ),
            "nothing carries across a schema change"
        );
        read_at_c1(cx).await.unwrap();
        assert_eq!(reads(), 3, "read again under the renamed schema");
    }
}
