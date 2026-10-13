//! One open graph on one server and branch: schema, branches, type counts, and every read and
//! write Studio sends for it (spec 003, 004, 008).

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{Either, Shared, select};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task};
use gq_language::{BuiltQuery, SchemaModel};
use graph_client::api::{
    BranchCreateOutput, BranchCreateRequest, BranchMergeOutput, BranchMergeRequest, ChangeOutput,
    ChangeRequest, CommitOutput, QueryRequest, ReadOutput, SnapshotDatasetOutput,
};
use graph_client::{BlobAt, BlobCell, BlobMeta, GraphClient, ReadError, WriteOutcome};

use crate::connection::Connection;
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
    /// A write on `branch` committed; views reading that branch may offer Refresh.
    Committed {
        branch: String,
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
    pinned_reads: Memo<PinnedRead, ReadOutput, Shared<Task<Result<ReadOutput, ReadError>>>>,
    pinned_blobs: Memo<(BlobCell, String), BlobMeta, Shared<Task<Result<BlobMeta, ReadError>>>>,
    /// Each branch's commit list, read once and shared until a commit lands on that branch.
    commit_lists: Memo<String, CommitList, Shared<Task<Result<CommitList, ReadError>>>>,
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
            _recheck: None,
            _schema_task: None,
            _branches_task: None,
            _datasets_task: None,
        };
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
                Ok(output) => {
                    let mut branches = output.branches;
                    branches.sort_by(|a, b| (a != "main").cmp(&(b != "main")).then(a.cmp(b)));
                    Loadable::Loaded(branches)
                }
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.branches = result;
                // A branch deleted and made again under its name has another history.
                this.commit_lists.clear();
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

    fn finish_write(
        &mut self,
        branch: String,
        description: String,
        outcome: OutcomeKind,
        cx: &mut Context<Self>,
    ) {
        self.writes_in_flight = self.writes_in_flight.saturating_sub(1);
        let committed = matches!(outcome, OutcomeKind::Committed { commit: Some(_) });
        self.outcomes.push(OutcomeEntry {
            at: SystemTime::now(),
            branch: branch.clone(),
            description,
            outcome,
        });
        if committed {
            self.reload_counts(cx);
            self.commit_lists.forget(&branch);
            cx.emit(SessionEvent::Committed { branch });
        }
        cx.emit(SessionEvent::ActivityChanged);
        cx.notify();
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

    /// A branch's commit list is read once for every view that shows it, and again only after
    /// a commit lands on that branch.
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
        cx.run_until_parked();
        let reads_of = |branch: &str| {
            lists
                .lock()
                .unwrap()
                .iter()
                .filter(|query| query.contains(&format!("branch={branch}")))
                .count()
        };

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
