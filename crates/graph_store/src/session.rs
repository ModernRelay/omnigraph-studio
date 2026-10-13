//! One open graph on one server and branch: schema, branches, type counts, and every read and
//! write Studio sends for it (spec 003, 004, 008).

use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{Either, Shared, select};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task};
use gq_language::{BuiltQuery, RelationshipGroup, SchemaModel, builder};
use graph_client::api::{
    BranchCreateOutput, BranchCreateRequest, BranchMergeOutput, BranchMergeRequest, ChangeOutput,
    ChangeRequest, CommitOutput, QueryRequest, ReadOutput, SnapshotDatasetOutput,
};
use graph_client::{BlobAt, BlobCell, BlobMeta, GraphClient, ReadError, WriteOutcome};

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

#[derive(Debug, Clone)]
pub struct OutcomeEntry {
    pub at: SystemTime,
    pub branch: String,
    pub description: String,
    pub outcome: OutcomeKind,
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
}

/// How many answers to reads pinned to a commit, and Blob cells at a commit, a session keeps.
const PINNED_READS: usize = 256;
const PINNED_BLOBS: usize = 2_048;

/// How many branches' commit lists a session keeps.
const COMMIT_LISTS: usize = 64;

/// How often a session asks after the heads of the branches its views show, while a window of
/// Studio's is active.
pub const HEAD_POLL: Duration = Duration::from_secs(30);

/// A view's interest in a branch's head: the session asks after the branch while one is held.
/// The session's own branch is always asked after.
pub struct HeadWatch {
    _alive: Rc<()>,
}

/// A branch's commits, newest first, shared by the views that show them.
pub type CommitList = Arc<Vec<CommitOutput>>;

/// A read at a commit: the commit, the query text and its parameters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PinnedRead {
    commit: String,
    query: String,
    params: String,
}

/// The first wait before a graph that answered 503 is asked again; it doubles up to the cap.
const RECHECK_FIRST: Duration = Duration::from_secs(5);
const RECHECK_CAP: Duration = Duration::from_secs(60);

/// Answers kept by key, with the read under way shared by everyone who asks meanwhile.
type ReadMemo<K, V> = Memo<K, V, Shared<Task<Result<V, ReadError>>>>;

pub struct GraphSession {
    pub connection: Entity<Connection>,
    pub graph_id: String,
    pub branch: String,
    pub schema: Loadable<Arc<SchemaModel>>,
    pub branches: Loadable<Vec<String>>,
    pub datasets: Loadable<Vec<SnapshotDatasetOutput>>,
    pub outcomes: Vec<OutcomeEntry>,
    pub reads_in_flight: usize,
    pub writes_in_flight: usize,
    /// Why the graph is unavailable, when it answered 503 (spec 003); reads resume, and this
    /// clears, when it answers again.
    pub unavailable: Option<String>,
    recheck_after: Duration,
    /// Answers to reads pinned to a commit, and those under way (they never change).
    pinned_reads: ReadMemo<PinnedRead, ReadOutput>,
    pinned_blobs: ReadMemo<(BlobCell, String), BlobMeta>,
    /// Each branch's commit list, read once and shared until a commit lands on that branch.
    commit_lists: ReadMemo<String, CommitList>,
    /// The branch list has been read once; reading it again forgets the commit lists.
    branches_read: bool,
    /// Relationship counts asked for this turn, read together when it ends.
    degrees: DegreeBatches,
    /// The commit each watched branch's head was last seen at, by the head watch or as one of
    /// Studio's own commits.
    heads: HashMap<String, String>,
    /// Branches views asked after besides the session's own, while their watch is held.
    head_watches: Vec<(String, Weak<()>)>,
    /// Head reads under way, by branch.
    head_reads: HashMap<String, Task<()>>,
    /// The branch list read again by the head watch, while under way.
    branch_recheck: Option<Task<()>>,
    /// A poll came while no window was active; the next activation asks at once.
    head_check_missed: bool,
    _head_poll: Option<Task<()>>,
    _recheck: Option<Task<()>>,
    _schema_task: Option<Task<()>>,
    _branches_task: Option<Task<()>>,
    _datasets_task: Option<Task<()>>,
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
            outcomes: Vec::new(),
            reads_in_flight: 0,
            writes_in_flight: 0,
            unavailable: None,
            recheck_after: RECHECK_FIRST,
            pinned_reads: Memo::new(PINNED_READS),
            pinned_blobs: Memo::new(PINNED_BLOBS),
            commit_lists: Memo::new(COMMIT_LISTS),
            branches_read: false,
            degrees: DegreeBatches::default(),
            heads: HashMap::new(),
            head_watches: Vec::new(),
            head_reads: HashMap::new(),
            branch_recheck: None,
            head_check_missed: false,
            _head_poll: None,
            _recheck: None,
            _schema_task: None,
            _branches_task: None,
            _datasets_task: None,
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

    pub fn client(&self, cx: &App) -> GraphClient {
        self.connection.read(cx).client().clone()
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

    /// Reloads the schema, the branch list and the type counts.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_schema(cx);
        self.reload_branches(cx);
        self.reload_counts(cx);
    }

    pub fn reload_schema(&mut self, cx: &mut Context<Self>) {
        self.schema = Loadable::Loading;
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        self._schema_task = Some(cx.spawn(async move |this, cx| {
            let read = client.schema(&graph).await;
            this.update(cx, |this, cx| this.note_read(&read, cx)).ok();
            let result = match read {
                Ok(output) => match SchemaModel::parse(&output.schema_source) {
                    Ok(model) => Loadable::Loaded(Arc::new(model)),
                    Err(error) => {
                        Loadable::Failed(format!("Studio could not read the schema: {error}"))
                    }
                },
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.schema = result;
                cx.emit(SessionEvent::SchemaChanged);
                cx.notify();
                this.check_heads(cx);
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
        };
        let shared = match self.pinned_reads.get(&key) {
            Some(Held::Done(output)) => return Task::ready(Ok(output)),
            Some(Held::Pending(shared)) => shared,
            None => {
                let shared = self.read_request(request, cx).shared();
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
            let key = (cell.clone(), commit);
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
        let request = ChangeRequest {
            query: query.text.clone(),
            name: Some(query.name.clone()),
            params: query.params_value(),
            branch: Some(self.branch.clone()),
        };
        self.write_request(request, if_commit, description, cx)
    }

    pub fn write_request(
        &mut self,
        request: ChangeRequest,
        if_commit: Option<String>,
        description: String,
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
                this.finish_write(branch, description, kind, cx);
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
                this.finish_write(branch, description, kind, cx);
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
                this.finish_write(name, description, kind, cx);
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
                this.finish_write(target, description, kind, cx);
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
}
