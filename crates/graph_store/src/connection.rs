//! One OmniGraph server: its client, contract check, capabilities and graphs (spec 003).

use std::collections::BTreeSet;
use std::sync::Arc;

use gpui::{Context, EventEmitter, Task};
use graph_client::api::{GraphInfo, HealthOutput};
use graph_client::{GraphClient, ReadError};
use http_client::HttpClient;

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
    pub state: ConnectionState,
    pub health: Option<HealthOutput>,
    pub operations: BTreeSet<String>,
    pub graphs: GraphListing,
    has_token: bool,
    _task: Option<Task<()>>,
}

impl EventEmitter<ConnectionEvent> for Connection {}

impl Connection {
    pub fn new(
        server: ServerEntry,
        token: Option<String>,
        http: Arc<dyn HttpClient>,
    ) -> anyhow::Result<Self> {
        let has_token = token.is_some();
        let client = GraphClient::new(&server.url, token, http)?;
        Ok(Self {
            server,
            client,
            state: ConnectionState::Idle,
            health: None,
            operations: BTreeSet::new(),
            graphs: GraphListing::Unknown,
            has_token,
            _task: None,
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
            })
            .ok();
        }));
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
