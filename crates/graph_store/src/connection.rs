//! One OmniGraph server: its client, contract check, capabilities and graphs (spec 003).

use std::sync::Arc;
use std::time::{Duration, Instant};

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
    ContractMismatch {
        served: Option<String>,
    },
    /// The server is older than Studio supports (`graph_client::MIN_SERVER_VERSION`), by the
    /// version its `/healthz` names.
    TooOld {
        version: String,
    },
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
                Some(served) => format!(
                    "Server speaks {served}; Studio speaks {}",
                    graph_client::SUPPORTED_CONTRACTS.join(", ")
                ),
                None => "Server did not say which contract it speaks".into(),
            },
            ConnectionState::TooOld { version } => {
                let (major, minor) = graph_client::MIN_SERVER_VERSION;
                format!("OmniGraph {version}; Studio needs {major}.{minor} or later")
            }
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
    /// How long the last health check took to answer: the server's round trip, shown beside
    /// it. `None` until one answers, and after one fails.
    pub latency: Option<Duration>,
    pub graphs: GraphListing,
    _task: Option<Task<()>>,
    /// The next health re-check (spec 003).
    _health_check: Option<Task<()>>,
    /// The token is still being read (from the keychain): a connect waits for it, rather than
    /// asking without one and being refused.
    token_pending: bool,
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
        let client = GraphClient::new(&server.url, token, http.clone())?;
        Ok(Self {
            server,
            client,
            http,
            state: ConnectionState::Idle,
            health: None,
            latency: None,
            graphs: GraphListing::Unknown,
            _task: None,
            _health_check: None,
            token_pending: false,
        })
    }

    /// Notes that the token is being read; [`Connection::set_token`] ends the wait, and a
    /// connect asked for meanwhile goes then.
    pub fn await_token(&mut self) {
        self.token_pending = true;
    }

    pub(crate) fn client(&self) -> &GraphClient {
        &self.client
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

    /// Replaces the token: typed in after the server refused the old one, or changed in the
    /// CLI's files (spec 003). Requests made after this use it, and a connection in use checks
    /// again; one not yet used connects when it is.
    pub fn set_token(
        &mut self,
        token: Option<String>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        self.token_pending = false;
        self.client = GraphClient::new(&self.server.url, token, self.http.clone())?;
        if self.state != ConnectionState::Idle {
            self.connect(cx);
        }
        Ok(())
    }

    /// Stops a connection attempt that is taking too long.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.state == ConnectionState::Connecting {
            self._task = None;
            // A check already scheduled would connect again, or mark the server down.
            self._health_check = None;
            self.state = ConnectionState::Idle;
            cx.emit(ConnectionEvent::StateChanged);
            cx.notify();
        }
    }

    /// Checks health and contract, then reads the graph list.
    pub fn connect(&mut self, cx: &mut Context<Self>) {
        self.state = ConnectionState::Connecting;
        // The probe schedules the next check when it ends.
        self._health_check = None;
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
        if self.token_pending {
            return;
        }
        let client = self.client.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            // Off the main thread, so its round trip is not lengthened by a busy frame.
            let outcome = cx
                .background_executor()
                .spawn(async move { probe(&client).await })
                .await;
            this.update(cx, |this, cx| {
                match outcome {
                    Probe::Ready {
                        health,
                        latency,
                        graphs,
                    } => {
                        this.health = Some(health);
                        this.latency = Some(latency);
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
            // Timed off the main thread, so a busy frame does not count as the server's time.
            let (health, latency) = cx
                .background_executor()
                .spawn(async move {
                    let started = Instant::now();
                    let health = client.health().await;
                    (health, started.elapsed())
                })
                .await;
            this.update(cx, |this, cx| this.apply_health_check(health, latency, cx))
                .ok();
        }));
    }

    fn apply_health_check(
        &mut self,
        health: Result<graph_client::ServerHealth, ReadError>,
        latency: Duration,
        cx: &mut Context<Self>,
    ) {
        let latency = health.is_ok().then_some(latency);
        // Shown in whole milliseconds, so a change under one is not a change.
        if latency.map(|l| l.as_millis()) != self.latency.map(|l| l.as_millis()) {
            self.latency = latency;
            cx.notify();
        }
        let next = match served(health) {
            Ok(_) if self.state == ConnectionState::Ready => ConnectionState::Ready,
            // Recovered: probe the graphs again, which schedules the next check.
            Ok(_) => {
                self.connect(cx);
                return;
            }
            Err(state) => state,
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
        latency: Duration,
        graphs: GraphListing,
    },
    Failed(ConnectionState),
}

/// A health answer as the server's state: one Studio can use, or why not.
fn served(
    health: Result<graph_client::ServerHealth, ReadError>,
) -> Result<graph_client::ServerHealth, ConnectionState> {
    match health {
        Ok(health) if !health.contract_supported() => Err(ConnectionState::ContractMismatch {
            served: health.contract,
        }),
        Ok(health) if !health.version_supported() => Err(ConnectionState::TooOld {
            version: health.health.version,
        }),
        Ok(health) => Ok(health),
        Err(ReadError::Transport(error)) => Err(ConnectionState::Unreachable(error)),
        Err(error) => Err(ConnectionState::Unreachable(error.to_string())),
    }
}

/// Health and the graph list, asked together: the list is dropped when the server cannot be
/// used.
async fn probe(client: &GraphClient) -> Probe {
    let timed_health = async {
        let started = Instant::now();
        let health = client.health().await;
        (health, started.elapsed())
    };
    let ((health, latency), graphs) = futures::join!(timed_health, client.graphs());
    let health = match served(health) {
        Ok(health) => health,
        Err(state) => return Probe::Failed(state),
    };
    let graphs = match graphs {
        Ok(list) => GraphListing::Listed(list.graphs),
        Err(ReadError::Refused(summary)) if summary.status == 401 => {
            return Probe::Failed(ConnectionState::Unauthorized);
        }
        Err(ReadError::Refused(summary)) if summary.status == 403 => GraphListing::NotAllowed,
        Err(error) => GraphListing::Failed(error.to_string()),
    };
    Probe::Ready {
        health: health.health,
        latency,
        graphs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use http_client::{AsyncBody, FakeHttpClient, Response};

    /// A server answering `/healthz` with `version`, and every other read with no graphs.
    fn connect_to(version: &'static str, cx: &mut TestAppContext) -> ConnectionState {
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            async move {
                let body = if path.ends_with("/healthz") {
                    format!(
                        r#"{{"status":"ok","version":"{version}","internal_schema_version":16}}"#
                    )
                } else {
                    r#"{"graphs":[]}"#.to_owned()
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
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        connection.read_with(cx, |connection, _| connection.state.clone())
    }

    /// A connection whose token is still being read waits to connect, sending nothing it
    /// would be refused; the token's arrival connects it. A cancel stops the check scheduled
    /// after a failure, which would otherwise connect again.
    #[gpui::test]
    async fn a_connect_waits_for_its_token_and_a_cancel_stays(cx: &mut TestAppContext) {
        let sent = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
        let seen = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let token = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            seen.lock().unwrap().push(token);
            async move {
                let body = if path.ends_with("/healthz") {
                    r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#
                } else {
                    r#"{"graphs":[]}"#
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
            source: ServerSource::Studio,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, None, http).unwrap());
        connection.update(cx, |connection, cx| {
            connection.await_token();
            connection.connect(cx);
        });
        cx.run_until_parked();
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Connecting)
        });
        assert!(
            sent.lock().unwrap().is_empty(),
            "nothing sent without the token"
        );
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("t".into()), cx).unwrap()
        });
        cx.run_until_parked();
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Ready)
        });
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .all(|token| token.as_deref() == Some("Bearer t"))
        );

        // A check is scheduled while ready; connecting and cancelling leave none behind.
        connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.cancel(cx);
            assert!(connection._health_check.is_none());
            assert_eq!(connection.state, ConnectionState::Idle);
        });
    }

    /// A 0.13 server speaks the same contract header as 0.14, so its version is what refuses
    /// it; an edge build of 0.14 connects.
    #[gpui::test]
    async fn a_server_older_than_0_14_is_refused_by_its_version(cx: &mut TestAppContext) {
        let refused = connect_to("0.13.0", cx);
        assert_eq!(
            refused,
            ConnectionState::TooOld {
                version: "0.13.0".into()
            }
        );
        assert_eq!(
            refused.label(),
            "OmniGraph 0.13.0; Studio needs 0.14 or later"
        );
        assert_eq!(
            connect_to("0.14.0-edge+d883c49", cx),
            ConnectionState::Ready
        );
    }
}
