//! One open graph on one server and branch: schema, branches, type counts, and every read and
//! write Studio sends for it (spec 003, 004, 008).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{Either, WeakShared, select};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task};
use gq_language::builder::{Depends, Seeds};
use gq_language::{BuiltQuery, RelationshipGroup, SchemaModel, builder};
use graph_client::api::{
    BranchCreateOutput, BranchCreateRequest, BranchMergeOutput, BranchMergeRequest,
    ChangeFeedOutput, ChangeOutput, ChangeRequest, CommitOutput, DiffOutput, EntityKind,
    GraphListResponse, InvokeStoredQueryRequest, MergeBaseOutput, QueryCatalogEntry, QueryRequest,
    ReadOutput, ReadTargetOutput, ReadinessOutput, SnapshotDatasetOutput, SnapshotOutput,
};
use graph_client::{
    ApiErrorSummary, BlobAt, BlobCell, BlobMeta, CommitChanges, FeedAt, GraphClient, ReadError,
    ServerHealth, WriteOutcome,
};
use serde_json::Value;

use crate::carry::{ChangeLog, FeedFold, Since, answer_unchanged};
use crate::connection::Connection;
use crate::degrees::{DEGREE_BATCH, DegreeBatchKey, DegreeBatches, page_counts};
use crate::memo::{Held, Memo};
use crate::scope::{ReadScope, Scope, ScopeSince};

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

/// A write [`GraphSession::external_write`] counts while it runs. It can travel to the task that
/// sends it, off the main thread.
#[must_use = "a write dropped without its outcome is recorded as unknown"]
pub struct ExternalWrite {
    done: oneshot::Sender<OutcomeKind>,
}

impl ExternalWrite {
    /// Records how the write ended.
    pub fn finish(self, outcome: OutcomeKind) {
        self.done.send(outcome).ok();
    }
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
    /// Another writer changed the schema, seen at a head move. The session reads under the new
    /// one now; a view still showing what it read under the old one offers Refresh, which takes
    /// it. Unlike [`SessionEvent::SchemaChanged`], nothing on screen changes by itself.
    SchemaMoved,
    /// The session's branch is not in a branch list read after it moved there (deleted
    /// elsewhere, or since the last run), so the session moved to main.
    BranchMissing {
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

/// Ids per read of [`GraphSession::node_names`], and the most nodes one call names: a review
/// of 20,000 changes asks a bounded number of small reads, and an end past the limit keeps its
/// id.
const NAME_BATCH: usize = 500;
const NAME_LIMIT: usize = 5_000;

/// How often a session asks after the heads of the branches its views show, while a window of
/// Studio's is active.
const HEAD_POLL: Duration = Duration::from_secs(30);

/// Changes asked for per page of a change-feed read, and the most pages one read takes. A read
/// that stops sooner leaves its remaining commits unknown: the views over them read again.
const FEED_PAGE: u32 = 1_000;
const FEED_PAGES: usize = 8;

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

/// One node's count through a relationship group, at a commit and schema epoch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DegreeAt {
    type_name: String,
    group: RelationshipGroup,
    id: String,
    commit: String,
    schema: u64,
}

/// How many nodes' counts a session keeps.
const DEGREE_COUNTS: usize = 16_384;

/// How many writes' outcomes the activity log keeps: an agent can write thousands of times in
/// one session.
const OUTCOMES: usize = 1_000;

/// Adds `commit` to the commits kept for `family`, the newest last and the last 16 at most. A
/// new family past `cap` takes the place of another: carrying across commits is a saving, not
/// a need, and a table's scroll seeds a family per row, so emptying the map at the cap would
/// forget every other read's commits at once.
fn remember<F: std::hash::Hash + Eq + Clone>(
    map: &mut HashMap<F, Vec<String>>,
    family: F,
    commit: &str,
    cap: usize,
) {
    if map.len() >= cap
        && !map.contains_key(&family)
        && let Some(other) = map.keys().next().cloned()
    {
        map.remove(&other);
    }
    let commits = map.entry(family).or_default();
    if commits.last().map(String::as_str) != Some(commit) {
        commits.retain(|kept| kept != commit);
        commits.push(commit.to_owned());
        if commits.len() > 16 {
            commits.remove(0);
        }
    }
}

/// A query's parameters as a memo key holds them: as sent.
fn params_key(params: &serde_json::Map<String, Value>) -> String {
    if params.is_empty() {
        String::new()
    } else {
        Value::Object(params.clone()).to_string()
    }
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
type ReadMemo<K, V> = Memo<K, V, Pending<V>>;

/// A read under way, as a read memo keeps it: a weak handle, so the read stops once every view
/// that asked for it has dropped it (`counted_read` then gives its slot back to the reads still
/// wanted), and a ticket, so its answer is filed for it alone (`Memo::finish`).
#[derive(Clone)]
pub(crate) struct Pending<V: 'static> {
    read: WeakShared<Task<Result<V, ReadError>>>,
    ticket: u64,
}

pub struct GraphSession {
    pub connection: Entity<Connection>,
    pub graph_id: String,
    pub branch: String,
    pub schema: Loadable<Arc<SchemaModel>>,
    pub branches: Loadable<Vec<String>>,
    pub datasets: Loadable<Vec<SnapshotDatasetOutput>>,
    /// Each type's row count from `datasets`, by name: the sidebar asks for every type each
    /// frame.
    /// Row counts by kind and type: a node type and an edge type may share a name.
    dataset_counts: HashMap<(EntityKind, String), i64>,
    /// The queries the server serves by name for this graph (spec 007), read with the schema.
    pub stored_queries: Loadable<Arc<Vec<QueryCatalogEntry>>>,
    /// Every write's outcome, oldest first: the newest [`OUTCOMES`].
    pub outcomes: Vec<OutcomeEntry>,
    pub reads_in_flight: usize,
    pub writes_in_flight: usize,
    /// Why the graph is unavailable, when it answered 503 (spec 003); reads resume, and this
    /// clears, when it answers again.
    pub unavailable: Option<String>,
    recheck_after: Duration,
    /// Answers to reads pinned to a commit, and those under way (they never change).
    pinned_reads: ReadMemo<PinnedRead, ReadOutput>,
    /// Numbers the reads the memos keep under way (`Pending`).
    next_ticket: u64,
    pinned_blobs: ReadMemo<(BlobCell, String, u64), BlobMeta>,
    /// Each node's count through a group, by commit: read in a batch, or what a page's counts
    /// gave (`builder::Seeds`). Shared by the table, the peek, the tree and the canvas.
    degree_counts: Memo<DegreeAt, u64, ()>,
    /// The commits each read's answer is kept at, by its query, parameters and schema epoch,
    /// and each node's count: where a miss at one commit looks for the same read at another,
    /// rather than going through every answer kept. Entries the memo let go are skipped and
    /// dropped as they are met.
    pinned_commits: HashMap<(String, String, u64), Vec<String>>,
    degree_commits: HashMap<(String, RelationshipGroup, String, u64), Vec<String>>,
    /// Which schema the session reads under: a new number each time the schema's text changes,
    /// the first one included. A read at an old commit uses the current schema, so the same
    /// query at the same commit can mean something else after a rename, a drop or an addition;
    /// every answer kept is kept under its epoch, and nothing crosses one.
    schema_epoch: u64,
    schema_text: Option<Arc<str>>,
    /// Each branch's commit list, read once and shared until a commit lands on that branch.
    commit_lists: ReadMemo<String, CommitList>,
    /// Branches' newest commits, read alone for a list of branches: forgotten with their
    /// commit lists.
    head_commits: ReadMemo<String, Option<CommitOutput>>,
    /// The branch list has been read once; reading it again forgets the commit lists.
    branches_read: bool,
    /// How many times the session has moved to another branch: a branch list asked for before
    /// the last move may not hold a branch made since, so only a later one shows it is gone.
    branch_moves: u64,
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
    /// What each view has read at the commit it shows ([`ReadScope`]), while it holds its scope.
    scopes: HashMap<u64, Scope>,
    next_scope: u64,
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
    /// A counts read is under way, and commits seen meanwhile want one more after it.
    counts_reading: bool,
    counts_again: bool,
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
            dataset_counts: HashMap::new(),
            stored_queries: Loadable::NotLoaded,
            outcomes: Vec::new(),
            reads_in_flight: 0,
            writes_in_flight: 0,
            unavailable: None,
            recheck_after: RECHECK_FIRST,
            pinned_reads: Memo::weighed(PINNED_READS, PINNED_READ_BYTES, read_bytes),
            next_ticket: 0,
            pinned_blobs: Memo::new(PINNED_BLOBS),
            degree_counts: Memo::new(DEGREE_COUNTS),
            pinned_commits: HashMap::new(),
            degree_commits: HashMap::new(),
            schema_epoch: 0,
            schema_text: None,
            commit_lists: Memo::new(COMMIT_LISTS),
            head_commits: Memo::new(COMMIT_LISTS),
            branches_read: false,
            branch_moves: 0,
            degrees: DegreeBatches::default(),
            heads: HashMap::new(),
            drafts: HashMap::new(),
            head_watches: Vec::new(),
            head_reads: HashMap::new(),
            scopes: HashMap::new(),
            next_scope: 0,
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
            counts_reading: false,
            counts_again: false,
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

    /// Reads the graph's stored-query catalog. A graph without stored queries answers an empty
    /// one; a 404 is a graph the server no longer serves, and shows as a failure.
    pub fn reload_stored_queries(&mut self, cx: &mut Context<Self>) {
        self.stored_queries = Loadable::Loading;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let read = cx.background_spawn(async move { client.stored_queries(&graph).await });
        self._stored_queries_task = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match read {
                Ok(output) => Loadable::Loaded(Arc::new(output.queries)),
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
            let parent = parent_of(&outcome, |output: &ChangeOutput| output.commit.as_ref());
            this.update(cx, |this, cx| {
                this.finish_write(branch, description, kind, parent, None, cx);
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
        // Read and compiled off the main thread: a large schema takes a while to compile.
        let read = cx.background_spawn(async move {
            let read = client.schema(&graph).await;
            let parsed = read
                .as_ref()
                .ok()
                .map(|output| parse_schema(&output.schema_source));
            (read.map(|_| ()), parsed)
        });
        self._schema_task = Some(cx.spawn(async move |this, cx| {
            let (read, parsed) = read.await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match (read, parsed) {
                (Ok(()), Some(parsed)) => parsed,
                (Err(error), _) => Loadable::Failed(error.to_string()),
                (Ok(()), None) => Loadable::Failed("The schema could not be read.".into()),
            };
            this.update(cx, |this, cx| this.install_schema(result, cx))
                .ok();
        }));
    }

    /// Takes a schema read from the server at the person's asking, or the first: views take it
    /// at once ([`SessionEvent::SchemaChanged`]).
    fn install_schema(&mut self, schema: Loadable<Arc<SchemaModel>>, cx: &mut Context<Self>) {
        if let Loadable::Loaded(model) = &schema {
            self.start_schema_epoch(model);
        }
        self.schema = schema;
        cx.emit(SessionEvent::SchemaChanged);
        cx.notify();
        self.check_heads(cx);
    }

    /// A text other than the one held starts a new schema epoch: what was kept under the old
    /// one, answers and change logs, is let go.
    fn start_schema_epoch(&mut self, model: &SchemaModel) {
        if self.schema_text.as_deref() == Some(&*model.source) {
            return;
        }
        self.schema_epoch += 1;
        self.schema_text = Some(model.source.clone());
        self.pinned_reads.clear();
        self.pinned_blobs.clear();
        self.degree_counts.clear();
        self.pinned_commits.clear();
        self.degree_commits.clear();
        // Each log starts again at its branch's head, so the next move is read.
        self.change_logs = self
            .heads
            .iter()
            .map(|(branch, head)| (branch.clone(), ChangeLog::starting_at(head.clone())))
            .collect();
    }

    /// Reads the schema again while keeping the one held, and takes it if its text changed: a
    /// head move showed another writer may have changed it.
    fn recheck_schema(
        &mut self,
        read: Option<graph_client::api::SchemaOutput>,
        cx: &mut Context<Self>,
    ) {
        // A reload under way reads the newest schema anyway, and views take it at once; a
        // recheck in its place would cancel it.
        if matches!(self.schema, Loadable::Loading) {
            return;
        }
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let known = self.schema_text.clone();
        let read = cx.background_spawn(async move {
            let read = match read {
                Some(output) => Ok(output),
                None => client.schema(&graph).await,
            };
            // Compiled off the main thread, and only when it is another text.
            let parsed = read
                .as_ref()
                .ok()
                .filter(|output| known.as_deref() != Some(output.schema_source.as_str()))
                .map(|output| parse_schema(&output.schema_source));
            (read.map(|_| ()), parsed)
        });
        self._schema_task = Some(cx.spawn(async move |this, cx| {
            let (read, parsed) = read.await;
            this.update(cx, |this, cx| {
                this.note_read(&read, cx);
                if let Some(Loadable::Loaded(model)) = parsed
                    && this.schema_text.as_deref() != Some(&*model.source)
                {
                    // The session reads under it from now; views take it on Refresh.
                    this.start_schema_epoch(&model);
                    this.schema = Loadable::Loaded(model);
                    cx.emit(SessionEvent::SchemaMoved);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    pub fn reload_branches(&mut self, cx: &mut Context<Self>) {
        self.branches = Loadable::Loading;
        // A poll's list asked for before this would land as the newer one.
        self.branch_recheck = None;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let moves = self.branch_moves;
        let read = cx.background_spawn(async move { client.branches(&graph).await });
        self._branches_task = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
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
                    this.head_commits.clear();
                }
                this.branches_read |= matches!(result, Loadable::Loaded(_));
                this.branches = result;
                cx.emit(SessionEvent::BranchesChanged);
                cx.notify();
                this.leave_missing_branch(moves, cx);
            })
            .ok();
        }));
    }

    /// Moves the session to main when the branch list, asked for after its last move (`moves`),
    /// does not hold its branch: every read of a branch that is gone fails.
    fn leave_missing_branch(&mut self, moves: u64, cx: &mut Context<Self>) {
        let Loadable::Loaded(branches) = &self.branches else {
            return;
        };
        if moves != self.branch_moves || self.branch == "main" || branches.contains(&self.branch) {
            return;
        }
        let branch = self.branch.clone();
        self.set_branch("main".into(), cx);
        cx.emit(SessionEvent::BranchMissing { branch });
    }

    pub fn reload_counts(&mut self, cx: &mut Context<Self>) {
        self.datasets = Loadable::Loading;
        self.dataset_counts.clear();
        self.read_counts(cx);
    }

    /// Reads the counts again after a commit on the session's branch: the ones shown stay until
    /// the new ones are in, rather than blanking every count at each commit.
    /// A burst of commits (an agent's writes) reads them once more after the read under way,
    /// rather than dropping it and sending another at each.
    fn refresh_counts(&mut self, cx: &mut Context<Self>) {
        if self.counts_reading {
            self.counts_again = true;
        } else if matches!(self.datasets, Loadable::Loaded(_)) {
            self.read_counts(cx);
        } else {
            self.reload_counts(cx);
        }
    }

    fn read_counts(&mut self, cx: &mut Context<Self>) {
        self.counts_reading = true;
        self.counts_again = false;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let branch = self.branch.clone();
        let read = cx.background_spawn(async move { client.snapshot(&graph, &branch).await });
        self._datasets_task = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match read {
                Ok(output) => Loadable::Loaded(output.datasets),
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.dataset_counts = match &result {
                    Loadable::Loaded(datasets) => datasets
                        .iter()
                        .map(|dataset| {
                            let key = (dataset.entity_kind, dataset.type_name.clone());
                            (key, dataset.entity_count)
                        })
                        .collect(),
                    _ => HashMap::new(),
                };
                this.datasets = result;
                this.counts_reading = false;
                cx.emit(SessionEvent::CountsChanged);
                cx.notify();
                if this.counts_again {
                    this.refresh_counts(cx);
                }
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

    /// The row count of a node or edge type at the branch head, from the snapshot.
    pub fn count_of(&self, kind: EntityKind, type_name: &str) -> Option<i64> {
        self.dataset_counts
            .get(&(kind, type_name.to_owned()))
            .copied()
    }

    pub fn set_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        if self.branch == branch {
            return;
        }
        self.branch = branch;
        self.branch_moves += 1;
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
        self.drafts_on(&self.branch)
    }

    /// Every unsaved value on `branch`.
    pub fn drafts_on(&self, branch: &str) -> impl Iterator<Item = (&DraftKey, &DraftEntry)> {
        self.drafts.get(branch).into_iter().flatten()
    }

    /// Keeps a property's unsaved value on the session's branch, replacing the one there.
    pub fn set_draft(&mut self, key: DraftKey, entry: DraftEntry, cx: &mut Context<Self>) {
        let branch = self.branch.clone();
        self.set_draft_on(&branch, key, entry, cx);
    }

    /// As [`GraphSession::set_draft`], on `branch`: an edit whose check comes back after the
    /// session moved stays on the branch it was typed on.
    pub fn set_draft_on(
        &mut self,
        branch: &str,
        key: DraftKey,
        entry: DraftEntry,
        cx: &mut Context<Self>,
    ) {
        let drafts = self.drafts.entry(branch.to_owned()).or_default();
        if drafts.get(&key) != Some(&entry) {
            drafts.insert(key, entry);
            cx.emit(SessionEvent::DraftsChanged);
            cx.notify();
        }
    }

    /// Lets a property's unsaved value go: saved, discarded or put back.
    pub fn remove_draft(&mut self, key: &DraftKey, cx: &mut Context<Self>) -> Option<DraftEntry> {
        let branch = self.branch.clone();
        self.remove_draft_on(&branch, key, cx)
    }

    /// As [`GraphSession::remove_draft`], on `branch`: a save that lands after the session moved
    /// to another branch lets go of the draft it saved, not that branch's.
    pub fn remove_draft_on(
        &mut self,
        branch: &str,
        key: &DraftKey,
        cx: &mut Context<Self>,
    ) -> Option<DraftEntry> {
        let removed = self.drafts.get_mut(branch)?.remove(key);
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

    /// Asks after `branch`'s head while the returned watch is held: a branch not watched yet
    /// at once, alone; the others at the next poll.
    pub fn watch_head(&mut self, branch: &str, cx: &mut Context<Self>) -> HeadWatch {
        let alive = Rc::new(());
        let new = !self
            .watched_branches()
            .iter()
            .any(|watched| watched == branch);
        self.head_watches
            .push((branch.to_owned(), Rc::downgrade(&alive)));
        if new && self.unavailable.is_none() && !self.head_reads.contains_key(branch) {
            self.read_head(branch.to_owned(), cx);
        }
        HeadWatch { _alive: alive }
    }

    /// A scope for a view of `branch`: the view reads what it shows through it
    /// ([`GraphSession::read_in`]) while it holds it.
    pub fn read_scope(&mut self, branch: &str) -> ReadScope {
        self.next_scope += 1;
        let (scope, handle) = Scope::new(self.next_scope, branch.to_owned());
        self.scopes.retain(|_, scope| scope.is_alive());
        self.scopes.insert(self.next_scope, scope);
        handle
    }

    /// The view shows none of what it read through `scope` any more, and now reads `branch`.
    pub fn reset_scope(&mut self, scope: &ReadScope, branch: &str) {
        if let Some(scope) = self.scopes.get_mut(&scope.id) {
            scope.reset(branch.to_owned());
        }
    }

    /// Whether `scope`'s branch, moving from the commit the view shows (`from`) to `to`, changed
    /// anything the view read through it.
    pub fn scope_since(&self, scope: &ReadScope, from: &str, to: &str) -> ScopeSince {
        let Some(scope) = self.scopes.get(&scope.id) else {
            return ScopeSince::Changed;
        };
        if from == to {
            return ScopeSince::Unchanged;
        }
        match self.changes_since(&scope.branch, from, to) {
            Since::Pending => ScopeSince::Pending,
            Since::Known(since) if scope.unchanged(&since) => ScopeSince::Unchanged,
            Since::Known(_) | Since::Unknown => ScopeSince::Changed,
        }
    }

    /// [`GraphSession::read`] for what a view shows: `scope` keeps what the answer depends on
    /// from the moment it is asked.
    pub fn read_in(
        &mut self,
        scope: &ReadScope,
        query: &BuiltQuery,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let id = scope.id;
        let entry = self
            .scopes
            .get_mut(&id)
            .map(|scope| scope.add(query.depends.clone()));
        let read = self.read(query, snapshot, cx);
        // The token goes with this task: dropped by the view, the read stops counting as out.
        cx.spawn(async move |this, cx| {
            let result = read.await;
            if let Some((entry, _token)) = entry {
                this.update(cx, |this, _| {
                    if let Some(scope) = this.scopes.get_mut(&id) {
                        match &result {
                            Ok(output) => scope.answered(entry, &output.rows),
                            Err(_) => scope.failed(entry),
                        }
                    }
                })
                .ok();
            }
            result
        })
    }

    /// Adds to what `scope`'s view shows an answer read outside it that depends on `depends`,
    /// with the nodes it lists when known (`None` counts the whole listed type).
    pub fn note_in(&mut self, scope: &ReadScope, depends: Depends, listed: Option<Vec<String>>) {
        if let Some(scope) = self.scopes.get_mut(&scope.id) {
            scope.add_known(depends, listed);
        }
    }

    /// [`GraphSession::degree`] for what a view shows: the count depends on the edges of the
    /// group's type at the node.
    pub fn degree_in(
        &mut self,
        scope: &ReadScope,
        type_name: &str,
        group: &RelationshipGroup,
        id: &str,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<u64, String>> {
        // A count lists no nodes: nothing it says waits for its answer.
        if let Some(scope) = self.scopes.get_mut(&scope.id) {
            let _ = scope.add(Depends::EdgesAt {
                edge: group.edge.clone(),
                nodes: vec![id.to_owned()],
                listed: None,
            });
        }
        self.degree(type_name, group, id, snapshot, cx)
    }

    /// [`GraphSession::blob_meta`] for what a view shows: the cell depends on its node or edge.
    pub fn blob_meta_in(
        &mut self,
        scope: &ReadScope,
        cell: BlobCell,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<BlobMeta, ReadError>> {
        if let Some(scope) = self.scopes.get_mut(&scope.id) {
            let _ = scope.add(if cell.edge {
                Depends::Types {
                    nodes: Vec::new(),
                    edges: vec![cell.type_name.clone()],
                }
            } else {
                Depends::Node {
                    type_name: cell.type_name.clone(),
                    id: cell.id.clone(),
                }
            });
        }
        self.blob_meta(cell, snapshot, cx)
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

    /// The type names what views on `branch` show depends on.
    fn read_names(&mut self, branch: &str) -> BTreeSet<String> {
        self.scopes.retain(|_, scope| scope.is_alive());
        self.scopes
            .values()
            .filter(|scope| scope.branch == branch)
            .flat_map(Scope::names)
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
        // The feed's pages are read and folded off the main thread: each change carries its
        // images, and a page can be large.
        let feed_branch = branch.clone();
        let fed = cx.background_spawn(async move {
            let branch = feed_branch;
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
            (commits, failed, next, schema_read)
        });
        let read = cx.spawn(async move |this, cx| {
            let (commits, failed, next, schema_read) = fed.await;
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
                    // The text just read, when there is one, is not read again.
                    let read = match schema_read {
                        Some(Ok(output)) => Some(output),
                        _ => None,
                    };
                    this.recheck_schema(read, cx);
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
        for branch in watched {
            if !self.head_reads.contains_key(&branch) {
                self.read_head(branch, cx);
            }
        }
    }

    /// Reads `branch`'s head with a read that returns no rows.
    fn read_head(&mut self, branch: String, cx: &mut Context<Self>) {
        let Some(probe) = self
            .schema_model()
            .and_then(|schema| builder::head_probe(&schema))
        else {
            return;
        };
        let request = QueryRequest {
            query: probe.text,
            name: Some(probe.name),
            params: None,
            branch: Some(branch.clone()),
            snapshot: None,
        };
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let key = branch.clone();
        let probe = cx.background_spawn(async move { client.query(&graph, &request).await });
        let read = cx.spawn(async move |this, cx| {
            let read = probe.await;
            this.update(cx, |this, cx| {
                this.head_reads.remove(&branch);
                this.note_read(&read, cx);
                if let Ok(ReadOutput {
                    graph_commit_id: Some(head),
                    ..
                }) = read
                {
                    this.saw_head(branch, head, false, None, cx);
                }
            })
            .ok();
        });
        self.head_reads.insert(key, read);
    }

    /// Reads the branch list again once it has been read, so a branch made or deleted elsewhere
    /// (the CLI, an agent, another window) appears; an unchanged list changes nothing.
    fn recheck_branches(&mut self, cx: &mut Context<Self>) {
        if self.branch_recheck.is_some() || !matches!(self.branches, Loadable::Loaded(_)) {
            return;
        }
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let moves = self.branch_moves;
        let read = cx.background_spawn(async move { client.branches(&graph).await });
        self.branch_recheck = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| {
                this.branch_recheck = None;
                this.note_read(&read, cx);
                let Ok(output) = read else {
                    return;
                };
                let branches = branch_order(output.branches);
                if this.branches.loaded() != Some(&branches) {
                    // As after any branch list read: a branch deleted and made again under its
                    // name has another history.
                    this.commit_lists.clear();
                    this.head_commits.clear();
                    this.branches = Loadable::Loaded(branches);
                    cx.emit(SessionEvent::BranchesChanged);
                    cx.notify();
                }
                this.leave_missing_branch(moves, cx);
            })
            .ok();
        }));
    }

    /// Records `branch`'s head. A move from a head already seen that is not one of Studio's own
    /// commits (`own`) is another writer's commit: the branch's commit list and counts are read
    /// again, as after Studio's own.
    fn saw_head(
        &mut self,
        branch: String,
        head: String,
        own: bool,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if own {
            // A head read sent before this commit could answer with the head before it.
            self.head_reads.remove(&branch);
        }
        let previous = self.heads.insert(branch.clone(), head.clone());
        if previous.as_deref() == Some(head.as_str()) {
            return;
        }
        if own
            && previous.is_some()
            && parent != previous.as_deref()
            && self.watched_branches().contains(&branch)
        {
            // Not on top of the head it replaces, which may be newer: a write that took
            // minutes answers after the watch saw another writer's commit on top of it. The
            // head is read again now rather than at the next poll.
            self.read_head(branch.clone(), cx);
        }
        // Before views hear of the move, so they find the read under way.
        self.read_changes(&branch, cx);
        if previous.is_some() && !own {
            self.commit_lists.forget(&branch);
            self.head_commits.forget(&branch);
            if branch == self.branch {
                self.refresh_counts(cx);
            }
            cx.emit(SessionEvent::Committed {
                branch: branch.clone(),
            });
        }
        cx.emit(SessionEvent::HeadMoved { branch, head });
    }

    /// Keeps what an answer's rows also answer (`builder::Seeds`): whole nodes as their node
    /// reads, counts as each node's count. Nothing already held is replaced, and nothing is kept
    /// when the schema changed while it was read.
    fn seed(&mut self, seeds: &Seeds, output: &ReadOutput, epoch: u64) {
        let (Some(commit), Some(schema)) = (output.graph_commit_id.clone(), self.schema_model())
        else {
            return;
        };
        if epoch != self.schema_epoch {
            return;
        }
        let rows = output
            .rows
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        match seeds {
            Seeds::Nodes { type_name } => {
                for row in rows {
                    let Some(id) = row
                        .get("n")
                        .and_then(|node| node.get("@id"))
                        .and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let Ok(query) = builder::node_by_id(&schema, type_name, id) else {
                        continue;
                    };
                    let key = self.pinned_key(&query, &commit);
                    if !self.pinned_reads.holds(&key) {
                        self.kept_at(&key);
                        let answer = ReadOutput {
                            query_name: query.name,
                            target: ReadTargetOutput {
                                branch: None,
                                snapshot: Some(commit.clone()),
                            },
                            row_count: 1,
                            columns: output.columns.clone(),
                            rows: Value::Array(vec![row.clone()]),
                            graph_commit_id: Some(commit.clone()),
                        };
                        self.pinned_reads.insert(key, answer);
                    }
                }
            }
            Seeds::Degrees {
                type_name,
                group,
                ids,
            } => {
                let Some(counts) = page_counts(&output.rows) else {
                    return;
                };
                for id in ids {
                    let key = DegreeAt {
                        type_name: type_name.clone(),
                        group: group.clone(),
                        id: id.clone(),
                        commit: commit.clone(),
                        schema: epoch,
                    };
                    let n = counts.get(id).copied().unwrap_or(0);
                    self.count_kept_at(&key);
                    self.degree_counts.insert(key, n);
                }
            }
        }
    }

    /// A node's count through a group kept at `key`'s commit, or at an earlier commit of the
    /// session's branch when no edge of the group's type at the node changed in between.
    fn degree_kept(&mut self, key: &DegreeAt) -> Option<u64> {
        if let Some(Held::Done(n)) = self.degree_counts.get(key) {
            return Some(n);
        }
        let family = (
            key.type_name.clone(),
            key.group.clone(),
            key.id.clone(),
            key.schema,
        );
        let commits = self.degree_commits.get(&family)?;
        let mut remaining: HashSet<&str> = commits
            .iter()
            .map(String::as_str)
            .filter(|commit| *commit != key.commit)
            .collect();
        let counts = &self.degree_counts;
        let mut carried = None;
        // One walk back from `key`'s commit gives the range since each kept commit it passes.
        self.change_logs
            .get(&self.branch)?
            .since_each(&key.commit, |start, since| {
                if !remaining.remove(start) {
                    return false;
                }
                let at = DegreeAt {
                    commit: start.to_owned(),
                    ..key.clone()
                };
                if let Some(n) = counts.peek(&at)
                    && !since.edges_at(&key.group.edge, &key.id)
                {
                    carried = Some(*n);
                    return true;
                }
                remaining.is_empty()
            });
        let carried = carried?;
        self.count_kept_at(key);
        self.degree_counts.insert(key.clone(), carried);
        Some(carried)
    }

    /// Notes that a count for `key` is kept, so a miss at another commit finds it.
    fn count_kept_at(&mut self, key: &DegreeAt) {
        let family = (
            key.type_name.clone(),
            key.group.clone(),
            key.id.clone(),
            key.schema,
        );
        remember(&mut self.degree_commits, family, &key.commit, DEGREE_COUNTS);
    }

    /// An answer to the same query and parameters read at an earlier commit of the session's
    /// branch, standing for `key`'s commit because no commit in between changed what it
    /// depends on (spec 015, carry-forward). Answered as read at `key`'s commit.
    fn carried(&mut self, key: &PinnedRead, depends: &Depends) -> Option<ReadOutput> {
        if *depends == Depends::Unknown {
            return None;
        }
        let family = (key.query.clone(), key.params.clone(), key.schema);
        let commits = self.pinned_commits.get(&family)?;
        let reads = &self.pinned_reads;
        let at = |commit: &str| PinnedRead {
            commit: commit.to_owned(),
            ..key.clone()
        };
        let (kept, gone): (Vec<&str>, Vec<&str>) = commits
            .iter()
            .map(String::as_str)
            .filter(|commit| *commit != key.commit)
            .partition(|commit| reads.peek(&at(commit)).is_some());
        let mut remaining: HashSet<&str> = kept.into_iter().collect();
        let gone: Vec<String> = gone.into_iter().map(str::to_owned).collect();
        let mut found = None;
        // One walk back from `key`'s commit gives the range since each kept answer it passes,
        // the newest first.
        if let Some(log) = self.change_logs.get(&self.branch) {
            log.since_each(&key.commit, |start, since| {
                if !remaining.remove(start) {
                    return false;
                }
                if let Some(output) = reads.peek(&at(start))
                    && answer_unchanged(since, depends, &output.rows)
                {
                    let mut output = output.clone();
                    output.graph_commit_id = Some(key.commit.clone());
                    output.target.branch = None;
                    output.target.snapshot = Some(key.commit.clone());
                    found = Some(output);
                    return true;
                }
                remaining.is_empty()
            });
        }
        if !gone.is_empty()
            && let Some(commits) = self.pinned_commits.get_mut(&family)
        {
            commits.retain(|commit| !gone.contains(commit));
        }
        found
    }

    /// The memo key of `query` read at `commit`, under the session's schema.
    fn pinned_key(&self, query: &BuiltQuery, commit: &str) -> PinnedRead {
        PinnedRead {
            commit: commit.to_owned(),
            query: query.text.clone(),
            params: params_key(&query.params),
            schema: self.schema_epoch,
        }
    }

    /// Notes that an answer for `key` is kept, so a miss at another commit finds it.
    fn kept_at(&mut self, key: &PinnedRead) {
        let family = (key.query.clone(), key.params.clone(), key.schema);
        remember(&mut self.pinned_commits, family, &key.commit, PINNED_READS);
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
        // An answer kept at its commit filed what its rows answer when it was read.
        let kept = snapshot.as_deref().is_some_and(|commit| {
            self.pinned_reads
                .peek(&self.pinned_key(query, commit))
                .is_some()
        });
        let read = self.read_kept(query, snapshot, cx);
        let Some(seeds) = query.seeds.clone().filter(|_| !kept) else {
            return read;
        };
        // What its rows also answer is kept too, at the commit and schema it was read under.
        let epoch = self.schema_epoch;
        cx.spawn(async move |this, cx| {
            let result = read.await;
            if let Ok(output) = &result {
                this.update(cx, |this, _| this.seed(&seeds, output, epoch))
                    .ok();
            }
            result
        })
    }

    /// [`GraphSession::read`] of `branch`'s head, though the session moved to another branch
    /// meanwhile: a view bound to the branch it read on reads it again there.
    pub fn read_on(
        &mut self,
        branch: &str,
        query: &BuiltQuery,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        if branch == self.branch {
            return self.read(query, None, cx);
        }
        let request = QueryRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: Some(branch.to_owned()),
            snapshot: None,
        };
        self.read_request(request, cx)
    }

    /// Names of the nodes `ends` lists, `(type, id)`, by their type's label property, read at
    /// `snapshot` (the session's branch when `None`): one read per type of at most
    /// [`NAME_BATCH`] ids, the first [`NAME_LIMIT`] ends only. A node whose label is its key, or
    /// that a read did not find, is left out, and so is every node of a read that failed: a
    /// name only replaces an id its caller already shows.
    pub fn node_names(
        &mut self,
        ends: Vec<(String, String)>,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<crate::NodeNames> {
        let Some(schema) = self.schema_model() else {
            return Task::ready(crate::NodeNames::default());
        };
        let mut by_type: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (type_name, id) in ends.into_iter().take(NAME_LIMIT) {
            by_type.entry(type_name).or_default().push(id);
        }
        let mut reads = Vec::new();
        for (type_name, ids) in by_type {
            for chunk in ids.chunks(NAME_BATCH) {
                if let Ok(Some(query)) = builder::node_labels(&schema, &type_name, chunk) {
                    reads.push((type_name.clone(), self.read(&query, snapshot.clone(), cx)));
                }
            }
        }
        cx.spawn(async move |_, _| {
            let mut names = crate::NodeNames::default();
            for (type_name, read) in reads {
                let Ok(output) = read.await else { continue };
                for row in output.rows.as_array().into_iter().flatten() {
                    if let (Some(id), Some(label)) = (
                        row.get("id").and_then(Value::as_str),
                        row.get("label").and_then(Value::as_str),
                    ) && !label.is_empty()
                    {
                        names.insert((type_name.clone(), id.to_owned()), label.to_owned());
                    }
                }
            }
            names
        })
    }

    /// [`GraphSession::read`] without its seeds.
    fn read_kept(
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
        let Some(commit) = request.snapshot.as_deref() else {
            return self.read_request(request, cx);
        };
        let key = self.pinned_key(query, commit);
        // Neither an answer nor a read someone still waits for: carried from an earlier
        // commit when nothing it depends on changed since.
        let none = match self.pinned_reads.held(&key) {
            None => true,
            Some(Held::Pending(pending)) => pending.read.upgrade().is_none(),
            Some(Held::Done(_)) => false,
        };
        if none && let Some(output) = self.carried(&key, &query.depends) {
            self.kept_at(&key);
            self.pinned_reads.insert(key, output.clone());
            return Task::ready(Ok(output));
        }
        self.share(
            |session| &mut session.pinned_reads,
            key,
            move |session, cx| session.read_at_commit(request, cx),
            Self::kept_at,
            cx,
        )
    }

    /// What `memo` keeps for `key`: its answer, the read under way that someone still waits
    /// for, or a new read `start` begins. Every asker's task files the answer under the read it
    /// shared, and `answered` runs when one is kept. Dropping every asker's task stops the read.
    fn share<K, V>(
        &mut self,
        memo: fn(&mut Self) -> &mut ReadMemo<K, V>,
        key: K,
        start: impl FnOnce(&mut Self, &mut Context<Self>) -> Task<Result<V, ReadError>>,
        answered: fn(&mut Self, &K),
        cx: &mut Context<Self>,
    ) -> Task<Result<V, ReadError>>
    where
        K: std::hash::Hash + Eq + Clone + 'static,
        V: Clone + 'static,
    {
        let shared = match memo(self).get(&key) {
            Some(Held::Done(value)) => return Task::ready(Ok(value)),
            Some(Held::Pending(pending)) => {
                pending.read.upgrade().map(|read| (read, pending.ticket))
            }
            None => None,
        };
        let (read, ticket) = match shared {
            Some(shared) => shared,
            None => {
                let read = start(self, cx).shared();
                self.next_ticket += 1;
                let ticket = self.next_ticket;
                if let Some(weak) = read.downgrade() {
                    memo(self).start(key.clone(), Pending { read: weak, ticket });
                }
                (read, ticket)
            }
        };
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, _| {
                let kept = memo(this).finish(&key, &result, |pending| pending.ticket == ticket);
                if kept && result.is_ok() {
                    answered(this, &key);
                }
            })
            .ok();
            result
        })
    }

    /// A read at `request`'s commit. When that commit is the session branch's head, it is sent
    /// as a read of the branch, which the server answers from its warm caches where a read
    /// pinned to a commit resolves the commit and opens the graph at it, 0.5 s or more slower
    /// (spec 015). Its answer counts only if it comes back at that commit, so the read still
    /// sees exactly the commit asked for; one from a newer head is read again, pinned. Only the
    /// head watch and Studio's own commits move a head, since reads can finish out of order.
    fn read_at_commit(
        &mut self,
        request: QueryRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let Some(commit) = request.snapshot.clone() else {
            return self.read_request(request, cx);
        };
        // A branch whose head is the commit answers the read faster than the commit does: the
        // session's own branch, else another watched one at it (a review's, an agent's).
        let at_head = if self.heads.get(&self.branch) == Some(&commit) {
            Some(self.branch.clone())
        } else {
            self.heads
                .iter()
                .find(|(_, head)| **head == commit)
                .map(|(branch, _)| branch.clone())
        };
        let Some(branch) = at_head else {
            return self.read_request(request, cx);
        };
        let at_branch = QueryRequest {
            branch: Some(branch),
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
    fn counted_read<T: Send + 'static>(
        &mut self,
        request: impl Future<Output = Result<T, ReadError>> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Task<Result<T, ReadError>> {
        self.reads_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let (answer_tx, answer_rx) = oneshot::channel();
        // Held by the caller's task: dropping that task drops this, which stops the read.
        let (waiting_tx, waiting_rx) = oneshot::channel::<std::convert::Infallible>();
        // The request, its body and its decoding run off the main thread, which only takes the
        // answer: a large page or feed decoded there would hold up frames.
        let request = cx.background_spawn(request);
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
            return self.share(
                |session| &mut session.pinned_blobs,
                key,
                move |session, cx| session.blob_meta_unpinned(cell, snapshot, cx),
                |_, _| {},
                cx,
            );
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
        let request = async move {
            let at = match &snapshot {
                Some(commit) => BlobAt::Snapshot(commit),
                None => BlobAt::Branch(&branch),
            };
            client.blob_head(&graph, &cell, at).await
        };
        self.counted_read(request, cx)
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
            self.head_commits.forget(&key);
        }
        let branch = key.clone();
        self.share(
            |session| &mut session.commit_lists,
            key,
            move |session, cx| {
                let client = session.client(cx);
                let graph = session.graph_id.clone();
                session.counted_read(
                    async move {
                        client
                            .commits(&graph, &branch)
                            .await
                            .map(|list| Arc::new(list.commits))
                    },
                    cx,
                )
            },
            |_, _| {},
            cx,
        )
    }

    /// `branch`'s newest commit, as a list of branches shows it: from its commit list when one
    /// is held, else read alone and kept until a commit lands on the branch. A branch with no
    /// commit has none.
    pub fn head_commit(
        &mut self,
        branch: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<CommitOutput>, ReadError>> {
        let key = branch.to_owned();
        if let Some(list) = self.commit_lists.peek(&key) {
            return Task::ready(Ok(list.first().cloned()));
        }
        let branch = key.clone();
        self.share(
            |session| &mut session.head_commits,
            key,
            move |session, cx| {
                let client = session.client(cx);
                let graph = session.graph_id.clone();
                session.counted_read(async move { client.head_commit(&graph, &branch).await }, cx)
            },
            |_, _| {},
            cx,
        )
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

    /// What merging `source` into `target` would do now, or its conflicts: a dry run, which
    /// writes nothing and takes no write gate, so it is a read here, not an outcome.
    pub fn merge_preview(
        &mut self,
        source: &str,
        target: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<graph_client::MergePreview, ReadError>> {
        let (client, graph) = (self.client(cx), self.graph_id.clone());
        let (source, target) = (source.to_owned(), target.to_owned());
        self.counted_read(
            async move { client.merge_preview(&graph, &source, &target).await },
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

    /// One page of the net changes from commit `from` to commit `to`.
    pub fn diff(
        &mut self,
        from: &str,
        to: &str,
        page_token: Option<String>,
        limit: u32,
        cx: &mut Context<Self>,
    ) -> Task<Result<DiffOutput, ReadError>> {
        let (client, graph) = (self.client(cx), self.graph_id.clone());
        let (from, to) = (from.to_owned(), to.to_owned());
        self.counted_read(
            async move {
                client
                    .diff(&graph, &from, &to, page_token.as_deref(), limit)
                    .await
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
        if let Some(commit) = &snapshot {
            let at = DegreeAt {
                type_name: type_name.to_owned(),
                group: group.clone(),
                id: id.to_owned(),
                commit: commit.clone(),
                schema: self.schema_epoch,
            };
            if let Some(n) = self.degree_kept(&at) {
                return Task::ready(Ok(n));
            }
        }
        let (tx, rx) = oneshot::channel();
        let key = DegreeBatchKey {
            type_name: type_name.to_owned(),
            group: group.clone(),
            snapshot,
            branch: self.branch.clone(),
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
            // Asked for at the head of a branch the session has since left: its head is not
            // this branch's, and the asker reads again on the move.
            if key.snapshot.is_none() && key.branch != self.branch {
                for (_, waiters) in nodes {
                    for waiter in waiters {
                        waiter
                            .send(Err(
                                "The branch changed while the count was asked for.".into()
                            ))
                            .ok();
                    }
                }
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
                    .map_err(|error| error.to_string())
                    .and_then(|output| {
                        page_counts(&output.rows)
                            .ok_or_else(|| "The server's counts could not be read.".to_owned())
                    });
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
        let branch = self.branch.clone();
        self.write_on(&branch, query, if_commit, description, None, cx)
    }

    /// [`Self::write`] on `branch`, whose outcome offers `follow_up` once it commits: each write
    /// of a chain goes where the first went, and a dialog's where its checks read, though the
    /// session moved to another branch meanwhile.
    pub fn write_on(
        &mut self,
        branch: &str,
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
            branch: Some(branch.to_owned()),
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
        // A request naming no branch is a branch or commit statement, whose target is in its
        // text: the answer names the branch it changed.
        let statement = request.branch.is_none();
        let asked = request
            .branch
            .clone()
            .unwrap_or_else(|| self.branch.clone());
        self.writes_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let work = cx.spawn(async move |this, cx| {
            let outcome = client.mutate(&graph, &request, if_commit.as_deref()).await;
            let kind = outcome_kind(&outcome, |output: &ChangeOutput| {
                output.commit.as_ref().map(|c| c.graph_commit_id.clone())
            });
            let parent = parent_of(&outcome, |output: &ChangeOutput| output.commit.as_ref());
            let branch = match &outcome {
                WriteOutcome::Committed(output) if statement && !output.branch.is_empty() => {
                    output.branch.clone()
                }
                _ => asked,
            };
            this.update(cx, |this, cx| {
                this.finish_write(branch, description, kind, parent, follow_up, cx);
                // A statement may have made or deleted a branch: the list is read again, and a
                // session on a branch it deleted moves to main.
                if statement {
                    this.reload_branches(cx);
                }
            })
            .ok();
            outcome
        });
        Self::to_outcome(work, cx)
    }

    /// A write sent on the person's behalf outside this session's own calls, as the agent's
    /// CLI writes through Studio's gateway (spec 013), from the moment it is sent: it counts
    /// among the writes in flight until [`ExternalWrite::finish`] records its outcome with
    /// Studio's own writes, and views of the branch learn of its commit. Dropped without an
    /// outcome, as when Studio stops the write's task, it is recorded as unknown.
    /// `creates_branch` reads the branch list again once it went through.
    pub fn external_write(
        &mut self,
        branch: String,
        description: String,
        creates_branch: bool,
        cx: &mut Context<Self>,
    ) -> ExternalWrite {
        let (done, outcome) = oneshot::channel();
        self.writes_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = outcome.await.unwrap_or_else(|_| OutcomeKind::Unknown {
                reason: "Studio stopped before the server answered.".into(),
            });
            this.update(cx, |this, cx| {
                let went_through = matches!(outcome, OutcomeKind::Committed { .. });
                this.finish_write(branch, description, outcome, None, None, cx);
                if creates_branch && went_through {
                    this.reload_branches(cx);
                }
            })
            .ok();
        })
        .detach();
        ExternalWrite { done }
    }

    /// Adds a write's outcome to the log, which keeps the newest [`OUTCOMES`].
    fn log_outcome(
        &mut self,
        branch: String,
        description: String,
        outcome: OutcomeKind,
        follow_up: Option<FollowUp>,
    ) {
        if self.outcomes.len() >= OUTCOMES {
            self.outcomes.drain(..=self.outcomes.len() - OUTCOMES);
        }
        self.outcomes.push(OutcomeEntry {
            at: SystemTime::now(),
            branch,
            description,
            outcome,
            follow_up,
        });
    }

    fn finish_write(
        &mut self,
        branch: String,
        description: String,
        outcome: OutcomeKind,
        parent: Option<String>,
        follow_up: Option<FollowUp>,
        cx: &mut Context<Self>,
    ) {
        self.writes_in_flight = self.writes_in_flight.saturating_sub(1);
        let commit = match &outcome {
            OutcomeKind::Committed { commit } => commit.clone(),
            _ => None,
        };
        self.log_outcome(branch.clone(), description, outcome, follow_up);
        if let Some(commit) = commit {
            self.committed(branch, commit, parent, cx);
        }
        cx.emit(SessionEvent::ActivityChanged);
        cx.notify();
    }

    /// One of Studio's writes committed `commit` on `branch`, over `parent` when the answer
    /// says: it is the branch's head.
    fn committed(
        &mut self,
        branch: String,
        commit: String,
        parent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // The counts are the session's branch's: a commit elsewhere (an agent's branch) leaves
        // them as they are.
        if branch == self.branch {
            self.refresh_counts(cx);
        }
        self.commit_lists.forget(&branch);
        self.head_commits.forget(&branch);
        cx.emit(SessionEvent::Committed {
            branch: branch.clone(),
        });
        self.saw_head(branch, commit, true, parent.as_deref(), cx);
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
                this.finish_write(branch, description, kind, None, None, cx);
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
                // Only a branch that is gone moves the session; a refused or unknown delete
                // leaves it where the person is.
                if this.branch == name && matches!(outcome, WriteOutcome::Committed(_)) {
                    this.set_branch("main".into(), cx);
                }
                this.finish_write(name, description, kind, None, None, cx);
                this.reload_branches(cx);
            })
            .ok();
            outcome
        });
        Self::to_outcome(work, cx)
    }

    /// Merges `source` into `target`, and deletes `source` after a merge that lands when
    /// `delete_branch` is set: the deletion is the server's, reported on the answer, and never
    /// undoes the merge. A session on the deleted branch moves to `target`.
    pub fn merge(
        &mut self,
        source: String,
        target: String,
        delete_branch: bool,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<BranchMergeOutput>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        let description = if delete_branch {
            format!("Merge {source} into {target} and delete {source}")
        } else {
            format!("Merge {source} into {target}")
        };
        let request = BranchMergeRequest {
            source: source.clone(),
            target: Some(target.clone()),
            delete_branch,
            dry_run: false,
        };
        self.writes_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        let work = cx.spawn(async move |this, cx| {
            let outcome = client.merge(&graph, &request).await;
            let kind = outcome_kind(&outcome, |output: &BranchMergeOutput| {
                output.commit.as_ref().map(|c| c.graph_commit_id.clone())
            });
            let parent = parent_of(&outcome, |output: &BranchMergeOutput| {
                output.commit.as_ref()
            });
            let deleted = matches!(
                &outcome,
                WriteOutcome::Committed(BranchMergeOutput {
                    branch_deleted: Some(true),
                    ..
                })
            );
            this.update(cx, |this, cx| {
                this.finish_write(target.clone(), description, kind, parent, None, cx);
                if deleted {
                    if this.branch == source {
                        this.set_branch(target, cx);
                    }
                    this.reload_branches(cx);
                }
            })
            .ok();
            outcome
        });
        Self::to_outcome(work, cx)
    }
}

/// How a write ended, with the commit it published. A write that published none (a branch
/// created or deleted, a merge already up to date, a mutation that matched nothing) is
/// `Committed { commit: None }`: it moves no head and shows no commit.
fn outcome_kind<T>(
    outcome: &WriteOutcome<T>,
    commit: impl Fn(&T) -> Option<String>,
) -> OutcomeKind {
    match outcome {
        WriteOutcome::Committed(value) => OutcomeKind::Committed {
            commit: commit(value).filter(|commit| !commit.is_empty()),
        },
        WriteOutcome::Refused(error) => OutcomeKind::Refused {
            message: error.body.error.clone(),
        },
        WriteOutcome::Unknown { reason } => OutcomeKind::Unknown {
            reason: reason.clone(),
        },
    }
}

/// The parent of the commit a write's answer names.
fn parent_of<T>(
    outcome: &WriteOutcome<T>,
    commit: impl Fn(&T) -> Option<&CommitOutput>,
) -> Option<String> {
    match outcome {
        WriteOutcome::Committed(value) => commit(value)?.parent_commit_id.clone(),
        _ => None,
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

    /// A scope on main showing answers that depend on these node and edge types.
    fn watching(session: &mut GraphSession, nodes: &[&str], edges: &[&str]) -> ReadScope {
        let scope = session.read_scope("main");
        let depends = Depends::Types {
            nodes: nodes.iter().map(|n| n.to_string()).collect(),
            edges: edges.iter().map(|e| e.to_string()).collect(),
        };
        session.note_in(&scope, depends, None);
        scope
    }

    /// A write that published no commit (a branch created or deleted, a merge already up to
    /// date) is done without one: it moves no head and offers no commit to view.
    #[test]
    fn a_write_without_a_commit_names_none() {
        assert!(matches!(
            outcome_kind(&WriteOutcome::Committed(()), |_| None),
            OutcomeKind::Committed { commit: None }
        ));
        assert!(matches!(
            outcome_kind(&WriteOutcome::Committed(()), |_| Some(String::new())),
            OutcomeKind::Committed { commit: None }
        ));
        assert!(matches!(
            outcome_kind(&WriteOutcome::Committed(()), |_| Some("c2".into())),
            OutcomeKind::Committed { commit: Some(commit) } if commit == "c2"
        ));
    }

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

    /// A session on `branch` of a server answering every request with `body`, a branch list.
    fn listing_session(
        body: impl Into<String>,
        branch: &str,
        cx: &mut TestAppContext,
    ) -> Entity<GraphSession> {
        let body: Arc<str> = body.into().into();
        let http = FakeHttpClient::create(move |_request| {
            let body = body.to_string();
            async move {
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
        let branch = branch.to_owned();
        cx.new(|cx| GraphSession::new(connection, "knowledge".into(), branch, cx))
    }

    /// A branch statement names its target only in its text: its outcome is the branch the
    /// answer names, not the session's, and the branch list is read again.
    #[gpui::test]
    async fn a_statement_is_recorded_on_the_branch_it_changed(cx: &mut TestAppContext) {
        let body = serde_json::json!({
            "branch": "release",
            "query_name": "merge",
            "affected_nodes": 0,
            "affected_edges": 0,
            "commit": { "graph_commit_id": "r9", "graph_manifest_version": 9, "created_at": 0 },
            "branches": ["main", "release"],
        });
        let session = listing_session(body.to_string(), "main", cx);
        cx.run_until_parked();
        let write = session.update(cx, |session, cx| {
            session.write_request(
                ChangeRequest {
                    query: "branch merge feature into release".into(),
                    name: None,
                    params: None,
                    branch: None,
                },
                None,
                "Run statement".into(),
                cx,
            )
        });
        write.await;
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.outcomes.last().unwrap().branch, "release");
            assert_eq!(session.head("release"), Some("r9"));
            assert_ne!(session.head("main"), Some("r9"), "main did not move");
        });
    }

    /// A node type and an edge type may share a name; each keeps its own count.
    #[gpui::test]
    async fn a_node_and_an_edge_of_one_name_keep_their_own_counts(cx: &mut TestAppContext) {
        // One answer for every request: the branch list and the snapshot read their fields.
        let dataset = |kind: &str, count: i64| {
            serde_json::json!({
                "entity_kind": kind,
                "type_name": "Knows",
                "dataset_path": "p",
                "published_dataset_version": 1,
                "entity_count": count,
            })
        };
        let body = serde_json::json!({
            "branches": ["main"],
            "graph_branch": "main",
            "graph_manifest_version": 1,
            "internal_schema_version": 14,
            "datasets": [dataset("node", 3), dataset("edge", 7)],
        })
        .to_string();
        let session = listing_session(body, "main", cx);
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.count_of(EntityKind::Node, "Knows"), Some(3));
            assert_eq!(session.count_of(EntityKind::Edge, "Knows"), Some(7));
        });
    }

    /// A commit on the session's branch reads its counts again and shows the old ones until
    /// the new are in; a commit on another branch (an agent's) reads none.
    #[gpui::test]
    async fn counts_stay_shown_while_a_commit_reads_them_again(cx: &mut TestAppContext) {
        use std::sync::atomic::AtomicUsize;
        let snapshots = Arc::new(AtomicUsize::new(0));
        let counted = snapshots.clone();
        // A gate the next snapshot read waits at, once one is set.
        let gate: Arc<std::sync::Mutex<Option<oneshot::Receiver<()>>>> = Arc::default();
        let held = gate.clone();
        let http = FakeHttpClient::create(move |request| {
            let mut wait = None;
            if request.uri().path().ends_with("/snapshot") {
                counted.fetch_add(1, Ordering::SeqCst);
                wait = held.lock().unwrap().take();
            }
            async move {
                if let Some(wait) = wait {
                    wait.await.ok();
                }
                let body = serde_json::json!({
                    "branches": ["main"],
                    "graph_branch": "main",
                    "graph_manifest_version": 1,
                    "internal_schema_version": 14,
                    "datasets": [],
                });
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
        let read = snapshots.load(Ordering::SeqCst);
        let committed = |commit: &str| OutcomeKind::Committed {
            commit: Some(commit.into()),
        };
        session.update(cx, |session, cx| {
            session
                .external_write("agent/x".into(), "w".into(), false, cx)
                .finish(committed("c2"));
        });
        cx.run_until_parked();
        assert_eq!(
            snapshots.load(Ordering::SeqCst),
            read,
            "another branch's commit"
        );
        session.update(cx, |session, cx| {
            session
                .external_write("main".into(), "w".into(), false, cx)
                .finish(committed("c3"));
        });
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert!(
                matches!(session.datasets, Loadable::Loaded(_)),
                "still shown"
            );
        });
        assert_eq!(snapshots.load(Ordering::SeqCst), read + 1);

        // A burst of commits while the counts are read: the read under way is kept, and one
        // more follows it.
        let (open, wait) = oneshot::channel::<()>();
        *gate.lock().unwrap() = Some(wait);
        let before = snapshots.load(Ordering::SeqCst);
        for commit in ["c4", "c5", "c6"] {
            session.update(cx, |session, cx| {
                session
                    .external_write("main".into(), "w".into(), false, cx)
                    .finish(committed(commit));
            });
            cx.run_until_parked();
        }
        assert_eq!(
            snapshots.load(Ordering::SeqCst),
            before + 1,
            "one read under way"
        );
        open.send(()).ok();
        cx.run_until_parked();
        assert_eq!(
            snapshots.load(Ordering::SeqCst),
            before + 2,
            "then one more"
        );
    }

    /// A write sent outside the session's own calls counts as running until its outcome is
    /// recorded; dropped without one, as when Studio stops its task, it is recorded as unknown.
    #[gpui::test]
    async fn an_external_write_counts_until_its_outcome_and_is_unknown_when_dropped(
        cx: &mut TestAppContext,
    ) {
        let session = listing_session(r#"{"branches":["main"]}"#, "main", cx);
        let first = session.update(cx, |session, cx| {
            session.external_write("main".into(), "sent".into(), false, cx)
        });
        let second = session.update(cx, |session, cx| {
            session.external_write("main".into(), "stopped".into(), false, cx)
        });
        session.read_with(cx, |session, _| assert_eq!(session.writes_in_flight, 2));
        first.finish(OutcomeKind::Refused {
            message: "no".into(),
        });
        drop(second);
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.writes_in_flight, 0);
            let outcomes: Vec<_> = session
                .outcomes
                .iter()
                .map(|entry| (entry.description.as_str(), &entry.outcome))
                .collect();
            assert!(matches!(
                outcomes.as_slice(),
                [
                    ("sent", OutcomeKind::Refused { .. }),
                    ("stopped", OutcomeKind::Unknown { .. })
                ]
            ));
        });
    }

    /// The activity log keeps the newest writes.
    #[gpui::test]
    async fn the_activity_log_keeps_the_newest_writes(cx: &mut TestAppContext) {
        let session = listing_session(r#"{"branches":["main"]}"#, "main", cx);
        for n in 0..=OUTCOMES {
            let refused = OutcomeKind::Refused {
                message: "no".into(),
            };
            session.update(cx, |session, cx| {
                session
                    .external_write("main".into(), format!("write {n}"), false, cx)
                    .finish(refused);
            });
            cx.run_until_parked();
        }
        session.read_with(cx, |session, _| {
            assert_eq!(session.outcomes.len(), OUTCOMES);
            assert_eq!(session.outcomes[0].description, "write 1");
            assert_eq!(
                session.outcomes.last().unwrap().description,
                format!("write {OUTCOMES}")
            );
        });
    }

    /// A family past the cap takes one place: the commits kept for the others stay.
    #[test]
    fn a_new_family_past_the_cap_takes_one_place() {
        let mut kept = HashMap::new();
        for family in 0..4 {
            remember(&mut kept, family, "c1", 4);
        }
        remember(&mut kept, 9, "c2", 4);
        assert_eq!(kept.len(), 4);
        assert_eq!(kept[&9], ["c2"]);
        assert_eq!(
            kept.values().filter(|commits| *commits == &["c1"]).count(),
            3
        );
        // A known family's newer commit takes no place.
        remember(&mut kept, 9, "c3", 4);
        assert_eq!(
            (kept.len(), kept[&9].clone()),
            (4, vec!["c2".to_owned(), "c3".to_owned()])
        );
    }

    /// A branch the branch list no longer holds cannot be read: the session moves to main and
    /// says so. A list asked for before the session last moved may not hold a branch made since,
    /// and moves nothing.
    #[gpui::test]
    async fn a_session_on_a_branch_that_is_gone_moves_to_main(cx: &mut TestAppContext) {
        let session = listing_session(r#"{"branches":["main","fix"]}"#, "gone", cx);
        let missing = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let heard = missing.clone();
        cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| {
                if let SessionEvent::BranchMissing { branch } = event {
                    heard.lock().unwrap().push(branch.clone());
                }
            })
            .detach()
        });
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert_eq!(session.branch, "main"));
        assert_eq!(*missing.lock().unwrap(), ["gone"]);

        session.update(cx, |session, cx| {
            let before = session.branch_moves;
            session.set_branch("made".into(), cx);
            session.leave_missing_branch(before, cx);
            assert_eq!(session.branch, "made", "a list from before the move");
            session.set_branch("fix".into(), cx);
            session.leave_missing_branch(session.branch_moves, cx);
            assert_eq!(session.branch, "fix", "a branch the list holds");
        });
        assert_eq!(missing.lock().unwrap().len(), 1);
    }

    /// Drafts belong to the branch they were typed on: another branch shows its own, and the
    /// first one's wait until it is shown again (spec 014).
    #[gpui::test]
    async fn drafts_wait_with_their_branch(cx: &mut TestAppContext) {
        let session = listing_session(r#"{"branches":["main","fix"]}"#, "main", cx);
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
                    let at = snapshot.clone().unwrap_or_else(|| match branch.as_deref() {
                        Some("review") => "r1".to_owned(),
                        _ => head.lock().unwrap().to_string(),
                    });
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

        // At the head of another watched branch (a review's): read through that branch.
        session.update(cx, |session, _| {
            session.heads.insert("review".into(), "r1".into());
        });
        sent.lock().unwrap().clear();
        let read = session.update(cx, |session, cx| {
            session.read(&query(3), Some("r1".into()), cx)
        });
        let output = read.await.unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            vec![(Some("review".to_owned()), None)],
            "sent as a read of the review's branch"
        );
        assert_eq!(output.target.snapshot.as_deref(), Some("r1"));
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

        // A count asked for at the head of a branch the session leaves in the same turn is not
        // read at the new branch's head.
        let at_head = session.update(cx, |session, cx| {
            let count = session.degree("Person", &group("Knows", "Person"), "p-1", None, cx);
            session.set_branch("fix".into(), cx);
            count
        });
        let error = at_head.await.unwrap_err();
        assert!(error.contains("branch changed"), "{error}");
        assert_eq!(reads.lock().unwrap().len(), 2, "nothing was read for it");
    }

    /// The session asks after its branch's head, and each watched branch's, once the schema
    /// loads, at every poll while a window is active, and when a window becomes active after a
    /// poll came while none was. Another writer's commit lands like Studio's own; Studio's own
    /// commit is the head without asking. Each poll reads the branch list too, so a branch made
    /// elsewhere appears.
    /// A branch list read again after a branch was made or deleted is not overwritten by a
    /// poll's list asked for before it, which answers later.
    #[gpui::test]
    async fn a_polls_older_branch_list_does_not_replace_a_reloaded_one(cx: &mut TestAppContext) {
        let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (gate_tx, gate_rx) = futures::channel::oneshot::channel::<()>();
        let gate = Arc::new(std::sync::Mutex::new(Some(gate_rx)));
        let (count, held) = (lists.clone(), gate.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (count, held) = (count.clone(), held.clone());
            async move {
                let body = if path.ends_with("/schema") {
                    serde_json::json!({ "schema_source": "node Person {\n  slug: String @key\n}\n" })
                } else if path.ends_with("/query") {
                    serde_json::json!({ "query_name": "studio_head", "target": { "branch": "main" }, "row_count": 0, "rows": [], "graph_commit_id": "c1" })
                } else if path.ends_with("/branches") {
                    // The first list, then the poll's (held until the reload landed), then the
                    // reload's, made after a branch was.
                    match count.fetch_add(1, Ordering::SeqCst) {
                        0 => serde_json::json!({ "branches": ["main"] }),
                        1 => {
                            let gate = held.lock().unwrap().take().unwrap();
                            gate.await.ok();
                            serde_json::json!({ "branches": ["main"] })
                        }
                        _ => serde_json::json!({ "branches": ["main", "made"] }),
                    }
                } else {
                    serde_json::json!({})
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
        let listed = |cx: &mut TestAppContext| {
            session.read_with(cx, |session, _| session.branches.loaded().cloned())
        };
        assert_eq!(listed(cx), Some(vec!["main".to_owned()]));

        // The poll asks; a branch is made meanwhile and the list is read again.
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        assert_eq!(listed(cx), Some(vec!["main".to_owned(), "made".to_owned()]));

        // The poll's answer, asked for before the branch was made, comes last.
        gate_tx.send(()).ok();
        cx.run_until_parked();
        assert_eq!(
            listed(cx),
            Some(vec!["main".to_owned(), "made".to_owned()]),
            "the newer list stays"
        );
    }

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

        // A watched branch is asked after at once, alone, then with the session's own at each
        // poll. A branch made elsewhere appears at the poll.
        probes.lock().unwrap().clear();
        let watch = session.update(cx, |session, cx| session.watch_head("feature", cx));
        cx.run_until_parked();
        assert_eq!(head_of(cx, "feature").as_deref(), Some("f1"));
        assert_eq!(
            *probes.lock().unwrap(),
            vec!["feature"],
            "only the new branch"
        );
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

        // A graph the server no longer serves: the catalog failed, it is not empty.
        serves.store(false, Ordering::SeqCst);
        session.update(cx, |session, cx| session.reload_stored_queries(cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert!(matches!(session.stored_queries, Loadable::Failed(_)))
        });
    }

    /// A list of branches reads each branch's newest commit alone, once, and again after a
    /// commit lands on it; a branch whose commit list is held reads nothing.
    #[gpui::test]
    async fn a_branch_head_is_read_alone_and_from_a_held_list(cx: &mut TestAppContext) {
        let lists: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let seen = lists.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let seen = seen.clone();
            async move {
                let body = if path.ends_with("/commits") {
                    seen.lock().unwrap().push(query);
                    r#"{"commits":[{"graph_commit_id":"c2","created_at":2,"graph_manifest_version":2,"published":true},{"graph_commit_id":"c1","created_at":1,"graph_manifest_version":1,"published":true}]}"#
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
        cx.run_until_parked();
        let asked = || std::mem::take(&mut *lists.lock().unwrap());
        asked();
        let head = |session: &Entity<GraphSession>, branch: &str, cx: &mut TestAppContext| {
            let read = session.update(cx, |session, cx| session.head_commit(branch, cx));
            cx.run_until_parked();
            futures::executor::block_on(read)
                .unwrap()
                .map(|commit| commit.graph_commit_id)
        };

        assert_eq!(head(&session, "main", cx).as_deref(), Some("c2"));
        assert_eq!(asked(), vec!["branch=main&limit=1"], "the head alone");
        assert_eq!(head(&session, "main", cx).as_deref(), Some("c2"));
        assert!(asked().is_empty(), "kept");

        // A branch whose whole list is held: its first commit, nothing read.
        let list = session.update(cx, |session, cx| session.commits("feature", false, cx));
        cx.run_until_parked();
        futures::executor::block_on(list).unwrap();
        asked();
        assert_eq!(head(&session, "feature", cx).as_deref(), Some("c2"));
        assert!(asked().is_empty(), "from the list");

        // A commit on main: its head is read again.
        session.update(cx, |session, cx| {
            session
                .external_write("main".into(), "w".into(), false, cx)
                .finish(OutcomeKind::Committed {
                    commit: Some("c3".into()),
                });
        });
        cx.run_until_parked();
        asked();
        head(&session, "main", cx);
        assert_eq!(asked(), vec!["branch=main&limit=1"]);
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

        // A read at a commit is kept under way for whoever asks next, but only while someone
        // waits: dropped by every asker, it stops too, and the next ask reads again.
        let pinned = BuiltQuery {
            name: "q".into(),
            text: "query q() { match { $p: Person } return { $p } }".into(),
            params: serde_json::Map::new(),
            depends: Depends::Unknown,
            seeds: None,
        };
        let asked = queries.load(Ordering::SeqCst);
        let read = session.update(cx, |session, cx| {
            session.read(&pinned, Some("c-old".into()), cx)
        });
        cx.run_until_parked();
        assert_eq!(queries.load(Ordering::SeqCst), asked + 1);
        session.read_with(cx, |session, _| assert_eq!(session.reads_in_flight, 1));
        drop(read);
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert_eq!(session.reads_in_flight, 0));
        let again = session.update(cx, |session, cx| {
            session.read(&pinned, Some("c-old".into()), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            queries.load(Ordering::SeqCst),
            asked + 2,
            "read again, not left waiting on the dropped one"
        );
        drop(again);
        cx.run_until_parked();

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

    /// A write that answers after the watch saw a newer commit on top of it does not leave the
    /// head on the older one: the head is read again at once.
    #[gpui::test]
    async fn a_late_answer_of_studios_own_write_does_not_leave_an_older_head(
        cx: &mut TestAppContext,
    ) {
        use crate::test_server::followed_session;
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n}\n";
        let (session, served) = followed_session(SCHEMA, |_| json!([]), cx);
        cx.run_until_parked();
        let _watch = session.update(cx, |session, _| watching(session, &["Person"], &[]));
        // Studio's write commits m; another writer commits n on top; the watch sees n first.
        served.lock().unwrap().commit("m", Vec::new());
        served.lock().unwrap().commit("n", Vec::new());
        session.update(cx, |session, cx| {
            session.saw_head("main".into(), "n".into(), false, None, cx)
        });
        cx.run_until_parked();
        session.update(cx, |session, cx| {
            session.committed("main".into(), "m".into(), Some("c1".into()), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            session.read_with(cx, |session, _| session.head("main").map(str::to_owned)),
            Some("n".into())
        );
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
                session.saw_head("main".into(), id.into(), false, None, cx)
            });
        };
        let since = |cx: &mut TestAppContext, from: &str, to: &str| {
            session.read_with(cx, |session, _| session.changes_since("main", from, to))
        };
        let feeds = || served.lock().unwrap().feeds.clone();
        let person = TypeRef::Node("Person".into());
        let company = TypeRef::Node("Company".into());

        let watch = session.update(cx, |session, _| watching(session, &["Person"], &[]));
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
        let _watch = session.update(cx, |session, _| watching(session, &["Person"], &[]));
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
        let _watch = session.update(cx, |session, _| watching(session, &["Person"], &["Knows"]));
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
    /// A page's rows answer the read of each node at that commit, with no request of their own;
    /// a page answered from what was kept is not filed again.
    #[gpui::test]
    async fn a_page_read_at_a_commit_answers_its_nodes(cx: &mut TestAppContext) {
        use crate::test_server::followed_session;
        use serde_json::json;

        const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n}\n";
        let sent = Arc::new(std::sync::Mutex::new(0usize));
        let counted = sent.clone();
        let (session, _) = followed_session(
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
        let page = builder::node_page(&schema, "Person", &[], None, &builder::PageStart::First, 10)
            .unwrap();
        let node = builder::node_by_id(&schema, "Person", "p1").unwrap();
        let read = |query: &BuiltQuery, cx: &mut TestAppContext| {
            let read = session.update(cx, |session, cx| session.read(query, Some("c1".into()), cx));
            cx.run_until_parked();
            read
        };
        let reads = || *sent.lock().unwrap();

        read(&page, cx).await.unwrap();
        read(&node, cx).await.unwrap();
        assert_eq!(reads(), 0, "the page's row answered it");

        // The node's answer goes; the page, read again from what was kept, files nothing.
        session.update(cx, |session, _| {
            let key = session.pinned_key(&node, "c1");
            session.pinned_reads.forget(&key);
        });
        read(&page, cx).await.unwrap();
        read(&node, cx).await.unwrap();
        assert_eq!(reads(), 1);
    }

    #[gpui::test]
    async fn answers_kept_under_one_schema_are_not_given_under_another(cx: &mut TestAppContext) {
        use crate::carry::Since;
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
        // takes it, telling views it moved rather than changing them.
        let _watch = session.update(cx, |session, _| watching(session, &["Person"], &["Knows"]));
        let events = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let heard = events.clone();
        cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| match event {
                SessionEvent::SchemaChanged => heard.lock().unwrap().push("changed"),
                SessionEvent::SchemaMoved => heard.lock().unwrap().push("moved"),
                _ => {}
            })
            .detach()
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
        assert_eq!(*events.lock().unwrap(), ["moved"], "views are not changed");
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

    /// What a read's rows also answer is kept: a page's whole nodes answer their node reads,
    /// and a page's counts each node's count, at the commit read. A count carries to a later
    /// commit that changed no edge of its group at the node (spec 015).
    #[gpui::test]
    async fn a_pages_rows_answer_node_reads_and_its_counts_each_nodes_count(
        cx: &mut TestAppContext,
    ) {
        use crate::test_server::{followed_session, node_change};
        use serde_json::json;

        const SCHEMA: &str =
            "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\n";
        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = sent.clone();
        let (session, served) = followed_session(
            SCHEMA,
            move |request| {
                let name = request["name"].as_str().unwrap_or_default().to_owned();
                seen.lock().unwrap().push(name.clone());
                match name.as_str() {
                    "studio_page" => json!([
                        { "n": { "@id": "p1", "slug": "p1", "name": "Ada" } },
                        { "n": { "@id": "p2", "slug": "p2", "name": "Grace" } },
                    ]),
                    "studio_page_degrees" => json!([{ "id": "p1", "n": 3 }]),
                    _ => json!([]),
                }
            },
            cx,
        );
        cx.run_until_parked();
        let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
        let sent_now = |name: &str| sent.lock().unwrap().iter().filter(|n| *n == name).count();
        let page = builder::node_page(&schema, "Person", &[], None, &builder::PageStart::First, 50)
            .unwrap();
        let read = session.update(cx, |session, cx| session.read(&page, None, cx));
        cx.run_until_parked();
        read.await.unwrap();

        let node = builder::node_by_id(&schema, "Person", "p2").unwrap();
        let read = session.update(cx, |session, cx| session.read(&node, Some("c1".into()), cx));
        let output = read.await.unwrap();
        assert_eq!(sent_now("studio_node"), 0, "the page answered it");
        assert_eq!(output.rows[0]["n"]["name"], json!("Grace"));

        let group = schema.relationships_of("Person")[0].clone();
        let ids = vec!["p1".to_owned(), "p2".to_owned()];
        let degrees = builder::page_degrees(&schema, "Person", &group, &ids).unwrap();
        let read = session.update(cx, |session, cx| {
            session.read(&degrees, Some("c1".into()), cx)
        });
        cx.run_until_parked();
        read.await.unwrap();
        let count = |cx: &mut TestAppContext, id: &str, commit: &str| {
            let read = session.update(cx, |session, cx| {
                session.degree("Person", &group, id, Some(commit.into()), cx)
            });
            cx.run_until_parked();
            read
        };
        assert_eq!(count(cx, "p1", "c1").await, Ok(3));
        assert_eq!(count(cx, "p2", "c1").await, Ok(0), "no row is none");
        assert_eq!(
            sent_now("studio_page_degrees"),
            1,
            "the counts were the page's"
        );

        // Another person changed, no edge at p1: its count carries.
        let _scope = session.update(cx, |session, _| watching(session, &["Person"], &["Knows"]));
        served
            .lock()
            .unwrap()
            .commit("c2", vec![node_change("Person", "p9")]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(count(cx, "p1", "c2").await, Ok(3));
        assert_eq!(sent_now("studio_page_degrees"), 1, "carried, not read");
    }
}
