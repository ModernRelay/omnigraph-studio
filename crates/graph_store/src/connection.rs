//! One Omnigraph server: its client, contract and version check, and graphs (spec 003).

use std::rc::Rc;
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
    /// Graph ids known without listing: the default graphs of the CLI targets on the server,
    /// and ids opened before.
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
    /// The server answered 401: the token sent was refused, or none was sent
    /// ([`Connection::refused_with_token`] says which, and [`Connection::token_unread`] why
    /// when the keychain did not give one).
    Unauthorized,
    Unreachable(String),
}

impl ConnectionState {
    /// The status-bar text for this state.
    pub fn label(&self) -> String {
        match self {
            ConnectionState::Idle => "Not connected".into(),
            ConnectionState::Connecting => "Connecting…".into(),
            ConnectionState::Ready => format!("contract {}", graph_client::CONTRACT),
            ConnectionState::ContractMismatch { served } => match served {
                Some(served) => format!(
                    "Server speaks {served}; Studio speaks {}",
                    graph_client::CONTRACT
                ),
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
    /// The token may not list graphs, but may discover them: an identity credential, which the
    /// server answers with the ids of the graphs it may know of and nothing about their state.
    Discovered(Vec<String>),
    /// The token may neither list nor discover graphs (`graph_list` needs a cluster policy,
    /// and a static token is refused discovery); graph ids come from the CLI's targets or are
    /// typed in.
    NotAllowed,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionEvent {
    StateChanged,
    /// The token was replaced by another: what was read with the old one, and what it was
    /// allowed to do, is to be read again.
    CredentialChanged,
}

/// Held by whatever shows a server (a window's graph, the Open dialog): its health is checked
/// while one is held ([`Connection::watch`]).
pub struct ConnectionWatch {
    _alive: Rc<()>,
}

/// A read of the token from the keychain, asked for by [`Connection::await_token`]: its answer
/// counts only while no token was set since ([`Connection::token_read`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenWait(u64);

pub struct Connection {
    pub server: ServerEntry,
    client: GraphClient,
    pub state: ConnectionState,
    pub health: Option<HealthOutput>,
    /// How long the last health check took to answer: the server's round trip, shown beside
    /// it. `None` until one answers, and after one fails.
    pub latency: Option<Duration>,
    pub graphs: GraphListing,
    /// Why the token last typed for this server could not be saved to the OS keychain: it
    /// lasts this run only. `None` once one is saved.
    pub token_unsaved: Option<String>,
    /// Why the token saved for this server could not be read from the OS keychain (locked,
    /// access denied, no keychain service): Studio connects without one, which a server that
    /// asks for none accepts. `None` once a token is set.
    pub token_unread: Option<String>,
    /// Whether the request the server refused with 401 ([`ConnectionState::Unauthorized`])
    /// carried a token. None is sent to a server with none configured: a managed CLI target,
    /// whose data token the CLI gets from the control plane, or a CLI target whose saved token
    /// is bound to another endpoint. Such a server asks for one rather than refusing it.
    pub refused_with_token: bool,
    /// Whether the client holds a token, so the next request sends one.
    has_token: bool,
    _task: Option<Task<()>>,
    /// The graph list read again after it could not be read ([`Connection::list_graphs_again`]).
    _listing: Option<Task<()>>,
    /// The next health re-check (spec 003).
    _health_check: Option<Task<()>>,
    /// The token is still being read (from the keychain): a connect waits for it, rather than
    /// asking without one and being refused.
    token_pending: bool,
    /// How many tokens were set: a keychain read asked for before the last one answers too
    /// late, and the token set since stands.
    tokens_set: u64,
    /// One more holder than the watches held ([`ConnectionWatch`]): health is checked only
    /// while one is.
    watchers: Rc<()>,
    /// A health check came while nothing watched the server: the next watch checks at once.
    check_missed: bool,
}

/// Health re-check interval after a failure (spec 003).
const FAILED_CHECK_INTERVAL: Duration = Duration::from_secs(120);
/// Interval when settings are not loaded, as in tests.
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(30);

impl EventEmitter<ConnectionEvent> for Connection {}

impl Connection {
    /// The status-bar text for its state: a 401 to a request that carried no token says the
    /// server asks for one (and that the keychain gave none, when it could not), rather than
    /// that a token was refused.
    pub fn label(&self) -> String {
        match (&self.state, &self.token_unread) {
            (ConnectionState::Unauthorized, Some(_)) => "Token not read from the keychain".into(),
            (ConnectionState::Unauthorized, None) if !self.refused_with_token => {
                "Asks for a token".into()
            }
            (state, _) => state.label(),
        }
    }

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
            latency: None,
            graphs: GraphListing::Unknown,
            token_unsaved: None,
            token_unread: None,
            refused_with_token: false,
            has_token,
            _task: None,
            _listing: None,
            _health_check: None,
            token_pending: false,
            tokens_set: 0,
            watchers: Rc::new(()),
            check_missed: false,
        })
    }

    /// Notes that the token is being read; [`Connection::token_read`] or
    /// [`Connection::set_token`] ends the wait, and a connect asked for meanwhile goes then.
    /// The read's answer is given back with the returned wait.
    pub fn await_token(&mut self) -> TokenWait {
        self.token_pending = true;
        TokenWait(self.tokens_set)
    }

    /// Takes the token read from the OS keychain for `wait`, which is none when none was saved.
    /// One that could not be read ends the wait too: Studio connects without a token and keeps
    /// why ([`Connection::token_unread`]). A server that asks for one answers 401, and a token
    /// typed in then connects. A token set since the read was asked for (typed in while the
    /// keychain was slow to answer) is newer, and stands.
    pub(crate) fn token_read(
        &mut self,
        wait: TokenWait,
        read: anyhow::Result<Option<(String, Vec<u8>)>>,
        cx: &mut Context<Self>,
    ) {
        if wait.0 != self.tokens_set {
            return;
        }
        let (token, unread) = match read {
            Ok(None) => (None, None),
            Ok(Some((_, bytes))) => match String::from_utf8(bytes) {
                Ok(token) => (Some(token), None),
                Err(_) => (None, Some("the keychain's entry is not text".to_owned())),
            },
            Err(error) => (None, Some(error.to_string())),
        };
        self.replace_token(token, unread, cx);
    }

    /// Records how saving the token to the OS keychain ended ([`Connection::token_unsaved`]).
    pub(crate) fn token_saved(&mut self, saved: Result<(), String>, cx: &mut Context<Self>) {
        let unsaved = saved.err();
        if unsaved != self.token_unsaved {
            self.token_unsaved = unsaved;
            cx.emit(ConnectionEvent::StateChanged);
            cx.notify();
        }
    }

    pub(crate) fn client(&self) -> &GraphClient {
        &self.client
    }

    /// Graph ids to offer: the listed or discovered graphs, then known ids the listing lacks.
    pub fn graph_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = match &self.graphs {
            GraphListing::Listed(graphs) => graphs.iter().map(|g| g.graph_id.clone()).collect(),
            GraphListing::Discovered(ids) => ids.clone(),
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

    /// Connects, unless it is connected or connecting already: a server a window shows or opens
    /// is connected once.
    pub fn connect_if_idle(&mut self, cx: &mut Context<Self>) {
        if self.state == ConnectionState::Idle {
            self.connect(cx);
        }
    }

    /// Replaces the token: typed in after the server refused the old one, or changed in the
    /// CLI's files (spec 003). Requests made after this use it, and a connection in use checks
    /// again; one not yet used connects when it is.
    pub fn set_token(&mut self, token: Option<String>, cx: &mut Context<Self>) {
        self.replace_token(token, None, cx);
    }

    /// Replaces the token, keeping `unread` as why the keychain did not give one.
    fn replace_token(
        &mut self,
        token: Option<String>,
        unread: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.token_pending = false;
        self.tokens_set += 1;
        self.has_token = token.is_some();
        if self.client.set_token(token) {
            cx.emit(ConnectionEvent::CredentialChanged);
        }
        let unread_changed = unread != self.token_unread;
        self.token_unread = unread;
        if self.state != ConnectionState::Idle {
            self.connect(cx);
        } else if unread_changed {
            cx.emit(ConnectionEvent::StateChanged);
            cx.notify();
        }
    }

    /// Keeps the server's health checked while the returned watch is held: a window that shows
    /// one of its graphs holds one, and so does the Open dialog listing it. With none held, a
    /// check that comes is skipped and the checks stop; the next watch checks at once and
    /// starts them again.
    pub fn watch(&mut self, cx: &mut Context<Self>) -> ConnectionWatch {
        let watch = ConnectionWatch {
            _alive: self.watchers.clone(),
        };
        self.list_graphs_again(cx);
        if std::mem::take(&mut self.check_missed)
            && !matches!(
                self.state,
                ConnectionState::Idle | ConnectionState::Connecting
            )
        {
            self.check_health_after(Duration::ZERO, cx);
        }
        watch
    }

    /// Whether a watch is held ([`Connection::watch`]).
    pub fn watched(&self) -> bool {
        Rc::strong_count(&self.watchers) > 1
    }

    /// Stops a connection attempt that is taking too long.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.state == ConnectionState::Connecting {
            self._task = None;
            // A check already scheduled would connect again, or mark the server down.
            self._health_check = None;
            self.check_missed = false;
            self.state = ConnectionState::Idle;
            cx.emit(ConnectionEvent::StateChanged);
            cx.notify();
        }
    }

    /// Reads the server's graph list again when the last read of it failed and the server
    /// is ready: when something comes to show the server (a window showing one of its graphs,
    /// the Open dialog listing it, [`Connection::watch`]) and on a graph's Refresh. A health
    /// check sees only the server, so a list that failed once stayed failed until Studio
    /// connected again. A list read, or refused, stays as it is.
    pub fn list_graphs_again(&mut self, cx: &mut Context<Self>) {
        if self.state != ConnectionState::Ready
            || !matches!(self.graphs, GraphListing::Failed(_))
            || self._listing.is_some()
        {
            return;
        }
        let client = self.client.clone();
        let token_sent = self.has_token;
        self._listing = Some(cx.spawn(async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async move { list_graphs(&client).await })
                .await;
            this.update(cx, |this, cx| {
                this._listing = None;
                match listed {
                    Ok(graphs) => this.graphs = graphs,
                    Err(state) => this.refused(state, token_sent),
                }
                cx.emit(ConnectionEvent::StateChanged);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Checks health and contract, then reads the graph list.
    pub fn connect(&mut self, cx: &mut Context<Self>) {
        self.state = ConnectionState::Connecting;
        // The probe reads the list itself.
        self._listing = None;
        // The probe schedules the next check when it ends.
        self._health_check = None;
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
        if self.token_pending {
            return;
        }
        let client = self.client.clone();
        // A new token connects again, replacing this probe: the probe's requests carry the
        // token held now.
        let token_sent = self.has_token;
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
                    Probe::Failed(state) => this.refused(state, token_sent),
                }
                cx.emit(ConnectionEvent::StateChanged);
                cx.notify();
                this.schedule_health_check(cx);
            })
            .ok();
        }));
    }

    /// Takes the state a failed read left, noting for a 401 whether that read carried a token
    /// ([`Self::refused_with_token`]).
    fn refused(&mut self, state: ConnectionState, token_sent: bool) {
        if state == ConnectionState::Unauthorized {
            self.refused_with_token = token_sent;
        }
        self.state = state;
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
        self.check_health_after(interval, cx);
    }

    /// Checks `/healthz` after `wait`, unless nothing watches the server by then: that check
    /// is missed, the checks stop, and the next watch makes it ([`Connection::watch`]).
    fn check_health_after(&mut self, wait: Duration, cx: &mut Context<Self>) {
        let client = self.client.clone();
        self._health_check = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let watched = this
                .update(cx, |this, _| {
                    this.check_missed = !this.watched();
                    !this.check_missed
                })
                .unwrap_or(false);
            if !watched {
                return;
            }
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
            // `/healthz` asks for no token, so it passes for a refused one too: probing again
            // would be refused alike, and would take the token field away while it is typed
            // in. A new token connects again.
            Ok(_) if self.state == ConnectionState::Unauthorized => ConnectionState::Unauthorized,
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
    let ((health, latency), graphs) = futures::join!(timed_health, list_graphs(client));
    let health = match served(health) {
        Ok(health) => health,
        Err(state) => return Probe::Failed(state),
    };
    let graphs = match graphs {
        Ok(graphs) => graphs,
        Err(state) => return Probe::Failed(state),
    };
    Probe::Ready {
        health: health.health,
        latency,
        graphs,
    }
}

/// What the server says about its graphs, or the state a refused token leaves the
/// connection in.
async fn list_graphs(client: &GraphClient) -> Result<GraphListing, ConnectionState> {
    Ok(match client.graphs().await {
        Ok(list) => GraphListing::Listed(list.graphs),
        Err(ReadError::Refused(summary)) if summary.status == 401 => {
            return Err(ConnectionState::Unauthorized);
        }
        // A credential that may not list the server's graphs may still discover its own.
        Err(ReadError::Refused(summary)) if summary.status == 403 => {
            match client.discover_graphs().await {
                Ok(found) => {
                    GraphListing::Discovered(found.graphs.into_iter().map(|g| g.graph_id).collect())
                }
                // A static token is refused discovery too; any other failure is a failed read.
                Err(ReadError::Refused(summary)) if summary.status == 403 => {
                    GraphListing::NotAllowed
                }
                Err(error) => GraphListing::Failed(error.to_string()),
            }
        }
        Err(error) => GraphListing::Failed(error.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use http_client::{AsyncBody, FakeHttpClient, Response};

    /// A server answering `/healthz` with `version`, and every other read with no graphs.
    /// The state a connection reaches to a server whose answers carry `contract`.
    fn connect_to(contract: &'static str, cx: &mut TestAppContext) -> ConnectionState {
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            async move {
                let body = if path.ends_with("/healthz") {
                    r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#
                } else {
                    r#"{"graphs":[]}"#
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, contract)
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

    /// A token that may not list the server's graphs discovers the ones it may know of; one
    /// refused discovery too (a static token) has graph ids typed in.
    #[gpui::test]
    async fn a_token_that_may_not_list_graphs_discovers_them(cx: &mut TestAppContext) {
        for discovers in [true, false] {
            let http = FakeHttpClient::create(move |request| {
                let path = request.uri().path().to_owned();
                async move {
                    let (status, body) = if path.ends_with("/healthz") {
                        (
                            200,
                            r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#,
                        )
                    } else if path.ends_with("/graphs/discovery") && discovers {
                        (
                            200,
                            r#"{"graphs":[{"graph_id":"knowledge","display_name":"knowledge"}]}"#,
                        )
                    } else {
                        (403, r#"{"error":"forbidden","code":"forbidden"}"#)
                    };
                    Ok(Response::builder()
                        .status(status)
                        .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
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
            connection.read_with(cx, |connection, _| {
                assert_eq!(connection.state, ConnectionState::Ready);
                if discovers {
                    assert_eq!(
                        connection.graphs,
                        GraphListing::Discovered(vec!["knowledge".into()])
                    );
                    assert_eq!(connection.graph_ids(), ["knowledge"]);
                } else {
                    assert_eq!(connection.graphs, GraphListing::NotAllowed);
                    assert!(connection.graph_ids().is_empty());
                }
            });
        }
    }

    /// A connection whose token is still being read waits to connect, sending nothing it
    /// would be refused; the token's arrival connects it, and the keychain's answer to that
    /// read, coming after, is dropped. A cancel stops the check scheduled after a failure,
    /// which would otherwise connect again.
    #[gpui::test]
    async fn a_connect_waits_for_its_token_and_a_cancel_stays(cx: &mut TestAppContext) {
        let sent = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
        let seen = sent.clone();
        // Whether the server answers a read sent without a token; it answers 401 otherwise.
        let open = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let answers_anyone = open.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let token = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let refused = token.is_none()
                && !answers_anyone.load(std::sync::atomic::Ordering::SeqCst)
                && !path.ends_with("/healthz");
            seen.lock().unwrap().push(token);
            async move {
                let (status, body) = if path.ends_with("/healthz") {
                    (
                        200,
                        r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#,
                    )
                } else if refused {
                    (401, r#"{"error":"unauthorized","code":"unauthorized"}"#)
                } else {
                    (200, r#"{"graphs":[]}"#)
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
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
        let first_read = connection.update(cx, |connection, cx| {
            let wait = connection.await_token();
            connection.connect(cx);
            wait
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
            connection.set_token(Some("t".into()), cx)
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
        // The keychain answering after a token was set: the token set stands, and nothing is
        // sent again with the keychain's.
        sent.lock().unwrap().clear();
        connection.update(cx, |connection, cx| {
            let saved = ("omnigraph-studio".to_owned(), b"from-keychain".to_vec());
            connection.token_read(first_read, Ok(Some(saved)), cx);
        });
        cx.run_until_parked();
        assert!(
            sent.lock().unwrap().is_empty(),
            "the late answer is dropped"
        );

        // A check is scheduled while ready; connecting and cancelling leave none behind.
        connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.cancel(cx);
            assert!(connection._health_check.is_none());
            assert_eq!(connection.state, ConnectionState::Idle);
        });

        // A token the keychain could not give: the wait ends and Studio connects without one,
        // keeping why. A server that asks for no token is ready.
        let read_failed = |cx: &mut TestAppContext| {
            sent.lock().unwrap().clear();
            connection.update(cx, |connection, cx| {
                let wait = connection.await_token();
                connection.connect(cx);
                connection.token_read(wait, Err(anyhow::anyhow!("the keychain is locked")), cx);
            });
            cx.run_until_parked();
            let sent = sent.lock().unwrap();
            assert!(!sent.is_empty(), "connected without the token");
            assert!(sent.iter().all(Option::is_none), "no token sent");
            drop(sent);
            connection.read_with(cx, |connection, _| {
                assert_eq!(
                    connection.token_unread.as_deref(),
                    Some("the keychain is locked")
                );
                connection.state.clone()
            })
        };
        open.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(read_failed(cx), ConnectionState::Ready);

        // A server that asks for one answers 401; a token typed in connects and ends the
        // keychain's reason. A token that could not be saved says so.
        open.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(read_failed(cx), ConnectionState::Unauthorized);
        sent.lock().unwrap().clear();
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("typed".into()), cx);
            connection.token_saved(Err("access denied".into()), cx);
        });
        cx.run_until_parked();
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Ready);
            assert_eq!(connection.token_unread, None);
            assert_eq!(connection.token_unsaved.as_deref(), Some("access denied"));
        });
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .all(|token| token.as_deref() == Some("Bearer typed"))
        );
    }

    /// A refused token stays refused through health checks, which pass without one: the
    /// graphs are not asked again with it.
    #[gpui::test]
    async fn a_refused_token_is_not_sent_again_by_a_health_check(cx: &mut TestAppContext) {
        let listed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let lists = listed.clone();
        let http = FakeHttpClient::create(move |request| {
            let health = request.uri().path().ends_with("/healthz");
            if !health {
                lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            async move {
                let (status, body) = if health {
                    (
                        200,
                        r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#,
                    )
                } else {
                    (401, r#"{"error":"unauthorized","code":"unauthorized"}"#)
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
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
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        // Shown, so its health is checked.
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let asked = listed.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..3 {
            cx.executor().advance_clock(FAILED_CHECK_INTERVAL);
            cx.run_until_parked();
            connection.read_with(cx, |connection, _| {
                assert_eq!(connection.state, ConnectionState::Unauthorized)
            });
        }
        assert_eq!(listed.load(std::sync::atomic::Ordering::SeqCst), asked);
    }

    /// A 401 says whether the request carried a token: a server sent none (a managed CLI
    /// target, a saved token bound to another endpoint) asks for one, and one sent a token
    /// refused it. Each new token connects again and says so anew.
    #[gpui::test]
    async fn a_401_says_whether_a_token_was_sent(cx: &mut TestAppContext) {
        let http = FakeHttpClient::create(move |request| {
            let health = request.uri().path().ends_with("/healthz");
            async move {
                let (status, body) = if health {
                    (
                        200,
                        r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#,
                    )
                } else {
                    (401, r#"{"error":"unauthorized","code":"unauthorized"}"#)
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "cloud".into(),
            url: "https://cluster.test".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, None, http).unwrap());
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        let said = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| {
                assert_eq!(connection.state, ConnectionState::Unauthorized);
                (connection.refused_with_token, connection.label())
            })
        };
        assert_eq!(said(cx), (false, "Asks for a token".to_owned()));
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("typed".into()), cx)
        });
        cx.run_until_parked();
        assert_eq!(said(cx), (true, "Token refused".to_owned()));
        connection.update(cx, |connection, cx| connection.set_token(None, cx));
        cx.run_until_parked();
        assert_eq!(said(cx), (false, "Asks for a token".to_owned()));
    }

    /// A graph list that could not be read is read again when something next shows the server,
    /// not left failed for as long as its health checks pass; one read is not read again.
    #[gpui::test]
    async fn a_graph_list_that_failed_is_read_again_when_next_shown(cx: &mut TestAppContext) {
        let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = lists.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let listed = if path.ends_with("/graphs") {
                Some(counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
            } else {
                None
            };
            async move {
                let (status, body) = match listed {
                    None => (
                        200,
                        r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#,
                    ),
                    Some(0) => (500, r#"{"error":"the cluster is reloading"}"#),
                    Some(_) => (
                        200,
                        r#"{"graphs":[{"graph_id":"knowledge","uri":"s3://g/knowledge","state":"ready","read_available":true,"write_available":true,"action":"none"}]}"#,
                    ),
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
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
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let _shown = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let listing = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| {
                (connection.state.clone(), connection.graph_ids())
            })
        };
        assert_eq!(listing(cx), (ConnectionState::Ready, Vec::<String>::new()));
        for _ in 0..2 {
            cx.executor().advance_clock(DEFAULT_CHECK_INTERVAL);
            cx.run_until_parked();
        }
        let failed = connection.read_with(cx, |connection, _| connection.graphs.clone());
        assert!(matches!(failed, GraphListing::Failed(_)), "{failed:?}");
        assert_eq!(lists.load(std::sync::atomic::Ordering::SeqCst), 1);

        let _again = connection.update(cx, |connection, cx| connection.watch(cx));
        cx.run_until_parked();
        assert_eq!(
            listing(cx),
            (ConnectionState::Ready, vec!["knowledge".to_owned()])
        );
        let _third = connection.update(cx, |connection, cx| connection.watch(cx));
        connection.update(cx, |connection, cx| connection.list_graphs_again(cx));
        cx.run_until_parked();
        assert_eq!(
            lists.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "read once more"
        );
    }

    /// Health is checked only while something shows the server: a check that comes while
    /// nothing watches is skipped and the checks stop, until a watch checks at once and starts
    /// them again.
    #[gpui::test]
    async fn health_is_checked_only_while_the_server_is_watched(cx: &mut TestAppContext) {
        let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = checks.clone();
        let http = FakeHttpClient::create(move |request| {
            let health = request.uri().path().ends_with("/healthz");
            if health {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            async move {
                let body = if health {
                    r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#
                } else {
                    r#"{"graphs":[]}"#
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
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
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let checked = || checks.swap(0, std::sync::atomic::Ordering::SeqCst);
        let wait = |cx: &mut TestAppContext| {
            cx.executor().advance_clock(DEFAULT_CHECK_INTERVAL);
            cx.run_until_parked();
        };
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        assert_eq!(checked(), 1, "the connect's own");
        for _ in 0..3 {
            wait(cx);
        }
        assert_eq!(checked(), 0, "nothing shows the server");

        let watch = connection.update(cx, |connection, cx| connection.watch(cx));
        cx.run_until_parked();
        assert_eq!(checked(), 1, "at once");
        wait(cx);
        wait(cx);
        assert_eq!(checked(), 2, "then at each interval");

        drop(watch);
        wait(cx);
        wait(cx);
        assert_eq!(checked(), 0);
        connection.read_with(cx, |connection, _| {
            assert!(!connection.watched());
            assert_eq!(connection.state, ConnectionState::Ready);
        });
    }

    /// A server that speaks contract 0.13 (Omnigraph 0.13, or a 0.14 build before the contract
    /// moved) is refused by its header; one that speaks 0.14 connects.
    #[gpui::test]
    async fn a_server_is_refused_by_the_contract_it_speaks(cx: &mut TestAppContext) {
        let refused = connect_to("0.13", cx);
        assert_eq!(
            refused,
            ConnectionState::ContractMismatch {
                served: Some("0.13".into())
            }
        );
        assert_eq!(refused.label(), "Server speaks 0.13; Studio speaks 0.14");
        assert_eq!(
            connect_to(graph_client::CONTRACT, cx),
            ConnectionState::Ready
        );
    }
}
