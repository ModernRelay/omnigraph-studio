//! One Omnigraph server: its client, contract and version check, and graphs (spec 003).

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use futures::channel::oneshot;
use gpui::{Context, EventEmitter, Task};
use graph_client::api::{GraphInfo, HealthOutput};
use graph_client::{Challenge, GraphClient, ReadError, Unauthorized};
use http_client::HttpClient;
use oauth_client::TokenError;
use studio_settings::StudioSettings;

use crate::cli_tokens::{Lease, NoToken, Token, TokenSource};
use crate::sign_in::{
    self, End, Kept, KeychainTurn, OAuthSource, Refusal, Service, SignIn, SignInContext,
};

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
    /// The server answered 401: the token sent was refused, the one sent never arrived, or none
    /// was sent ([`Connection::refusal`] says which).
    /// Also a server whose token source gave none ([`Connection::token_unread`] says why), which
    /// is asked nothing without one.
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

/// What a window says while its server asks for a sign-in nobody holds, and what it offers
/// ([`Connection::sign_in_prompt`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInPrompt {
    /// The server's name, as the window names it.
    pub server: String,
    /// The sentence: who asks, where the sign-in is, or why Studio cannot make it.
    pub says: String,
    pub offers: Option<PromptAction>,
}

/// The one action a sign-in prompt offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAction {
    /// Opens the sign-in service in the browser ([`Connection::sign_in`]).
    SignIn,
    /// Ends the sign-in waiting for the browser ([`Connection::cancel_sign_in`]).
    Cancel,
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

/// Why a 401 stands while a sign-in is held ([`Connection::held_refusal`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldRefusal {
    /// The sign-in's service did not answer: the sign-in stands, and is asked again.
    NotAnswering,
    /// The 401 refused no token though one was sent: it never arrived.
    NotReceived,
    /// The server refused the token: it may not admit the account.
    NotAdmitted,
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
    /// What the 401 to the connect's probe or to the graph list said of the token it sent
    /// ([`ConnectionState::Unauthorized`]), judged as every request's is ([`Unauthorized`]):
    /// refused, never received (a proxy that drops it), or none sent. `None` while no such 401
    /// stands, as when the source gave no token ([`Self::token_unread`]).
    pub refusal: Option<Unauthorized>,
    /// The token held is the one the current source last gave: an ask that fails for a reason
    /// that may pass leaves it in use while it lives ([`Self::keeps_token`]).
    token_from_source: bool,
    /// Where the token comes from each time it is asked ([`Connection::with_token_source`]): a
    /// CLI server's is the CLI's `credentials export`. `None` for a server given its token.
    token_source: Option<Arc<dyn TokenSource>>,
    /// The token held was given (typed in), so the source is not asked until
    /// [`Connection::refresh_token`] or a new source ([`Connection::take_token_from`]).
    token_given: bool,
    /// The token held is the one the source just gave for the connect under way.
    token_fresh: bool,
    /// The source was asked again after a 401 that refused its token, which it is once until a
    /// connect is ready.
    asked_after_refusal: bool,
    /// The token ([`GraphClient::token_serial`]) a 401 to the connect's probe or to the graph
    /// list refused ([`Connection::refused`], [`Connection::refusal`]): while it is
    /// held, the next is a new credential whatever asks for it, a connect from that state
    /// included, as a transport change starts, and a failure to give one does not keep it. A
    /// probe the server answers clears it.
    probe_refused: Option<u64>,
    /// The source has answered since it was set, with a token or why it gave none: requests
    /// sent from then carry what it gave ([`Connection::token_ready`]).
    token_answered: bool,
    /// Raised while a window waits for the source's answer ([`Connection::wait_for_token`]):
    /// the CLI serves this ask before the others waiting for it. Lowered when it answers.
    hurry: Arc<AtomicBool>,
    /// The source asked again while the connection stands (after a 401 that refused its token, on
    /// Refresh, for a changed target, for a renewal), and what for. A change of source ends it
    /// ([`Connection::replace_source`]).
    _asking: Option<(Ask, Task<()>)>,
    /// Answers a request that found the token held ended ([`GraphClient::token_ended`]), from
    /// the first connect on: asks for the next, checks a server away, or releases the requests
    /// waiting ([`Connection::token_ended`]).
    _ended: Option<Task<()>>,
    /// The connect under way ([`Connection::connect`]) and its step: asking its token source
    /// for a token (an ask a window waits on outlives a Cancel), or probing the server with what
    /// it holds.
    _connect: Option<(ConnectStep, Task<()>)>,
    /// The graph list read again after it could not be read ([`Connection::list_graphs_again`]).
    _listing: Option<Task<()>>,
    /// The next health re-check (spec 003), or one a request started because it found the token
    /// ended while the server could not be reached ([`Connection::token_ended`]).
    _health_check: Option<HealthCheck>,
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
    /// A managed target's CLI login, held back until the server's 401 says whether it offers a
    /// sign-in of its own (spec 016, When Studio signs in): one that does is signed in to, and
    /// this is dropped; one that offers none takes this as its token source.
    standby: Option<Arc<dyn TokenSource>>,
    /// The server's 401 offered no sign-in while a managed target's CLI login was held back, so
    /// that login is the token source from then on ([`Self::standby`]).
    offers_no_sign_in: bool,
    /// The organization a sign-in to the server must be in: the one a managed target's profile
    /// names ([`TokenSource::organization`]).
    organization: Option<String>,
    /// The managed profile `organization` was named for.
    organization_profile: Option<String>,
    /// The sign-in service the server's 401 named, once read and checked; none again after a
    /// sign-in failed, as what it named may be what failed.
    service: Option<Service>,
    /// Where the server's 401 said its sign-in details are, read again for a sign-in after one
    /// failed.
    sign_in_details: Option<String>,
    /// The sign-in the token comes from, when it comes from one.
    oauth: Option<OAuthSource>,
    /// How long the short-lived token held stands, and whose it is: the next one of the same
    /// holder ([`Lease::holder`]), from the same source, replaces it quietly, unless a 401 stands
    /// or it answers a Refresh.
    lease: Option<Lease>,
    /// The sign-in under way: reading its service, waiting for the browser, or finishing.
    signing_in: Option<SigningIn>,
    /// What the keychain holds for the target that a sign-in that ended left there, which the
    /// next launch reads: its item, kept for the target made anonymous again, or one the
    /// keychain would not delete. While a sign-in is held, it answers for the item itself.
    left: Kept,
    /// The ask for the next short-lived token, before the one held ends.
    _renewal: Option<Task<()>>,
    /// The renewal came while nothing watched the server: the next watch asks, unless a renewal
    /// was scheduled since ([`Connection::schedule_renewal`]).
    renewal_missed: bool,
    /// How many checks in a row have come after a sign-in's service did not answer: the next
    /// comes after the ready interval, doubled for each, up to a failed check's. Any answer
    /// starts it again.
    unanswered_checks: u32,
}

/// A sign-in under way.
struct SigningIn {
    _task: Task<()>,
    /// Set once it is finishing, when it is not cancelled ([`Connection::end_sign_ins`]).
    finish: Option<Told>,
    /// The sign-in it would replace, when the service ended that one meanwhile, and why: ended
    /// as replaced, which this one waits for before it reaches the keychain, and said should
    /// this one not be taken.
    replaced: Option<(OAuthSource, String)>,
}

impl SigningIn {
    fn new(task: Task<()>) -> Self {
        Self {
            _task: task,
            finish: None,
            replaced: None,
        }
    }
}

/// How a finishing sign-in is to end once kept, when a sign-out or a drop ended it meanwhile, as
/// one held would; untold, it is taken. The connection and the sign-in finished share it.
type Told = Arc<Mutex<Option<End>>>;

/// A sign-in whose keychain work is done, until the connection takes it: the sign-in, the token
/// of an earlier item the keychain let go of (held by nothing, so revoked), what it was told, and
/// its item's keychain turn. Dropped untaken (its connection released before it took it), it ends
/// as told, or untold as for a retired connection ([`End::Dropped`]), and its turn passes on only
/// once that ending has dealt with the item.
struct Finished {
    source: Option<OAuthSource>,
    let_go: Option<String>,
    told: Told,
    done: Option<oneshot::Sender<()>>,
}

impl Finished {
    /// The connection takes it, and answers for all of it from here.
    fn take(
        mut self,
    ) -> (
        OAuthSource,
        Option<String>,
        Option<End>,
        oneshot::Sender<()>,
    ) {
        (
            self.source.take().expect("a finished sign-in, taken once"),
            self.let_go.take(),
            told(&self.told),
            self.done
                .take()
                .expect("a finished sign-in's keychain turn"),
        )
    }
}

impl Drop for Finished {
    fn drop(&mut self) {
        let (Some(source), Some(done)) = (self.source.take(), self.done.take()) else {
            return;
        };
        if let Some(token) = self.let_go.take() {
            source.revoke(token);
        }
        let ending = source.end(told(&self.told).unwrap_or(End::Dropped), None);
        smol::spawn(async move {
            ending.await;
            done.send(()).ok();
        })
        .detach();
    }
}

/// What `told` says now.
fn told(told: &Told) -> Option<End> {
    *told.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A health check under way ([`Connection::check_health_after`]).
struct HealthCheck {
    /// For one a request started because it found the token ended: the requests it answers
    /// for, those waiting when it started ([`GraphClient::waits_begun`]). It is made whether
    /// or not anything watches, and its answer connects again, which asks for the next token,
    /// leaves those requests to an ask begun meanwhile ([`Connection::next_token_coming`]), or
    /// releases them.
    for_waits: Option<u64>,
    _task: Task<()>,
}

/// A connect's step ([`Connection::connect`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectStep {
    /// Asking the token source for a token, whose answer goes on with the connect.
    Asking,
    /// Probing the server with the token held.
    Probing,
}

/// What an ask for a token is for, which decides what its answer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// A connect waits for it, and goes on with what it gives.
    Connect,
    /// The token held is near its end. The next one of the same holder, from the same source,
    /// replaces it quietly, unless a 401 stands; a Refresh's never does
    /// ([`Connection::token_answered`] decides for every answer, a connect's too).
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

/// Health re-check interval after a failure (spec 003).
pub(crate) const FAILED_CHECK_INTERVAL: Duration = Duration::from_secs(120);
/// Interval when settings are not loaded, as in tests.
pub(crate) const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(30);

impl EventEmitter<ConnectionEvent> for Connection {}

impl Connection {
    /// What a 401 says of the sign-in held ([`Self::held_refusal`]), in the status bar.
    fn held_sign_in_label(&self) -> String {
        match self.held_refusal() {
            HeldRefusal::NotAnswering => "Sign-in not answering",
            HeldRefusal::NotReceived => "Token not received",
            HeldRefusal::NotAdmitted => "Not admitted",
        }
        .into()
    }

    /// Why a 401 stands while a sign-in is held, as the status bar and the Open dialog both say
    /// it: its service did not answer (the sign-in stands; it was neither refused nor ended),
    /// the token sent never arrived, or else the server does not admit the account.
    pub fn held_refusal(&self) -> HeldRefusal {
        if self.token_unanswered {
            HeldRefusal::NotAnswering
        } else if self.token_not_received() {
            HeldRefusal::NotReceived
        } else {
            HeldRefusal::NotAdmitted
        }
    }

    /// The status-bar text for its state: a 401 to a request that carried no token says the
    /// server asks for one, and a token source that gave none says so, rather than that a token
    /// was refused.
    pub fn label(&self) -> String {
        if self.state == ConnectionState::Unauthorized {
            match &self.sign_in {
                SignIn::NotOffered if self.signing_in.is_some() => return "Signing in…".into(),
                SignIn::NotOffered => {}
                SignIn::Reading => return "Reading how to sign in".into(),
                // A sign-in as someone else that failed, the one held standing: the server does
                // not admit it, its service did not answer, or the token sent never arrived.
                SignIn::Offered { .. } if self.oauth.is_some() => return self.held_sign_in_label(),
                SignIn::Offered { .. } => return "Sign in needed".into(),
                SignIn::Unavailable(_) => return "Sign-in unavailable".into(),
                SignIn::Waiting { .. } | SignIn::Finishing { .. } => return "Signing in…".into(),
                SignIn::SignedIn { .. } => return self.held_sign_in_label(),
            }
        }
        match (&self.state, &self.token_unread) {
            (ConnectionState::Unauthorized, Some(_)) => "No token from the CLI".into(),
            (ConnectionState::Unauthorized, None) if self.token_not_received() => {
                "Token not received".into()
            }
            (ConnectionState::Unauthorized, None)
                if self.refusal != Some(Unauthorized::TokenRefused) =>
            {
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
        let client = GraphClient::new(&server.url, token, http)?;
        Ok(Self {
            server,
            client,
            state: ConnectionState::Idle,
            health: None,
            latency: None,
            graphs: GraphListing::Unknown,
            token_unread: None,
            token_unanswered: false,
            refusal: None,
            token_from_source: false,
            token_source: None,
            token_given: false,
            token_fresh: false,
            asked_after_refusal: false,
            probe_refused: None,
            token_answered: false,
            hurry: Arc::default(),
            _asking: None,
            _ended: None,
            _connect: None,
            _listing: None,
            _health_check: None,
            watchers: Rc::new(()),
            check_missed: false,
            sign_in: SignIn::NotOffered,
            sign_in_context: None,
            standby: None,
            offers_no_sign_in: false,
            organization: None,
            organization_profile: None,
            service: None,
            sign_in_details: None,
            oauth: None,
            lease: None,
            signing_in: None,
            left: Kept::Nothing,
            _renewal: None,
            renewal_missed: false,
            unanswered_checks: 0,
        })
    }

    /// Signs in to the server through OAuth when it asks and the connection has no other
    /// credential (spec 016).
    pub(crate) fn with_sign_in(mut self, context: Option<SignInContext>) -> Self {
        self.sign_in_context = context;
        self
    }

    /// Takes its token from `source` each time it is asked: on each connect, before anything is
    /// sent; once more after a 401 that refused its token; and on Refresh
    /// ([`Connection::refresh_token`]). A server with a source is sent nothing without a token
    /// from it.
    pub(crate) fn with_token_source(mut self, source: Option<Arc<dyn TokenSource>>) -> Self {
        self.token_source = self.hold_back(source);
        self
    }

    /// The source to take for `source`: none while it is a managed target's CLI login and the
    /// server has not said that it offers no sign-in, which is then held back as the standby
    /// ([`Self::standby`]). Notes the organization a sign-in must be in.
    fn hold_back(&mut self, source: Option<Arc<dyn TokenSource>>) -> Option<Arc<dyn TokenSource>> {
        let profile = source.as_ref().and_then(|source| source.login_profile());
        let login = profile.is_some();
        // A managed login of the same profile whose organization the CLI would not say this time
        // (a read it missed) keeps the one said before, so a sign-in held for it stays; one the
        // CLI says names none, or another profile's, takes what it names.
        let unread = source
            .as_ref()
            .is_some_and(|source| source.organization_unread());
        self.organization = if unread && profile == self.organization_profile {
            self.organization.take()
        } else {
            source.as_ref().and_then(|source| source.organization())
        };
        self.organization_profile = profile;
        if login && !self.offers_no_sign_in {
            self.standby = source;
            return None;
        }
        self.standby = None;
        source
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
        self.replace_source(Some(Arc::new(Waiting)));
    }

    /// Replaces where the token comes from (the CLI target changed), ending every ask of the old
    /// one. A connection in use, or one whose connect was asking (a window waiting on it among
    /// them), asks the new one at once; a connect under way starts again with it. With none, where
    /// a source was held the token held is dropped (the target is anonymous now) and a connection
    /// in use connects again; otherwise nothing changes, so a sign-in held, or a token typed for an
    /// anonymous target, stays.
    pub(crate) fn take_token_from(
        &mut self,
        source: Option<Arc<dyn TokenSource>>,
        cx: &mut Context<Self>,
    ) {
        let organization = self.organization.clone();
        let source = self.hold_back(source);
        // A sign-in held, or a sign-in service read, for another organization than the target's
        // now is not the target's: its tokens were checked against the other.
        if self.organization != organization {
            if self.oauth.is_some() {
                self.drop_sign_in(cx);
            } else {
                self.service = None;
            }
        }
        // An anonymous target signed in keeps its sign-in: the CLI names no credential for it,
        // and a managed one's login stays held back.
        if source.is_none() && self.oauth.is_some() {
            return;
        }
        let was_ready = self.token_ready();
        if source.is_some() {
            self.drop_sign_in(cx);
        }
        let had = self.token_source.is_some();
        let connect_asked = self.replace_source(source);
        // A window reads with the new source's token, waiting for its answer, not with what the
        // old one gave; one waiting for the old one's answer reads without a token now if there
        // is no new one. A token typed in no longer stands.
        if self.token_source.is_some() {
            self.token_answered = false;
            self.token_given = false;
            self.asked_after_refusal = false;
        }
        if !was_ready && self.token_ready() {
            cx.emit(ConnectionEvent::TokenReady);
        }
        // The token held is the old source's: the new one's failure does not keep it.
        self.token_from_source = false;
        if self.token_source.is_some() {
            // A connect under way starts again, asking the new source: its probe would send the
            // old source's token. After a Cancel, a window still waiting asks it.
            if self.state == ConnectionState::Connecting {
                self.connect(cx);
            } else if connect_asked {
                self.ask_for_token(Ask::Connect, cx);
            } else if self.state != ConnectionState::Idle {
                self.ask_for_token(Ask::Again, cx);
            }
        } else if had {
            self.token_given = false;
            self.forget_lease();
            self.replace_token(None, cx);
        }
    }

    /// Sends through `http` from now on, as when the CLI target's proxy or certificate authority
    /// changed: every holder of the server's transport moves to it (the agent's gateway, and a
    /// sign-in's requests to the server's address, a code exchange under way among them), and a
    /// server in use is checked again through it, so one the old transport could not reach does
    /// not wait for its next health check. A sign-in keeps going: a refresh under way finishes
    /// and is kept, as a new sign-in read from the keychain here would race it for the refresh
    /// token it rotates. The registry asks the token source again too, since the target
    /// changed: the source it takes starts this connect again ([`Self::take_token_from`]).
    pub(crate) fn replace_http(&mut self, http: Arc<dyn HttpClient>, cx: &mut Context<Self>) {
        self.client.set_http(http);
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
        // Its client stops first, so no request goes out in between, with the token held or,
        // once that is dropped, without one.
        self.client.retire(why.clone());
        self.drop_sign_in(cx);
        self.replace_source(None);
        self.token_given = false;
        self.token_answered = true;
        self.client.set_token(None);
        self.token_from_source = false;
        self.token_unanswered = false;
        self._connect = None;
        self._listing = None;
        self._health_check = None;
        self.state = ConnectionState::Unreachable(why);
        cx.emit(ConnectionEvent::CredentialChanged);
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
    }

    /// A request of one of the server's graphs (a read, a write, the agent's through its
    /// gateway) was answered 401 refusing the source's token it carried
    /// ([`Unauthorized::TokenRefused`]; one that refused none, the token never arriving,
    /// is not told here): the token may have lapsed (a managed data token lives an hour), with `/healthz`, which
    /// asks for none, still answering. The source is told ([`TokenSource::note_refused`]), then
    /// asked again, once for the token held: a renewed token replaces it and what was read is
    /// read again ([`ConnectionEvent::CredentialChanged`]); the same token leaves the read
    /// refused. A token typed in, or a server without a source, is not asked about. A refused
    /// write is never sent again.
    pub fn request_refused(&mut self, cx: &mut Context<Self>) {
        if !self.has_token() || !self.asks_token_source() || !self.token_answered {
            return;
        }
        // Once for the token held, until the server takes a request with it
        // ([`GraphClient::refusal_asked`]): a server that does not admit the account is not
        // asked in a loop by a source that gives a new token each time, and a token that lapses
        // later is. Nor beside an ask after a refusal already out, whose answer is that try.
        let generation = self.client.credential_changes();
        if self.client.refusal_asked() == Some(generation)
            || self.asking() == Some(Ask::AfterRefusal)
        {
            return;
        }
        // A token past its end (one the source could not renew, as after a wake before the
        // network) was refused for that: the source is asked for its renewal, whose answer
        // replaces it quietly, not as a refusal of the account that every graph reads everything
        // again after. An ask under way gives it already.
        if self.token_from_source && self.client.holds_ended_token() {
            if !self.next_token_coming() {
                self.client.note_refusal_asked(Some(generation));
                self.ask_for_token(Ask::Renewal, cx);
            }
            return;
        }
        self.client.note_refusal_asked(Some(generation));
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

    /// Takes the token from `source` from now on; every change of source comes through here. The
    /// old source's asks, a connect's among them, are answered for nobody, so they end: one
    /// answered later could put back a token that was dropped (the person's signed-out one among
    /// them), and a CLI export each runs with them would hold the CLI's turn from the new
    /// source's asks. Returns whether a connect was asking, for the caller to ask in its place.
    fn replace_source(&mut self, source: Option<Arc<dyn TokenSource>>) -> bool {
        self.token_source = source;
        self._asking = None;
        let connect_asked = self.connect_step() == Some(ConnectStep::Asking);
        if connect_asked {
            self._connect = None;
        }
        connect_asked
    }

    /// Asks the token source for a token, for what `ask` says. A connect's ask is the connect's
    /// task; another replaces the one before it.
    fn ask_for_token(&mut self, ask: Ask, cx: &mut Context<Self>) {
        let Some(source) = self.token_source.clone() else {
            return;
        };
        // A Refresh while the source is asked again after a refusal asks in its place, as one
        // after the refusal: it reads the source as it is now (a credential put right meanwhile),
        // and its answer still replaces the refused token as a new credential.
        let ask = match (ask, self.asking()) {
            (Ask::Again, Some(Ask::AfterRefusal)) => Ask::AfterRefusal,
            _ => ask,
        };
        // A renewal waits for any other ask under way, which gives the next token too and
        // schedules the next renewal.
        if ask == Ask::Renewal && self.asking().is_some_and(|asking| asking != Ask::Renewal) {
            return;
        }
        let hurry = self.hurry.clone();
        // Only an ask of the source in use answers: a change of source ends the old one's
        // ([`Self::replace_source`]).
        let asking = cx.spawn(async move |this, cx| {
            let answer = cx.background_executor().spawn(source.token(hurry)).await;
            this.update(cx, |this, cx| {
                if ask == Ask::Connect {
                    this._connect = None;
                } else {
                    this._asking = None;
                }
                this.token_answered(answer, ask, cx);
            })
            .ok();
        });
        if ask == Ask::Connect {
            self._connect = Some((ConnectStep::Asking, asking));
        } else {
            self._asking = Some((ask, asking));
        }
    }

    /// Whether a sign-in finishing replaces the one held, which gives the token until it is
    /// taken.
    fn finishing_over_held(&self) -> bool {
        self.oauth.is_some() && self.finishing().is_some()
    }

    /// What a sign-in finishing was told, while one is: it is not cancelled then.
    fn finishing(&self) -> Option<&Told> {
        self.signing_in.as_ref()?.finish.as_ref()
    }

    /// Whether a failure that may pass leaves the token held in use: the current source gave
    /// it, and it has not ended.
    fn keeps_token(&self) -> bool {
        self.has_token() && self.token_from_source && !self.client.holds_ended_token()
    }

    /// Whether an ask under way gives the next token, which the requests that found the token
    /// ended wait for: one of the source in use, or a connect's (one a window waits on outlives
    /// a Cancel).
    fn next_token_coming(&self) -> bool {
        self.asking().is_some() || self.connect_step() == Some(ConnectStep::Asking)
    }

    /// The step of the connect under way, if one is.
    fn connect_step(&self) -> Option<ConnectStep> {
        self._connect.as_ref().map(|(step, _)| *step)
    }

    /// Whether the client holds a token, so the next request sends one.
    pub fn has_token(&self) -> bool {
        self.client.holds_token()
    }

    /// The ask under way, if any, which is of the source in use.
    fn asking(&self) -> Option<Ask> {
        self._asking.as_ref().map(|(ask, _)| *ask)
    }

    /// Takes the source's answer to `ask`. A token connects (for a connect under way, or when it
    /// is a new credential). A source that could not answer leaves a token it gave in use while it lives
    /// ([`Self::keeps_token`]), unless the server refused that token
    /// (while the source is asked again after the 401, or the 401 to a connect stands); so does a
    /// sign-in a
    /// finishing one replaces, until that one is taken. Otherwise no token
    /// sends nothing more, the connection refused with why. The answer to a connect cancelled
    /// while a window waited for its token ([`Self::cancel`]) is taken, and the connection
    /// stays as Cancel left it.
    fn token_answered(&mut self, answer: Result<Token, NoToken>, ask: Ask, cx: &mut Context<Self>) {
        let connecting = ask == Ask::Connect && self.state == ConnectionState::Connecting;
        // The source is asked again after the server's 401 to the token held: this answers that
        // ask, or another (a connect) while it is out, and gives the one more try.
        let after_refusal = ask == Ask::AfterRefusal || self.asking() == Some(Ask::AfterRefusal);
        // The server refused the token held: after its 401 to a read, or since a probe's 401
        // refused this very token (a connect started since). A failure to give the next does not
        // keep the refused one.
        let refused = after_refusal || self.probe_refused == Some(self.client.token_serial());
        let was_ready = self.token_ready();
        self.token_answered = true;
        self.hurry.store(false, Ordering::SeqCst);
        if !was_ready {
            cx.emit(ConnectionEvent::TokenReady);
        }
        let token = match answer {
            Ok(token) => token,
            // So does the answer of the sign-in a finishing one replaces, that it ended: the
            // token it gave stands until the finishing one is taken, a moment later, with its own.
            Err(no)
                if (no.passing || self.finishing_over_held()) && !refused && self.keeps_token() =>
            {
                self.keep_token(ask, connecting, cx);
                return;
            }
            Err(no) => {
                // A token the source gave that ended (the machine slept past its renewal, and
                // the network is not back) stays held: the source's next answer renews it, as it
                // would have before the sleep, rather than being a new credential that every
                // graph reads everything again under. A refused one is not kept.
                let keep_ended = no.passing
                    && !refused
                    && self.token_from_source
                    && self.client.holds_ended_token();
                self.no_token(no, keep_ended, connecting, cx);
                return;
            }
        };
        let lease = token.lease().cloned();
        self.note_signed_in();
        if let Some(lease) = &lease {
            self.schedule_renewal(lease.renew_at, cx);
        }
        // The same holder's next short-lived token, from the source that gave the one held: a
        // renewal, or a connect's answer once the renewal fell due (one put off while the server
        // could not be reached). Nothing read with the last one is read again. A token after a
        // refusal, or while a 401 stands, is a new credential, tried at once, and what was
        // refused is read again; so is a Refresh's, as the person asked to read again.
        // A 401 standing is a probe's (`refusal`); an ended token kept while its source could not
        // answer leaves the connection unauthorized with none, and its next is its renewal.
        let unauthorized = self.state == ConnectionState::Unauthorized && self.refusal.is_some();
        let renewal = !refused
            && !unauthorized
            && ask != Ask::Again
            && self.token_from_source
            && self.has_token()
            && lease.is_some()
            && lease.as_ref().map(|lease| &lease.holder)
                == self.lease.as_ref().map(|lease| &lease.holder);
        self.lease = lease;
        self.token_from_source = true;
        self.token_unanswered = false;
        // A request that would send it after its end waits for the next, which this asks for.
        let ends = self.lease.as_ref().map(|lease| lease.expires_at);
        let changed = if renewal {
            // Another token: a request refused under it asks again.
            if self.client.renew_token(token.into_string(), ends) {
                self.client.note_refusal_asked(None);
            }
            false
        } else {
            self.client
                .set_token_ending(Some(token.into_string()), ends)
        };
        // A connect's token while the ask after a refusal is out is that ask's answer: it ends.
        if after_refusal && ask == Ask::Connect {
            self._asking = None;
        }
        // A token given after a refusal, a probe's or a request's, is that refusal's one more
        // try, the refused one given back included: a request refused under it asks nothing
        // more.
        if after_refusal {
            self.client
                .note_refusal_asked(Some(self.client.credential_changes()));
        }
        if changed {
            cx.emit(ConnectionEvent::CredentialChanged);
        }
        let unread_changed = self.token_unread.take().is_some();
        // A renewal of an ended token kept while unauthorized connects again, which makes the
        // connection ready with it.
        let renewed_unauthorized = renewal && self.state == ConnectionState::Unauthorized;
        if connecting || (changed && self.state != ConnectionState::Idle) || renewed_unauthorized {
            self.token_fresh = true;
            self.connect(cx);
        } else if renewal || unread_changed {
            cx.emit(ConnectionEvent::StateChanged);
            cx.notify();
        }
    }

    /// Says the sign-in the token comes from as signed in, when it comes from one. A sign-in
    /// under way keeps saying so while the one it replaces answers.
    fn note_signed_in(&mut self) {
        if self.signing_in.is_some() {
            return;
        }
        if let Some(source) = &self.oauth {
            self.sign_in = SignIn::SignedIn {
                service: source.host(),
                account: source.account(),
            };
        }
    }

    /// The source could not answer `ask` now, or is a sign-in a finishing one replaces, and the
    /// token it gave before still lives: it stands, unchanged and unsaid (spec 016, refreshes
    /// are silent). A connect goes on with it,
    /// and a renewal is asked again halfway to the token's end.
    fn keep_token(&mut self, ask: Ask, connecting: bool, cx: &mut Context<Self>) {
        // A Refresh answered so: a renewal due meanwhile waited for it ([`Self::ask_for_token`]).
        if matches!(ask, Ask::Renewal | Ask::Again)
            && let Some(lease) = &mut self.lease
        {
            let now = SystemTime::now();
            let left = lease.expires_at.duration_since(now).unwrap_or_default();
            // The health check waits for the next renewal too: too near the token's end for
            // another, none before it ends.
            lease.renew_at = if left / 2 >= RENEWAL_RETRY_MIN {
                now + left / 2
            } else {
                lease.expires_at
            };
            let at = (lease.renew_at < lease.expires_at).then_some(lease.renew_at);
            if let Some(at) = at {
                self.schedule_renewal(at, cx);
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
    /// health check ([`Self::token_unanswered`]). With `keep_ended`, the ended token it gave
    /// before stays held with its lease, as the credential the next answer renews; a request
    /// that carries it meanwhile is refused for it, which asks the source once more
    /// ([`Self::request_refused`]).
    fn no_token(
        &mut self,
        no: NoToken,
        keep_ended: bool,
        connecting: bool,
        cx: &mut Context<Self>,
    ) {
        if !keep_ended {
            self.token_from_source = false;
            self.forget_lease();
            if self.client.set_token(None) {
                cx.emit(ConnectionEvent::CredentialChanged);
            }
        }
        self.token_unanswered = no.passing;
        if no.passing {
            self.note_signed_in();
            self.token_unread = Some(no.why);
        } else if let Some(source) = self.oauth.take() {
            self.replace_source(None);
            self.token_unread = None;
            match &mut self.signing_in {
                None => {
                    self.sign_in = SignIn::Offered {
                        service: source.host(),
                        why: Some(no.why),
                    };
                    // The service ended it: a token it did not end (a refusal other than an
                    // ended grant) is revoked unless the keychain keeps it.
                    self.end_sign_in(source, End::Dropped, cx);
                }
                // A sign-in under way keeps its state, and answers for the item; why this one
                // ended is said should it not be taken.
                Some(under_way) => {
                    under_way.replaced = Some((source.clone(), no.why));
                    self.end_replaced(source);
                }
            }
        } else {
            self.token_unread = Some(no.why);
        }
        if connecting || self.state != ConnectionState::Idle {
            self._connect = None;
            self.refusal = None;
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
    /// (spec 003): a token source is not asked over it until [`Self::refresh_token`] (the
    /// person's Refresh, or a change to the CLI) or a new source. Requests made after this use
    /// it, and a connection in use checks again; one not yet used connects when it is.
    pub fn set_token(&mut self, token: Option<String>, cx: &mut Context<Self>) {
        let was_ready = self.token_ready();
        self.token_given = true;
        // Its lease was the token replaced's: no renewal of that one, under way or owed,
        // replaces this.
        self.forget_lease();
        self._asking = None;
        if !was_ready {
            cx.emit(ConnectionEvent::TokenReady);
        }
        self.replace_token(token, cx);
    }

    /// Replaces the token: why the token source gave none no longer stands.
    fn replace_token(&mut self, token: Option<String>, cx: &mut Context<Self>) {
        self.token_from_source = false;
        self.token_unanswered = false;
        if self.client.set_token(token) {
            cx.emit(ConnectionEvent::CredentialChanged);
        }
        let unread_changed = self.token_unread.take().is_some();
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
            self.check_health_after(Duration::ZERO, None, cx);
        }
        watch
    }

    /// Whether a watch is held ([`Connection::watch`]).
    pub fn watched(&self) -> bool {
        Rc::strong_count(&self.watchers) > 1
    }

    /// Stops a connection attempt that is taking too long. A connect still waiting for its
    /// token keeps asking for it while a window waits for that token too
    /// ([`Self::wait_for_token`]), or while the token held is another source's (the one before
    /// a change to the CLI target): the window's first reads go once it answers, the answer
    /// replaces the old token, and the connect stops there ([`Self::token_answered`]). Dropped,
    /// the ask would leave the window waiting for a token nothing asks for, empty until it is
    /// hidden and shown again, and the old source's token in the client, where the agent's
    /// gateway would still send it.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.state == ConnectionState::Connecting {
            let keeps_asking =
                !self.token_ready() && (self.hurry.load(Ordering::SeqCst) || self.has_token());
            if !keeps_asking {
                self._connect = None;
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
        let token_sent = self.has_token();
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
        self.listen_for_ended_tokens(cx);
        self.state = ConnectionState::Connecting;
        // The probe reads the list itself.
        self._listing = None;
        // The probe schedules the next check when it ends; one started for an ended token is
        // this connect's to answer.
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
        let token_sent = self.has_token();
        let probe = cx.spawn(async move |this, cx| {
            // Off the main thread, so its round trip is not lengthened by a busy frame.
            let outcome = cx
                .background_executor()
                .spawn(async move { probe(&client).await })
                .await;
            this.update(cx, |this, cx| {
                this._connect = None;
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
                        this.probe_refused = None;
                    }
                    Probe::Failed(failed) => this.refused(failed, token_sent, cx),
                }
                cx.emit(ConnectionEvent::StateChanged);
                cx.notify();
                this.schedule_health_check(cx);
            })
            .ok();
        });
        self._connect = Some((ConnectStep::Probing, probe));
    }

    /// From the first connect on, asks for the next token when a request finds the one held
    /// ended ([`Self::token_ended`]).
    fn listen_for_ended_tokens(&mut self, cx: &mut Context<Self>) {
        if self._ended.is_some() {
            return;
        }
        let ended = self.client.token_ended();
        self._ended = Some(cx.spawn(async move |this, cx| {
            while ended.recv().await.is_ok() {
                if this.update(cx, |this, cx| this.token_ended(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// A request found the token held ended, as after the machine slept past its renewal. A
    /// tell that comes once the token was renewed or replaced (a request waiting tells again
    /// each second) finds the one held standing, and changes nothing. An ask under way, or a
    /// connect's ask (one a window waits on outlives a Cancel), gives the next; a ready server,
    /// or a connect probing with that token, has its source asked for it, which the request
    /// waits for. A server that could not be reached is checked at once, whether or not
    /// anything shows it: back, it connects again, which asks; still away, the requests waiting
    /// when the check began are released, unless an ask begun meanwhile gives the next, and the
    /// next request to find it so has it checked again, as the network may be back by then.
    /// Elsewhere (the server refused the token, a connect was cancelled, no source to ask)
    /// nothing gives the next now, and the requests waiting are released. The end stands
    /// throughout, so a request during a later connect's ask waits for its token.
    fn token_ended(&mut self, cx: &mut Context<Self>) {
        if !self.client.holds_ended_token() || self.next_token_coming() {
            return;
        }
        let asks = self.asks_token_source();
        match self.state {
            ConnectionState::Ready if asks => self.ask_for_token(Ask::Renewal, cx),
            ConnectionState::Connecting
                if asks && self.connect_step() == Some(ConnectStep::Probing) =>
            {
                self.ask_for_token(Ask::Renewal, cx)
            }
            // One check at a time, however many requests found the token ended.
            ConnectionState::Unreachable(_) => {
                if !self
                    ._health_check
                    .as_ref()
                    .is_some_and(|check| check.for_waits.is_some())
                {
                    let waiting = self.client.waits_begun();
                    self.check_health_after(Duration::ZERO, Some(waiting), cx);
                }
            }
            _ => self.client.release_waits(self.client.waits_begun()),
        }
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

    /// Takes the state a failed read left, noting what a 401 said of the token sent
    /// ([`Self::refusal`]). A token from the source that the server
    /// refused is asked for once more: a rotated one connects again, the same one stays
    /// refused, and a probe's 401 asks nothing again until a connect is ready,
    /// [`Self::refresh_token`] (the person's Refresh, or a change to the CLI) or a new source. A
    /// 401 to a graph's request (a read, a write, the agent's) has its own bound
    /// ([`Self::request_refused`]). One the server did not refuse is not asked about.
    fn refused(&mut self, failed: Failed, token_sent: bool, cx: &mut Context<Self>) {
        // A managed target's CLI login was held back for this answer: a server that offers a
        // sign-in is signed in to, and its login dropped; one that offers none, or one Studio
        // cannot sign in to here, takes the login and connects with it.
        if failed.state == ConnectionState::Unauthorized && !token_sent && self.standby.is_some() {
            let offers = failed
                .challenge
                .as_deref()
                .and_then(Challenge::resource_metadata)
                .is_some();
            if offers && self.sign_in_context.is_some() {
                self.standby = None;
            } else {
                self.offers_no_sign_in = true;
                let login = self.standby.take();
                self.take_token_from(login, cx);
                return;
            }
        }
        if failed.state == ConnectionState::Unauthorized {
            let refusal = Unauthorized::judge(failed.challenge.as_deref(), token_sent);
            self.refusal = Some(refusal);
            let refused = refusal == Unauthorized::TokenRefused;
            if refused {
                self.probe_refused = Some(self.client.token_serial());
            }
            // A refused token is not renewed: the ask below is the one more try, and a token
            // the server takes schedules its own renewal.
            self._renewal = None;
            self.renewal_missed = false;
            // A 401 that refused no token (one that never arrived, as a proxy drops it) asks
            // nothing: the next token would fare no better.
            if refused && !self.asked_after_refusal && self.asks_token_source() {
                self.asked_after_refusal = true;
                if let Some(source) = &self.token_source {
                    source.note_refused();
                }
                self.ask_for_token(Ask::AfterRefusal, cx);
            }
            match failed
                .challenge
                .as_deref()
                .and_then(Challenge::resource_metadata)
            {
                Some(metadata) => self.offered_sign_in(metadata.to_owned(), cx),
                // A server that no longer offers a sign-in asks for a token instead.
                None if matches!(self.sign_in, SignIn::Unavailable(_)) => {
                    self.sign_in = SignIn::NotOffered;
                }
                None => {}
            }
        }
        self.state = failed.state;
    }

    /// The server's 401 offers a sign-in, its details at `metadata`: Studio reads them, and a
    /// sign-in the keychain kept for this target is refreshed without asking. A connection whose
    /// credential comes from the CLI keeps it: a refused one is said as refused (spec 016, When
    /// Studio signs in). Read once: a sign-out keeps what was read, and a failed sign-in reads
    /// them again before the next ([`Self::sign_in`]). Details that could not be read are read
    /// again at the next offer.
    fn offered_sign_in(&mut self, metadata: String, cx: &mut Context<Self>) {
        let Some(context) = self.sign_in_context.clone() else {
            return;
        };
        self.sign_in_details = Some(metadata.clone());
        if (self.token_source.is_some() && self.oauth.is_none())
            || !matches!(self.sign_in, SignIn::NotOffered | SignIn::Unavailable(_))
            || self.signing_in.is_some()
        {
            return;
        }
        let Ok(endpoint) = url::Url::parse(&self.server.url) else {
            return;
        };
        self.sign_in = SignIn::Reading;
        let clients = oauth_client::Clients::new(
            endpoint,
            self.client.transport(),
            context.elsewhere.clone(),
        );
        let target = self.server.name.clone();
        let organization = self.organization.clone();
        self.signing_in = Some(SigningIn::new(cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(sign_in::discover(
                    clients,
                    metadata,
                    context.identities.clone(),
                    organization,
                ))
                .await;
            let kept = match &found {
                Ok(service) => {
                    let (before, done) = context.turns.next(&service.key(&target));
                    if let Some(before) = before {
                        before.await;
                    }
                    let kept = context
                        .keychain
                        .read(service.key(&target))
                        .await
                        .ok()
                        .flatten();
                    done.send(()).ok();
                    kept
                }
                Err(_) => None,
            };
            this.update(cx, |this, cx| {
                this.signing_in = None;
                match found {
                    Err(refusal) => this.sign_in = SignIn::Unavailable(refusal),
                    Ok(service) => {
                        let host = service.host();
                        this.service = Some(service.clone());
                        // What the keychain holds now is what was read: nothing left there to say
                        // when the read found none, and a sign-in taken when it found one.
                        this.left = Kept::Nothing;
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
        })));
    }

    /// Signs in through the person's browser, as they asked (spec 016, Signing in): Studio
    /// listens on a loopback port, opens the sign-in service's page, and exchanges the code it
    /// returns with. The listener closes after one answer, five minutes, or Cancel.
    pub fn sign_in(&mut self, cx: &mut Context<Self>) {
        // One finishing is not replaced: it is kept, and ends as it was told meanwhile.
        if self.finishing().is_some() {
            return;
        }
        let Some(context) = self.sign_in_context.clone() else {
            return;
        };
        let Some(service) = self.service.clone() else {
            if let Some(details) = self.sign_in_details.clone() {
                self.read_details_then_sign_in(details, context, cx);
            }
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
        self.signing_in = Some(SigningIn::new(cx.spawn(async move |this, cx| {
            let redirect = callback.redirect_uri().to_owned();
            let issuer = service.server.issuer.clone();
            let exchanged = service.clone();
            let background = cx.background_executor().clone();
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
                    // The exchange runs on its own once the code is sent: the service redeems
                    // it whether or not the sign-in waits, so a sign-in cancelled meanwhile
                    // drops the answer, whose guard revokes the token it gives.
                    let (answer, answered) = oneshot::channel();
                    background
                        .spawn(async move {
                            let service = exchanged;
                            let tokens = oauth_client::exchange(
                                service.token_client().as_ref(),
                                &service.server,
                                &code,
                                &redirect,
                                &pkce.verifier,
                                service.expected(),
                            )
                            .await;
                            let taken = match tokens {
                                // Revoked unless a sign-in comes to hold it (the sign-in
                                // cancelled, or the connection gone before it finishes), made
                                // here so no moment holds the token unguarded.
                                Ok(tokens) => {
                                    let unclaimed = sign_in::Unclaimed::new(
                                        service,
                                        tokens.refresh_token.clone(),
                                    );
                                    Ok((tokens, unclaimed))
                                }
                                // Tokens Studio may not use: nothing will hold their refresh
                                // token, so it is revoked.
                                Err(TokenError::Unusable { why, refresh_token }) => {
                                    if let Some(refresh) = refresh_token {
                                        service.revoke(refresh);
                                    }
                                    Err(why)
                                }
                                Err(error) => Err(error.to_string()),
                            };
                            answer.send(taken).ok();
                        })
                        .detach();
                    answered.await.unwrap_or_else(|_| {
                        Err("The sign-in stopped before the service answered.".to_owned())
                    })
                })
                .await;
            // Finishing from here, not to be cancelled: the sign-in this one replaces (another
            // account, after Not admitted) ends in its turn, and the keychain work runs on its own
            // to its end, the connection released meanwhile or not.
            let signed_in = match signed_in {
                Ok((tokens, unclaimed)) => {
                    let told = Arc::new(Mutex::new(None));
                    // The sign-in under way owns this task, so it is there; were it not, nothing
                    // would take the tokens, and their guard would revoke them.
                    let Ok(Some(taken)) = this.update(cx, |this, cx| {
                        let under_way = this.signing_in.as_mut()?;
                        under_way.finish = Some(told.clone());
                        // The one it replaces: held, or ended by the service meanwhile.
                        let previous = this
                            .oauth
                            .clone()
                            .or_else(|| under_way.replaced.take().map(|(previous, _)| previous));
                        this.sign_in = SignIn::Finishing {
                            service: service.host(),
                        };
                        cx.emit(ConnectionEvent::StateChanged);
                        cx.notify();
                        Some((
                            context.turns.next(&service.key(&target)),
                            previous,
                            service.clone(),
                        ))
                    }) else {
                        return;
                    };
                    let ((before, done), previous, current) = taken;
                    let keychain = context.keychain.clone();
                    let (answer, answered) = oneshot::channel();
                    cx.background_executor()
                        .spawn(async move {
                            if let Some(before) = before {
                                before.await;
                            }
                            let key = current.key(&target);
                            // One account per target: the one replaced ends, its refresh token
                            // revoked, and the keychain keeps only the new one's, or none, never
                            // the old account's.
                            let previous_holds_item = previous
                                .as_ref()
                                .is_some_and(|previous| previous.kept_in_keychain() == Kept::This);
                            if let Some(previous) = previous {
                                previous.end(End::Replaced, None).await;
                            }
                            let refresh = tokens.refresh_token.clone();
                            // An earlier item that one does not answer for (left by a sign-in that
                            // ended, for the next launch) is written over or forgotten below.
                            let earlier = if previous_holds_item {
                                None
                            } else {
                                keychain
                                    .read(key.clone())
                                    .await
                                    .ok()
                                    .flatten()
                                    .and_then(|(_, secret)| String::from_utf8(secret).ok())
                                    .filter(|earlier| refresh.as_ref() != Some(earlier))
                            };
                            // Kept or not, this sign-in leaves the keychain no earlier one: one it
                            // will not delete is said, as the next launch would read it.
                            let kept = sign_in::written_or_forgotten(
                                keychain.as_ref(),
                                key,
                                tokens.account.subject.clone(),
                                refresh.as_deref(),
                            )
                            .await;
                            // The earlier item's token: gone from the keychain, it is held by
                            // nothing and revoked; still there (the keychain would not let go), the
                            // sign-in keeps it, to revoke once the item no longer holds it.
                            let (left, let_go) = match (&kept, earlier) {
                                (Kept::Left(_), earlier) => (earlier, None),
                                (_, earlier) => (None, earlier),
                            };
                            // The sign-in answers for the token from here, taken by the connection
                            // or, dropped untaken, ended as told.
                            unclaimed.claim();
                            let finished = Finished {
                                source: Some(OAuthSource::signed_in(
                                    current, &target, keychain, tokens, kept, left,
                                )),
                                let_go,
                                told,
                                done: Some(done),
                            };
                            answer.send(finished).ok();
                        })
                        .detach();
                    answered
                        .await
                        .map_err(|_| "Studio stopped before it kept the sign-in.".to_owned())
                }
                Err(why) => Err(why),
            };
            // A sign-in finished that the connection, gone, does not take is dropped with this.
            this.update(cx, |this, cx| {
                let replaced = this
                    .signing_in
                    .take()
                    .and_then(|under_way| under_way.replaced);
                match signed_in {
                    Ok(finished) => this.take_sign_in(finished, cx),
                    Err(why) => {
                        // What the sign-in service's details named may be what failed: the next
                        // sign-in reads them again (spec 016, discovery).
                        this.service = None;
                        // And why the one it would have replaced ended meanwhile, if it did.
                        let why = match replaced {
                            Some((_, ended)) => format!("{why} {ended}"),
                            None => why,
                        };
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
        })));
    }

    /// Takes a sign-in finished. Told meanwhile
    /// to end (a sign-out, a drop), it ends so in the keychain turn its finishing held, so no
    /// other work reads the item between its write and that ending; the target made anonymous
    /// again then, the server's answer offers a sign-in, which reads the item after.
    fn take_sign_in(&mut self, finished: Finished, cx: &mut Context<Self>) {
        let (source, let_go, told, done) = finished.take();
        if let Some(token) = let_go {
            source.revoke(token);
        }
        match told {
            Some(how) => {
                self.end_in_turn(source, how, (None, done), cx);
                if self.token_source.is_none()
                    && self.state == ConnectionState::Unauthorized
                    && self.sign_in == SignIn::NotOffered
                {
                    self.connect(cx);
                }
            }
            None => {
                done.send(()).ok();
                self.use_sign_in(source, cx);
            }
        }
    }

    /// Reads the sign-in service's details at `details` again, then signs in, for a sign-in after
    /// one failed. Details that cannot be read are said: beside a sign-in held, which stands, as
    /// why another could not start.
    fn read_details_then_sign_in(
        &mut self,
        details: String,
        context: SignInContext,
        cx: &mut Context<Self>,
    ) {
        let Ok(endpoint) = url::Url::parse(&self.server.url) else {
            return;
        };
        self.sign_in = SignIn::Reading;
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
        let clients = oauth_client::Clients::new(
            endpoint,
            self.client.transport(),
            context.elsewhere.clone(),
        );
        let organization = self.organization.clone();
        self.signing_in = Some(SigningIn::new(cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(sign_in::discover(
                    clients,
                    details,
                    context.identities,
                    organization,
                ))
                .await;
            this.update(cx, |this, cx| {
                // The held sign-in the service ended meanwhile, and why, said should the next
                // one not be taken.
                let replaced = this
                    .signing_in
                    .take()
                    .and_then(|under_way| under_way.replaced);
                match found {
                    Ok(service) => {
                        this.service = Some(service);
                        this.sign_in(cx);
                        if let Some(under_way) = &mut this.signing_in {
                            under_way.replaced = replaced;
                        }
                    }
                    Err(refusal) => {
                        this.sign_in = match (&this.oauth, replaced) {
                            (Some(held), _) => SignIn::Offered {
                                service: held.host(),
                                why: Some(refusal.why),
                            },
                            (None, Some((_, ended))) => SignIn::Unavailable(Refusal {
                                why: format!("{} {ended}", refusal.why),
                                ..refusal
                            }),
                            (None, None) => SignIn::Unavailable(refusal),
                        };
                        cx.emit(ConnectionEvent::StateChanged);
                        cx.notify();
                    }
                }
            })
            .ok();
        })));
    }

    /// Stops waiting for the browser: its listener closes, and nothing it returns is taken. A
    /// sign-in held (one this would have replaced) is said again, or why it ended meanwhile.
    pub fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if let SignIn::Waiting { service } = &self.sign_in {
            self.sign_in = SignIn::Offered {
                service: service.clone(),
                why: self
                    .signing_in
                    .take()
                    .and_then(|under_way| under_way.replaced)
                    .map(|(_, why)| why),
            };
            self.note_signed_in();
            cx.emit(ConnectionEvent::StateChanged);
            cx.notify();
        }
    }

    /// Signs out: the tokens are dropped, the keychain item deleted and, where the service
    /// takes it, the refresh token revoked; the connection checks again without a token. A
    /// keychain that keeps the item is said, as the next launch would read it back.
    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.oauth.take() else {
            return;
        };
        let was_ready = self.token_ready();
        self.replace_source(None);
        if !was_ready {
            cx.emit(ConnectionEvent::TokenReady);
        }
        self.forget_lease();
        let finishing = self.end_sign_ins(Some(source.clone()), End::SignOut, cx);
        // One finishing is kept, then signed out of, and a sign-in is offered after that, as
        // none can start before.
        self.sign_in = if finishing {
            SignIn::NotOffered
        } else {
            SignIn::Offered {
                service: source.host(),
                why: None,
            }
        };
        self.replace_token(None, cx);
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
    }

    /// Whether the person is asked for a token: the server refused the one sent, or none was sent,
    /// and offers no sign-in, with none under way (one finishing that a drop or a rename ended is
    /// kept before anything else is asked). One sent that never arrived is not asked for: a token
    /// typed would be dropped the same way.
    pub fn asks_token(&self) -> bool {
        self.state == ConnectionState::Unauthorized
            && !self.token_not_received()
            && self.sign_in == SignIn::NotOffered
            && self.signing_in.is_none()
    }

    /// Whether the server's 401 refused no token though one was sent: it never arrived (a proxy
    /// between them dropped it, say), so a token typed would be dropped the same way.
    pub fn token_not_received(&self) -> bool {
        self.state == ConnectionState::Unauthorized
            && self.refusal == Some(Unauthorized::TokenNotReceived)
    }

    /// Holds a sign-in under way, as a test's state.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_sign_in_for_tests(&mut self) {
        self.signing_in = Some(SigningIn::new(Task::ready(())));
    }

    /// Whether a sign-in is under way, whatever the sign-in state says: one finishing that a drop
    /// or a rename ended is kept before anything else is offered.
    pub fn sign_in_under_way(&self) -> bool {
        self.signing_in.is_some()
    }

    /// Whether a sign-in is held, whatever the sign-in state says: one a sign-in as someone else
    /// would replace stays held until that one is taken.
    pub fn holds_sign_in(&self) -> bool {
        self.oauth.is_some()
    }

    /// What a window shows in place of the server's content while the server asks for a sign-in
    /// that nobody holds (spec 016, What the person sees): its reads are refused until the person
    /// signs in, so a refused read's 401 says nothing they can act on. `None` while a sign-in is
    /// held (its own states are the chip's), or the server asks none. Said as the Open dialog
    /// says it, with the server's name.
    pub fn sign_in_prompt(&self) -> Option<SignInPrompt> {
        if self.state != ConnectionState::Unauthorized || self.holds_sign_in() {
            return None;
        }
        let server = &self.server.name;
        let (says, offers) = match &self.sign_in {
            SignIn::Offered { service, why } => (
                match why {
                    Some(why) => format!("{why} The sign-in is at {service}."),
                    None => format!("{server} asks you to sign in at {service}."),
                },
                Some(PromptAction::SignIn),
            ),
            SignIn::Reading => (
                format!("{server} asks you to sign in. Studio is reading how."),
                None,
            ),
            SignIn::Waiting { service } => (
                format!("Finish signing in at {service} in your browser."),
                Some(PromptAction::Cancel),
            ),
            SignIn::Finishing { service } => (format!("Finishing the sign-in at {service}."), None),
            SignIn::Unavailable(why) => (
                format!("{server} asks for a sign-in Studio cannot make. {why}"),
                None,
            ),
            SignIn::NotOffered if self.sign_in_under_way() => {
                ("Studio is finishing a sign-in.".to_owned(), None)
            }
            SignIn::NotOffered | SignIn::SignedIn { .. } => return None,
        };
        Some(SignInPrompt {
            server: server.clone(),
            says,
            offers,
        })
    }

    /// What the keychain holds for the target, which the next launch reads: the sign-in held
    /// answers for it; with none held, an item a sign-in that ended left there, if any.
    pub fn keychain(&self) -> Kept {
        match &self.oauth {
            Some(source) => source.kept_in_keychain(),
            None => self.left.clone(),
        }
    }

    /// The keychain's refusal to delete an item it holds for the target that nobody is signed
    /// in with, which Studio may read again later ([`Self::keychain`]).
    pub fn keychain_left(&self) -> Option<String> {
        match self.keychain() {
            Kept::Left(refused) => Some(refused),
            Kept::This | Kept::Nothing => None,
        }
    }

    /// When the token held ends, as a test's view of a renewal taken.
    #[cfg(test)]
    pub(crate) fn lease_ends(&self) -> Option<SystemTime> {
        self.lease.as_ref().map(|lease| lease.expires_at)
    }

    /// Says what a sign-in that ended left in the keychain, as a test's state.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_left_for_tests(&mut self, left: Kept) {
        self.left = left;
    }

    /// Ends the sign-in held, if any, and the one under way, for `how` (a sign-out or a drop).
    /// One reading its service or waiting for the browser is cancelled. One finishing is not: its
    /// keychain work keeps its turn, it then ends for `how` as one held would, and the one held
    /// ends as replaced by it. A sign-out stands over a drop after it. Whether one was finishing.
    fn end_sign_ins(
        &mut self,
        held: Option<OAuthSource>,
        how: End,
        cx: &mut Context<Self>,
    ) -> bool {
        let finishing = match self.finishing() {
            Some(told) => {
                let mut told = told.lock().unwrap_or_else(PoisonError::into_inner);
                if *told != Some(End::SignOut) {
                    *told = Some(how);
                }
                true
            }
            None => {
                self.signing_in = None;
                false
            }
        };
        match held {
            Some(held) if finishing => self.end_replaced(held),
            Some(held) => self.end_sign_in(held, how, cx),
            None => {}
        }
        finishing
    }

    /// Ends `source`, held, as replaced by the sign-in under way, which answers for the item:
    /// its token is revoked, and it takes no keychain turn, as neither its ending nor a refresh
    /// of it answered after touches the item; the sign-in under way waits for it before it
    /// writes. An item the keychain would not delete stays said, should the sign-in under way
    /// not be taken.
    fn end_replaced(&mut self, source: OAuthSource) {
        if self.names(&source) {
            self.left = source.kept_in_keychain();
        }
        drop(source.end(End::Replaced, None));
    }

    /// Whether `source`'s keychain item is the target's under its name now: one made under a
    /// name the target has since lost is that name's ([`Self::rename`]).
    fn names(&self, source: &OAuthSource) -> bool {
        source.key() == source.service_key(&self.server.name)
    }

    /// Ends `source` for `how` in its item's keychain turn ([`OAuthSource::end`]), and says what
    /// the keychain then holds there, while no sign-in is held and the item is the target's under
    /// its name now. Detached, so the ending finishes when the connection is gone first.
    fn end_sign_in(&mut self, source: OAuthSource, how: End, cx: &mut Context<Self>) {
        let turn = match &self.sign_in_context {
            Some(context) => context.turns.next(&source.key()),
            // Nothing signs in without a context, so there is no keychain work to order.
            None => (None, oneshot::channel().0),
        };
        self.end_in_turn(source, how, turn, cx);
    }

    /// Ends `source` for `how` in `turn`, a keychain turn at its item already taken, as
    /// [`Self::end_sign_in`] does in the next one.
    fn end_in_turn(
        &mut self,
        source: OAuthSource,
        how: End,
        (before, done): (Option<KeychainTurn>, oneshot::Sender<()>),
        cx: &mut Context<Self>,
    ) {
        let ending = source.end(how, before);
        cx.spawn(async move |this, cx| {
            let kept = ending.await;
            this.update(cx, |this, cx| {
                // A sign-in held answers for the item itself; with none, this says what is left
                // under the target's name.
                if this.oauth.is_none() && this.names(&source) {
                    this.left = kept;
                    cx.emit(ConnectionEvent::StateChanged);
                    cx.notify();
                }
            })
            .ok();
            done.send(()).ok();
        })
        .detach();
    }

    /// Renames the target, as the CLI's configuration did. The keychain item is the target's
    /// (spec 016, Custody), so to a sign-in the old target is gone and a new one is at the same
    /// address: a sign-in held or under way is dropped as for a target the CLI no longer lists,
    /// what it leaves kept under the old name, and the server's answer offers a sign-in under
    /// the new one, which reads that name's item.
    pub(crate) fn rename(&mut self, name: String, cx: &mut Context<Self>) {
        if self.server.name == name {
            return;
        }
        let held = self.oauth.is_some();
        let signs_in = held || self.signing_in.is_some() || self.sign_in != SignIn::NotOffered;
        // Dropped under the old name, so what it leaves is that name's.
        if signs_in {
            self.drop_sign_in(cx);
        }
        self.server.name = name;
        self.left = Kept::Nothing;
        if held {
            let was_ready = self.token_ready();
            self.replace_source(None);
            if !was_ready {
                cx.emit(ConnectionEvent::TokenReady);
            }
        }
        if signs_in && self.token_source.is_none() {
            self.replace_token(None, cx);
        }
        cx.emit(ConnectionEvent::StateChanged);
        cx.notify();
    }

    /// Takes the token from `source`, a sign-in just made or kept, and connects with it.
    fn use_sign_in(&mut self, source: OAuthSource, cx: &mut Context<Self>) {
        self.left = Kept::Nothing;
        self.oauth = Some(source.clone());
        self.replace_source(Some(Arc::new(source)));
        self.token_given = false;
        self.asked_after_refusal = false;
        // The token held, if any, is another credential's: the sign-in's failure does not keep it.
        self.token_from_source = false;
        self.forget_lease();
        self.connect(cx);
    }

    /// Forgets the sign-in: the CLI's credential for the target replaces it, the connection is
    /// retired, or the target renamed ([`Self::rename`]). The keychain keeps what it holds, for
    /// the target under that name made anonymous again, and an item it kept but would not delete
    /// stays said while the name stands; a sign-in whose token the keychain does not hold is
    /// revoked, since nothing else would hold it ([`End::Dropped`]). One finishing is kept, then
    /// dropped so.
    fn drop_sign_in(&mut self, cx: &mut Context<Self>) {
        let held = self.oauth.take();
        self.end_sign_ins(held, End::Dropped, cx);
        self.service = None;
        self.sign_in_details = None;
        self.forget_lease();
        self.sign_in = SignIn::NotOffered;
    }

    /// Forgets the lease of the token held, with the renewal of it scheduled or owed: the token
    /// was replaced or dropped, or the sign-in that gave it ended.
    fn forget_lease(&mut self) {
        self.lease = None;
        self._renewal = None;
        self.renewal_missed = false;
    }

    /// Asks for the next short-lived token `at` that time, if a view still shows the server;
    /// otherwise the next view to show it asks. A renewal scheduled replaces one owed: a token
    /// given since answered it, or this retry asks in its place.
    fn schedule_renewal(&mut self, at: SystemTime, cx: &mut Context<Self>) {
        self.renewal_missed = false;
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

    /// Checks `/healthz` again after the settings' interval while the server is ready; after a
    /// sign-in's service did not answer, after that interval doubled for each check in a row it
    /// has not ([`Self::unanswered_checks`]), up to two minutes; and every two minutes after
    /// another failure. A recovered server is probed in full again. A timer stands still while
    /// the machine sleeps, so the first check after a wake comes within one interval of it.
    fn schedule_health_check(&mut self, cx: &mut Context<Self>) {
        let ready = cx
            .try_global::<StudioSettings>()
            .map(|settings| Duration::from_secs(settings.studio.health_check_seconds.max(5)))
            .unwrap_or(DEFAULT_CHECK_INTERVAL);
        let unanswered = self.token_unanswered && self.oauth.is_some();
        if !unanswered {
            self.unanswered_checks = 0;
        }
        let interval = if self.state == ConnectionState::Ready {
            ready
        } else if unanswered {
            // A sign-in's service that could not answer (the network back after a wake) is asked
            // again soon, then less often while it does not answer; a CLI that could not answer
            // is run again at a failed check's pace.
            let wait = ready
                .saturating_mul(1 << self.unanswered_checks.min(8))
                .min(FAILED_CHECK_INTERVAL);
            self.unanswered_checks += 1;
            wait
        } else {
            FAILED_CHECK_INTERVAL
        };
        self.check_health_after(interval, None, cx);
    }

    /// Checks `/healthz` after `wait`, unless nothing watches the server by then: that check
    /// is missed, the checks stop, and the next watch makes it ([`Connection::watch`]). One a
    /// request started for an ended token ([`HealthCheck::for_waits`], [`Self::token_ended`])
    /// is made either way.
    fn check_health_after(
        &mut self,
        wait: Duration,
        for_waits: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        // The checks go on: none is missed for the next watch to make.
        self.check_missed = false;
        let client = self.client.clone();
        let check = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let watched = this
                .update(cx, |this, _| {
                    this.check_missed = !this.watched() && for_waits.is_none();
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
            this.update(cx, |this, cx| {
                // Answered: it is under way no more, whatever comes next.
                this._health_check = None;
                this.apply_health_check(health, latency, for_waits, cx)
            })
            .ok();
        });
        self._health_check = Some(HealthCheck {
            for_waits,
            _task: check,
        });
    }

    fn apply_health_check(
        &mut self,
        health: Result<graph_client::ServerHealth, ReadError>,
        latency: Duration,
        for_waits: Option<u64>,
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
            self.check_health_after(RECHECK, None, cx);
            return;
        }
        let next = match served(health) {
            Ok(_) if self.state == ConnectionState::Ready => ConnectionState::Ready,
            // `/healthz` asks for no token, so it passes for a refused one too: probing again
            // would be refused alike, and would take the token field away while it is typed
            // in. A new token connects again; a source that could not answer is asked again,
            // and sign-in details that could not be read are read again, by the connect below.
            // Details read that Studio cannot use are not: they stand until Retry.
            Ok(_)
                if self.state == ConnectionState::Unauthorized
                    && !self.token_unanswered
                    && !matches!(&self.sign_in, SignIn::Unavailable(refusal) if refusal.passing) =>
            {
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
        // Not back: the requests that waited for this check send what is held, unless an ask
        // begun meanwhile (a Refresh, a changed target) gives the next. One that began waiting
        // during it waits on, and tells again, which checks again: the network may have come
        // back after this check began.
        if let Some(through) = for_waits
            && self.state != ConnectionState::Ready
            && !self.next_token_coming()
        {
            self.client.release_waits(through);
        }
        // A renewal the wall clock has passed is asked now. The executor's timers stand still
        // while the machine sleeps, so after a sleep the renewal's own timer fires that much late,
        // the token held ended meanwhile; this check comes within its interval of waking.
        if self.state == ConnectionState::Ready
            && self.asking().is_none()
            && self
                .lease
                .as_ref()
                .is_some_and(|lease| SystemTime::now() >= lease.renew_at)
        {
            self.ask_for_token(Ask::Renewal, cx);
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

/// A read that left the connection unusable: the state it leaves, and a 401's challenge, which
/// says whether it refused the token sent ([`Unauthorized::judge`]) and where the server's
/// sign-in details are (`resource_metadata`), when it offers a sign-in. `None` from a server
/// that sends none.
struct Failed {
    state: ConnectionState,
    challenge: Option<Box<Challenge>>,
}

impl From<ConnectionState> for Failed {
    fn from(state: ConnectionState) -> Self {
        Self {
            state,
            challenge: None,
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
                challenge: summary.challenge,
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
    use futures::FutureExt as _;
    use futures::future::BoxFuture;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use http_client::{AsyncBody, FakeHttpClient, Response};

    /// A managed login whose profile's organization the CLI would not say this time keeps the
    /// one it said before, so a sign-in held for it stays; one the CLI says names none, another
    /// organization or another profile takes what it names, and a target with no source names
    /// none. Before, a read the CLI missed took the organization away, and with it the sign-in.
    #[gpui::test]
    fn a_login_that_does_not_say_its_organization_keeps_the_one_before(cx: &mut TestAppContext) {
        /// A login of a profile, in an organization or none, or `Err` when the CLI would not say.
        struct Login(&'static str, Result<Option<&'static str>, ()>);
        impl TokenSource for Login {
            fn token(&self, _: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>> {
                futures::future::pending().boxed()
            }

            fn login_profile(&self) -> Option<String> {
                Some(self.0.into())
            }

            fn organization(&self) -> Option<String> {
                self.1.ok().flatten().map(str::to_owned)
            }

            fn organization_unread(&self) -> bool {
                self.1.is_err()
            }
        }
        let server = ServerEntry {
            name: "work".into(),
            url: "https://cluster.example.com".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let http = FakeHttpClient::with_404_response();
        let connection = cx.new(|_| Connection::new(server, None, http).unwrap());
        let take = |login: Option<Login>, cx: &mut TestAppContext| {
            let source = login.map(|login| Arc::new(login) as Arc<dyn TokenSource>);
            connection.update(cx, |c, cx| {
                c.take_token_from(source, cx);
                c.organization.clone()
            })
        };
        let org = |name| Ok(Some(name));
        let work = |cx: &mut TestAppContext| take(Some(Login("work", org("org_1"))), cx);
        assert_eq!(work(cx).as_deref(), Some("org_1"));
        let missed = take(Some(Login("work", Err(()))), cx);
        assert_eq!(missed.as_deref(), Some("org_1"), "kept");
        assert_eq!(take(Some(Login("work", Ok(None))), cx), None, "names none");
        work(cx);
        let other = take(Some(Login("personal", Err(()))), cx);
        assert_eq!(other, None, "another profile");
        let moved = take(Some(Login("work", org("org_2"))), cx);
        assert_eq!(moved.as_deref(), Some("org_2"));
        assert_eq!(take(None, cx), None, "no source");
    }

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

    /// The answer of a token source the connection no longer takes its token from is not taken:
    /// the token dropped when the source was replaced stays dropped. Before, a Refresh or a
    /// renewal under way when the source changed (a sign-out, a sign-in, the CLI's credential)
    /// answered afterwards and put its token back.
    #[gpui::test]
    async fn a_token_answered_after_its_source_was_replaced_is_not_taken(cx: &mut TestAppContext) {
        let gated = Arc::new(Gated::default());
        let connection = gated_server(gated.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        cx.run_until_parked();
        connection.read_with(cx, |c, _| {
            assert_eq!(c.state, ConnectionState::Ready);
            assert!(c.has_token());
        });
        // A Refresh waits for its answer while the target is made anonymous.
        let (answer, gate) = oneshot::channel();
        *gated.0.lock().unwrap() = Some(gate);
        connection.update(cx, |c, cx| c.refresh_token(cx));
        cx.run_until_parked();
        connection.update(cx, |c, cx| c.take_token_from(None, cx));
        cx.run_until_parked();
        connection.read_with(cx, |c, _| assert!(!c.has_token()));
        answer.send(Token::typed("stale".into())).ok();
        cx.run_until_parked();
        connection.read_with(cx, |c, _| {
            assert!(!c.has_token(), "the replaced source's answer was taken")
        });
    }

    /// A connect that waited on a token source replaced meanwhile asks the source now in use in
    /// its place, the old ask ended: one that gives the token already held starts no connect of
    /// its own, and without this the connection stayed connecting.
    #[gpui::test]
    async fn a_connect_whose_source_was_replaced_asks_the_new_one(cx: &mut TestAppContext) {
        let first = Arc::new(Gated::default());
        let connection = gated_server(first.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        cx.run_until_parked();
        let (answer, gate) = oneshot::channel();
        *first.0.lock().unwrap() = Some(gate);
        connection.update(cx, |c, cx| c.connect(cx));
        cx.run_until_parked();
        connection.read_with(cx, |c, _| assert_eq!(c.state, ConnectionState::Connecting));
        // The CLI's credential changes to a source that gives the same token.
        connection.update(cx, |c, cx| {
            c.take_token_from(Some(Arc::new(Gated::default())), cx)
        });
        cx.run_until_parked();
        assert!(answer.is_canceled(), "the old source's ask ended");
        connection.read_with(cx, |c, _| assert_eq!(c.state, ConnectionState::Ready));

        // A window waiting for the token, its connect cancelled: the source in use is asked, the
        // token gets ready, and the connection stays as Cancel left it.
        let waited = Arc::new(Gated::default());
        let connection = gated_server(waited.clone(), cx);
        let (answer, gate) = oneshot::channel();
        *waited.0.lock().unwrap() = Some(gate);
        connection.update(cx, |c, cx| c.wait_for_token(cx));
        cx.run_until_parked();
        connection.update(cx, |c, cx| c.cancel(cx));
        connection.update(cx, |c, cx| {
            c.take_token_from(Some(Arc::new(Gated::default())), cx)
        });
        cx.run_until_parked();
        assert!(answer.is_canceled(), "the old source's ask ended");
        connection.read_with(cx, |c, _| {
            assert!(c.token_ready(), "the waiting window gets its token");
            assert_eq!(c.state, ConnectionState::Idle);
        });

        // The same, the target made anonymous instead: the window hears the token is ready.
        let waited = Arc::new(Gated::default());
        let connection = gated_server(waited.clone(), cx);
        let readied = Rc::new(std::cell::Cell::new(0));
        let heard = readied.clone();
        let _heard = cx.update(|cx| {
            cx.subscribe(&connection, move |_, event: &ConnectionEvent, _| {
                if matches!(event, ConnectionEvent::TokenReady) {
                    heard.set(heard.get() + 1);
                }
            })
        });
        let (answer, gate) = oneshot::channel::<Token>();
        *waited.0.lock().unwrap() = Some(gate);
        connection.update(cx, |c, cx| c.wait_for_token(cx));
        cx.run_until_parked();
        connection.update(cx, |c, cx| c.cancel(cx));
        connection.update(cx, |c, cx| c.take_token_from(None, cx));
        cx.run_until_parked();
        assert!(answer.is_canceled(), "the old source's ask ended");
        connection.read_with(cx, |c, _| assert!(c.token_ready()));
        assert_eq!(
            readied.get(),
            1,
            "the waiting window heard the token is ready"
        );
    }

    /// A token source that answers "first" at once, or, once given a gate, what comes through it.
    #[derive(Default)]
    struct Gated(std::sync::Mutex<Option<oneshot::Receiver<Token>>>);

    impl TokenSource for Gated {
        fn token(&self, _: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>> {
            let gate = self.0.lock().unwrap().take();
            async move {
                match gate {
                    Some(gate) => gate.await.map_err(|_| NoToken::passing("no answer")),
                    None => Ok(Token::typed("first".into())),
                }
            }
            .boxed()
        }
    }

    /// A healthy server with no graphs, whose token comes from `source`.
    fn gated_server(source: Arc<Gated>, cx: &mut TestAppContext) -> Entity<Connection> {
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
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let source: Arc<dyn TokenSource> = source;
        cx.new(|_| {
            Connection::new(server, None, http)
                .unwrap()
                .with_token_source(Some(source))
        })
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
        // The health checks, a public route's, carry none.
        let sent = sent.lock().unwrap().clone();
        assert!(sent.iter().any(Option::is_some), "{sent:?}");
        assert!(
            sent.iter().flatten().all(|token| token == "Bearer t"),
            "{sent:?}"
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
                (connection.refusal, connection.label())
            })
        };
        let needed = Some(Unauthorized::TokenNeeded);
        assert_eq!(said(cx), (needed, "Asks for a token".to_owned()));
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("typed".into()), cx)
        });
        cx.run_until_parked();
        let refused = Some(Unauthorized::TokenRefused);
        assert_eq!(said(cx), (refused, "Token refused".to_owned()));
        connection.update(cx, |connection, cx| connection.set_token(None, cx));
        cx.run_until_parked();
        assert_eq!(said(cx), (needed, "Asks for a token".to_owned()));
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
    /// once for that token and telling it first: a renewed token replaces the lapsed one, and a
    /// read refused under the renewed one asks nothing more, though the source would give a new
    /// token each time, as a sign-in's refresh does, until a read with it is taken
    /// (`session::tests::a_token_given_after_a_refusal_is_asked_about_again_once_a_read_took_it`);
    /// a token typed in is not asked about.
    /// Before, each refusal under each new token asked again, so a server that does not admit
    /// the account was asked in a loop, every read sent again each time.
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
        *source.given.lock().unwrap() = vec!["lapsed", "renewed", "again", "and again"];
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

        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!((asked(), refused()), (2, 1));
        assert_eq!(changes(cx), lapsed + 1, "the renewed token replaced it");

        // Refused again under the renewed token, read again after it came: asked nothing more.
        for _ in 0..3 {
            connection.update(cx, |connection, cx| connection.request_refused(cx));
            cx.run_until_parked();
        }
        assert_eq!(
            (asked(), refused(), changes(cx)),
            (2, 1, lapsed + 1),
            "a token renewed for a refusal is not asked about again"
        );

        connection.update(cx, |connection, cx| {
            connection.set_token(Some("typed".into()), cx);
            connection.request_refused(cx);
        });
        cx.run_until_parked();
        assert_eq!(asked(), 2, "a token typed in is not asked about");
    }

    /// A request's 401 beside an ask after a refusal already out (a probe's) asks nothing more:
    /// that ask's answer is the one more try. Before, it asked again, replacing the ask under
    /// way, and the CLI exported twice.
    #[gpui::test]
    async fn a_refusal_beside_an_ask_already_out_asks_nothing_more(cx: &mut TestAppContext) {
        let source = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(source.clone(), cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        let asked = || source.asked.load(Ordering::SeqCst);
        assert_eq!(asked(), 1);
        // The probe's 401 refuses the token: the source is asked once more, and holds its answer.
        let (_release, gate) = oneshot::channel::<()>();
        *source.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| {
            let failed = Failed {
                state: ConnectionState::Unauthorized,
                challenge: Some(Box::new(Challenge {
                    scheme: "Bearer".into(),
                    params: vec![("error".into(), "invalid_token".into())],
                })),
            };
            connection.refused(failed, true, cx);
        });
        cx.run_until_parked();
        assert_eq!(asked(), 2, "asked once after the probe's refusal");
        // A graph's request is refused meanwhile: the ask out answers for it.
        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!(asked(), 2, "asked again beside the ask out");
    }

    /// The ask after a probe's 401 is the one more try for the graph's requests too: a read
    /// refused under the token it gave, the refused one given back included, asks nothing until
    /// a request of the graph with it is taken. Before, a source that gave the refused token back was asked again by
    /// the next read's 401: one more CLI export, or refresh, for a token already asked about.
    #[gpui::test]
    async fn the_ask_after_a_probes_refusal_is_the_reads_one_more_try(cx: &mut TestAppContext) {
        let source = Arc::new(Scripted::default());
        source.unnumbered.store(true, Ordering::SeqCst);
        let (connection, _, _) = sourced(source.clone(), cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        let asked = || source.asked.load(Ordering::SeqCst);
        assert_eq!(asked(), 1);
        // The probe's 401 refuses the token: the source is asked once more, and gives it back.
        connection.update(cx, |connection, cx| {
            let failed = Failed {
                state: ConnectionState::Unauthorized,
                challenge: Some(Box::new(Challenge {
                    scheme: "Bearer".into(),
                    params: vec![("error".into(), "invalid_token".into())],
                })),
            };
            connection.refused(failed, true, cx);
        });
        cx.run_until_parked();
        assert_eq!(asked(), 2, "asked once after the probe's refusal");
        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!(asked(), 2, "a read refused under that token asked again");
        // A request the server took with it (as the client notes) lets a later refusal ask.
        connection.update(cx, |connection, cx| {
            connection.client().note_refusal_asked(None);
            connection.request_refused(cx);
        });
        cx.run_until_parked();
        assert_eq!(asked(), 3);
    }

    /// A renewal due while the source is asked again after a refused read leaves that ask to
    /// answer: its token replaces the refused one as a new credential, so what was refused is
    /// read again. Before, the renewal took the ask's place and replaced the token quietly, and
    /// the refused read was never read again.
    #[gpui::test]
    async fn a_renewal_leaves_an_ask_after_a_refusal_to_answer(cx: &mut TestAppContext) {
        use std::sync::atomic::AtomicUsize;
        /// Leased tokens of one holder, each ask's answer held while a gate is set.
        #[derive(Default)]
        struct Gated {
            asked: AtomicUsize,
            gate: std::sync::Mutex<Option<futures::future::Shared<oneshot::Receiver<()>>>>,
        }
        impl TokenSource for Gated {
            fn token(
                &self,
                _: Arc<AtomicBool>,
            ) -> futures::future::BoxFuture<'static, Result<Token, NoToken>> {
                let asked = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
                let gate = self.gate.lock().unwrap().clone();
                Box::pin(async move {
                    if let Some(gate) = gate {
                        gate.await.ok();
                    }
                    let now = SystemTime::now();
                    Ok(Token::leased(
                        format!("token {asked}"),
                        Lease {
                            expires_at: now + Duration::from_secs(3600),
                            renew_at: now + Duration::from_secs(1800),
                            holder: "user_1".into(),
                        },
                    ))
                })
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
        let source = Arc::new(Gated::default());
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
        let changes = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| connection.client().credential_changes())
        };
        let refused_under = changes(cx);
        let (release, gate) = oneshot::channel::<()>();
        *source.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 2);
        // The renewal comes due while that ask waits.
        connection.update(cx, |connection, cx| {
            connection.ask_for_token(Ask::Renewal, cx)
        });
        release.send(()).unwrap();
        cx.run_until_parked();
        assert_eq!(
            (source.asked.load(Ordering::SeqCst), changes(cx)),
            (2, refused_under + 1),
            "the ask after the refusal answered, its token a new credential"
        );
    }

    /// A renewal the wall clock has passed is asked at the next health check, though its own
    /// timer has not fired: the executor's timers stand still while the machine sleeps, so after
    /// a sleep that timer fires late. Before, the token held, ended meanwhile, was sent until it.
    #[gpui::test]
    async fn a_renewal_the_clock_passed_in_a_sleep_is_asked_at_the_next_check(
        cx: &mut TestAppContext,
    ) {
        use std::sync::atomic::AtomicUsize;
        #[derive(Default)]
        struct Leasing {
            asked: AtomicUsize,
        }
        impl TokenSource for Leasing {
            fn token(
                &self,
                _: Arc<AtomicBool>,
            ) -> futures::future::BoxFuture<'static, Result<Token, NoToken>> {
                let asked = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
                let now = SystemTime::now();
                Box::pin(async move {
                    Ok(Token::leased(
                        format!("token {asked}"),
                        Lease {
                            expires_at: now + Duration::from_secs(7200),
                            renew_at: now + Duration::from_secs(3600),
                            holder: "user_1".into(),
                        },
                    ))
                })
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
        let source = Arc::new(Leasing::default());
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
        assert_eq!(asked(), 1);
        // The machine slept past the renewal: the wall clock moved, the executor's did not.
        connection.update(cx, |connection, _| {
            connection.lease.as_mut().unwrap().renew_at = SystemTime::now() - Duration::from_secs(1)
        });
        cx.executor().advance_clock(DEFAULT_CHECK_INTERVAL);
        cx.run_until_parked();
        assert_eq!(asked(), 2, "renewed at the check after waking");
        cx.executor().advance_clock(DEFAULT_CHECK_INTERVAL);
        cx.run_until_parked();
        assert_eq!(asked(), 2, "the renewed token is not asked about again");
    }

    /// Leased tokens of one holder (an hour to live, renewed at half), each ask's answer held
    /// while a gate is set, and `None` among the answers for one it cannot give now.
    #[derive(Default)]
    struct Scripted {
        asked: std::sync::atomic::AtomicUsize,
        answers: std::sync::Mutex<Vec<Option<&'static str>>>,
        /// How long each answer's token lives, an hour and more when none is left.
        lives: std::sync::Mutex<Vec<Duration>>,
        gate: std::sync::Mutex<Option<futures::future::Shared<oneshot::Receiver<()>>>>,
        /// Each answer's token is the answer as written, not numbered by the ask: the same
        /// token again, as a sign-in gives the one it holds while it is fresh.
        unnumbered: AtomicBool,
    }

    impl TokenSource for Scripted {
        fn token(
            &self,
            _: Arc<AtomicBool>,
        ) -> futures::future::BoxFuture<'static, Result<Token, NoToken>> {
            let asked = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
            let answer = {
                let mut answers = self.answers.lock().unwrap();
                if answers.is_empty() {
                    Some("token")
                } else {
                    answers.remove(0)
                }
            };
            let unnumbered = self.unnumbered.load(Ordering::SeqCst);
            let gate = self.gate.lock().unwrap().take();
            let lives = {
                let mut lives = self.lives.lock().unwrap();
                if lives.is_empty() {
                    Duration::from_secs(7200)
                } else {
                    lives.remove(0)
                }
            };
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.await.ok();
                }
                let now = SystemTime::now();
                answer
                    .map(|token| {
                        Token::leased(
                            if unnumbered {
                                token.to_owned()
                            } else {
                                format!("{token} {asked}")
                            },
                            Lease {
                                expires_at: now + lives,
                                renew_at: now + Duration::from_secs(3600),
                                holder: "user_1".into(),
                            },
                        )
                    })
                    .ok_or_else(|| NoToken::passing("the sign-in service did not answer"))
            })
        }
    }

    /// The token each graph-list request carried, in order.
    type Carried = Arc<std::sync::Mutex<Vec<String>>>;

    /// A connection to a server that answers every request, its token from `source`, the token
    /// each graph-list request carried, and its client.
    fn sourced(
        source: Arc<Scripted>,
        cx: &mut TestAppContext,
    ) -> (Entity<Connection>, Carried, Arc<dyn HttpClient>) {
        let sent: Carried = Arc::default();
        let log = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            if path.ends_with("/graphs") {
                let token = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                log.lock().unwrap().push(token);
            }
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
            name: "signed".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let held: Arc<dyn TokenSource> = source;
        let http: Arc<dyn HttpClient> = http;
        let given = http.clone();
        let connection = cx.new(|_| {
            Connection::new(server, None, given)
                .unwrap()
                .with_token_source(Some(held))
        });
        (connection, sent, http)
    }

    /// An ask of a source replaced since (a sign-out then a sign-in, the CLI's credential) holds
    /// back nothing: the new source's renewal is asked. Before, an ask after a refusal still
    /// under way when its source was replaced held back every renewal of the next, whose tokens
    /// then lapsed.
    #[gpui::test]
    async fn an_ask_of_a_replaced_source_holds_back_no_renewal(cx: &mut TestAppContext) {
        let first = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(first.clone(), cx);
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let (_never, gate) = oneshot::channel::<()>();
        *first.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!(
            first.asked.load(Ordering::SeqCst),
            2,
            "asked after the refusal"
        );

        // Replaced while that ask waits, as by a sign-out and a sign-in.
        let next = Arc::new(Scripted::default());
        let held: Arc<dyn TokenSource> = next.clone();
        connection.update(cx, |connection, cx| {
            connection.state = ConnectionState::Idle;
            connection.take_token_from(Some(held), cx);
        });
        let before = next.asked.load(Ordering::SeqCst);
        connection.update(cx, |connection, cx| {
            connection.ask_for_token(Ask::Renewal, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            next.asked.load(Ordering::SeqCst),
            before + 1,
            "the new source's renewal asked"
        );
    }

    /// A Refresh the source cannot answer now keeps the token held, and asks for its renewal
    /// again halfway to its end, as a renewal answered so does: a renewal due while the Refresh
    /// was under way waited for it. Before, that renewal was lost, and the token renewed only
    /// at the next health check, or lapsed first.
    #[gpui::test]
    async fn a_refresh_that_keeps_the_token_asks_for_its_renewal_again(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut settings = studio_settings::StudioSettings::default();
            settings.studio.health_check_seconds = 36_000;
            cx.set_global(settings);
        });
        let source = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(source.clone(), cx);
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let asked = || source.asked.load(Ordering::SeqCst);
        assert_eq!(asked(), 1);
        let (release, gate) = oneshot::channel::<()>();
        *source.gate.lock().unwrap() = Some(gate.shared());
        *source.answers.lock().unwrap() = vec![None];
        connection.update(cx, |connection, cx| connection.refresh_token(cx));
        cx.run_until_parked();
        assert_eq!(asked(), 2, "the Refresh under way");
        // The renewal comes due while the Refresh waits, and waits for it.
        cx.executor().advance_clock(Duration::from_secs(3600));
        cx.run_until_parked();
        assert_eq!(asked(), 2);
        release.send(()).unwrap();
        cx.run_until_parked();
        assert_eq!(
            connection.read_with(cx, |connection, _| connection.state.clone()),
            ConnectionState::Ready,
            "the token held stands"
        );
        cx.executor().advance_clock(Duration::from_secs(3600));
        cx.run_until_parked();
        assert_eq!(
            asked(),
            3,
            "the renewal asked again halfway to the token's end"
        );
    }

    /// The same holder's next token from the source that gave the one held replaces it quietly
    /// for a renewal and for a connect once the server is back (its renewal put off while it
    /// could not be reached); a Refresh's is a new credential, as the person asked to read
    /// again. After a refused read the next is a
    /// new credential, tried at once: after a refused read (a Refresh asks in place of the ask
    /// after it, as one, and a connect's answer that comes first is one too, and the one more
    /// try), or while the server refuses it. So is a new source's token. Before, only a renewal's answer was quiet, so a reconnect had every
    /// open graph forget what it read and read it all again.
    #[gpui::test]
    async fn the_same_holders_next_token_is_no_new_credential(cx: &mut TestAppContext) {
        let source = Arc::new(Scripted::default());
        let (connection, sent, _) = sourced(source.clone(), cx);
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let credential = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| connection.client().credential_changes())
        };
        let first = credential(cx);

        // Back after the server could not be reached: the connect asks, and probes with the
        // next token.
        connection.update(cx, |connection, cx| {
            connection.state = ConnectionState::Unreachable("away".into());
            connection.connect(cx);
        });
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 2);
        assert_eq!(
            sent.lock().unwrap().last().map(String::as_str),
            Some("Bearer token 2"),
            "the probe carried the next token"
        );
        assert_eq!(
            connection.read_with(cx, |connection, _| connection.state.clone()),
            ConnectionState::Ready
        );
        assert_eq!(
            credential(cx),
            first,
            "a reconnect's token is no new credential"
        );

        connection.update(cx, |connection, cx| connection.refresh_token(cx));
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 3);
        assert_eq!(credential(cx), first + 1, "a Refresh's is a new credential");

        // A read refused: a Refresh meanwhile asks in place of the ask after it.
        let (_held, gate) = oneshot::channel::<()>();
        *source.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 4);
        connection.update(cx, |connection, cx| connection.refresh_token(cx));
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 5, "the Refresh asks");
        assert_eq!(
            credential(cx),
            first + 2,
            "the token after a refused read is a new credential"
        );
        // As the ask after the refusal, it is the one more try: a read it carries that is
        // refused asks nothing more.
        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 5);

        // A read refused again, once one was taken, and a connect whose answer comes before the
        // ask after it, which then ends.
        let (held, gate) = oneshot::channel::<()>();
        *source.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| {
            connection.client().note_refusal_asked(None);
            connection.request_refused(cx);
        });
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 6);
        connection.update(cx, |connection, cx| {
            connection.state = ConnectionState::Unreachable("away".into());
            connection.connect(cx);
        });
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 7);
        assert_eq!(
            credential(cx),
            first + 3,
            "the connect's token, while the ask after a refusal is out, is a new credential"
        );
        assert!(held.is_canceled(), "the ask after the refusal ended");
        // It is that one more try: a read it carries that is refused asks nothing more.
        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 7);

        // Another source's, though its account is the same.
        let next: Arc<dyn TokenSource> = Arc::new(Scripted::default());
        connection.update(cx, |connection, cx| {
            connection.take_token_from(Some(next), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            credential(cx),
            first + 4,
            "a new source's token is a new credential"
        );
    }

    /// The one more try a token given after a refusal is ends with that token: a quiet
    /// renewal's token, refused for a read, has the source asked again; the same token given
    /// again keeps it. Before, the marker outlived its token, and a later refusal asked nothing
    /// until a read was taken.
    #[gpui::test]
    async fn a_quiet_renewal_ends_the_one_more_try(cx: &mut TestAppContext) {
        let source = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(source.clone(), cx);
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 2);
        let noted = |cx: &mut TestAppContext| {
            connection.read_with(cx, |c, _| c.client().refusal_asked().is_some())
        };
        assert!(noted(cx));
        // The same token given again.
        source.unnumbered.store(true, Ordering::SeqCst);
        *source.answers.lock().unwrap() = vec![Some("token 2")];
        connection.update(cx, |connection, cx| {
            connection.ask_for_token(Ask::Renewal, cx)
        });
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 3);
        assert!(noted(cx), "the same token is still the one more try");
        source.unnumbered.store(false, Ordering::SeqCst);
        connection.update(cx, |connection, cx| {
            connection.ask_for_token(Ask::Renewal, cx)
        });
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 4);
        assert!(!noted(cx));
        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!(
            source.asked.load(Ordering::SeqCst),
            5,
            "the renewed token's refusal asks"
        );
    }

    /// A new source ends the asks of the one it replaces, which nobody answers: a CLI export
    /// each runs ends with it rather than hold the CLI's turn from the new source's asks, and a
    /// connect under way starts again, asking the new source once. Before, a Refresh's ask ran
    /// on, and so did a connect's (a changed transport starts one) before the new source was
    /// asked twice, by a Refresh and by the connect again, and a connect's probe went out with
    /// the old source's token.
    #[gpui::test]
    async fn a_new_source_ends_the_ask_of_the_one_it_replaces(cx: &mut TestAppContext) {
        let source = Arc::new(Scripted::default());
        let (connection, sent, http) = sourced(source.clone(), cx);
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let (held, gate) = oneshot::channel::<()>();
        *source.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| connection.refresh_token(cx));
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 2);
        let next = Arc::new(Scripted::default());
        connection.update(cx, |connection, cx| {
            connection.replace_http(http, cx);
            connection.take_token_from(Some(next.clone() as Arc<dyn TokenSource>), cx);
        });
        cx.run_until_parked();
        assert!(held.is_canceled(), "the old source's ask ended");
        assert_eq!(next.asked.load(Ordering::SeqCst), 1);

        // A connect asking, and the source replaced.
        let (held, gate) = oneshot::channel::<()>();
        *next.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        assert_eq!(next.asked.load(Ordering::SeqCst), 2);
        let third = Arc::new(Scripted::default());
        connection.update(cx, |connection, cx| {
            connection.take_token_from(Some(third.clone() as Arc<dyn TokenSource>), cx)
        });
        cx.run_until_parked();
        assert!(
            held.is_canceled(),
            "the connect's ask of the old source ended"
        );
        assert_eq!(third.asked.load(Ordering::SeqCst), 1, "asked once");
        assert_eq!(
            connection.read_with(cx, |connection, _| connection.state.clone()),
            ConnectionState::Ready
        );

        // A connect about to probe with the old source's token: it probes with the new one's.
        let fourth = Arc::new(Scripted::default());
        *fourth.answers.lock().unwrap() = vec![Some("fourth")];
        let before = sent.lock().unwrap().len();
        connection.update(cx, |connection, cx| {
            connection.token_fresh = true;
            connection.connect(cx);
            connection.take_token_from(Some(fourth.clone() as Arc<dyn TokenSource>), cx);
        });
        cx.run_until_parked();
        assert_eq!(sent.lock().unwrap()[before..], ["Bearer fourth 1"]);
        assert_eq!(fourth.asked.load(Ordering::SeqCst), 1);
    }

    /// A Refresh the source cannot answer now, while the server refuses the token held, drops
    /// it, as the ask after the refusal would: the connection says why, and the next health
    /// check asks again. Before, it kept the refused token, which no health check asked about
    /// again. So does a connect from that state, and its token is a new credential: before, the
    /// connect's state hid the refusal.
    #[gpui::test]
    async fn a_refresh_that_fails_after_a_refusal_drops_the_refused_token(cx: &mut TestAppContext) {
        let source = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(source.clone(), cx);
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        *source.answers.lock().unwrap() = vec![None];
        connection.update(cx, |connection, cx| {
            connection.state = ConnectionState::Unauthorized;
            connection.refusal = Some(Unauthorized::TokenRefused);
            connection.probe_refused = Some(connection.client().token_serial());
            connection.refresh_token(cx);
        });
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 2);
        connection.read_with(cx, |connection, _| {
            assert!(!connection.has_token(), "the refused token is dropped");
            assert!(connection.token_unanswered, "asked again at the next check");
            assert_eq!(connection.state, ConnectionState::Unauthorized);
        });

        // A connect from the refused state (a changed transport starts one): the same holder's
        // next token is a new credential, and a failure to give one drops the refused token.
        let source = Arc::new(Scripted::default());
        let (connection, _, http) = sourced(source.clone(), cx);
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let credential = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| connection.client().credential_changes())
        };
        // The probe's 401 to the token held, its one more ask spent.
        let refuse = |connection: &mut Connection, cx: &mut Context<Connection>| {
            connection.asked_after_refusal = true;
            connection.refused(ConnectionState::Unauthorized.into(), true, cx);
        };
        let first = credential(cx);
        connection.update(cx, |connection, cx| {
            refuse(connection, cx);
            connection.connect(cx);
        });
        cx.run_until_parked();
        assert_eq!(credential(cx), first + 1, "a new credential");
        // A probe the server answers with the token held ends the refusal: the next is quiet.
        connection.update(cx, |connection, cx| {
            refuse(connection, cx);
            connection.token_fresh = true;
            connection.connect(cx);
        });
        cx.run_until_parked();
        let taken = credential(cx);
        connection.update(cx, |connection, cx| {
            connection.ask_for_token(Ask::Renewal, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            credential(cx),
            taken,
            "quiet once the server took the token"
        );
        *source.answers.lock().unwrap() = vec![None];
        connection.update(cx, |connection, cx| {
            refuse(connection, cx);
            connection.replace_http(http.clone(), cx);
        });
        cx.run_until_parked();
        connection.read_with(cx, |connection, _| {
            assert!(!connection.has_token(), "the refused token is dropped");
            assert!(connection.token_unanswered);
        });
    }

    /// A 401 whose challenge says the token was not refused (a proxy that drops it, a server
    /// that asks for one): while it stands, even a renewal's token connects again, as any token
    /// does then, and a failure to give one keeps the token, as nothing refused it; a connect
    /// started from it renews the token quietly.
    #[gpui::test]
    async fn a_401_that_refused_no_token_is_no_refusal_of_it(cx: &mut TestAppContext) {
        let source = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(source.clone(), cx);
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let credential = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| connection.client().credential_changes())
        };
        let asks_for_one = |connection: &mut Connection, cx: &mut Context<Connection>| {
            // A bare `Bearer` challenge: the server found no token.
            let failed = Failed {
                state: ConnectionState::Unauthorized,
                challenge: Some(Box::new(Challenge {
                    scheme: "Bearer".into(),
                    params: Vec::new(),
                })),
            };
            connection.refused(failed, true, cx);
        };
        let first = credential(cx);
        connection.update(cx, |connection, cx| asks_for_one(connection, cx));
        cx.run_until_parked();
        assert_eq!(
            source.asked.load(Ordering::SeqCst),
            1,
            "the source is asked nothing"
        );
        connection.update(cx, |connection, cx| {
            connection.ask_for_token(Ask::Renewal, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            connection.read_with(cx, |connection, _| connection.state.clone()),
            ConnectionState::Ready,
            "the renewal's token connected again"
        );
        assert_eq!(credential(cx), first + 1);
        *source.answers.lock().unwrap() = vec![None];
        connection.update(cx, |connection, cx| {
            asks_for_one(connection, cx);
            connection.refresh_token(cx);
        });
        cx.run_until_parked();
        assert!(
            connection.read_with(cx, |connection, _| connection.has_token()),
            "a token nothing refused is kept"
        );
        connection.update(cx, |connection, cx| {
            asks_for_one(connection, cx);
            connection.connect(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            connection.read_with(cx, |connection, _| connection.state.clone()),
            ConnectionState::Ready
        );
        assert_eq!(credential(cx), first + 1, "nothing refused the token");
    }

    /// A server whose sign-in details could not be read, that no longer offers a sign-in, asks
    /// for a token instead. Before, it stayed "Sign-in unavailable", connected again at every
    /// health check.
    #[gpui::test]
    async fn a_server_that_no_longer_offers_a_sign_in_asks_for_a_token(cx: &mut TestAppContext) {
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            async move {
                let (status, body) = if path.ends_with("/healthz") {
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
                    .header("www-authenticate", "Bearer")
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "typed".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("typed".into()), http).unwrap());
        connection.update(cx, |connection, cx| {
            connection.sign_in =
                SignIn::Unavailable(Refusal::passing("the details did not answer"));
            connection.connect(cx);
        });
        cx.run_until_parked();
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Unauthorized);
            assert_eq!(connection.sign_in, SignIn::NotOffered);
        });
    }

    /// A token whose lease has ended is not sent: a request has the next asked for and goes
    /// with it, the connect's own probe included. Before, a request sent
    /// between waking from a sleep and the next health check carried the ended token, and a
    /// write had to be sent again.
    #[gpui::test]
    async fn a_request_with_an_ended_token_waits_for_the_next(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let source = Arc::new(Scripted::default());
        // The first token is over when given, as one a clock stepped past is: the connect's
        // probe has the next asked for and goes with it.
        *source.lives.lock().unwrap() = vec![Duration::ZERO];
        let (connection, sent, http) = sourced(source.clone(), cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        for _ in 0..400 {
            cx.run_until_parked();
            if connection.read_with(cx, |connection, _| connection.state.clone())
                == ConnectionState::Ready
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            connection.read_with(cx, |connection, _| connection.state.clone()),
            ConnectionState::Ready
        );
        assert_eq!(
            source.asked.load(Ordering::SeqCst),
            2,
            "asked again for the probe"
        );
        assert_eq!(
            *sent.lock().unwrap(),
            ["Bearer token 2"],
            "the over one never sent"
        );

        // A ready server's token ends, as across a sleep: a request has the next asked for.
        let client = connection.update(cx, |connection, _| {
            let ended = SystemTime::now() - Duration::from_secs(1);
            connection.client.set_token_ends(Some(ended));
            connection.client.clone()
        });
        let request = std::thread::spawn(move || smol::block_on(client.graphs()).is_ok());
        for _ in 0..400 {
            cx.run_until_parked();
            if request.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(request.join().unwrap());
        assert_eq!(
            source.asked.load(Ordering::SeqCst),
            3,
            "the next token asked for"
        );
        assert_eq!(
            *sent.lock().unwrap(),
            ["Bearer token 2", "Bearer token 3"],
            "the ended one never sent"
        );

        // A server that refused the token asks for no next one: a request sends what is held,
        // without waiting.
        let client = connection.update(cx, |connection, _| {
            connection.state = ConnectionState::Unauthorized;
            let ended = SystemTime::now() - Duration::from_secs(1);
            connection.client.set_token_ends(Some(ended));
            connection.client.clone()
        });
        let started = std::time::Instant::now();
        let request = std::thread::spawn(move || smol::block_on(client.graphs()).is_ok());
        for _ in 0..400 {
            cx.run_until_parked();
            if request.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(request.join().unwrap());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(source.asked.load(Ordering::SeqCst), 3, "nothing asked");

        // The server could not be reached, and is back (the network after a wake): a request with
        // the ended token has it checked at once, which connects again and asks for the next, and
        // goes with that.
        let _watch = connection.update(cx, |connection, cx| connection.watch(cx));
        let client = connection.update(cx, |connection, _| {
            connection.state = ConnectionState::Unreachable("the network was down".into());
            let ended = SystemTime::now() - Duration::from_secs(1);
            connection.client.set_token_ends(Some(ended));
            connection.client.clone()
        });
        let request = std::thread::spawn(move || smol::block_on(client.graphs()).is_ok());
        for _ in 0..400 {
            cx.run_until_parked();
            if request.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(request.join().unwrap());
        assert_eq!(
            source.asked.load(Ordering::SeqCst),
            4,
            "asked once the server answered"
        );
        assert_eq!(sent.lock().unwrap().pop().unwrap(), "Bearer token 4");

        // The person cancels a slow connect, and the CLI target changes: the connection idles
        // with the old source's token, which ends, until a window opens one of its graphs and
        // waits for the new source's token.
        let (stalled, gate) = oneshot::channel::<()>();
        *source.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| {
            connection.replace_http(http.clone(), cx)
        });
        cx.run_until_parked();
        connection.update(cx, |connection, cx| {
            connection.cancel(cx);
            connection.take_token_from(Some(source.clone() as Arc<dyn TokenSource>), cx);
        });
        drop(stalled);
        assert_eq!(source.asked.load(Ordering::SeqCst), 5);
        // Idle, nothing gives the next: a request goes at once with what is held, and the end
        // stands for the next connect.
        let client = connection.update(cx, |connection, _| {
            connection
                .client
                .set_token_ends(Some(SystemTime::now() - Duration::from_secs(1)));
            connection.client.clone()
        });
        let before = sent.lock().unwrap().len();
        let idle = client.clone();
        let started = std::time::Instant::now();
        let request = std::thread::spawn(move || smol::block_on(idle.graphs()).is_ok());
        for _ in 0..400 {
            cx.run_until_parked();
            if request.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(request.join().unwrap());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(sent.lock().unwrap()[before..], ["Bearer token 4"]);
        let (answer, gate) = oneshot::channel::<()>();
        *source.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| connection.wait_for_token(cx));
        // A request while the connect asks waits for its token, and still does after a Cancel
        // leaves the ask to the waiting window, past the second it tells again.
        let before = sent.lock().unwrap().len();
        let waits_before =
            connection.read_with(cx, |connection, _| connection.client.waits_begun());
        let request = std::thread::spawn(move || smol::block_on(client.graphs()).is_ok());
        for _ in 0..400 {
            cx.run_until_parked();
            if connection.read_with(cx, |connection, _| connection.client.waits_begun())
                > waits_before
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            connection.read_with(cx, |connection, _| connection.client.waits_begun())
                > waits_before,
            "the request waiting"
        );
        connection.update(cx, |connection, cx| connection.cancel(cx));
        for _ in 0..150 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!request.is_finished(), "waits for the ask's token");
        assert_eq!(
            connection.read_with(cx, |connection, _| connection.state.clone()),
            ConnectionState::Idle
        );
        answer.send(()).unwrap();
        for _ in 0..400 {
            cx.run_until_parked();
            if request.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(request.join().unwrap());
        assert_eq!(source.asked.load(Ordering::SeqCst), 6);
        assert_eq!(sent.lock().unwrap()[before..], ["Bearer token 6"]);
    }

    /// A request still waiting when the renewal lands may tell once more, as it does each
    /// second; that tell finds the renewed token standing, asks nothing and leaves its end, so
    /// once that token ends a request waits for the next. Before, such a tell took the renewed
    /// token for one that does not end.
    #[gpui::test]
    async fn a_late_tell_leaves_the_renewed_tokens_end_standing(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let source = Arc::new(Scripted::default());
        let (connection, sent, _) = sourced(source.clone(), cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        assert_eq!(
            connection.read_with(cx, |connection, _| connection.state.clone()),
            ConnectionState::Ready
        );
        assert_eq!(source.asked.load(Ordering::SeqCst), 1);
        // A tell while the token held stands, as one sent before a renewal landed comes.
        let ends = connection.update(cx, |connection, cx| {
            connection.token_ended(cx);
            connection.lease_ends()
        });
        cx.run_until_parked();
        assert_eq!(
            source.asked.load(Ordering::SeqCst),
            1,
            "the tell asks nothing"
        );
        assert!(ends.is_some());
        assert_eq!(
            connection.read_with(cx, |connection, _| connection.client.token_ends()),
            ends,
            "the end stands"
        );
        // The token then ends, as across a sleep: a request waits for the next.
        let client = connection.update(cx, |connection, _| {
            connection
                .client
                .set_token_ends(Some(SystemTime::now() - Duration::from_secs(1)));
            connection.client.clone()
        });
        let request = std::thread::spawn(move || smol::block_on(client.graphs()).is_ok());
        for _ in 0..400 {
            cx.run_until_parked();
            if request.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(request.join().unwrap());
        assert_eq!(source.asked.load(Ordering::SeqCst), 2, "the next asked for");
        assert_eq!(
            *sent.lock().unwrap(),
            ["Bearer token 1", "Bearer token 2"],
            "the ended one sent once only, by the connect"
        );
    }

    /// A renewal that came while nothing showed the server is owed to the token it would have
    /// renewed: a new source's token (the CLI target changed) drops it with that token's lease,
    /// and the next watch asks nothing. Before, it asked the new source again for nothing.
    #[gpui::test]
    async fn a_new_source_is_not_asked_for_the_renewal_owed_to_the_one_before(
        cx: &mut TestAppContext,
    ) {
        let source = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(source.clone(), cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        // Past the token's renewal, with nothing showing the server: the renewal is owed.
        cx.executor().advance_clock(Duration::from_secs(3600));
        cx.run_until_parked();
        assert!(connection.read_with(cx, |connection, _| connection.renewal_missed));
        let next = Arc::new(Scripted::default());
        connection.update(cx, |connection, cx| {
            connection.take_token_from(Some(next.clone() as Arc<dyn TokenSource>), cx)
        });
        cx.run_until_parked();
        assert_eq!(next.asked.load(Ordering::SeqCst), 1, "asked for its token");
        let _watch = connection.update(cx, |connection, cx| connection.watch(cx));
        cx.run_until_parked();
        assert_eq!(next.asked.load(Ordering::SeqCst), 1, "nothing owed to it");
        assert_eq!(source.asked.load(Ordering::SeqCst), 1);
    }

    /// A renewal owed while nothing showed the server is answered by a renewal a request asked
    /// for meanwhile, its token having ended: the next watch asks nothing more. Before, it asked
    /// again for a token renewed moments ago; the sign-in answered from memory, but the answer
    /// was taken again and the next renewal rescheduled.
    #[gpui::test]
    async fn a_renewal_owed_is_settled_by_one_a_request_asked_for(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let source = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(source.clone(), cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        // Past the token's renewal, with nothing showing the server: the renewal is owed.
        cx.executor().advance_clock(Duration::from_secs(3600));
        cx.run_until_parked();
        assert!(connection.read_with(cx, |connection, _| connection.renewal_missed));
        // The token ends, and an agent's request has the next asked for.
        let client = connection.update(cx, |connection, _| {
            connection
                .client
                .set_token_ends(Some(SystemTime::now() - Duration::from_secs(1)));
            connection.client.clone()
        });
        let request = std::thread::spawn(move || smol::block_on(client.graphs()).is_ok());
        for _ in 0..400 {
            cx.run_until_parked();
            if request.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(request.join().unwrap());
        assert_eq!(
            source.asked.load(Ordering::SeqCst),
            2,
            "renewed for the request"
        );
        let _watch = connection.update(cx, |connection, cx| connection.watch(cx));
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 2, "nothing owed");
    }

    /// A token typed in outlives the lease of the one it replaced: no renewal of that one, owed
    /// or under way, replaces it. Before, the replaced sign-in's renewal replaced the typed token
    /// quietly.
    #[gpui::test]
    async fn a_token_typed_in_is_not_replaced_by_the_renewal_of_the_one_before(
        cx: &mut TestAppContext,
    ) {
        let source = Arc::new(Scripted::default());
        let (connection, sent, _) = sourced(source.clone(), cx);
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 1);
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("typed".into()), cx)
        });
        cx.run_until_parked();
        // Past the replaced token's renewal.
        cx.executor().advance_clock(Duration::from_secs(3600));
        cx.run_until_parked();
        assert_eq!(source.asked.load(Ordering::SeqCst), 1, "no renewal asked");
        let client = connection.read_with(cx, |connection, _| connection.client.clone());
        smol::block_on(client.graphs()).ok();
        assert_eq!(sent.lock().unwrap().pop().unwrap(), "Bearer typed");

        // A renewal already under way when a token is typed answers for nothing.
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("given".into()), cx)
        });
        cx.run_until_parked();
        let (release, gate) = oneshot::channel::<()>();
        *source.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| {
            connection.token_given = false;
            connection.ask_for_token(Ask::Renewal, cx);
        });
        cx.run_until_parked();
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("typed again".into()), cx)
        });
        release.send(()).unwrap();
        cx.run_until_parked();
        smol::block_on(client.graphs()).ok();
        assert_eq!(sent.lock().unwrap().pop().unwrap(), "Bearer typed again");
    }

    /// A request that finds the token ended while the server cannot be reached has it checked
    /// at once, whether or not anything shows the server, and one check serves the requests
    /// waiting, however slow; still away, the request is released and goes with what is held,
    /// not after the 45 seconds a wait may last. The end stands, so each request after has the
    /// server checked again. A check answers only for the requests waiting when it began: one
    /// that began waiting during it, the network back meanwhile, goes with the next token.
    #[gpui::test]
    async fn a_server_still_away_releases_the_request_its_check_was_for(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let down = Arc::new(AtomicBool::new(false));
        let hold = Arc::new(AtomicBool::new(false));
        let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let answers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tokens: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let (away, holding, checked, answered_checks, carried) = (
            down.clone(),
            hold.clone(),
            checks.clone(),
            answers.clone(),
            tokens.clone(),
        );
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let down = away.load(Ordering::SeqCst);
            let health = path.ends_with("/healthz");
            if health {
                checked.fetch_add(1, Ordering::SeqCst);
            } else if path.ends_with("/graphs") && !down {
                let token = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                carried.lock().unwrap().push(token);
            }
            // A check begun while the test holds checks answers only once it lets go, however
            // long that is, as the network a check finds down at its start may be back by then.
            let held = health && holding.load(Ordering::SeqCst);
            let holding = holding.clone();
            let answered_checks = answered_checks.clone();
            async move {
                while held && holding.load(Ordering::SeqCst) {
                    smol::Timer::after(Duration::from_millis(10)).await;
                }
                if health {
                    answered_checks.fetch_add(1, Ordering::SeqCst);
                }
                if down {
                    anyhow::bail!("the network is down");
                }
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
        let source = Arc::new(Scripted::default());
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
        let mut watch = Some(connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        }));
        cx.run_until_parked();
        down.store(true, Ordering::SeqCst);
        let client = connection.update(cx, |connection, _| {
            connection.state = ConnectionState::Unreachable("the network is down".into());
            let ended = SystemTime::now() - Duration::from_secs(1);
            connection.client.set_token_ends(Some(ended));
            connection.client.clone()
        });
        let checked = || checks.load(Ordering::SeqCst);
        // Sends a request with the ended token, running the app until it is answered: whether
        // it was, and how long it took.
        fn answered(client: &GraphClient, cx: &mut TestAppContext) -> (bool, Duration) {
            let client = client.clone();
            let started = std::time::Instant::now();
            let request = std::thread::spawn(move || smol::block_on(client.graphs()).is_ok());
            for _ in 0..600 {
                cx.run_until_parked();
                if request.is_finished() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(request.is_finished(), "released");
            (request.join().unwrap(), started.elapsed())
        }
        let send = |client: &GraphClient| {
            let client = client.clone();
            std::thread::spawn(move || smol::block_on(client.graphs()).is_ok())
        };
        let run = |cx: &mut TestAppContext, turns: usize| {
            for _ in 0..turns {
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let run_until = |cx: &mut TestAppContext, what: &str, done: &dyn Fn() -> bool| {
            for _ in 0..600 {
                if done() {
                    return;
                }
                run(cx, 1);
            }
            panic!("timed out waiting for {what}");
        };
        // A slow check: the request, telling again a second on and more while it is under way,
        // starts no other, and goes at its answer.
        hold.store(true, Ordering::SeqCst);
        let before = checked();
        let request = send(&client);
        run_until(cx, "the check", &|| checked() > before);
        run(cx, 150);
        assert_eq!(checked() - before, 1, "one check");
        assert!(!request.is_finished(), "waits for the check");
        hold.store(false, Ordering::SeqCst);
        run_until(cx, "the request", &|| request.is_finished());
        assert!(!request.join().unwrap(), "the network still down");
        assert_eq!(checked() - before, 1, "one check");
        // Each request after has the server checked again, the network may be back, and goes at
        // that check's answer, well within the wait; so does one once nothing shows the server.
        for unwatched in [false, true] {
            if unwatched {
                drop(watch.take());
            }
            let before = checked();
            let (sent, took) = answered(&client, cx);
            assert!(!sent, "the network still down");
            assert!(took < Duration::from_secs(5), "{took:?}");
            assert_eq!(checked() - before, 1, "checked, unwatched: {unwatched}");
        }
        assert_eq!(source.asked.load(Ordering::SeqCst), 1, "nothing asked");
        // The network comes back while a check, begun for one request, is under way, and a
        // second request begins waiting during it: the check's answer releases the first only,
        // and the second, telling again, has the server checked again, finds it back, and goes
        // with the next token.
        hold.store(true, Ordering::SeqCst);
        let before = tokens.lock().unwrap().len();
        let checks_before = checked();
        let first = send(&client);
        run_until(cx, "the first's check", &|| checked() > checks_before);
        let begun = client.waits_begun();
        let second = send(&client);
        run_until(cx, "the second waiting", &|| client.waits_begun() > begun);
        down.store(false, Ordering::SeqCst);
        hold.store(false, Ordering::SeqCst);
        run_until(cx, "both requests", &|| {
            first.is_finished() && second.is_finished()
        });
        first.join().unwrap();
        assert!(
            second.join().unwrap(),
            "the second sent once the server answered"
        );
        assert_eq!(source.asked.load(Ordering::SeqCst), 2, "the next asked for");
        // The first goes with what is held, its check having found the server away; the
        // second, and the connect's probe, with the next.
        let carried = tokens.lock().unwrap()[before..].to_vec();
        assert_eq!(
            carried
                .iter()
                .filter(|token| *token == "Bearer token 1")
                .count(),
            1,
            "only the first sent the ended token: {carried:?}"
        );
        assert_eq!(carried.last().map(String::as_str), Some("Bearer token 2"));

        // Away again, and the person presses Refresh while a check is under way: the check's
        // answer releases nothing, as the ask gives the next, which the request goes with.
        down.store(true, Ordering::SeqCst);
        let client = connection.update(cx, |connection, _| {
            connection.state = ConnectionState::Unreachable("the network is down".into());
            connection
                .client
                .set_token_ends(Some(SystemTime::now() - Duration::from_secs(1)));
            connection.client.clone()
        });
        hold.store(true, Ordering::SeqCst);
        let before = tokens.lock().unwrap().len();
        let checks_before = checked();
        let request = send(&client);
        run_until(cx, "the check", &|| checked() > checks_before);
        let (answer, gate) = oneshot::channel::<()>();
        *source.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| connection.refresh_token(cx));
        run_until(cx, "the Refresh's ask", &|| {
            source.asked.load(Ordering::SeqCst) == 3
        });
        let answers_before = answers.load(Ordering::SeqCst);
        down.store(false, Ordering::SeqCst);
        hold.store(false, Ordering::SeqCst);
        run_until(cx, "the check's answer", &|| {
            answers.load(Ordering::SeqCst) > answers_before
        });
        run(cx, 120);
        assert!(!request.is_finished(), "waits for the Refresh's token");
        answer.send(()).unwrap();
        run_until(cx, "the request", &|| request.is_finished());
        assert!(request.join().unwrap());
        let carried = tokens.lock().unwrap()[before..].to_vec();
        assert!(
            !carried.contains(&"Bearer token 2".to_owned()),
            "the ended token never sent: {carried:?}"
        );
        assert!(
            carried.contains(&"Bearer token 3".to_owned()),
            "{carried:?}"
        );
    }

    /// A source that is not a sign-in (the CLI) that could not answer is asked again at a failed
    /// check's pace, not a ready server's: each ask runs the CLI. A sign-in's service is asked
    /// again sooner (`sign_in::tests::a_sign_in_not_answering_after_a_wake_is_asked_again_soon`).
    /// The ended token it could not renew (the machine slept past its renewal, the network not
    /// back) stays held, and the next answer renews it: the credential never changes, so no
    /// graph reads everything again. Before, the ended token was dropped and the next was a new
    /// credential.
    #[gpui::test]
    async fn a_source_that_could_not_answer_is_asked_again_at_a_failed_checks_pace(
        cx: &mut TestAppContext,
    ) {
        let source = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(source.clone(), cx);
        let _watch = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        let asked = || source.asked.load(Ordering::SeqCst);
        assert_eq!(asked(), 1);
        let credential = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| connection.client.credential_changes())
        };
        let first = credential(cx);
        *source.answers.lock().unwrap() = vec![None];
        connection.update(cx, |connection, cx| {
            connection
                .client
                .set_token_ends(Some(SystemTime::now() - Duration::from_secs(1)));
            connection.ask_for_token(Ask::Renewal, cx);
        });
        cx.run_until_parked();
        assert_eq!(asked(), 2);
        connection.read_with(cx, |connection, _| {
            assert!(connection.has_token(), "the ended token stays held");
            assert!(
                !connection.token_not_received(),
                "a token kept unsent was not dropped on its way"
            );
        });
        assert_eq!(credential(cx), first);
        cx.executor().advance_clock(DEFAULT_CHECK_INTERVAL);
        cx.run_until_parked();
        assert_eq!(asked(), 2, "not at a ready server's pace");
        cx.executor().advance_clock(FAILED_CHECK_INTERVAL);
        cx.run_until_parked();
        assert_eq!(asked(), 3, "asked again at a failed check's");
        connection.read_with(cx, |connection, _| {
            assert!(connection.has_token());
            assert_eq!(connection.state, ConnectionState::Ready);
        });
        assert_eq!(
            credential(cx),
            first,
            "the next token renewed the ended one"
        );
    }

    /// A request refused for the ended token kept while its source could not answer (the server
    /// back before the source, after a wake) asks the source for its renewal: the answer
    /// replaces it quietly and the connection is ready again, the credential unchanged. Before,
    /// the 401 counted as a refusal of the account, and its answer was a new credential under
    /// which every graph read everything again.
    #[gpui::test]
    async fn a_refusal_of_an_ended_token_asks_for_its_renewal(cx: &mut TestAppContext) {
        let source = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(source.clone(), cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        let asked = || source.asked.load(Ordering::SeqCst);
        let held = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| {
                (
                    connection.client.credential_changes(),
                    connection.client.token_serial(),
                )
            })
        };
        let (first, serial) = held(cx);
        // The token ends and its renewal cannot be had now: it stays held.
        *source.answers.lock().unwrap() = vec![None];
        connection.update(cx, |connection, cx| {
            connection
                .client
                .set_token_ends(Some(SystemTime::now() - Duration::from_secs(1)));
            connection.ask_for_token(Ask::Renewal, cx);
        });
        cx.run_until_parked();
        assert_eq!(asked(), 2);
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Unauthorized);
            assert!(connection.has_token());
        });
        // A graph's request is refused for it: its renewal is asked for, and given.
        connection.update(cx, |connection, cx| connection.request_refused(cx));
        cx.run_until_parked();
        assert_eq!(asked(), 3, "the renewal asked for");
        let (credential, renewed) = held(cx);
        assert_eq!(credential, first, "the renewal is no new credential");
        assert_ne!(renewed, serial, "the ended token replaced");
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Ready)
        });
    }

    /// A Cancel of the connect a new token source started (a change to the CLI target) keeps
    /// asking that source while the token held is the old one's: its answer replaces the old
    /// token, and the connection stays as Cancel left it. Before, the ask was dropped and the
    /// old source's token stayed in the client, where the agent's gateway would still send it.
    #[gpui::test]
    async fn a_cancel_after_a_source_change_still_replaces_the_old_token(cx: &mut TestAppContext) {
        let source = Arc::new(Scripted::default());
        let (connection, _, _) = sourced(source.clone(), cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        let serial = |cx: &mut TestAppContext| {
            connection.read_with(cx, |connection, _| connection.client.token_serial())
        };
        let old = serial(cx);
        // The target changed: the connect starts again, asking the new source, which holds
        // its answer.
        let next = Arc::new(Scripted::default());
        *next.answers.lock().unwrap() = vec![Some("next")];
        let (release, gate) = oneshot::channel::<()>();
        *next.gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.take_token_from(Some(next.clone() as Arc<dyn TokenSource>), cx);
        });
        cx.run_until_parked();
        assert_eq!(next.asked.load(Ordering::SeqCst), 1);
        connection.update(cx, |connection, cx| connection.cancel(cx));
        release.send(()).unwrap();
        cx.run_until_parked();
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Idle)
        });
        assert_ne!(
            serial(cx),
            old,
            "the new source's token replaced the old one's"
        );
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
