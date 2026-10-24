//! One OmniGraph server: its client, contract and version check, and graphs (spec 003).

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::{Context, EventEmitter, Task};
use graph_client::api::{GraphInfo, HealthOutput};
use graph_client::{GraphClient, ReadError};
use http_client::HttpClient;
use studio_settings::StudioSettings;

use crate::cli_tokens::{Token, TokenSource};

/// Where Studio learned about a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerSource {
    /// A server target the OmniGraph CLI lists; its token, if any, is the CLI's to give.
    Cli,
    /// `OMNIGRAPH_STUDIO_SERVER` in the environment, for development and tests; its token,
    /// `OMNIGRAPH_STUDIO_TOKEN`, lasts the run.
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
    /// ([`Connection::refused_with_token`] says which). Also a server whose token source gave
    /// none ([`Connection::token_unread`] says why), which is asked nothing without one.
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
    /// The connection knows the token its requests carry: its token source answered, or a token
    /// was given ([`Connection::token_ready`]). A graph's first reads wait for it.
    TokenReady,
}

/// Held by whatever shows a server (a window's graph, the Open dialog): its health is checked
/// while one is held ([`Connection::watch`]).
pub struct ConnectionWatch {
    _alive: Rc<()>,
}

pub struct Connection {
    pub server: ServerEntry,
    client: GraphClient,
    pub state: ConnectionState,
    pub health: Option<HealthOutput>,
    /// How long the last health check took to answer: the server's round trip, shown beside
    /// it. `None` until one answers, and after one fails.
    pub latency: Option<Duration>,
    pub graphs: GraphListing,
    /// Why the token source gave no token (the OmniGraph CLI's reason): Studio then sends the
    /// server nothing. `None` once a token is set.
    pub token_unread: Option<String>,
    /// Whether the server refused the token of the request it answered with 401
    /// ([`ConnectionState::Unauthorized`]): its challenge says `error="invalid_token"`, or, from
    /// a server that sends no challenge, the request carried one. Otherwise the server asks for
    /// a token rather than refusing one.
    pub refused_with_token: bool,
    /// Whether the client holds a token, so the next request sends one.
    has_token: bool,
    /// Where the token comes from each time it is asked ([`Connection::with_token_source`]): a
    /// CLI server's is the CLI's `credentials export`. `None` for a server given its token.
    token_source: Option<Arc<dyn TokenSource>>,
    /// The token held was given (typed in), so the source is not asked until
    /// [`Connection::refresh_token`].
    token_given: bool,
    /// The token held is the one the source just gave for the connect under way.
    token_fresh: bool,
    /// The source was asked again after a 401, which it is once until a connect is ready.
    asked_after_refusal: bool,
    /// The source has answered since it was set, with a token or why it gave none: requests
    /// sent from then carry what it gave ([`Connection::token_ready`]).
    token_answered: bool,
    /// Raised while a window waits for the source's answer ([`Connection::wait_for_token`]):
    /// the CLI serves this ask before the others waiting for it. Lowered when it answers.
    hurry: Arc<AtomicBool>,
    /// The source asked again while the connection stands (after a 401, on Refresh, for a
    /// changed target).
    _asking: Option<Task<()>>,
    _task: Option<Task<()>>,
    /// The graph list read again after it could not be read ([`Connection::list_graphs_again`]).
    _listing: Option<Task<()>>,
    /// The next health re-check (spec 003).
    _health_check: Option<Task<()>>,
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
    /// server asks for one, and a token source that gave none says so, rather than that a token
    /// was refused.
    pub fn label(&self) -> String {
        match (&self.state, &self.token_unread) {
            (ConnectionState::Unauthorized, Some(_)) => "No token from the CLI".into(),
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
            token_unread: None,
            refused_with_token: false,
            has_token,
            token_source: None,
            token_given: false,
            token_fresh: false,
            asked_after_refusal: false,
            token_answered: false,
            hurry: Arc::default(),
            _asking: None,
            _task: None,
            _listing: None,
            _health_check: None,
            watchers: Rc::new(()),
            check_missed: false,
        })
    }

    /// Takes its token from `source` each time it is asked: on each connect, before anything is
    /// sent; once more after a 401; and on Refresh ([`Connection::refresh_token`]). A server
    /// with a source is sent nothing without a token from it.
    pub(crate) fn with_token_source(mut self, source: Option<Arc<dyn TokenSource>>) -> Self {
        self.token_source = source;
        self
    }

    /// A connection whose token source never answers, so connecting it sends nothing: for the
    /// tests of crates that show a server.
    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_for_a_token(&mut self) {
        struct Waiting;
        impl TokenSource for Waiting {
            fn token(
                &self,
                _: Arc<AtomicBool>,
            ) -> futures::future::BoxFuture<'static, Result<Token, String>> {
                use futures::FutureExt as _;
                futures::future::pending().boxed()
            }
        }
        self.token_source = Some(Arc::new(Waiting));
    }

    /// Replaces where the token comes from (the CLI target changed) and asks it at once; with
    /// none, the token is dropped (the target is anonymous now).
    pub(crate) fn take_token_from(
        &mut self,
        source: Option<Arc<dyn TokenSource>>,
        cx: &mut Context<Self>,
    ) {
        let had = self.token_source.is_some();
        self.token_source = source;
        if self.token_source.is_some() {
            self.refresh_token(cx);
        } else if had {
            self.token_given = false;
            self.replace_token(None, None, cx);
        }
    }

    /// Whether requests sent now carry the token they would carry once the token source has
    /// answered: the connection has no source (a server given its token, an anonymous CLI
    /// target), a token was given, or the source has answered since it was set. A graph's first
    /// reads wait until it does ([`ConnectionEvent::TokenReady`]), so they are not sent without
    /// the token the CLI is about to give.
    pub fn token_ready(&self) -> bool {
        !self.asks_token_source() || self.token_answered
    }

    /// A window waits for this connection's token: its ask goes before the CLI's other work
    /// waiting, and a connection never connected connects, which asks for it. Nothing changes
    /// once the token is ready.
    pub fn wait_for_token(&mut self, cx: &mut Context<Self>) {
        if self.token_ready() {
            return;
        }
        self.hurry.store(true, Ordering::SeqCst);
        self.connect_if_idle(cx);
    }

    /// Asks the token source again, as the person's Refresh does: a token typed in no longer
    /// stands, and a token that changed connects again. A connection not yet used asks when it
    /// connects.
    pub fn refresh_token(&mut self, cx: &mut Context<Self>) {
        if self.token_source.is_none() {
            return;
        }
        self.token_given = false;
        self.asked_after_refusal = false;
        if self.state != ConnectionState::Idle {
            self.ask_for_token(false, cx);
        }
    }

    /// Asks the token source for a token. For a connect (`connecting`), the connect goes on with
    /// it; asked again, only a token that changed connects again.
    fn ask_for_token(&mut self, connecting: bool, cx: &mut Context<Self>) {
        let Some(source) = self.token_source.clone() else {
            return;
        };
        let hurry = self.hurry.clone();
        let asking = cx.spawn(async move |this, cx| {
            let answer = cx.background_executor().spawn(source.token(hurry)).await;
            this.update(cx, |this, cx| this.token_answered(answer, connecting, cx))
                .ok();
        });
        if connecting {
            self._task = Some(asking);
        } else {
            self._asking = Some(asking);
        }
    }

    /// Takes the source's answer. A token connects (for a connect under way, or when it changed);
    /// no token sends nothing more, the connection refused with why.
    fn token_answered(
        &mut self,
        answer: Result<Token, String>,
        connecting: bool,
        cx: &mut Context<Self>,
    ) {
        if !connecting {
            self._asking = None;
        }
        let was_ready = self.token_ready();
        self.token_answered = true;
        self.hurry.store(false, Ordering::SeqCst);
        if !was_ready {
            cx.emit(ConnectionEvent::TokenReady);
        }
        match answer {
            Ok(token) => {
                self.has_token = true;
                let changed = self.client.set_token(Some(token.into_string()));
                if changed {
                    cx.emit(ConnectionEvent::CredentialChanged);
                }
                let unread_changed = self.token_unread.take().is_some();
                if connecting || (changed && self.state != ConnectionState::Idle) {
                    self.token_fresh = true;
                    self.connect(cx);
                } else if unread_changed {
                    cx.emit(ConnectionEvent::StateChanged);
                    cx.notify();
                }
            }
            Err(reason) => {
                self.has_token = false;
                if self.client.set_token(None) {
                    cx.emit(ConnectionEvent::CredentialChanged);
                }
                self.token_unread = Some(reason);
                if connecting || self.state != ConnectionState::Idle {
                    self._task = None;
                    self.refused_with_token = false;
                    self.state = ConnectionState::Unauthorized;
                    self.schedule_health_check(cx);
                }
                cx.emit(ConnectionEvent::StateChanged);
                cx.notify();
            }
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

    /// Replaces the token with one given to it, typed in after the server refused the old one
    /// (spec 003): a token source is not asked over it until Refresh. Requests made after this
    /// use it, and a connection in use checks again; one not yet used connects when it is.
    pub fn set_token(&mut self, token: Option<String>, cx: &mut Context<Self>) {
        let was_ready = self.token_ready();
        self.token_given = true;
        if !was_ready {
            cx.emit(ConnectionEvent::TokenReady);
        }
        self.replace_token(token, None, cx);
    }

    /// Replaces the token, keeping `unread` as why the token source gave none.
    fn replace_token(
        &mut self,
        token: Option<String>,
        unread: Option<String>,
        cx: &mut Context<Self>,
    ) {
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
                    Err(failed) => this.refused(failed, token_sent, cx),
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
        // A server with a token source is asked nothing before the source answers.
        if !std::mem::take(&mut self.token_fresh) && self.asks_token_source() {
            self.ask_for_token(true, cx);
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
                        this.asked_after_refusal = false;
                    }
                    Probe::Failed(failed) => this.refused(failed, token_sent, cx),
                }
                cx.emit(ConnectionEvent::StateChanged);
                cx.notify();
                this.schedule_health_check(cx);
            })
            .ok();
        }));
    }

    /// The managed profile whose login the CLI exchanges for this server's token, when it does
    /// ([`TokenSource::login_profile`]).
    pub fn login_profile(&self) -> Option<String> {
        self.token_source.as_ref()?.login_profile()
    }

    /// Whether a connect asks its token source first: it has one, and no token was given.
    fn asks_token_source(&self) -> bool {
        self.token_source.is_some() && !self.token_given
    }

    /// Takes the state a failed read left, noting for a 401 whether the server refused the
    /// token sent ([`Self::refused_with_token`]). A token from the source that the server
    /// refused is asked for once more: a rotated one connects again, the same one stays
    /// refused, and nothing is asked again until a connect is ready or the person refreshes.
    fn refused(&mut self, failed: Failed, token_sent: bool, cx: &mut Context<Self>) {
        if failed.state == ConnectionState::Unauthorized {
            self.refused_with_token = failed.token_refused.unwrap_or(token_sent);
            if token_sent && !self.asked_after_refusal && self.asks_token_source() {
                self.asked_after_refusal = true;
                self.ask_for_token(false, cx);
            }
        }
        self.state = failed.state;
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
    Failed(Failed),
}

/// A read that left the connection unusable: the state it leaves, and for a 401 whether the
/// server's challenge says it refused the token sent (`None` from a server that sends none,
/// one before OmniGraph #1122).
struct Failed {
    state: ConnectionState,
    token_refused: Option<bool>,
}

impl From<ConnectionState> for Failed {
    fn from(state: ConnectionState) -> Self {
        Self {
            state,
            token_refused: None,
        }
    }
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
        Err(state) => return Probe::Failed(state.into()),
    };
    let graphs = match graphs {
        Ok(graphs) => graphs,
        Err(failed) => return Probe::Failed(failed),
    };
    Probe::Ready {
        health: health.health,
        latency,
        graphs,
    }
}

/// What the server says about its graphs, or the state a refused token leaves the
/// connection in.
async fn list_graphs(client: &GraphClient) -> Result<GraphListing, Failed> {
    Ok(match client.graphs().await {
        Ok(list) => GraphListing::Listed(list.graphs),
        Err(ReadError::Refused(summary)) if summary.status == 401 => {
            return Err(Failed {
                state: ConnectionState::Unauthorized,
                token_refused: summary.challenge.as_deref().map(|c| c.token_refused()),
            });
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

    /// A connection whose token source has not answered waits to connect, sending nothing it
    /// would be refused; a token typed in connects it. A cancel stops the check scheduled
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
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, None, http).unwrap());
        connection.update(cx, |connection, cx| {
            connection.wait_for_a_token();
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

        // A check is scheduled while ready; connecting and cancelling leave none behind.
        connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.cancel(cx);
            assert!(connection._health_check.is_none());
            assert_eq!(connection.state, ConnectionState::Idle);
        });
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
            source: ServerSource::Cli,
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
            source: ServerSource::Cli,
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
            source: ServerSource::Cli,
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

    /// A server that speaks contract 0.13 (OmniGraph 0.13, or a 0.14 build before the contract
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
