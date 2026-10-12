//! One open graph on one server and branch: schema, branches, type counts, and every read and
//! write Studio sends for it (spec 003, 004, 008).

use std::sync::Arc;
use std::time::SystemTime;

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task};
use gq_language::{BuiltQuery, SchemaModel};
use graph_client::api::{
    BranchCreateOutput, BranchCreateRequest, BranchMergeOutput, BranchMergeRequest, ChangeOutput,
    ChangeRequest, QueryRequest, ReadOutput, SnapshotDatasetOutput,
};
use graph_client::{GraphClient, ReadError, WriteOutcome};

use crate::connection::Connection;

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
}

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
            let result = match client.schema(&graph).await {
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
            let result = match client.branches(&graph).await {
                Ok(output) => {
                    let mut branches = output.branches;
                    branches.sort_by(|a, b| (a != "main").cmp(&(b != "main")).then(a.cmp(b)));
                    Loadable::Loaded(branches)
                }
                Err(error) => Loadable::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
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
            let result = match client.snapshot(&graph, &branch).await {
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

    /// Runs a read on the session's branch, or at `snapshot` when given (constitution II).
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
        self.read_request(request, cx)
    }

    pub fn read_request(
        &mut self,
        request: QueryRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let client = self.client(cx);
        let graph = self.graph_id.clone();
        self.reads_in_flight += 1;
        cx.emit(SessionEvent::ActivityChanged);
        cx.spawn(async move |this, cx| {
            let result = client.query(&graph, &request).await;
            this.update(cx, |this, cx| {
                this.reads_in_flight = this.reads_in_flight.saturating_sub(1);
                cx.emit(SessionEvent::ActivityChanged);
                cx.notify();
            })
            .ok();
            result
        })
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
        cx.spawn(async move |this, cx| {
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
        })
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
        cx.spawn(async move |this, cx| {
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
        })
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
        cx.spawn(async move |this, cx| {
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
        })
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
        cx.spawn(async move |this, cx| {
            let outcome = client.merge(&graph, &request).await;
            let kind = outcome_kind(&outcome, |output: &BranchMergeOutput| {
                output.commit.as_ref().map(|c| c.graph_commit_id.clone())
            });
            this.update(cx, |this, cx| {
                this.finish_write(target, description, kind, cx);
            })
            .ok();
            outcome
        })
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
