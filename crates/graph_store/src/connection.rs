//! One Omnigraph server: its client, contract and version check, and graphs (spec 003).

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use gpui::{Context, EventEmitter, Task};
use graph_client::api::{GraphInfo, HealthOutput};
use graph_client::{GraphClient, ReadError};
use http_client::HttpClient;
use studio_settings::StudioSettings;

use crate::cli_tokens::{Lease, NoToken, Token, TokenSource};
use crate::sign_in::{self, OAuthSource, Service, SignIn, SignInContext};

/// Where Studio learned about a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerSource {
    /// A server target the Omnigraph CLI lists; its token, if any, is the CLI's to give.
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

impl ServerEntry {
    /// How what Studio saves of a graph names its server ([`crate::GraphSession::overrides_key`]):
    /// the target's name and its address, `<name>@<address>` (a target's name holds no `@`).
    /// The CLI gives a server target no identity that outlives its name: the owner and profile
    /// ids `credentials status` reports belong to the name, are new after a rename, and stay
    /// with the name when it is given to another server. A name alone would hand one server's
    /// saved state to another that took its name; with the address, that server starts with
    /// nothing saved, as a target renamed or moved to another address does.
    pub fn saved_key(&self) -> String {
        format!("{}@{}", self.name, self.url)
    }
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
    /// Why the token source gave no token (the Omnigraph CLI's reason, or why a sign-in's service
    /// did not answer): Studio then sends the server nothing. `None` once a token is set.
    pub token_unread: Option<String>,
    /// The source could not answer the last ask ([`NoToken::passing`]) and no token is held:
    /// [`Self::token_unread`] says why, a sign-in stands, and the next health check connects
    /// again, which asks again.
    pub token_unanswered: bool,
    /// Whether the server refused the token of the request it answered with 401
    /// ([`ConnectionState::Unauthorized`]): its challenge says `error="invalid_token"`, or, from
    /// a server that sends no challenge, the request carried one. Otherwise the server asks for
    /// a token rather than refusing one.
    pub refused_with_token: bool,
    /// Whether the client holds a token, so the next request sends one.
    has_token: bool,
    /// The token held is the one the current source last gave: an ask that fails for a reason
    /// that may pass leaves it in use while it lives ([`Self::keeps_token`]).
    token_from_source: bool,
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
    /// The credential generation ([`GraphClient::credential_changes`]) a graph read was last
    /// refused under ([`Connection::read_refused`]): the source is asked again once for each.
    read_refused_under: Option<u64>,
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
    /// What the person sees of signing in to the server (spec 016).
    pub sign_in: SignIn,
    /// What Studio needs to sign in to a server that asks; `None` for a connection that never
    /// signs in (the environment's server, tests).
    sign_in_context: Option<SignInContext>,
    /// The client the server is reached with, which a sign-in's requests to its address take.
    http: Arc<dyn HttpClient>,
    /// The sign-in service the server's 401 named, once read and checked.
    service: Option<Service>,
    /// The sign-in the token comes from, when it comes from one.
    oauth: Option<OAuthSource>,
    /// How long the short-lived token held stands, and whose it is: the next one of the same
    /// holder ([`Lease::holder`]) replaces it quietly.
    lease: Option<Lease>,
    /// The sign-in under way: reading its service, or waiting for the browser.
    _signing_in: Option<Task<()>>,
    /// The ask for the next short-lived token, before the one held ends.
    _renewal: Option<Task<()>>,
    /// The renewal came while nothing watched the server: the next watch asks.
    renewal_missed: bool,
}

/// What an ask for a token is for, which decides what its answer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// A connect waits for it, and goes on with what it gives.
    Connect,
    /// The token held is near its end: the next one of the same holder replaces it quietly.
    Renewal,
    /// The server refused the token held: its answer is tried at once, and no failure keeps
    /// the refused token.
    AfterRefusal,
    /// Asked again while the connection stands (Refresh, a changed target): a token that
    /// changed connects again.
    Again,
}

/// A renewal that failed for a reason that may pass is asked again halfway to the end of the
/// token held, while that is this far away or more.
const RENEWAL_RETRY_MIN: Duration = Duration::from_secs(5);

/// The HTTP client of a retired connection ([`Connection::retire`]): every request fails with
/// why, and nothing is sent.
struct Unusable(String);

impl HttpClient for Unusable {
    fn user_agent(&self) -> Option<&http_client::http::HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&http_client::Url> {
        None
    }

    fn send(
        &self,
        _: http_client::http::Request<http_client::AsyncBody>,
    ) -> futures::future::BoxFuture<
        'static,
        anyhow::Result<http_client::Response<http_client::AsyncBody>>,
    > {
        let why = self.0.clone();
        Box::pin(async move { Err(anyhow::anyhow!(why)) })
    }
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
        if self.state == ConnectionState::Unauthorized {
            // A sign-in whose service did not answer stands; it was neither refused nor ended.
            if self.token_unanswered && matches!(self.sign_in, SignIn::SignedIn { .. }) {
                return "Sign-in not answering".into();
            }
            match &self.sign_in {
                SignIn::NotOffered => {}
                SignIn::Reading => return "Reading how to sign in".into(),
                SignIn::Offered { .. } => return "Sign in needed".into(),
                SignIn::Unavailable(_) => return "Sign-in unavailable".into(),
                SignIn::Waiting { .. } => return "Signing in…".into(),
                SignIn::SignedIn { .. } => return "Not admitted".into(),
            }
        }
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
        let client = GraphClient::new(&server.url, token, http.clone())?;
        Ok(Self {
            server,
            client,
            state: ConnectionState::Idle,
            health: None,
            latency: None,
            graphs: GraphListing::Unknown,
            token_unread: None,
            token_unanswered: false,
            refused_with_token: false,
            has_token,
            token_from_source: false,
            token_source: None,
            token_given: false,
            token_fresh: false,
            asked_after_refusal: false,
            read_refused_under: None,
            token_answered: false,
            hurry: Arc::default(),
            _asking: None,
            _task: None,
            _listing: None,
            _health_check: None,
            watchers: Rc::new(()),
            check_missed: false,
            sign_in: SignIn::NotOffered,
            sign_in_context: None,
            http,
            service: None,
            oauth: None,
            lease: None,
            _signing_in: None,
            _renewal: None,
            renewal_missed: false,
        })
    }

    /// Signs in to the server through OAuth when it asks and the connection has no other
    /// credential (spec 016).
    pub(crate) fn with_sign_in(mut self, context: Option<SignInContext>) -> Self {
        self.sign_in_context = context;
        self
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
            ) -> futures::future::BoxFuture<'static, Result<Token, NoToken>> {
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
        // An anonymous target signed in keeps its sign-in: the CLI names no credential for it.
        if source.is_none() && self.oauth.is_some() {
            return;
        }
        if source.is_some() {
            self.drop_sign_in();
        }
        let had = self.token_source.is_some();
        self.token_source = source;
        // The token held is the old source's: the new one's failure does not keep it.
        self.token_from_source = false;
        if self.token_source.is_some() {
            self.refresh_token(cx);
        } else if had {
            self.token_given = false;
            self.replace_token(None, None, cx);
        }
    }

    /// Sends through `http` from now on, as when the CLI target's proxy or certificate authority
    /// changed: every holder of the client moves to it (the agent's gateway among them), and a
    /// server in use is checked again through it, so one the old transport could not reach does
    /// not wait for its next health check. The registry asks the token source again too, since
    /// the target changed.
    pub(crate) fn replace_http(&mut self, http: Arc<dyn HttpClient>, cx: &mut Context<Self>) {
        self.http = http.clone();
        self.client.set_http(http.clone());
        // A sign-in keeps going through the new client: its requests to the server's address
        // take it from now on, and a refresh under way finishes and is kept. A new sign-in read
        // from the keychain here would race that refresh for the refresh token it rotates.
        if let Some(service) = &mut self.service {
            service.clients = service.clients.through(http.clone());
        }
        if let Some(source) = &self.oauth {
            source.reach_through(http);
        }
        cx.emit(ConnectionEvent::CredentialChanged);
        if self.state != ConnectionState::Idle {
            self.connect(cx);
        }
    }

    /// Stops the connection for good, as when its CLI target is gone or names a transport Studio
    /// cannot use: the token and where it came from are dropped, every request fails with `why`
    /// without leaving Studio, and nothing is checked again. A window showing one of its graphs
    /// says so; opening the graph again reaches the target as the CLI lists it now.
    pub(crate) fn retire(&mut self, why: String, cx: &mut Context<Self>) {
        self.drop_sign_in();
        self.token_source = None;
        self.token_given = false;
        self.token_answered = true;
        self.client.set_token(None);
        self.has_token = false;
        self.token_from_source = false;
        self.token_unanswered = false;
        self.client.set_http(Arc::new(Unusable(why.clone())));
        self._asking = None;
        self._task = None;
        self._listing = None;
        self._health_check = None;
        self.state = ConnectionState::Unreachable(why);
        cx.emit(ConnectionEvent::CredentialChanged);
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
    }

    /// A read on one of the server's graphs was answered 401 while it carried the source's token:
    /// the token may have lapsed (a managed data token lives an hour), with `/healthz`, which
    /// carries none, still answering. The source is told ([`TokenSource::note_refused`]), then
    /// asked again, once for the token held: a renewed token replaces it and what was read is
    /// read again ([`ConnectionEvent::CredentialChanged`]); the same token leaves the read
    /// refused. A token typed in, or a server without a source, is not asked about. A refused
    /// write is never sent again.
    pub fn read_refused(&mut self, cx: &mut Context<Self>) {
        if !self.has_token || !self.asks_token_source() || !self.token_answered {
            return;
        }
        let generation = self.client.credential_changes();
        if self.read_refused_under == Some(generation) {
            return;
        }
        self.read_refused_under = Some(generation);
        if let Some(source) = &self.token_source {
            source.note_refused();
        }
        self.ask_for_token(Ask::AfterRefusal, cx);
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
            self.ask_for_token(Ask::Again, cx);
        }
    }

    /// Asks the token source for a token, for what `ask` says. A connect's ask is the connect's
    /// task; another replaces the one before it.
    fn ask_for_token(&mut self, ask: Ask, cx: &mut Context<Self>) {
        let Some(source) = self.token_source.clone() else {
            return;
        };
        let hurry = self.hurry.clone();
        let asking = cx.spawn(async move |this, cx| {
            let answer = cx.background_executor().spawn(source.token(hurry)).await;
            this.update(cx, |this, cx| this.token_answered(answer, ask, cx))
                .ok();
        });
        if ask == Ask::Connect {
            self._task = Some(asking);
        } else {
            self._asking = Some(asking);
        }
    }

    /// Whether a failure that may pass leaves the token held in use: the current source gave
    /// it, and it has not ended.
    fn keeps_token(&self) -> bool {
        self.has_token
            && self.token_from_source
            && self
                .lease
                .as_ref()
                .is_none_or(|lease| SystemTime::now() < lease.expires_at)
    }

    /// Takes the source's answer to `ask`. A token connects (for a connect under way, or when it
    /// changed). A source that could not answer leaves a token it gave in use while it lives
    /// ([`Self::keeps_token`]), unless the server just refused that token. Otherwise no token
    /// sends nothing more, the connection refused with why. The answer to a connect cancelled
    /// while a window waited for its token ([`Self::cancel`]) is taken, and the connection
    /// stays as Cancel left it.
    fn token_answered(&mut self, answer: Result<Token, NoToken>, ask: Ask, cx: &mut Context<Self>) {
        if ask != Ask::Connect {
            self._asking = None;
        }
        let connecting = ask == Ask::Connect && self.state == ConnectionState::Connecting;
        let was_ready = self.token_ready();
        self.token_answered = true;
        self.hurry.store(false, Ordering::SeqCst);
        if !was_ready {
            cx.emit(ConnectionEvent::TokenReady);
        }
        let token = match answer {
            Ok(token) => token,
            Err(no) if no.passing && ask != Ask::AfterRefusal && self.keeps_token() => {
                self.keep_token(ask, connecting, cx);
                return;
            }
            Err(no) => {
                self.no_token(no, connecting, cx);
                return;
            }
        };
        let lease = token.lease().cloned();
        self.note_signed_in();
        if let Some(lease) = &lease {
            self.schedule_renewal(lease.renew_at, cx);
        }
        // A renewal, the same holder's next short-lived token while the connection stands:
        // nothing read with the last one is read again.
        let renewal = ask == Ask::Renewal
            && self.has_token
            && lease.is_some()
            && lease.as_ref().map(|lease| &lease.holder)
                == self.lease.as_ref().map(|lease| &lease.holder);
        self.lease = lease;
        self.token_from_source = true;
        self.token_unanswered = false;
        if renewal {
            self.client.renew_token(token.into_string());
            cx.emit(ConnectionEvent::StateChanged);
            cx.notify();
            return;
        }
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

    /// Says the sign-in the token comes from as signed in, when it comes from one.
    fn note_signed_in(&mut self) {
        if let Some(source) = &self.oauth {
            self.sign_in = SignIn::SignedIn {
                service: source.service().host(),
                account: source.account(),
                kept: source.kept_in_keychain(),
            };
        }
    }

    /// The source could not answer `ask` now, and the token it gave before still lives: it
    /// stands, unchanged and unsaid (spec 016, refreshes are silent). A connect goes on with it,
    /// and a renewal is asked again halfway to the token's end.
    fn keep_token(&mut self, ask: Ask, connecting: bool, cx: &mut Context<Self>) {
        if ask == Ask::Renewal
            && let Some(lease) = &self.lease
        {
            let now = SystemTime::now();
            let left = lease.expires_at.duration_since(now).unwrap_or_default();
            if left / 2 >= RENEWAL_RETRY_MIN {
                self.schedule_renewal(now + left / 2, cx);
            }
        }
        if connecting {
            self.token_fresh = true;
            self.connect(cx);
        }
    }

    /// The source gave no token, and none is kept: nothing more is sent, and the connection says
    /// why. A source that answered it has none ends a sign-in, which the server offers again;
    /// one that could not answer leaves a sign-in standing, and is asked again at the next
    /// health check ([`Self::token_unanswered`]).
    fn no_token(&mut self, no: NoToken, connecting: bool, cx: &mut Context<Self>) {
        self.has_token = false;
        self.token_from_source = false;
        self.lease = None;
        self._renewal = None;
        if self.client.set_token(None) {
            cx.emit(ConnectionEvent::CredentialChanged);
        }
        self.token_unanswered = no.passing;
        if no.passing {
            self.note_signed_in();
            self.token_unread = Some(no.why);
        } else if let Some(source) = self.oauth.take() {
            self.token_source = None;
            self.token_unread = None;
            self.sign_in = SignIn::Offered {
                service: source.service().host(),
                why: Some(no.why),
            };
        } else {
            self.token_unread = Some(no.why);
        }
        if connecting || self.state != ConnectionState::Idle {
            self._task = None;
            self.refused_with_token = false;
            self.state = ConnectionState::Unauthorized;
            self.schedule_health_check(cx);
        }
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
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
        self.token_from_source = false;
        self.token_unanswered = false;
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
        if std::mem::take(&mut self.renewal_missed) {
            self.ask_for_token(Ask::Renewal, cx);
        }
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

    /// Stops a connection attempt that is taking too long. A connect still waiting for its
    /// token while a window waits for that token too ([`Self::wait_for_token`]) keeps asking
    /// for it: the window's first reads go once it answers, and the connect stops there
    /// ([`Self::token_answered`]). Dropped, the ask would leave the window waiting for a token
    /// nothing asks for, empty until it is hidden and shown again.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.state == ConnectionState::Connecting {
            let window_waits = !self.token_ready() && self.hurry.load(Ordering::SeqCst);
            if !window_waits {
                self._task = None;
            }
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
            self.ask_for_token(Ask::Connect, cx);
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

    /// The CLI profile whose managed data token this server's token is, when it is one
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
            // A refused token is not renewed: the ask below is the one more try, and a token
            // the server takes schedules its own renewal.
            self._renewal = None;
            self.renewal_missed = false;
            if token_sent && !self.asked_after_refusal && self.asks_token_source() {
                self.asked_after_refusal = true;
                if let Some(source) = &self.token_source {
                    source.note_refused();
                }
                self.ask_for_token(Ask::AfterRefusal, cx);
            }
            if let Some(metadata) = failed.sign_in {
                self.offered_sign_in(metadata, cx);
            }
        }
        self.state = failed.state;
    }

    /// The server's 401 offers a sign-in, its details at `metadata`: Studio reads them, and a
    /// sign-in the keychain kept for this target is refreshed without asking. A connection whose
    /// credential comes from the CLI keeps it: a refused one is said as refused (spec 016, When
    /// Studio signs in). Read once; a sign-out or a failed sign-in keeps what was read.
    fn offered_sign_in(&mut self, metadata: String, cx: &mut Context<Self>) {
        let Some(context) = self.sign_in_context.clone() else {
            return;
        };
        if (self.token_source.is_some() && self.oauth.is_none())
            || self.sign_in != SignIn::NotOffered
            || self._signing_in.is_some()
        {
            return;
        }
        let Ok(endpoint) = url::Url::parse(&self.server.url) else {
            return;
        };
        self.sign_in = SignIn::Reading;
        let clients =
            oauth_client::Clients::new(endpoint, self.http.clone(), context.elsewhere.clone());
        let target = self.server.name.clone();
        self._signing_in = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(sign_in::discover(
                    clients,
                    metadata,
                    context.identities.clone(),
                ))
                .await;
            let kept = match &found {
                Ok(service) => context
                    .keychain
                    .read(service.key(&target))
                    .await
                    .ok()
                    .flatten(),
                Err(_) => None,
            };
            this.update(cx, |this, cx| {
                this._signing_in = None;
                match found {
                    Err(why) => this.sign_in = SignIn::Unavailable(why),
                    Ok(service) => {
                        let service = this.current(service);
                        let host = service.host();
                        this.service = Some(service.clone());
                        this.sign_in = SignIn::Offered {
                            service: host,
                            why: None,
                        };
                        if let Some((subject, refresh)) = kept
                            && let Ok(refresh) = String::from_utf8(refresh)
                        {
                            let source = OAuthSource::kept(
                                service,
                                &target,
                                context.keychain.clone(),
                                subject,
                                refresh,
                            );
                            this.use_sign_in(source, cx);
                        }
                    }
                }
                cx.emit(ConnectionEvent::StateChanged);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Signs in through the person's browser, as they asked (spec 016, Signing in): Studio
    /// listens on a loopback port, opens the sign-in service's page, and exchanges the code it
    /// returns with. The listener closes after one answer, five minutes, or Cancel.
    pub fn sign_in(&mut self, cx: &mut Context<Self>) {
        let (Some(service), Some(context)) = (self.service.clone(), self.sign_in_context.clone())
        else {
            return;
        };
        let started = oauth_client::Callback::bind().and_then(|callback| {
            Ok((
                callback,
                oauth_client::Pkce::new()?,
                oauth_client::random_token()?,
            ))
        });
        let (callback, pkce, state) = match started {
            Ok(started) => started,
            Err(error) => {
                self.sign_in = SignIn::Offered {
                    service: service.host(),
                    why: Some(format!("Studio could not wait for the sign-in: {error}.")),
                };
                cx.emit(ConnectionEvent::StateChanged);
                cx.notify();
                return;
            }
        };
        let page = oauth_client::authorization_url(
            &service.server,
            &service.client_id,
            callback.redirect_uri(),
            &pkce,
            &state,
            &service.resource.resource,
        );
        cx.open_url(page.as_str());
        self.sign_in = SignIn::Waiting {
            service: service.host(),
        };
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
        let target = self.server.name.clone();
        let host = oauth_client::host_of(&self.server.url);
        // The sign-in this one replaces, as another account after Not admitted.
        let previous = self.oauth.clone();
        self._signing_in = Some(cx.spawn(async move |this, cx| {
            let redirect = callback.redirect_uri().to_owned();
            let issuer = service.server.issuer.clone();
            let exchanged = service.clone();
            let signed_in = cx
                .background_executor()
                .spawn(async move {
                    let code = smol::future::or(
                        async { Some(callback.code(&state, &issuer, &host).await) },
                        async {
                            smol::Timer::after(oauth_client::SIGN_IN_WAIT).await;
                            None
                        },
                    )
                    .await
                    .ok_or("The sign-in was not finished within five minutes.".to_owned())?
                    .map_err(|error| error.to_string())?;
                    let service = exchanged;
                    oauth_client::exchange(
                        service.clients.for_url(&service.server.token_endpoint),
                        &service.server,
                        &code,
                        &redirect,
                        &pkce.verifier,
                        service.expected(),
                    )
                    .await
                    .map_err(|error| error.to_string())
                })
                .await;
            // One account per target: the one replaced ends, its refresh token revoked, and the
            // keychain keeps only the new one's, or none, never the old account's.
            if signed_in.is_ok()
                && let Some(previous) = &previous
            {
                previous.replaced().await;
            }
            let kept = match &signed_in {
                Ok(tokens) => match &tokens.refresh_token {
                    Some(refresh) => context
                        .keychain
                        .write(
                            service.key(&target),
                            tokens.account.subject.clone(),
                            refresh.as_bytes().to_vec(),
                        )
                        .await
                        .is_ok(),
                    None => {
                        context.keychain.delete(service.key(&target)).await.ok();
                        false
                    }
                },
                Err(_) => false,
            };
            this.update(cx, |this, cx| {
                this._signing_in = None;
                match signed_in {
                    Ok(tokens) => {
                        let source = OAuthSource::signed_in(
                            this.current(service),
                            &target,
                            context.keychain.clone(),
                            tokens,
                            kept,
                        );
                        this.use_sign_in(source, cx);
                    }
                    Err(why) => {
                        this.sign_in = SignIn::Offered {
                            service: service.host(),
                            why: Some(why),
                        };
                        cx.emit(ConnectionEvent::StateChanged);
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    /// Stops waiting for the browser: its listener closes, and nothing it returns is taken.
    pub fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if let SignIn::Waiting { service } = &self.sign_in {
            self.sign_in = SignIn::Offered {
                service: service.clone(),
                why: None,
            };
            self._signing_in = None;
            cx.emit(ConnectionEvent::StateChanged);
            cx.notify();
        }
    }

    /// Signs out: the tokens are dropped, the keychain item deleted and, where the service
    /// takes it, the refresh token revoked; the connection checks again without a token.
    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.oauth.take() else {
            return;
        };
        self.token_source = None;
        self.lease = None;
        self._renewal = None;
        cx.background_executor().spawn(source.sign_out()).detach();
        self.sign_in = SignIn::Offered {
            service: source.service().host(),
            why: None,
        };
        self.replace_token(None, None, cx);
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
    }

    /// `service`, read or signed in through clients the connection may have replaced since it
    /// started ([`Self::replace_http`]): its requests to the server's address take the client
    /// held now, not the one a discovery or browser wait started with.
    fn current(&self, service: Service) -> Service {
        Service {
            clients: service.clients.through(self.http.clone()),
            ..service
        }
    }

    /// Takes the token from `source`, a sign-in just made or kept, and connects with it.
    fn use_sign_in(&mut self, source: OAuthSource, cx: &mut Context<Self>) {
        self.oauth = Some(source.clone());
        self.token_source = Some(Arc::new(source));
        self.token_given = false;
        self.asked_after_refusal = false;
        // The token held, if any, is another credential's: the sign-in's failure does not keep it.
        self.token_from_source = false;
        self.lease = None;
        self._renewal = None;
        self.renewal_missed = false;
        self.connect(cx);
    }

    /// Forgets the sign-in, which the CLI's credential for the target replaces. The keychain
    /// keeps it, for the target made anonymous again.
    fn drop_sign_in(&mut self) {
        self.oauth = None;
        self.service = None;
        self.lease = None;
        self._renewal = None;
        self._signing_in = None;
        self.sign_in = SignIn::NotOffered;
    }

    /// Asks for the next short-lived token `at` that time, if a view still shows the server;
    /// otherwise the next view to show it asks.
    fn schedule_renewal(&mut self, at: SystemTime, cx: &mut Context<Self>) {
        let wait = at.duration_since(SystemTime::now()).unwrap_or_default();
        self._renewal = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |this, cx| {
                // A server not ready is asked again by its next connect, which asks for a token.
                if this.state != ConnectionState::Ready {
                    return;
                }
                if this.watched() {
                    this.ask_for_token(Ask::Renewal, cx);
                } else {
                    this.renewal_missed = true;
                }
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
        // How soon a ready server whose check failed once is checked again.
        const RECHECK: Duration = Duration::from_secs(5);
        // The first failure of a ready server after a look that answered (a slow answer, a
        // moment without network) is checked again soon, the server still shown ready: only a
        // second failure in a row shows it unreachable. The latency is that look's answer.
        let first_failure =
            health.is_err() && self.state == ConnectionState::Ready && self.latency.is_some();
        let latency = health.is_ok().then_some(latency);
        // Shown in whole milliseconds, so a change under one is not a change.
        if latency.map(|l| l.as_millis()) != self.latency.map(|l| l.as_millis()) {
            self.latency = latency;
            cx.notify();
        }
        if first_failure {
            self.check_health_after(RECHECK, cx);
            return;
        }
        let next = match served(health) {
            Ok(_) if self.state == ConnectionState::Ready => ConnectionState::Ready,
            // `/healthz` asks for no token, so it passes for a refused one too: probing again
            // would be refused alike, and would take the token field away while it is typed
            // in. A new token connects again, and a source that could not answer is asked
            // again by the connect below.
            Ok(_) if self.state == ConnectionState::Unauthorized && !self.token_unanswered => {
                ConnectionState::Unauthorized
            }
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
/// one before Omnigraph #1122).
struct Failed {
    state: ConnectionState,
    token_refused: Option<bool>,
    /// Where the 401's challenge says the server's sign-in details are
    /// (`resource_metadata`), when it offers a sign-in.
    sign_in: Option<String>,
}

impl From<ConnectionState> for Failed {
    fn from(state: ConnectionState) -> Self {
        Self {
            state,
            token_refused: None,
            sign_in: None,
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
                sign_in: summary
                    .challenge
                    .as_deref()
                    .and_then(|c| c.resource_metadata())
                    .map(str::to_owned),
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

    /// One failed check of a ready server is checked again soon, the server still shown ready;
    /// a second failure in a row shows it unreachable.
    #[gpui::test]
    async fn one_failed_check_of_a_ready_server_is_checked_again_soon(cx: &mut TestAppContext) {
        let down = Arc::new(AtomicBool::new(false));
        let failing = down.clone();
        let http = FakeHttpClient::create(move |request| {
            let health = request.uri().path().ends_with("/healthz");
            let failed = health && failing.load(Ordering::SeqCst);
            async move {
                let (status, body) = if failed {
                    (503, r#"{"error":"down"}"#)
                } else if health {
                    (
                        200,
                        r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#,
                    )
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
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let _shown = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let state = |cx: &mut TestAppContext| connection.read_with(cx, |c, _| c.state.clone());
        assert_eq!(state(cx), ConnectionState::Ready);

        down.store(true, Ordering::SeqCst);
        cx.executor().advance_clock(DEFAULT_CHECK_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            state(cx),
            ConnectionState::Ready,
            "one failure is checked again"
        );
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();
        assert!(
            matches!(state(cx), ConnectionState::Unreachable(_)),
            "{:?}",
            state(cx)
        );
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

    /// A graph read refused with 401 while it carried the source's token asks the source again,
    /// once for that token and telling it first: a renewed token replaces the lapsed one, a
    /// second refusal of the same token asks nothing more, and a token typed in is not asked
    /// about.
    #[gpui::test]
    async fn a_read_refused_for_its_token_asks_the_source_once(cx: &mut TestAppContext) {
        use std::sync::atomic::AtomicUsize;
        #[derive(Default)]
        struct Renewing {
            given: std::sync::Mutex<Vec<&'static str>>,
            asked: AtomicUsize,
            refused: AtomicUsize,
        }
        impl TokenSource for Renewing {
            fn token(
                &self,
                _: Arc<AtomicBool>,
            ) -> futures::future::BoxFuture<'static, Result<Token, NoToken>> {
                self.asked.fetch_add(1, Ordering::SeqCst);
                let mut given = self.given.lock().unwrap();
                let token = if given.len() > 1 {
                    given.remove(0)
                } else {
                    given[0]
                };
                Box::pin(async move { Ok(Token::new(token)) })
            }
            fn note_refused(&self) {
                self.refused.fetch_add(1, Ordering::SeqCst);
            }
        }
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
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let source = Arc::new(Renewing::default());
        *source.given.lock().unwrap() = vec!["lapsed", "renewed"];
        let server = ServerEntry {
            name: "managed".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let held: Arc<dyn TokenSource> = source.clone();
        let connection = cx.new(|_| {
            Connection::new(server, None, http)
                .unwrap()
                .with_token_source(Some(held))
        });
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        let asked = || source.asked.load(Ordering::SeqCst);
        let refused = || source.refused.load(Ordering::SeqCst);
        let changes = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| connection.client().credential_changes())
        };
        assert_eq!(asked(), 1);
        let lapsed = changes(cx);

        connection.update(cx, |connection, cx| connection.read_refused(cx));
        cx.run_until_parked();
        assert_eq!((asked(), refused()), (2, 1));
        assert_eq!(changes(cx), lapsed + 1, "the renewed token replaced it");

        // Refused again under the renewed token: asked once more, which gives the same one.
        connection.update(cx, |connection, cx| connection.read_refused(cx));
        cx.run_until_parked();
        assert_eq!((asked(), refused()), (3, 2));
        connection.update(cx, |connection, cx| connection.read_refused(cx));
        cx.run_until_parked();
        assert_eq!(
            (asked(), refused()),
            (3, 2),
            "the same token is not asked about twice"
        );

        connection.update(cx, |connection, cx| {
            connection.set_token(Some("typed".into()), cx);
            connection.read_refused(cx);
        });
        cx.run_until_parked();
        assert_eq!(asked(), 3, "a token typed in is not asked about");
    }

    /// A renewal the source cannot answer now leaves the token held in use, unsaid: the
    /// connection stays ready, the credential is the same, and the renewal is asked again
    /// halfway to the token's end, whose answer replaces it quietly. Before, the working
    /// token was dropped and the server shown refused.
    #[gpui::test]
    async fn a_renewal_that_cannot_be_answered_keeps_the_token_held(cx: &mut TestAppContext) {
        use std::sync::atomic::AtomicUsize;
        /// Tokens that live two minutes and are renewed after one, or `None` for an ask it
        /// cannot answer now.
        struct Leasing {
            given: std::sync::Mutex<Vec<Option<&'static str>>>,
            asked: AtomicUsize,
        }
        impl TokenSource for Leasing {
            fn token(
                &self,
                _: Arc<AtomicBool>,
            ) -> futures::future::BoxFuture<'static, Result<Token, NoToken>> {
                self.asked.fetch_add(1, Ordering::SeqCst);
                let given = self.given.lock().unwrap().remove(0);
                let now = SystemTime::now();
                let answer = given
                    .map(|token| {
                        Token::leased(
                            token.into(),
                            Lease {
                                expires_at: now + Duration::from_secs(120),
                                renew_at: now + Duration::from_secs(60),
                                holder: "user_1".into(),
                            },
                        )
                    })
                    .ok_or_else(|| NoToken::passing("the sign-in service did not answer"));
                Box::pin(async move { answer })
            }
        }
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
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let source = Arc::new(Leasing {
            given: std::sync::Mutex::new(vec![Some("first"), None, Some("second")]),
            asked: AtomicUsize::new(0),
        });
        let server = ServerEntry {
            name: "signed".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let held: Arc<dyn TokenSource> = source.clone();
        let connection = cx.new(|_| {
            Connection::new(server, None, http)
                .unwrap()
                .with_token_source(Some(held))
        });
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let asked = || source.asked.load(Ordering::SeqCst);
        let held = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| {
                (
                    connection.state.clone(),
                    connection.client().credential_changes(),
                    connection.token_unread.clone(),
                )
            })
        };
        assert_eq!(asked(), 1);
        let (state, changes, unread) = held(cx);
        assert_eq!((state, unread), (ConnectionState::Ready, None));

        // The renewal, a minute in, is not answered: the token stands, and is asked for again
        // halfway to its end, a minute later, not before.
        cx.executor().advance_clock(Duration::from_secs(60));
        cx.run_until_parked();
        assert_eq!(asked(), 2);
        assert_eq!(held(cx), (ConnectionState::Ready, changes, None));
        cx.executor().advance_clock(Duration::from_secs(50));
        cx.run_until_parked();
        assert_eq!(asked(), 2, "not asked again before halfway");
        cx.executor().advance_clock(Duration::from_secs(15));
        cx.run_until_parked();
        assert_eq!(asked(), 3);
        assert_eq!(
            held(cx),
            (ConnectionState::Ready, changes, None),
            "the next token replaced it quietly"
        );
    }
}
