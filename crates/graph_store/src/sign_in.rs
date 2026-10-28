//! Signing in to a server through OAuth (spec 016): the sign-in service a server's 401 names,
//! the keychain that keeps a refresh token, and the token source that refreshes from it.
//!
//! A connection signs in only to a server whose 401 offers it (`resource_metadata`) and for which
//! it holds no other credential: a CLI target with `auth: anonymous`. The browser opens only when
//! the person chooses Sign in; a kept sign-in is refreshed without asking.

#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{BoxFuture, Shared};
use gpui::App;
use http_client::HttpClient;
use oauth_client::{
    Account, AuthorizationServer, ClientIdentities, Clients, Expected, ProtectedResource,
    TokenError, Tokens,
};

use crate::cli_tokens::{Lease, NoToken, Token, TokenSource};

/// The URL of Studio's Client ID Metadata Document. Not published yet (spec 016, open question
/// 1): until it is, a sign-in service that takes only metadata documents does not accept Studio.
const METADATA_DOCUMENT: Option<&str> = None;

/// Client IDs registered for Studio, by issuer: none yet (spec 016, Client identity).
const REGISTERED: &[(&str, &str)] = &[];

/// How long before an access token ends Studio asks for the next (spec 016, Limits); for a
/// token that lives two minutes or less, halfway through its life.
pub(crate) const REFRESH_LEAD: Duration = Duration::from_secs(60);

/// The soonest a token is renewed after it was given.
const MIN_RENEWAL: Duration = Duration::from_secs(1);

/// The wait before a failed refresh is tried once more: a moment in tests, whose clock it does
/// not follow.
const REFRESH_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(5)
};

/// Studio's client identity: its metadata document and the IDs registered for it, and for
/// development the client ID `OMNIGRAPH_STUDIO_OAUTH_CLIENT` names, used at any sign-in service
/// (`script/dev-sign-in`).
pub fn studio_identities() -> ClientIdentities {
    ClientIdentities {
        metadata_document: METADATA_DOCUMENT.map(str::to_owned),
        registered: REGISTERED
            .iter()
            .map(|(issuer, id)| ((*issuer).to_owned(), (*id).to_owned()))
            .collect(),
        development: std::env::var("OMNIGRAPH_STUDIO_OAUTH_CLIENT")
            .ok()
            .filter(|id| !id.is_empty()),
    }
}

/// A keychain item: its user, the account, and its secret.
pub type KeychainItem = (String, Vec<u8>);

/// A keychain item read, `None` when the keychain holds none for the key.
pub type KeychainRead = BoxFuture<'static, Result<Option<KeychainItem>, String>>;

/// The OS keychain, one item per key with a user and a secret, as GPUI reaches it.
pub trait Keychain: Send + Sync + 'static {
    fn read(&self, key: String) -> KeychainRead;
    fn write(
        &self,
        key: String,
        user: String,
        secret: Vec<u8>,
    ) -> BoxFuture<'static, Result<(), String>>;
    fn delete(&self, key: String) -> BoxFuture<'static, Result<(), String>>;
}

enum KeychainRequest {
    Read(String),
    Write(String, String, Vec<u8>),
    Delete(String),
}

type KeychainReply = oneshot::Sender<Result<Option<KeychainItem>, String>>;

/// The platform's keychain, reachable from any thread: GPUI's credential calls are made on the
/// main thread, so each request is sent there and answered.
pub struct PlatformKeychain {
    requests: mpsc::UnboundedSender<(KeychainRequest, KeychainReply)>,
}

impl PlatformKeychain {
    pub fn start(cx: &mut App) -> Arc<dyn Keychain> {
        let (requests, mut received) = mpsc::unbounded::<(KeychainRequest, KeychainReply)>();
        cx.spawn(async move |cx| {
            while let Some((request, reply)) = received.next().await {
                let answer = match request {
                    KeychainRequest::Read(key) => {
                        let read = cx.update(|cx| cx.read_credentials(&key));
                        read.await.map_err(|error| error.to_string())
                    }
                    KeychainRequest::Write(key, user, secret) => {
                        let write = cx.update(|cx| cx.write_credentials(&key, &user, &secret));
                        write
                            .await
                            .map(|()| None)
                            .map_err(|error| error.to_string())
                    }
                    KeychainRequest::Delete(key) => {
                        let delete = cx.update(|cx| cx.delete_credentials(&key));
                        delete
                            .await
                            .map(|()| None)
                            .map_err(|error| error.to_string())
                    }
                };
                reply.send(answer).ok();
            }
        })
        .detach();
        Arc::new(Self { requests })
    }

    fn ask(&self, request: KeychainRequest) -> KeychainRead {
        let (reply, answer) = oneshot::channel();
        let sent = self.requests.unbounded_send((request, reply));
        async move {
            sent.map_err(|_| "Studio is quitting".to_owned())?;
            answer.await.map_err(|_| "Studio is quitting".to_owned())?
        }
        .boxed()
    }
}

impl Keychain for PlatformKeychain {
    fn read(&self, key: String) -> KeychainRead {
        self.ask(KeychainRequest::Read(key))
    }

    fn write(
        &self,
        key: String,
        user: String,
        secret: Vec<u8>,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.ask(KeychainRequest::Write(key, user, secret))
            .map(|done| done.map(|_| ()))
            .boxed()
    }

    fn delete(&self, key: String) -> BoxFuture<'static, Result<(), String>> {
        self.ask(KeychainRequest::Delete(key))
            .map(|done| done.map(|_| ()))
            .boxed()
    }
}

/// A keychain in memory, for tests: what it holds can be read back.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct MemoryKeychain {
    pub items: Mutex<HashMap<String, KeychainItem>>,
    /// Writes are refused, as a keychain that will not keep the sign-in.
    pub refuses_writes: AtomicBool,
    /// How many times it was read.
    pub reads: std::sync::atomic::AtomicUsize,
}

#[cfg(any(test, feature = "test-support"))]
impl Keychain for MemoryKeychain {
    fn read(&self, key: String) -> KeychainRead {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let item = self
            .items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned();
        futures::future::ready(Ok(item)).boxed()
    }

    fn write(
        &self,
        key: String,
        user: String,
        secret: Vec<u8>,
    ) -> BoxFuture<'static, Result<(), String>> {
        if self.refuses_writes.load(Ordering::SeqCst) {
            return futures::future::ready(Err("the keychain refused".to_owned())).boxed();
        }
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, (user, secret));
        futures::future::ready(Ok(())).boxed()
    }

    fn delete(&self, key: String) -> BoxFuture<'static, Result<(), String>> {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        futures::future::ready(Ok(())).boxed()
    }
}

/// What a connection needs to sign in: Studio's own client, for a sign-in service on another
/// host than the server's; the keychain; and Studio's client identity.
#[derive(Clone)]
pub struct SignInContext {
    pub elsewhere: Arc<dyn HttpClient>,
    pub keychain: Arc<dyn Keychain>,
    pub identities: ClientIdentities,
}

/// A server's sign-in service as Studio found and checked it.
#[derive(Clone)]
pub struct Service {
    pub resource: ProtectedResource,
    pub server: AuthorizationServer,
    pub client_id: String,
    pub(crate) clients: Clients,
}

impl Service {
    /// The sign-in service's host, as the person reads it.
    pub fn host(&self) -> String {
        oauth_client::host_of(&self.server.issuer)
    }

    pub(crate) fn expected(&self) -> Expected<'_> {
        Expected {
            issuer: &self.server.issuer,
            resource: &self.resource.resource,
            client_id: &self.client_id,
            organization: None,
        }
    }

    /// The keychain item of the sign-in to this service for the CLI target `target`: one per
    /// target, issuer and resource (spec 016, Custody).
    pub(crate) fn key(&self, target: &str) -> String {
        format!(
            "omnigraph-studio-oauth:{target}#{}#{}",
            self.server.issuer, self.resource.resource
        )
    }
}

/// Reads and checks the server's sign-in details, its sign-in service's, and which client ID
/// Studio uses there.
pub(crate) async fn discover(
    clients: Clients,
    metadata_url: String,
    identities: ClientIdentities,
) -> Result<Service, String> {
    let resource = oauth_client::protected_resource(&clients, &metadata_url)
        .await
        .map_err(|refusal| refusal.0)?;
    let server = oauth_client::authorization_server(&clients, &resource.issuer)
        .await
        .map_err(|refusal| refusal.0)?;
    let client_id = identities.for_server(&server).ok_or_else(|| {
        format!(
            "The sign-in service at {} does not accept Omnigraph Studio.",
            oauth_client::host_of(&server.issuer)
        )
    })?;
    Ok(Service {
        resource,
        server,
        client_id,
        clients,
    })
}

/// What the person sees of a connection's sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    /// The server has offered no sign-in, or the connection's credential comes from elsewhere.
    NotOffered,
    /// The server's 401 offers a sign-in, and Studio is reading its details.
    Reading,
    /// Studio can sign in at `service`; `why` is why it is not signed in, when a sign-in or
    /// refresh failed.
    Offered {
        service: String,
        why: Option<String>,
    },
    /// The server offers a sign-in Studio cannot use, and why.
    Unavailable(String),
    /// The browser is open at `service`.
    Waiting { service: String },
    /// Signed in at `service` as `account`; `kept` says the keychain kept the sign-in.
    SignedIn {
        service: String,
        account: String,
        kept: bool,
    },
}

/// The tokens of one sign-in: the access token, refreshed from the refresh token the keychain
/// keeps (spec 016, Refresh). One refresh runs at a time; others wait for its answer.
#[derive(Clone)]
pub(crate) struct OAuthSource {
    inner: Arc<Inner>,
}

struct Inner {
    service: Service,
    /// The clients its requests take: the service's at first, then those a new transport for
    /// the server's address makes ([`OAuthSource::reach_through`]).
    clients: Mutex<Clients>,
    key: String,
    keychain: Arc<dyn Keychain>,
    held: smol::lock::Mutex<Held>,
    account: Mutex<Account>,
    /// The keychain kept the refresh token held.
    kept: AtomicBool,
    /// The server refused the access token held: the next ask refreshes.
    refused: AtomicBool,
    /// The sign-in ended (`invalid_grant`, or signed out): no refresh is tried.
    ended: AtomicBool,
}

struct Held {
    access: Option<Access>,
    refresh: Option<String>,
    /// The refresh under way, which every ask joins. It runs on a task of its own to its end, so
    /// an ask dropped mid-exchange (a connect replaced, a renewal overtaken) cannot lose the
    /// refresh token the service has rotated by then.
    running: Option<Shared<oneshot::Receiver<Result<(), NoToken>>>>,
}

struct Access {
    token: String,
    expires_at: SystemTime,
    renew_at: SystemTime,
}

impl Access {
    fn new(token: String, expires_at: SystemTime) -> Self {
        let now = SystemTime::now();
        let lives = expires_at.duration_since(now).unwrap_or_default();
        Self {
            token,
            expires_at,
            // Never sooner than a second away, so no token, however short, renews in a loop.
            renew_at: (expires_at - REFRESH_LEAD.min(lives / 2)).max(now + MIN_RENEWAL),
        }
    }
}

impl OAuthSource {
    /// A sign-in just made, its tokens and whether the keychain kept them.
    pub(crate) fn signed_in(
        service: Service,
        target: &str,
        keychain: Arc<dyn Keychain>,
        tokens: Tokens,
        kept: bool,
    ) -> Self {
        Self::new(
            service,
            target,
            keychain,
            Held {
                access: Some(Access::new(tokens.access_token, tokens.expires_at)),
                refresh: tokens.refresh_token,
                running: None,
            },
            tokens.account,
            kept,
        )
    }

    /// A sign-in the keychain kept: its first ask refreshes.
    pub(crate) fn kept(
        service: Service,
        target: &str,
        keychain: Arc<dyn Keychain>,
        subject: String,
        refresh: String,
    ) -> Self {
        Self::new(
            service,
            target,
            keychain,
            Held {
                access: None,
                refresh: Some(refresh),
                running: None,
            },
            Account {
                subject,
                organization: None,
            },
            true,
        )
    }

    fn new(
        service: Service,
        target: &str,
        keychain: Arc<dyn Keychain>,
        held: Held,
        account: Account,
        kept: bool,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                key: service.key(target),
                clients: Mutex::new(service.clients.clone()),
                service,
                keychain,
                held: smol::lock::Mutex::new(held),
                account: Mutex::new(account),
                kept: AtomicBool::new(kept),
                refused: AtomicBool::new(false),
                ended: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn service(&self) -> &Service {
        &self.inner.service
    }

    pub(crate) fn account(&self) -> String {
        self.inner
            .account
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .subject
            .clone()
    }

    pub(crate) fn kept_in_keychain(&self) -> bool {
        self.inner.kept.load(Ordering::SeqCst)
    }

    /// Sends the sign-in's requests to the server's address through `http` from now on: the
    /// CLI target's transport changed. The sign-in stands, and a refresh under way finishes
    /// with its client and keeps what it gives.
    pub(crate) fn reach_through(&self, http: Arc<dyn HttpClient>) {
        let mut clients = self
            .inner
            .clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *clients = clients.through(http);
    }

    /// Ends the sign-in: drops its tokens, deletes the keychain item and, when the service
    /// publishes a revocation endpoint, revokes the refresh token there, once, within 10
    /// seconds (spec 016, Custody).
    pub(crate) fn sign_out(&self) -> BoxFuture<'static, ()> {
        self.end(true)
    }

    /// Ends the sign-in another replaces, for the same target: drops its tokens and revokes its
    /// refresh token, leaving the keychain item to the sign-in that replaces it.
    pub(crate) fn replaced(&self) -> BoxFuture<'static, ()> {
        self.end(false)
    }

    fn end(&self, forget: bool) -> BoxFuture<'static, ()> {
        let inner = self.inner.clone();
        inner.ended.store(true, Ordering::SeqCst);
        async move {
            let refresh = {
                let mut held = inner.held.lock().await;
                held.access = None;
                held.refresh.take()
            };
            if forget {
                inner.keychain.delete(inner.key.clone()).await.ok();
            }
            if let Some(refresh) = refresh {
                let service = &inner.service;
                let http = inner.token_client();
                oauth_client::revoke(http.as_ref(), &service.server, &service.client_id, &refresh)
                    .await
                    .ok();
            }
        }
        .boxed()
    }
}

impl TokenSource for OAuthSource {
    fn token(&self, _hurry: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>> {
        Inner::token(self.inner.clone()).boxed()
    }

    fn note_refused(&self) {
        self.inner.refused.store(true, Ordering::SeqCst);
    }
}

impl Inner {
    fn token_client(&self) -> Arc<dyn HttpClient> {
        self.clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .client_for(&self.service.server.token_endpoint)
    }

    /// The access token held while it is fresh; otherwise the one the refresh under way gives,
    /// starting it when none is. The refresh is not this ask's: dropped, the ask leaves it running.
    /// A refresh that did not end may pass; a sign-in that ended does not.
    async fn token(self: Arc<Self>) -> Result<Token, NoToken> {
        let running = {
            let mut held = self.held.lock().await;
            if self.ended.load(Ordering::SeqCst) {
                return Err(NoToken::lasting("Signed out."));
            }
            let refused = self.refused.swap(false, Ordering::SeqCst);
            if !refused
                && let Some(access) = &held.access
                && SystemTime::now() < access.renew_at
            {
                return Ok(self.leased(access));
            }
            if held.refresh.is_none() {
                held.access = None;
                return Err(NoToken::lasting("The sign-in has ended. Sign in again."));
            }
            match &held.running {
                Some(running) => running.clone(),
                None => {
                    let (done, running) = oneshot::channel();
                    let running = running.shared();
                    held.running = Some(running.clone());
                    let inner = self.clone();
                    smol::spawn(async move {
                        let refreshed = inner.refresh().await;
                        done.send(refreshed).ok();
                    })
                    .detach();
                    running
                }
            }
        };
        match running.await {
            Ok(Ok(())) => {
                let held = self.held.lock().await;
                held.access
                    .as_ref()
                    .map(|access| self.leased(access))
                    .ok_or_else(|| NoToken::lasting("The sign-in has ended. Sign in again."))
            }
            Ok(Err(no)) => Err(no),
            Err(_) => Err(NoToken::passing(
                "The sign-in's refresh stopped before it answered.",
            )),
        }
    }

    /// Refreshes the access token from the refresh token held, keeping what the service gives.
    async fn refresh(&self) -> Result<(), NoToken> {
        let mut held = self.held.lock().await;
        let refreshed = self.refresh_held(&mut held).await;
        held.running = None;
        refreshed
    }

    async fn refresh_held(&self, held: &mut Held) -> Result<(), NoToken> {
        if self.ended.load(Ordering::SeqCst) {
            return Err(NoToken::lasting("Signed out."));
        }
        let Some(refresh) = held.refresh.clone() else {
            held.access = None;
            return Err(NoToken::lasting("The sign-in has ended. Sign in again."));
        };
        let service = &self.service;
        let mut failed_once = false;
        loop {
            let http = self.token_client();
            match oauth_client::refresh(
                http.as_ref(),
                &service.server,
                &refresh,
                service.expected(),
            )
            .await
            {
                Ok(tokens) => {
                    let subject = tokens.account.subject.clone();
                    if let Some(rotated) =
                        tokens.refresh_token.as_ref().filter(|new| **new != refresh)
                    {
                        // The rotated token replaces the kept one before the new access token
                        // is used: the old one no longer refreshes.
                        let kept = self
                            .keychain
                            .write(
                                self.key.clone(),
                                subject.clone(),
                                rotated.as_bytes().to_vec(),
                            )
                            .await
                            .is_ok();
                        self.kept.store(kept, Ordering::SeqCst);
                        held.refresh = Some(rotated.clone());
                    }
                    held.access = Some(Access::new(tokens.access_token, tokens.expires_at));
                    *self.account.lock().unwrap_or_else(PoisonError::into_inner) = tokens.account;
                    return Ok(());
                }
                Err(TokenError::InvalidGrant(why)) => {
                    held.access = None;
                    held.refresh = None;
                    self.ended.store(true, Ordering::SeqCst);
                    self.keychain.delete(self.key.clone()).await.ok();
                    return Err(NoToken::lasting(format!(
                        "Signed out: {why} Sign in again."
                    )));
                }
                Err(TokenError::Refused(why)) => {
                    held.access = None;
                    return Err(NoToken::lasting(why));
                }
                // A refresh that failed may have rotated the token anyway: once more, after a
                // pause. Then the sign-in stands, and the connection asks again later (spec
                // 016, Refresh).
                Err(TokenError::Failed(_)) if !failed_once => {
                    failed_once = true;
                    smol::Timer::after(REFRESH_RETRY).await;
                }
                Err(TokenError::Failed(why)) => {
                    held.access = None;
                    return Err(NoToken::passing(why));
                }
            }
        }
    }

    fn leased(&self, access: &Access) -> Token {
        let holder = self
            .account
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .subject
            .clone();
        Token::leased(
            access.token.clone(),
            Lease {
                expires_at: access.expires_at,
                renew_at: access.renew_at,
                holder,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};
    use serde_json::{Value, json};

    use super::*;
    use crate::connection::{Connection, ConnectionState, ServerEntry, ServerSource};

    const SERVER: &str = "https://graphs.test";
    const RESOURCE: &str = "https://graphs.test/mcp";
    const ISSUER: &str = "https://id.test";
    const CLIENT: &str = "https://studio.test/client.json";

    /// A server that takes the access tokens its sign-in service issued, and that service:
    /// codes and refresh tokens are single-use, and each refresh rotates.
    #[derive(Default)]
    struct Served {
        accepted: HashSet<String>,
        codes: HashSet<String>,
        refresh: HashSet<String>,
        issued: u32,
        /// The server refuses every token, as one that does not admit the account.
        admits_nobody: bool,
        /// How long each access token lives.
        lives: u64,
        /// The sign-in service gives no refresh token.
        no_refresh: bool,
        /// The sign-in service's issuer, `ISSUER` when empty: one on the server's own origin
        /// takes the target's client for its token and revocation requests.
        issuer: String,
        /// The server holds its sign-in details' answer until this is lowered.
        hold_metadata: bool,
        /// The sign-in service holds a token answer, made already (a refresh token rotated),
        /// until this is lowered.
        hold_token: bool,
        /// The sign-in service fails every token request (503) and keeps its grants.
        token_down: bool,
        /// The tokens the server refused, in order.
        refused: Vec<String>,
        /// Each request, after the name of the client it went through.
        requests: Vec<String>,
        log: Vec<String>,
    }

    fn jwt(claims: Value) -> String {
        format!(
            "{}.{}.c2ln",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    impl Served {
        fn issuer(&self) -> String {
            if self.issuer.is_empty() {
                ISSUER.to_owned()
            } else {
                self.issuer.clone()
            }
        }
    }

    fn issue(served: &mut Served) -> Value {
        served.issued += 1;
        let access = jwt(json!({
            "iss": served.issuer(), "aud": RESOURCE, "client_id": CLIENT, "sub": "user_1",
            "org_id": "org_1", "iat": now(), "exp": now() + served.lives,
            "n": served.issued,
        }));
        served.accepted.insert(access.clone());
        if served.no_refresh {
            return json!({"token_type": "Bearer", "access_token": access, "expires_in": served.lives});
        }
        let refresh = format!("r{}", served.issued);
        served.refresh.insert(refresh.clone());
        json!({"token_type": "Bearer", "access_token": access, "expires_in": served.lives, "refresh_token": refresh})
    }

    fn form(body: &str) -> HashMap<String, String> {
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn answer(
        served: &mut Served,
        method: &str,
        url: &str,
        bearer: Option<String>,
        body: String,
    ) -> (u16, Vec<(&'static str, String)>, Value) {
        let challenge = |refused: bool| {
            let error = if refused {
                r#"error="invalid_token", "#
            } else {
                ""
            };
            vec![(
                "www-authenticate",
                format!(
                    r#"Bearer {error}resource_metadata="{SERVER}/.well-known/oauth-protected-resource/mcp""#
                ),
            )]
        };
        let issuer = served.issuer();
        let issued_at = url::Url::parse(&issuer).unwrap();
        let server_metadata = format!(
            "{}/.well-known/oauth-authorization-server{}",
            issued_at.origin().ascii_serialization(),
            issued_at.path().trim_end_matches('/')
        );
        match (method, url) {
            ("GET", u) if u == format!("{SERVER}/healthz") => (
                200,
                vec![],
                json!({"status": "ok", "version": "0.14.0", "internal_schema_version": 16}),
            ),
            ("GET", u) if u.starts_with(&format!("{SERVER}/graphs")) => match bearer {
                Some(token) if served.accepted.contains(&token) && !served.admits_nobody => {
                    (200, vec![], json!({"graphs": []}))
                }
                Some(token) => {
                    served.refused.push(token);
                    (
                        401,
                        challenge(true),
                        json!({"error": "invalid bearer token", "code": "unauthorized"}),
                    )
                }
                None => (
                    401,
                    challenge(false),
                    json!({"error": "one bearer credential is required", "code": "unauthorized"}),
                ),
            },
            ("GET", u) if u == format!("{SERVER}/.well-known/oauth-protected-resource/mcp") => (
                200,
                vec![],
                json!({"resource": RESOURCE, "authorization_servers": [issuer], "bearer_methods_supported": ["header"]}),
            ),
            ("GET", u) if u == server_metadata => (
                200,
                vec![],
                json!({
                    "issuer": issuer, "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"), "revocation_endpoint": format!("{issuer}/revoke"),
                    "code_challenge_methods_supported": ["S256"], "client_id_metadata_document_supported": true,
                }),
            ),
            ("POST", u) if u == format!("{issuer}/token") => {
                let form = form(&body);
                served.log.push(format!(
                    "token {} resource={}",
                    form["grant_type"],
                    form.get("resource").map_or("-", String::as_str)
                ));
                if served.token_down {
                    return (503, vec![], json!({}));
                }
                let granted = match form["grant_type"].as_str() {
                    "authorization_code" => served.codes.remove(&form["code"]),
                    _ => served.refresh.remove(&form["refresh_token"]),
                };
                if granted
                    && form.get("resource").map(String::as_str) == Some(RESOURCE)
                    && form["client_id"] == CLIENT
                {
                    (200, vec![], issue(served))
                } else {
                    (
                        400,
                        vec![],
                        json!({"error": "invalid_grant", "error_description": "The grant is not valid."}),
                    )
                }
            }
            ("POST", u) if u == format!("{issuer}/revoke") => {
                served.log.push(format!("revoke {}", form(&body)["token"]));
                (200, vec![], json!({}))
            }
            _ => (404, vec![], json!({})),
        }
    }

    fn fake(served: Arc<Mutex<Served>>) -> Arc<dyn HttpClient> {
        through("first", served)
    }

    /// A client named `name`, which notes each request it sends.
    fn through(name: &'static str, served: Arc<Mutex<Served>>) -> Arc<dyn HttpClient> {
        FakeHttpClient::create(move |request| {
            let served = served.clone();
            async move {
                let method = request.method().to_string();
                let url = request.uri().to_string();
                served
                    .lock()
                    .unwrap()
                    .requests
                    .push(format!("{name} {url}"));
                let bearer = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .map(str::to_owned);
                let mut body = String::new();
                futures::AsyncReadExt::read_to_string(&mut request.into_body(), &mut body).await?;
                if url.contains("/.well-known/oauth-protected-resource") {
                    while served.lock().unwrap().hold_metadata {
                        smol::Timer::after(Duration::from_millis(10)).await;
                    }
                }
                let (status, headers, page) =
                    answer(&mut served.lock().unwrap(), &method, &url, bearer, body);
                if url.ends_with("/token") {
                    while served.lock().unwrap().hold_token {
                        smol::Timer::after(Duration::from_millis(10)).await;
                    }
                }
                let mut response = Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT);
                for (name, value) in headers {
                    response = response.header(name, value);
                }
                Ok(response.body(AsyncBody::from(page.to_string()))?)
            }
        })
    }

    fn connection(
        served: &Arc<Mutex<Served>>,
        keychain: Arc<MemoryKeychain>,
        cx: &mut TestAppContext,
    ) -> Entity<Connection> {
        let http = fake(served.clone());
        let server = ServerEntry {
            name: "work".into(),
            url: SERVER.into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let context = SignInContext {
            elsewhere: http.clone(),
            keychain,
            identities: ClientIdentities {
                metadata_document: Some(CLIENT.into()),
                registered: Vec::new(),
                development: None,
            },
        };
        cx.new(|_| {
            Connection::new(server, None, http)
                .unwrap()
                .with_sign_in(Some(context))
        })
    }

    /// Runs the app until `done`, letting work woken from other threads (the browser's request
    /// to the loopback listener) in.
    fn until(cx: &mut TestAppContext, what: &str, done: impl Fn(&mut TestAppContext) -> bool) {
        for _ in 0..400 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    /// Plays the browser: approves the sign-in Studio opened, and returns to its listener.
    fn approve(served: &Arc<Mutex<Served>>, opened: &str) -> std::thread::JoinHandle<String> {
        let opened = url::Url::parse(opened).unwrap();
        let query: HashMap<String, String> = opened.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], CLIENT);
        assert_eq!(query["resource"], RESOURCE);
        assert_eq!(query["code_challenge_method"], "S256");
        served.lock().unwrap().codes.insert("c1".into());
        let callback = url::Url::parse(&query["redirect_uri"]).unwrap();
        assert_eq!(callback.host_str(), Some("127.0.0.1"));
        let issuer = served.lock().unwrap().issuer();
        let path = format!("/callback?code=c1&state={}&iss={issuer}", query["state"]);
        let port = callback.port().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(stream, "GET {path} HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n").unwrap();
            let mut page = String::new();
            stream.read_to_string(&mut page).unwrap();
            page
        })
    }

    fn sign_in_state(
        connection: &Entity<Connection>,
        cx: &mut TestAppContext,
    ) -> (ConnectionState, SignIn, String) {
        connection.read_with(cx, |c, _| (c.state.clone(), c.sign_in.clone(), c.label()))
    }

    #[gpui::test]
    async fn a_server_that_offers_sign_in_is_signed_in_through_the_browser(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            lives: 300,
            ..Served::default()
        }));
        let keychain = Arc::new(MemoryKeychain::default());
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the offer", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
        });
        let (state, offer, label) = sign_in_state(&connection, cx);
        assert_eq!(state, ConnectionState::Unauthorized);
        assert_eq!(
            offer,
            SignIn::Offered {
                service: "id.test".into(),
                why: None
            }
        );
        assert_eq!(label, "Sign in needed");
        // Nothing opened a browser on its own.
        assert_eq!(cx.opened_url(), None);

        connection.update(cx, |c, cx| c.sign_in(cx));
        assert_eq!(
            sign_in_state(&connection, cx).1,
            SignIn::Waiting {
                service: "id.test".into()
            }
        );
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        assert!(
            browser
                .join()
                .unwrap()
                .contains("Signed in to graphs.test. You can close this tab.")
        );
        assert_eq!(
            sign_in_state(&connection, cx).1,
            SignIn::SignedIn {
                service: "id.test".into(),
                account: "user_1".into(),
                kept: true
            }
        );
        let key = format!("omnigraph-studio-oauth:work#{ISSUER}#{RESOURCE}");
        assert_eq!(
            keychain.items.lock().unwrap()[&key],
            ("user_1".to_owned(), b"r1".to_vec())
        );

        // Signing out revokes the refresh token, forgets it, and the server asks again.
        connection.update(cx, |c, cx| c.sign_out(cx));
        until(cx, "the sign-out", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Unauthorized
        });
        until(cx, "the revocation", |_| {
            served.lock().unwrap().log.iter().any(|l| l == "revoke r1")
        });
        assert!(keychain.items.lock().unwrap().is_empty());
        assert_eq!(
            sign_in_state(&connection, cx).1,
            SignIn::Offered {
                service: "id.test".into(),
                why: None
            }
        );

        // Signed in again, then as another account whose grant gives no refresh token: the
        // sign-in replaced is revoked, and the keychain keeps no account rather than the old one,
        // which the next launch would restore.
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the second sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        browser.join().unwrap();
        assert_eq!(keychain.items.lock().unwrap()[&key].1, b"r2".to_vec());
        served.lock().unwrap().no_refresh = true;
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in as another account", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::SignedIn { kept: false, .. }
            ) && sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        browser.join().unwrap();
        until(cx, "the replaced sign-in's revocation", |_| {
            served.lock().unwrap().log.iter().any(|l| l == "revoke r2")
        });
        assert!(keychain.items.lock().unwrap().is_empty());
    }

    #[gpui::test]
    async fn a_kept_sign_in_is_refreshed_without_a_browser_and_renewed_quietly(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        // Tokens that live two seconds are renewed after one.
        let served = Arc::new(Mutex::new(Served {
            lives: 2,
            ..Served::default()
        }));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        keychain.items.lock().unwrap().insert(
            format!("omnigraph-studio-oauth:work#{ISSUER}#{RESOURCE}"),
            ("user_1".into(), b"kept".to_vec()),
        );
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the refresh", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        assert_eq!(cx.opened_url(), None);
        let key = format!("omnigraph-studio-oauth:work#{ISSUER}#{RESOURCE}");
        assert_eq!(
            keychain.items.lock().unwrap()[&key].1,
            b"r1".to_vec(),
            "the rotated token is kept"
        );

        let _watch = connection.update(cx, |c, cx| c.watch(cx));
        let changes = connection.read_with(cx, |c, _| c.client().credential_changes());
        std::thread::sleep(Duration::from_millis(1100));
        cx.executor().advance_clock(Duration::from_secs(2));
        until(cx, "the renewal", |_| served.lock().unwrap().issued == 2);
        cx.run_until_parked();
        assert_eq!(keychain.items.lock().unwrap()[&key].1, b"r2".to_vec());
        assert_eq!(
            connection.read_with(cx, |c, _| c.client().credential_changes()),
            changes,
            "a renewal is not another credential"
        );
        let log = served.lock().unwrap().log.clone();
        assert!(
            log.iter()
                .all(|line| line.ends_with(&format!("resource={RESOURCE}"))),
            "{log:?}"
        );
    }

    /// A sign-in whose service does not answer stands. At launch, with no token yet, the server
    /// says the sign-in is not answering, opens no browser, keeps the keychain's sign-in and asks
    /// again at the next health check. While a token lives, a renewal that fails leaves it in
    /// use, unsaid. Before, either ended the sign-in, and a moment without network cost a
    /// browser sign-in. (The renewal asked again halfway to the token's end is the connection's,
    /// `connection::tests::a_renewal_that_cannot_be_answered_keeps_the_token_held`.)
    #[gpui::test]
    async fn a_sign_in_whose_service_does_not_answer_stands(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        // Tokens that live four seconds are renewed after two.
        let served = Arc::new(Mutex::new(Served {
            lives: 4,
            token_down: true,
            ..Served::default()
        }));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        let key = format!("omnigraph-studio-oauth:work#{ISSUER}#{RESOURCE}");
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key.clone(), ("user_1".into(), b"kept".to_vec()));
        let refreshes = || {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|line| line.starts_with("token refresh_token"))
                .count()
        };
        let signed_in = SignIn::SignedIn {
            service: "id.test".into(),
            account: "user_1".into(),
            kept: true,
        };
        let connection = connection(&served, keychain.clone(), cx);
        let _watch = connection.update(cx, |c, cx| c.watch(cx));
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the refresh and its retry", |_| refreshes() == 2);
        cx.run_until_parked();
        let (state, sign_in, label) = sign_in_state(&connection, cx);
        assert_eq!(state, ConnectionState::Unauthorized);
        assert_eq!(sign_in, signed_in);
        assert_eq!(label, "Sign-in not answering");
        assert!(connection.read_with(cx, |c, _| c.token_unanswered));
        assert_eq!(cx.opened_url(), None);
        assert_eq!(keychain.items.lock().unwrap()[&key].1, b"kept".to_vec());

        // The service answers again: the next health check asks, and the server is ready.
        served.lock().unwrap().token_down = false;
        cx.executor().advance_clock(Duration::from_secs(120));
        until(cx, "the sign-in's token", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        assert_eq!(served.lock().unwrap().issued, 1);

        // Down again when the token is renewed, halfway through its life: it stays in use.
        served.lock().unwrap().token_down = true;
        let changes = connection.read_with(cx, |c, _| c.client().credential_changes());
        std::thread::sleep(Duration::from_millis(2100));
        cx.executor().advance_clock(Duration::from_secs(2));
        until(cx, "the failed renewal", |_| refreshes() == 5);
        cx.run_until_parked();
        let (state, sign_in, _) = sign_in_state(&connection, cx);
        assert_eq!(state, ConnectionState::Ready);
        assert_eq!(sign_in, signed_in);
        assert!(!connection.read_with(cx, |c, _| c.token_unanswered));
        assert_eq!(
            connection.read_with(cx, |c, _| c.client().credential_changes()),
            changes,
            "the token held stands"
        );
        assert_eq!(keychain.items.lock().unwrap()[&key].1, b"r1".to_vec());
    }

    /// A target whose transport changed (another certificate authority, another proxy) is
    /// reached through a new client, and its sign-in goes on through it: nothing is read from
    /// the keychain, which a refresh under way could rotate under a second sign-in, and a token
    /// that lapses afterwards is refreshed with the server's requests on the new client.
    #[gpui::test]
    async fn a_new_transport_keeps_the_sign_in_through_it(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            lives: 300,
            ..Served::default()
        }));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        let key = format!("omnigraph-studio-oauth:work#{ISSUER}#{RESOURCE}");
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key.clone(), ("user_1".into(), b"kept".to_vec()));
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        let reads = keychain.reads.load(Ordering::SeqCst);

        served.lock().unwrap().requests.clear();
        let next = through("next", served.clone());
        connection.update(cx, |c, cx| c.replace_http(next, cx));
        cx.run_until_parked();
        assert!(matches!(
            sign_in_state(&connection, cx).1,
            SignIn::SignedIn { .. }
        ));
        assert_eq!(
            keychain.reads.load(Ordering::SeqCst),
            reads,
            "the keychain is not read"
        );
        assert_eq!(served.lock().unwrap().issued, 1);
        // The server is checked again through the new client, unasked.
        let checked = format!("next {SERVER}/healthz");
        until(cx, "the check through the new client", |_| {
            served.lock().unwrap().requests.contains(&checked)
        });

        // The token lapses: the server refuses it, and the sign-in refreshes it.
        served.lock().unwrap().accepted.clear();
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the refresh through the new client", |cx| {
            served.lock().unwrap().issued == 2
                && sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        let served = served.lock().unwrap();
        let to_server: Vec<&String> = served
            .requests
            .iter()
            .filter(|r| r.contains(SERVER))
            .collect();
        assert!(
            !to_server.is_empty() && to_server.iter().all(|r| r.starts_with("next ")),
            "{to_server:?}"
        );
        assert_eq!(keychain.items.lock().unwrap()[&key].1, b"r2".to_vec());
    }

    /// A sign-in read, or made in the browser, while the target's transport changes takes the
    /// new client when it finishes: requests to the server's address, to a sign-in service there
    /// included, do not stay on the proxy or certificate authority the task started with.
    #[gpui::test]
    async fn a_sign_in_finished_after_a_transport_change_takes_the_new_client(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            lives: 300,
            issuer: format!("{SERVER}/oauth"),
            hold_metadata: true,
            ..Served::default()
        }));
        let connection = connection(&served, Arc::default(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the sign-in details asked for", |_| {
            let served = served.lock().unwrap();
            served
                .requests
                .iter()
                .any(|r| r.contains("oauth-protected-resource"))
        });
        let next = through("next", served.clone());
        connection.update(cx, |c, cx| c.replace_http(next, cx));
        served.lock().unwrap().hold_metadata = false;
        until(cx, "the offer", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
        });

        connection.update(cx, |c, cx| c.sign_in(cx));
        let third = through("third", served.clone());
        connection.update(cx, |c, cx| c.replace_http(third, cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        browser.join().unwrap();
        connection.update(cx, |c, cx| c.sign_out(cx));
        until(cx, "the revocation", |_| {
            served.lock().unwrap().log.iter().any(|l| l == "revoke r1")
        });
        let requests = served.lock().unwrap().requests.clone();
        // The exchange went through the client its sign-in started with, the one discovery
        // finished under; what the sign-in kept takes the newest.
        assert!(
            requests.contains(&format!("next {SERVER}/oauth/token")),
            "{requests:?}"
        );
        assert!(
            requests.contains(&format!("third {SERVER}/oauth/revoke")),
            "{requests:?}"
        );
        for stale in [
            format!("first {SERVER}/oauth/token"),
            format!("first {SERVER}/oauth/revoke"),
        ] {
            assert!(!requests.contains(&stale), "{requests:?}");
        }
    }

    /// A refresh is not the ask's that started it: an ask dropped once the service has rotated
    /// the refresh token (a connect replaced, a renewal overtaken) leaves the refresh to finish,
    /// and the next ask is answered with what it gave, without asking the service again. Before,
    /// the dropped ask took the exchange with it, the next refresh sent the rotated-away token,
    /// and the person was signed out.
    #[test]
    fn a_dropped_ask_leaves_its_refresh_to_finish() {
        let served = Arc::new(Mutex::new(Served {
            lives: 300,
            hold_token: true,
            ..Served::default()
        }));
        served.lock().unwrap().refresh.insert("kept".into());
        let http = fake(served.clone());
        let clients = Clients::new(url::Url::parse(SERVER).unwrap(), http.clone(), http);
        let identities = ClientIdentities {
            metadata_document: Some(CLIENT.into()),
            registered: Vec::new(),
            development: None,
        };
        let metadata = format!("{SERVER}/.well-known/oauth-protected-resource/mcp");
        let service = smol::block_on(discover(clients, metadata, identities)).unwrap();
        let keychain = Arc::new(MemoryKeychain::default());
        let source = OAuthSource::kept(
            service,
            "work",
            keychain.clone(),
            "user_1".into(),
            "kept".into(),
        );
        let rotated = || {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|line| line.starts_with("token refresh_token"))
        };
        let dropped = smol::block_on(smol::future::or(
            async { Some(source.token(Arc::default()).await) },
            async {
                while !rotated() {
                    smol::Timer::after(Duration::from_millis(10)).await;
                }
                None
            },
        ));
        assert!(dropped.is_none(), "the ask was dropped mid-exchange");
        served.lock().unwrap().hold_token = false;
        let token = smol::block_on(source.token(Arc::default()));
        assert!(token.is_ok(), "{token:?}");
        assert_eq!(
            served.lock().unwrap().issued,
            1,
            "the service was asked once"
        );
        let key = format!("omnigraph-studio-oauth:work#{ISSUER}#{RESOURCE}");
        assert_eq!(keychain.items.lock().unwrap()[&key].1, b"r1".to_vec());
    }

    #[gpui::test]
    async fn a_sign_in_the_service_ended_or_the_server_does_not_admit_is_said(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        // A kept refresh token the service no longer honours: signed out, and forgotten.
        let served = Arc::new(Mutex::new(Served {
            lives: 300,
            ..Served::default()
        }));
        let keychain = Arc::new(MemoryKeychain::default());
        let key = format!("omnigraph-studio-oauth:work#{ISSUER}#{RESOURCE}");
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key.clone(), ("user_1".into(), b"stale".to_vec()));
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the failed refresh", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: Some(_), .. }
            )
        });
        let (state, offer, label) = sign_in_state(&connection, cx);
        assert_eq!(state, ConnectionState::Unauthorized);
        assert_eq!(label, "Sign in needed");
        let SignIn::Offered { why: Some(why), .. } = offer else {
            unreachable!()
        };
        assert!(
            why.starts_with("Signed out: The grant is not valid."),
            "{why}"
        );
        assert!(keychain.items.lock().unwrap().is_empty());

        // A fresh sign-in the server refuses, and refuses again after one refresh. Its tokens
        // live two seconds, and the server is watched, so a renewal left running would refresh
        // again after one.
        let _watch = connection.update(cx, |c, cx| c.watch(cx));
        served.lock().unwrap().admits_nobody = true;
        served.lock().unwrap().lives = 2;
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the refusal", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::SignedIn { .. })
                && sign_in_state(&connection, cx).0 == ConnectionState::Unauthorized
                && served.lock().unwrap().issued == 2
        });
        browser.join().unwrap();
        cx.run_until_parked();
        assert_eq!(sign_in_state(&connection, cx).2, "Not admitted");
        std::thread::sleep(Duration::from_millis(1200));
        cx.executor().advance_clock(Duration::from_secs(3));
        for _ in 0..20 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(sign_in_state(&connection, cx).2, "Not admitted");
        let served = served.lock().unwrap();
        assert_eq!(served.issued, 2, "refreshed once, not in a loop");
        // The refreshed token was sent, and refused too.
        let refused: HashSet<&String> = served.refused.iter().collect();
        assert_eq!(refused.len(), 2, "{:?}", served.refused);
    }

    #[gpui::test]
    async fn a_server_given_a_token_by_the_cli_is_not_signed_in_to(cx: &mut TestAppContext) {
        struct Refused;
        impl TokenSource for Refused {
            fn token(&self, _: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>> {
                futures::future::ready(Ok(Token::typed("from-the-cli".into()))).boxed()
            }
        }
        let served = Arc::new(Mutex::new(Served {
            lives: 300,
            ..Served::default()
        }));
        let connection = connection(&served, Arc::default(), cx);
        connection.update(cx, |c, cx| {
            *c = std::mem::replace(
                c,
                Connection::new(c.server.clone(), None, fake(served.clone())).unwrap(),
            )
            .with_token_source(Some(Arc::new(Refused)));
            c.connect(cx);
        });
        cx.run_until_parked();
        let (state, offer, label) = sign_in_state(&connection, cx);
        assert_eq!(state, ConnectionState::Unauthorized);
        assert_eq!(offer, SignIn::NotOffered);
        assert_eq!(label, "Token refused");
    }

    /// One request as a browser sends it, without following a redirect: the `location` of a
    /// redirect, else the page.
    fn browse(url: &str) -> String {
        use std::io::{Read as _, Write as _};
        let url = url::Url::parse(url).unwrap();
        let mut stream =
            std::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).unwrap();
        let path = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n\r\n",
            url.host_str().unwrap()
        )
        .unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
            .lines()
            .find_map(|line| {
                line.strip_prefix("location: ")
                    .or_else(|| line.strip_prefix("Location: "))
            })
            .map(str::to_owned)
            .unwrap_or(answer)
    }

    /// The sign-in against a real Omnigraph server: `script/dev-sign-in` serves one of v0.14.0
    /// that trusts the bed's sign-in service, behind TLS from the bed's own certificate
    /// authority, which the CLI target names (`transport.tls.ca`).
    #[gpui::test]
    #[ignore = "needs the sign-in bed: run script/dev-sign-in, set STUDIO_SIGN_IN_E2E_DATA to its data directory"]
    async fn a_real_server_is_signed_in_to_through_the_bed(cx: &mut TestAppContext) {
        let data = std::path::PathBuf::from(
            std::env::var("STUDIO_SIGN_IN_E2E_DATA").expect("set STUDIO_SIGN_IN_E2E_DATA"),
        );
        let endpoint = std::env::var("STUDIO_SIGN_IN_E2E_SERVER")
            .unwrap_or_else(|_| "https://127.0.0.1:8791".into());
        cx.executor().allow_parking();
        let ca = data.join("keys/ca.pem");
        let transport = crate::cli_config::CliTransport {
            proxy: None,
            roots: Some(crate::cli_config::CliRoots {
                replace: false,
                bundle: std::fs::read(&ca).unwrap(),
                file: ca,
            }),
        };
        let http = crate::registry::transport_client(&transport).unwrap();
        let control = |subject: &str| {
            let request = http_client::http::Request::post(format!("{endpoint}/oauth/_bed"))
                .body(AsyncBody::from(json!({"subject": subject}).to_string()))
                .unwrap();
            assert_eq!(smol::block_on(http.send(request)).unwrap().status(), 200);
        };
        control("user_alice");
        let elsewhere: Arc<dyn HttpClient> =
            Arc::new(reqwest_client::ReqwestClient::user_agent(crate::USER_AGENT).unwrap());
        let keychain = Arc::new(MemoryKeychain::default());
        let context = SignInContext {
            elsewhere,
            keychain: keychain.clone(),
            identities: ClientIdentities {
                metadata_document: Some(CLIENT.into()),
                registered: Vec::new(),
                development: None,
            },
        };
        let server = ServerEntry {
            name: "oauth-bed".into(),
            url: endpoint.clone(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| {
            Connection::new(server, None, http.clone())
                .unwrap()
                .with_sign_in(Some(context))
        });
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the offer", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
        });
        assert_eq!(sign_in_state(&connection, cx).2, "Sign in needed");

        connection.update(cx, |c, cx| c.sign_in(cx));
        let opened = cx.opened_url().unwrap();
        let back = browse(&opened);
        assert!(
            back.starts_with("http://127.0.0.1:"),
            "the sign-in page answered: {back}"
        );
        let page = std::thread::spawn(move || browse(&back));
        until(cx, "the sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        assert!(page.join().unwrap().contains("Signed in to 127.0.0.1:"));
        let (_, signed_in, _) = sign_in_state(&connection, cx);
        assert!(
            matches!(&signed_in, SignIn::SignedIn { account, kept: true, .. } if account == "user_alice"),
            "{signed_in:?}"
        );
        let graphs = connection.read_with(cx, |c, _| c.graph_ids());
        assert!(graphs.contains(&"knowledge".to_owned()), "{graphs:?}");

        // An account the server's admission does not name is refused after one refresh.
        connection.update(cx, |c, cx| c.sign_out(cx));
        until(cx, "the sign-out", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
        });
        assert!(keychain.items.lock().unwrap().is_empty());
        control("user_mallory");
        connection.update(cx, |c, cx| c.sign_in(cx));
        let back = browse(&cx.opened_url().unwrap());
        let page = std::thread::spawn(move || browse(&back));
        until(cx, "the refusal", |cx| {
            sign_in_state(&connection, cx).2 == "Not admitted"
        });
        page.join().unwrap();
        control("user_alice");
    }
}
