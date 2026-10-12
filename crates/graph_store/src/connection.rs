//! One OmniGraph server: its client, contract check, capabilities and graphs (spec 003).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, EventEmitter, Task};
use graph_client::api::{GraphInfo, HealthOutput};
use graph_client::{GraphClient, ReadError};
use http_client::HttpClient;
use studio_settings::StudioSettings;

/// Where Studio learned about a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerSource {
    /// `~/.omnigraph/config.yaml`; the token is resolved as the CLI resolves it.
    Cli,
    /// Added in Studio; the token is in the OS keychain.
    Studio,
    /// `OMNIGRAPH_STUDIO_SERVER` in the environment, for development and tests.
    Environment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEntry {
    pub name: String,
    pub url: String,
    pub source: ServerSource,
    /// Graph ids known without listing: CLI profiles and ids opened before.
    pub known_graphs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Idle,
    Connecting,
    Ready,
    ContractMismatch { served: Option<String> },
    Unauthorized,
    Unreachable(String),
}

impl ConnectionState {
    /// The status-bar text for this state.
    pub fn label(&self) -> String {
        match self {
            ConnectionState::Idle => "Not connected".into(),
            ConnectionState::Connecting => "Connecting…".into(),
            ConnectionState::Ready => format!("contract {}", graph_client::SUPPORTED_CONTRACTS[0]),
            ConnectionState::ContractMismatch { served } => match served {
                Some(served) => format!("Server speaks {served}; Studio speaks 0.13"),
                None => "Server did not say which contract it speaks".into(),
            },
            ConnectionState::Unauthorized => "Token refused".into(),
            ConnectionState::Unreachable(_) => "Unreachable".into(),
        }
    }
}

/// What the server said about its graphs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphListing {
    Unknown,
    Listed(Vec<GraphInfo>),
    /// The token may not list graphs (`graph_list` needs a cluster policy); graph ids come from
    /// the CLI's profiles or are typed in.
    NotAllowed,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionEvent {
    StateChanged,
}

pub struct Connection {
    pub server: ServerEntry,
    client: GraphClient,
    http: Arc<dyn HttpClient>,
    pub state: ConnectionState,
    pub health: Option<HealthOutput>,
    pub operations: BTreeSet<String>,
    pub graphs: GraphListing,
    has_token: bool,
    _task: Option<Task<()>>,
    /// The next health re-check (spec 003).
    _health_check: Option<Task<()>>,
}

/// Health re-check interval after a failure (spec 003).
const FAILED_CHECK_INTERVAL: Duration = Duration::from_secs(120);
/// Interval when settings are not loaded, as in tests.
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(30);

impl EventEmitter<ConnectionEvent> for Connection {}

impl Connection {
    pub fn new(
        server: ServerEntry,
        token: Option<String>,
        http: Arc<dyn HttpClient>,
    ) -> anyhow::Result<Self> {
        let has_token = token.is_some();
        let client = GraphClient::new(&server.url, token, http.clone())?;
        Ok(Self {
            server,
            client,
            http,
            state: ConnectionState::Idle,
            health: None,
            operations: BTreeSet::new(),
            graphs: GraphListing::Unknown,
            has_token,
            _task: None,
            _health_check: None,
        })
    }

    pub fn client(&self) -> &GraphClient {
        &self.client
    }

    pub fn has_token(&self) -> bool {
        self.has_token
    }

    /// Whether the server's OpenAPI document advertises an operation (constitution X).
    pub fn supports(&self, operation_id: &str) -> bool {
        self.operations.contains(operation_id)
    }

    /// Graph ids to offer: the listed graphs, then known ids the listing lacks.
    pub fn graph_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = match &self.graphs {
            GraphListing::Listed(graphs) => graphs.iter().map(|g| g.graph_id.clone()).collect(),
            _ => Vec::new(),
        };
        for id in &self.server.known_graphs {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }

    pub fn graph_info(&self, graph_id: &str) -> Option<&GraphInfo> {
        match &self.graphs {
            GraphListing::Listed(graphs) => graphs.iter().find(|g| g.graph_id == graph_id),
            _ => None,
        }
    }

    pub fn remember_graph(&mut self, graph_id: &str) {
        if !self.server.known_graphs.iter().any(|g| g == graph_id) {
            self.server.known_graphs.push(graph_id.to_owned());
        }
    }

    /// Replaces the token after the server refused the old one (spec 003); requests made after
    /// this use it, and the connection checks again.
    pub fn set_token(
        &mut self,
        token: Option<String>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        self.client = GraphClient::new(&self.server.url, token.clone(), self.http.clone())?;
        self.has_token = token.is_some();
        self.connect(cx);
        Ok(())
    }

    /// Stops a connection attempt that is taking too long.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.state == ConnectionState::Connecting {
            self._task = None;
            self.state = ConnectionState::Idle;
            cx.emit(ConnectionEvent::StateChanged);
            cx.notify();
        }
    }

    /// Checks health and contract, then reads capabilities and the graph list.
    pub fn connect(&mut self, cx: &mut Context<Self>) {
        self.state = ConnectionState::Connecting;
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
        let client = self.client.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let outcome = probe(&client).await;
            this.update(cx, |this, cx| {
                match outcome {
                    Probe::Ready {
                        health,
                        operations,
                        graphs,
                    } => {
                        this.health = Some(health);
                        this.operations = operations;
                        this.graphs = graphs;
                        this.state = ConnectionState::Ready;
                    }
                    Probe::Failed(state) => this.state = state,
                }
                cx.emit(ConnectionEvent::StateChanged);
                cx.notify();
                this.schedule_health_check(cx);
            })
            .ok();
        }));
    }

    /// Checks `/healthz` again after the settings' interval while the server is ready, and
    /// every two minutes after a failure. A recovered server is probed in full again. A timer
    /// that slept through a system sleep fires on wake, which is the check on wake.
    fn schedule_health_check(&mut self, cx: &mut Context<Self>) {
        let interval = if self.state == ConnectionState::Ready {
            cx.try_global::<StudioSettings>()
                .map(|settings| Duration::from_secs(settings.studio.health_check_seconds.max(5)))
                .unwrap_or(DEFAULT_CHECK_INTERVAL)
        } else {
            FAILED_CHECK_INTERVAL
        };
        let client = self.client.clone();
        self._health_check = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(interval).await;
            let health = client.health().await;
            this.update(cx, |this, cx| this.apply_health_check(health, cx))
                .ok();
        }));
    }

    fn apply_health_check(
        &mut self,
        health: Result<graph_client::ServerHealth, ReadError>,
        cx: &mut Context<Self>,
    ) {
        let next = match health {
            Ok(health) if !health.contract_supported() => ConnectionState::ContractMismatch {
                served: health.contract.clone(),
            },
            Ok(_) if self.state == ConnectionState::Ready => ConnectionState::Ready,
            // Recovered: probe capabilities and graphs again, which schedules the next check.
            Ok(_) => {
                self.connect(cx);
                return;
            }
            Err(ReadError::Transport(error)) => ConnectionState::Unreachable(error),
            Err(error) => ConnectionState::Unreachable(error.to_string()),
        };
        if next != self.state {
            self.state = next;
            cx.emit(ConnectionEvent::StateChanged);
            cx.notify();
        }
        self.schedule_health_check(cx);
    }
}

enum Probe {
    Ready {
        health: HealthOutput,
        operations: BTreeSet<String>,
        graphs: GraphListing,
    },
    Failed(ConnectionState),
}

async fn probe(client: &GraphClient) -> Probe {
    let health = match client.health().await {
        Ok(health) => health,
        Err(ReadError::Transport(error)) => {
            return Probe::Failed(ConnectionState::Unreachable(error));
        }
        Err(error) => return Probe::Failed(ConnectionState::Unreachable(error.to_string())),
    };
    if !health.contract_supported() {
        return Probe::Failed(ConnectionState::ContractMismatch {
            served: health.contract.clone(),
        });
    }
    let operations = client.operations().await.unwrap_or_default();
    let graphs = match client.graphs().await {
        Ok(list) => GraphListing::Listed(list.graphs),
        Err(ReadError::Refused(summary)) if summary.status == 401 => {
            return Probe::Failed(ConnectionState::Unauthorized);
        }
        Err(ReadError::Refused(summary)) if summary.status == 403 => GraphListing::NotAllowed,
        Err(error) => GraphListing::Failed(error.to_string()),
    };
    Probe::Ready {
        health: health.health,
        operations,
        graphs,
    }
}
