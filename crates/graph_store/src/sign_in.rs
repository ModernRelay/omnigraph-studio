//! Signing in to a server through OAuth (spec 016): the sign-in service a server's 401 names,
//! the keychain that keeps a refresh token, and the token source that refreshes from it.
//!
//! A connection signs in only to a server whose 401 offers it (`resource_metadata`) and for which
//! it holds no other credential: a CLI target with `auth: anonymous`, or a managed one, whose CLI
//! login is held back until the server's 401 says it offers no sign-in, and is then used instead.
//! The browser opens only when the person chooses Sign in; a kept sign-in is refreshed without
//! asking.

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
pub use oauth_client::Refusal;
use oauth_client::{
    Account, AuthorizationServer, ClientIdentities, Clients, Expected, ProtectedResource,
    TokenError, Tokens,
};

use crate::cli_tokens::{Lease, NoToken, Token, TokenSource};

/// The URL of Studio's Client ID Metadata Document, served by omnigraph.dev (spec 016, Client
/// identity). It is Studio's client ID at every sign-in service that takes metadata documents:
/// another URL is another client, and everyone signed in would sign in again.
const METADATA_DOCUMENT: Option<&str> =
    Some("https://www.omnigraph.dev/oauth/studio-client-metadata.json");

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
    /// Deletes the item at `key`. macOS and Windows refuse to delete an item that is not
    /// there; [`forget`] takes that as done.
    fn delete(&self, key: String) -> BoxFuture<'static, Result<(), String>>;
}

/// Deletes `key`'s item, and says what the keychain holds there after: nothing once the next
/// launch would read none, so a key that held nothing is no refusal, and neither is one the
/// keychain cannot read: a launch takes such a read as nothing kept, as where no keychain is
/// reachable at all (Linux with no Secret Service). A keychain unreadable only for now (macOS,
/// locked) can then keep an item unsaid, which the next launch reads. Otherwise the item it
/// would not delete, with its refusal.
pub(crate) async fn forgotten(keychain: &dyn Keychain, key: String) -> Kept {
    let Err(refused) = keychain.delete(key.clone()).await else {
        return Kept::Nothing;
    };
    match keychain.read(key).await {
        Ok(Some(_)) => Kept::Left(refused),
        _ => Kept::Nothing,
    }
}

/// What is said of a keychain item left at a target, `refused` being the keychain's refusal to
/// delete it: Studio reads it again when the server next offers a sign-in.
pub fn left_in_keychain(refused: &str) -> String {
    format!(
        "The keychain kept a sign-in it would not delete ({refused}), which Studio may read \
         again later."
    )
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

/// A keychain in memory, for tests: what it holds can be read back. Unlike the platform's, which
/// answers one request at a time in order and finishes one whose caller has gone, a request
/// held at a gate lets later ones by, and one dropped does not land; tests order work through
/// the keychain turns.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct MemoryKeychain {
    pub items: Arc<Mutex<HashMap<String, KeychainItem>>>,
    /// Writes are refused, as a keychain that will not keep the sign-in.
    pub refuses_writes: AtomicBool,
    /// Deletes are refused, as a keychain that will not let go of it.
    pub refuses_deletes: AtomicBool,
    /// Reads fail, as a keychain that cannot be reached.
    pub fails_reads: AtomicBool,
    /// Writes asked for wait for this, as a keychain slow to answer.
    pub write_gate: Mutex<Option<Shared<oneshot::Receiver<()>>>>,
    /// Deletes asked for wait for this, as a keychain slow to answer.
    pub delete_gate: Mutex<Option<Shared<oneshot::Receiver<()>>>>,
    /// How many times it was read.
    pub reads: std::sync::atomic::AtomicUsize,
    /// How many writes were asked of it.
    pub writes: std::sync::atomic::AtomicUsize,
}

#[cfg(any(test, feature = "test-support"))]
impl Keychain for MemoryKeychain {
    fn read(&self, key: String) -> KeychainRead {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fails_reads.load(Ordering::SeqCst) {
            return futures::future::ready(Err("the keychain could not be read".to_owned()))
                .boxed();
        }
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
        self.writes.fetch_add(1, Ordering::SeqCst);
        let refused = self.refuses_writes.load(Ordering::SeqCst);
        let gate = self
            .write_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let items = self.items.clone();
        async move {
            if let Some(gate) = gate {
                gate.await.ok();
            }
            if refused {
                return Err("the keychain refused".to_owned());
            }
            items
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key, (user, secret));
            Ok(())
        }
        .boxed()
    }

    /// As macOS and Windows do, an item that is not there is not deleted.
    fn delete(&self, key: String) -> BoxFuture<'static, Result<(), String>> {
        let refused = self.refuses_deletes.load(Ordering::SeqCst);
        let gate = self
            .delete_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let items = self.items.clone();
        async move {
            if let Some(gate) = gate {
                gate.await.ok();
            }
            if refused {
                return Err("the keychain refused".to_owned());
            }
            match items
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&key)
            {
                Some(_) => Ok(()),
                None => Err("no such item".to_owned()),
            }
        }
        .boxed()
    }
}

/// What a connection needs to sign in: Studio's own client, for a sign-in service on another
/// host than the server's; the keychain, and the turns its items' work takes; and Studio's
/// client identity.
#[derive(Clone)]
pub struct SignInContext {
    pub elsewhere: Arc<dyn HttpClient>,
    pub keychain: Arc<dyn Keychain>,
    pub turns: KeychainTurns,
    pub identities: ClientIdentities,
}

/// The keychain work at each item in turn, shared by every connection that reaches it (one
/// retired and the one that replaces it for the same target among them), so none overtakes
/// another: a sign-in's write or delete, an ending, a launch's read (spec 016, Custody).
#[derive(Clone, Default)]
pub struct KeychainTurns {
    ends: Arc<Mutex<HashMap<String, KeychainTurn>>>,
}

impl KeychainTurns {
    /// The next turn at `key`'s item: the work to wait for first (the end of its queue until
    /// now), and what to send once this turn's work is done. The queue's new end waits for both,
    /// so a turn given up still holds the next behind the one before it.
    pub(crate) fn next(&self, key: &str) -> (Option<KeychainTurn>, oneshot::Sender<()>) {
        let (done, own) = oneshot::channel::<()>();
        let mut ends = self.ends.lock().unwrap_or_else(PoisonError::into_inner);
        let before = ends.remove(key);
        let after = before.clone();
        ends.insert(
            key.to_owned(),
            async move {
                if let Some(after) = after {
                    after.await;
                }
                own.await.ok();
            }
            .boxed()
            .shared(),
        );
        (before, done)
    }
}

/// A server's sign-in service as Studio found and checked it.
#[derive(Clone)]
pub struct Service {
    pub resource: ProtectedResource,
    pub server: AuthorizationServer,
    pub client_id: String,
    /// The organization the target names, which a token's `org_id` must equal: a managed
    /// target's profile's; `None` for a target that names none, and the server checks it alone.
    pub organization: Option<String>,
    pub(crate) clients: Clients,
}

impl Service {
    /// Revokes `refresh` where the service publishes a revocation endpoint: once, within 10
    /// seconds, and not retried, in the background, so nobody waits on the network (spec 016,
    /// Custody). The sign-in ends whatever the service answers.
    pub(crate) fn revoke(&self, refresh: String) {
        let http = self.token_client();
        let service = self.clone();
        smol::spawn(async move {
            oauth_client::revoke(http.as_ref(), &service.server, &service.client_id, &refresh)
                .await
                .ok();
        })
        .detach();
    }

    /// The client its token and revocation requests take.
    pub(crate) fn token_client(&self) -> Arc<dyn HttpClient> {
        self.clients.client_for(&self.server.token_endpoint)
    }

    /// The sign-in service's host, as the person reads it.
    pub fn host(&self) -> String {
        oauth_client::host_of(&self.server.issuer)
    }

    pub(crate) fn expected(&self) -> Expected<'_> {
        Expected {
            issuer: &self.server.issuer,
            resource: &self.resource.resource,
            client_id: &self.client_id,
            organization: self.organization.as_deref(),
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
/// Studio uses there; a token from it must be in `organization` when the target names one.
pub(crate) async fn discover(
    clients: Clients,
    metadata_url: String,
    identities: ClientIdentities,
    organization: Option<String>,
) -> Result<Service, Refusal> {
    let resource = oauth_client::protected_resource(&clients, &metadata_url).await?;
    let server = oauth_client::authorization_server(&clients, &resource.issuer).await?;
    let client_id = identities.client_id(&server)?;
    Ok(Service {
        resource,
        server,
        client_id,
        organization,
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
    /// The server offers a sign-in Studio cannot use, and why. Details that could not be read
    /// now ([`Refusal::passing`]) are read again at each health check; any are on Retry.
    Unavailable(Refusal),
    /// The browser is open at `service`.
    Waiting { service: String },
    /// The service gave the sign-in its tokens, and Studio is keeping them: no longer to be
    /// cancelled, as the sign-in it replaces is ending. A sign-out or a drop meanwhile shows
    /// nothing offered, and ends it once kept, as one held.
    Finishing { service: String },
    /// Signed in at `service` as `account`. What the keychain holds for the target is the
    /// connection's to say ([`crate::connection::Connection::keychain`]).
    SignedIn { service: String, account: String },
}

/// What the keychain holds for a target, which the next launch reads (spec 016, Custody).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// The sign-in held, or, with none held, one a sign-in that ended left for the next launch
    /// (a drop keeps it for the target made anonymous again).
    This,
    /// Nothing: a sign-in held lasts until Studio quits.
    Nothing,
    /// An item the keychain would not delete (an earlier sign-in's, or this account's token from
    /// before), which Studio may read again later: the keychain's refusal.
    Left(String),
}

/// Why a sign-in ends, which decides what becomes of its keychain item and its refresh token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// The person signed out: the item is deleted and the refresh token revoked.
    SignOut,
    /// Another sign-in for the target replaces it: the item is left to that one, and the
    /// refresh token revoked.
    Replaced,
    /// The connection no longer takes its token from it (the CLI's credential, a retired
    /// connection, a renamed target, the service ending it): the item stays, given a token a refresh rotated
    /// meanwhile (forgotten if the keychain will not take it, or if the refresh found it spent),
    /// and the refresh token is revoked unless the keychain keeps it for the next launch.
    Dropped,
}

/// One turn's wait at a keychain item ([`KeychainTurns`]): the work before it, done.
pub(crate) type KeychainTurn = Shared<BoxFuture<'static, ()>>;

/// A refresh's answer, its token, which every ask waiting for it shares.
type Refreshing = Shared<oneshot::Receiver<Result<Token, NoToken>>>;

/// A sign-in's refreshes: the one under way, which every ask joins without waiting for the
/// tokens it holds, and how many have ended, with the last one's failure.
#[derive(Default)]
struct Refreshes {
    running: Option<Refreshing>,
    ended: u64,
    failed: Option<NoToken>,
}

/// The tokens of one sign-in: the access token, refreshed from the refresh token the keychain
/// keeps (spec 016, Refresh). One refresh runs at a time; others wait for its answer.
#[derive(Clone)]
pub(crate) struct OAuthSource {
    inner: Arc<Inner>,
}

struct Inner {
    /// Its sign-in service, whose clients its requests take: the server's transport for the
    /// server's address, which a changed target replaces for every holder
    /// ([`graph_client::GraphClient::transport`]), and Studio's own client elsewhere.
    service: Service,
    key: String,
    keychain: Arc<dyn Keychain>,
    held: smol::lock::Mutex<Held>,
    account: Mutex<Account>,
    /// What the keychain holds for the target.
    kept: Mutex<Kept>,
    /// The server refused the access token held: the next ask refreshes.
    refused: AtomicBool,
    /// The sign-in ended ([`OAuthSource::end`], or the service ended its grant): no refresh
    /// begins, and one under way runs to its end, its retry included, touching the keychain no
    /// more.
    ended: AtomicBool,
    /// Its ending, once begun: a second ending answers with the first's.
    ending: Mutex<Option<Shared<BoxFuture<'static, Kept>>>>,
    /// Its refreshes. One runs on a task of its own to its end, so an ask dropped mid-exchange
    /// (a connect replaced, a renewal overtaken) cannot lose the refresh token the service has
    /// rotated by then.
    refreshes: Mutex<Refreshes>,
    /// The token of an earlier item the keychain kept at the target, which this sign-in does not
    /// hold (`Kept::Left`): revoked once the item no longer holds it ([`Inner::keep`]).
    left: Mutex<Option<String>>,
    /// Held across each keychain operation a refresh makes, which it begins only while the
    /// sign-in stands: an ending, with `ended` set, waits on it for one in progress, never for
    /// the refresh's request, and none follows.
    writing: smol::lock::Mutex<()>,
}

struct Held {
    access: Option<Access>,
    refresh: Option<String>,
    /// What a refresh answered after the sign-in ended learned of the keychain's item, which it
    /// leaves to the ending ([`OAuthSource::end`]).
    stale: Option<Stale>,
}

/// What a refresh answered after its sign-in ended learned of the keychain's item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stale {
    /// The token held is a rotation the keychain was not given.
    Rotated,
    /// The item's token no longer refreshes: the service ended the grant, or rotated it away
    /// with tokens Studio refuses.
    Spent,
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
    /// A sign-in just made, its tokens and what the keychain holds for its target, with the
    /// token of an earlier item there the keychain would not let go of (`left`).
    pub(crate) fn signed_in(
        service: Service,
        target: &str,
        keychain: Arc<dyn Keychain>,
        tokens: Tokens,
        kept: Kept,
        left: Option<String>,
    ) -> Self {
        let source = Self::new(
            service,
            target,
            keychain,
            Held {
                access: Some(Access::new(tokens.access_token, tokens.expires_at)),
                refresh: tokens.refresh_token,
                stale: None,
            },
            tokens.account,
            kept,
        );
        *source
            .inner
            .left
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = left;
        source
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
                stale: None,
            },
            Account {
                subject,
                organization: None,
            },
            Kept::This,
        )
    }

    fn new(
        service: Service,
        target: &str,
        keychain: Arc<dyn Keychain>,
        held: Held,
        account: Account,
        kept: Kept,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                key: service.key(target),
                service,
                keychain,
                held: smol::lock::Mutex::new(held),
                account: Mutex::new(account),
                kept: Mutex::new(kept),
                refused: AtomicBool::new(false),
                ended: AtomicBool::new(false),
                ending: Mutex::new(None),
                refreshes: Mutex::new(Refreshes::default()),
                left: Mutex::new(None),
                writing: smol::lock::Mutex::new(()),
            }),
        }
    }

    /// Its sign-in service's host, as the person reads it.
    pub(crate) fn host(&self) -> String {
        self.inner.service.host()
    }

    /// The key a sign-in to its service at `target` is kept under ([`Service::key`]).
    pub(crate) fn service_key(&self, target: &str) -> String {
        self.inner.service.key(target)
    }

    pub(crate) fn account(&self) -> String {
        self.inner.subject()
    }

    /// The key of its keychain item.
    pub(crate) fn key(&self) -> String {
        self.inner.key()
    }

    pub(crate) fn kept_in_keychain(&self) -> Kept {
        self.inner.kept()
    }

    /// Revokes `token`, an earlier item's the keychain let go of, through the server's transport
    /// as it is now for a service at the server's address, Studio's own client elsewhere.
    pub(crate) fn revoke(&self, token: String) {
        self.inner.service.revoke(token);
    }

    /// Ends the sign-in for `how`. No refresh begins after this, and one under way touches the
    /// keychain no more but holds what it is given. Once `before` (the keychain work ahead of
    /// this at its item) has settled, and a keychain write a refresh has in progress, the item is
    /// dealt with as `how` says; the refresh token is revoked after, in the background (spec 016,
    /// Custody). It runs on its own once begun, resolves to what the keychain holds for the
    /// target then, and ends the sign-in once: a second ending answers with the first's.
    pub(crate) fn end(&self, how: End, before: Option<KeychainTurn>) -> BoxFuture<'static, Kept> {
        let mut ending = self
            .inner
            .ending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(ending) = ending.as_ref() {
            return ending.clone().boxed();
        }
        let inner = self.inner.clone();
        inner.ended.store(true, Ordering::SeqCst);
        let (done, ended) = oneshot::channel();
        smol::spawn(async move {
            if let Some(before) = before {
                before.await;
            }
            // Only a drop waits for a refresh in flight (up to its request's 15 seconds and a
            // retry), to keep the token it rotates or forget one it learns is spent; a sign-out
            // deletes the item without waiting, so Studio quitting meanwhile leaves the next
            // launch nothing to sign in with.
            let settled = match how {
                End::Dropped => Some(inner.settle().await),
                End::SignOut | End::Replaced => None,
            };
            let writing = inner.writing.lock().await;
            let was = inner.kept();
            let kept = match (how, &was, &settled) {
                (End::SignOut, ..) => inner.forget().await,
                // Kept for the target made anonymous again, a token a refresh rotated after the
                // drop began included; one the keychain will not take is forgotten instead.
                (End::Dropped, Kept::This, Some((Some(refresh), Some(Stale::Rotated)))) => {
                    written_or_forgotten(
                        inner.keychain.as_ref(),
                        inner.key(),
                        inner.subject(),
                        Some(refresh),
                    )
                    .await
                }
                // Its token no longer refreshes: forgotten, as the refresh would have before the
                // drop.
                (End::Dropped, Kept::This, Some((_, Some(Stale::Spent)))) => inner.forget().await,
                // A replaced sign-in's item is the replacing sign-in's to answer for.
                (End::Dropped | End::Replaced, ..) => was,
            };
            inner.keep(kept.clone());
            drop(writing);
            // A sign-in dropped that the keychain keeps is the next launch's to use.
            let keeps_token = how == End::Dropped && kept == Kept::This;
            done.send(kept).ok();
            // The token to revoke is the one held once a refresh in flight has settled.
            let (refresh, _) = match settled {
                Some(settled) => settled,
                None => inner.settle().await,
            };
            if !keeps_token && let Some(refresh) = refresh {
                inner.service.revoke(refresh);
            }
        })
        .detach();
        let shared = async move { ended.await.unwrap_or(Kept::Nothing) }
            .boxed()
            .shared();
        *ending = Some(shared.clone());
        shared.boxed()
    }
}

/// Writes `refresh` for `subject` at `key`, or, with none given or one the keychain will not take,
/// forgets the item there ([`forgotten`]): what the keychain then holds for the target.
pub(crate) async fn written_or_forgotten(
    keychain: &dyn Keychain,
    key: String,
    subject: String,
    refresh: Option<&str>,
) -> Kept {
    if let Some(refresh) = refresh
        && keychain
            .write(key.clone(), subject, refresh.as_bytes().to_vec())
            .await
            .is_ok()
    {
        return Kept::This;
    }
    forgotten(keychain, key).await
}

/// A refresh token the service gave that no sign-in holds yet: dropped unclaimed (the sign-in
/// cancelled as the service answered, or the connection gone before it finished), it is revoked,
/// so no token the service honours is left with nobody holding it.
pub(crate) struct Unclaimed {
    service: Service,
    refresh: Option<String>,
}

impl Unclaimed {
    pub(crate) fn new(service: Service, refresh: Option<String>) -> Self {
        Self { service, refresh }
    }

    /// A sign-in holds the token now, and answers for it.
    pub(crate) fn claim(mut self) {
        self.refresh = None;
    }
}

impl Drop for Unclaimed {
    fn drop(&mut self) {
        if let Some(refresh) = self.refresh.take() {
            self.service.revoke(refresh);
        }
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
    fn key(&self) -> String {
        self.key.clone()
    }

    /// What the keychain holds for the target, as this sign-in last said it.
    fn kept(&self) -> Kept {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn subject(&self) -> String {
        self.account
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .subject
            .clone()
    }

    /// Deletes its keychain item ([`forgotten`]).
    async fn forget(&self) -> Kept {
        forgotten(self.keychain.as_ref(), self.key()).await
    }

    /// The refresh token held once a refresh in flight has settled, taken for the ending, and
    /// what that refresh learned of the keychain's item after the sign-in ended.
    async fn settle(&self) -> (Option<String>, Option<Stale>) {
        let mut held = self.held.lock().await;
        held.access = None;
        (held.refresh.take(), held.stale.take())
    }

    /// The access token held while it is fresh; otherwise the one the refresh under way gives,
    /// starting it when none is. The refresh is not this ask's: dropped, the ask leaves it running.
    /// A refresh that did not end may pass; a sign-in that ended does not.
    async fn token(self: Arc<Self>) -> Result<Token, NoToken> {
        let (under_way, seen) = {
            let refreshes = self
                .refreshes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (refreshes.running.clone(), refreshes.ended)
        };
        let running = match under_way {
            // Its answer is a new token, whatever the server refused.
            Some(running) => {
                self.refused.store(false, Ordering::SeqCst);
                running
            }
            None => {
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
                    // Nothing to refresh with: the token held stands until it ends, its renewal
                    // due then, which ends the sign-in, not while the token still holds.
                    if !refused
                        && let Some(access) = &mut held.access
                        && SystemTime::now() < access.expires_at
                    {
                        access.renew_at = access.expires_at;
                        return Ok(self.leased(access));
                    }
                    held.access = None;
                    return Err(NoToken::lasting("The sign-in has ended. Sign in again."));
                }
                let mut refreshes = self
                    .refreshes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                match (&refreshes.running, &refreshes.failed) {
                    // Begun while this waited for the tokens.
                    (Some(under_way), _) => under_way.clone(),
                    // One ended while this waited, and failed: its answer, not another refresh.
                    (None, Some(failed)) if refreshes.ended != seen => return Err(failed.clone()),
                    (None, _) => {
                        let (done, answer) = oneshot::channel();
                        let answer = answer.shared();
                        refreshes.running = Some(answer.clone());
                        let inner = self.clone();
                        smol::spawn(async move {
                            let refreshed = inner.refresh().await;
                            done.send(refreshed).ok();
                        })
                        .detach();
                        answer
                    }
                }
            }
        };
        match running.await {
            // Ended since the refresh answered: its token is not handed out.
            Ok(Ok(_)) if self.ended.load(Ordering::SeqCst) => Err(NoToken::lasting("Signed out.")),
            Ok(answer) => answer,
            Err(_) => Err(NoToken::passing(
                "The sign-in's refresh stopped before it answered.",
            )),
        }
    }

    /// Refreshes the access token from the refresh token held, keeping what the service gives.
    async fn refresh(&self) -> Result<Token, NoToken> {
        let mut held = self.held.lock().await;
        let refreshed = self.refresh_held(&mut held).await.and_then(|()| {
            held.access
                .as_ref()
                .map(|access| self.leased(access))
                .ok_or_else(|| NoToken::lasting("The sign-in has ended. Sign in again."))
        });
        // Over while the tokens are still held, so an ask after it finds what it left.
        let mut refreshes = self
            .refreshes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        refreshes.running = None;
        refreshes.ended += 1;
        refreshes.failed = refreshed.as_ref().err().cloned();
        refreshed
    }

    /// Says what the keychain holds for the target now. An earlier item's token this sign-in
    /// kept, as the keychain would not let go of it, is revoked once the item no longer holds
    /// it, which nothing else then does (spec 016, Custody).
    fn keep(&self, kept: Kept) {
        let let_go = !matches!(kept, Kept::Left(_));
        *self.kept.lock().unwrap_or_else(PoisonError::into_inner) = kept;
        if let_go
            && let Some(token) = self
                .left
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            self.service.revoke(token);
        }
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
        let http = service.token_client();
        let mut failed_once = false;
        loop {
            let answered =
                oauth_client::refresh(http.as_ref(), &service.server, &refresh, service.expected())
                    .await;
            // Keychain work only while the sign-in stands, decided under the lock its ending
            // takes, so none lands after the ending's.
            let writing = self.writing.lock().await;
            let ended = self.ended.load(Ordering::SeqCst);
            match answered {
                // Ended while the service answered: the keychain is not written, so the ending
                // is the last it hears of this sign-in; the rotated token is held for the ending,
                // which revokes it or, for a drop, gives it to the keychain, as the one it
                // replaced no longer counts.
                Ok(tokens) if ended => {
                    if let Some(rotated) = tokens.refresh_token.filter(|new| *new != refresh) {
                        held.refresh = Some(rotated);
                        held.stale = Some(Stale::Rotated);
                    }
                    return Err(NoToken::lasting("Signed out."));
                }
                Ok(tokens) => {
                    let subject = tokens.account.subject.clone();
                    if let Some(rotated) =
                        tokens.refresh_token.as_ref().filter(|new| **new != refresh)
                    {
                        // The rotated token replaces the kept one before the new access token
                        // is used: the old one no longer refreshes.
                        let written = self
                            .keychain
                            .write(self.key(), subject.clone(), rotated.as_bytes().to_vec())
                            .await
                            .is_ok();
                        let kept = match (written, self.kept()) {
                            (true, _) => Kept::This,
                            // What it holds is this account's token from before, which no longer
                            // refreshes, or which a service that keeps old tokens honoured would
                            // sign the next launch in with: forgotten, as a sign-in that keeps
                            // nothing forgets, and said when the keychain will not let go.
                            (false, Kept::This) => self.forget().await,
                            // An earlier sign-in it holds instead stays said.
                            (false, was) => was,
                        };
                        self.keep(kept);
                        held.refresh = Some(rotated.clone());
                    }
                    held.access = Some(Access::new(tokens.access_token, tokens.expires_at));
                    *self.account.lock().unwrap_or_else(PoisonError::into_inner) = tokens.account;
                    return Ok(());
                }
                // The service ended the grant. A sign-in ended already leaves the item to its
                // ending, which may have given it to the sign-in replacing it.
                Err(TokenError::InvalidGrant(why)) => {
                    held.access = None;
                    held.refresh = None;
                    if ended {
                        held.stale = Some(Stale::Spent);
                    } else {
                        self.ended.store(true, Ordering::SeqCst);
                        let kept = self.forget().await;
                        self.keep(kept);
                    }
                    return Err(NoToken::lasting(format!(
                        "Signed out: {why} Sign in again."
                    )));
                }
                Err(TokenError::Refused(why)) => {
                    held.access = None;
                    return Err(NoToken::lasting(why));
                }
                // Tokens Studio may not use. A refresh token the service rotated with them is
                // held by nothing, so it is revoked, and the one from before, which no longer
                // refreshes, is forgotten as a refused rotation's is (unless the sign-in ended,
                // leaving the item to its ending).
                Err(TokenError::Unusable { why, refresh_token }) => {
                    held.access = None;
                    if let Some(rotated) = refresh_token.filter(|new| *new != refresh) {
                        self.service.revoke(rotated);
                        // The token held is spent by that rotation: nothing to revoke later.
                        held.refresh = None;
                        if ended {
                            held.stale = Some(Stale::Spent);
                        } else if self.kept() == Kept::This {
                            let kept = self.forget().await;
                            self.keep(kept);
                        }
                    }
                    return Err(NoToken::lasting(why));
                }
                // A refresh that failed may have rotated the token anyway: once more, after a
                // pause, whether or not the sign-in ended meanwhile, as its answer may hold the
                // token the failed request rotated, which the ending then revokes or keeps. Then
                // the sign-in stands, and the connection asks again later (spec 016, Refresh).
                Err(TokenError::Failed(_)) if !failed_once => {
                    drop(writing);
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
        let holder = self.subject();
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
    use crate::connection::{
        Connection, ConnectionState, PromptAction, ServerEntry, ServerSource, SignInPrompt,
    };

    const SERVER: &str = "https://graphs.test";
    const RESOURCE: &str = "https://graphs.test/mcp";
    const ISSUER: &str = "https://id.test";
    const CLIENT: &str = "https://studio.test/client.json";

    /// The keychain key of a sign-in to the fake service for the target `target`.
    fn key(target: &str) -> String {
        format!("omnigraph-studio-oauth:{target}#{ISSUER}#{RESOURCE}")
    }

    /// Studio's client identity at the fake service.
    fn identities() -> ClientIdentities {
        ClientIdentities {
            metadata_document: Some(CLIENT.into()),
            registered: Vec::new(),
            development: None,
        }
    }

    /// The CLI's credential for the target: a token typed for it.
    struct Cli;

    impl TokenSource for Cli {
        fn token(&self, _: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>> {
            futures::future::ready(Ok(Token::typed("from-the-cli".into()))).boxed()
        }
    }

    /// A server that takes the access tokens its sign-in service issued, and that service:
    /// codes and refresh tokens are single-use, and each refresh rotates. Its tokens live five
    /// minutes unless a test says otherwise.
    struct Served {
        accepted: HashSet<String>,
        codes: HashSet<String>,
        refresh: HashSet<String>,
        issued: u32,
        /// The server refuses every token, as one that does not admit the account.
        admits_nobody: bool,
        /// The server's 401 names no sign-in details, as a managed cluster's does before its
        /// sign-in is enabled.
        offers_no_sign_in: bool,
        /// How long each access token lives.
        lives: u64,
        /// The sign-in service gives no refresh token.
        no_refresh: bool,
        /// The sign-in service's issuer, `ISSUER` when empty: one on the server's own origin
        /// takes the target's client for its token and revocation requests.
        issuer: String,
        /// The server holds its sign-in details' answer until this is lowered.
        hold_metadata: bool,
        /// The server fails this many more requests for its sign-in details (503).
        fails_metadata: u32,
        /// The sign-in service holds a token answer, made already (a refresh token rotated),
        /// until this is lowered.
        hold_token: bool,
        /// The sign-in service holds its answer to a refresh alone, as `hold_token` does.
        hold_refresh: bool,
        /// The sign-in service fails every token request (503) and keeps its grants.
        token_down: bool,
        /// The sign-in service fails this many more token requests, keeping its grants: 503,
        /// or `fails_status` when set.
        fails_tokens: u32,
        fails_status: u16,
        fails_error: &'static str,
        /// The sign-in service refuses every refresh for good (`invalid_target`) and keeps its
        /// grants.
        refuses_target: bool,
        /// The tokens the server refused, in order.
        refused: Vec<String>,
        /// Each request, after the name of the client it went through.
        requests: Vec<String>,
        log: Vec<String>,
    }

    impl Default for Served {
        fn default() -> Self {
            Self {
                accepted: Default::default(),
                codes: Default::default(),
                refresh: Default::default(),
                issued: Default::default(),
                lives: 300,
                admits_nobody: Default::default(),
                offers_no_sign_in: Default::default(),
                no_refresh: Default::default(),
                issuer: Default::default(),
                hold_metadata: Default::default(),
                fails_metadata: Default::default(),
                hold_token: Default::default(),
                hold_refresh: Default::default(),
                token_down: Default::default(),
                fails_tokens: Default::default(),
                fails_status: Default::default(),
                fails_error: Default::default(),
                refuses_target: Default::default(),
                refused: Default::default(),
                requests: Default::default(),
                log: Default::default(),
            }
        }
    }

    /// A keychain holding the sign-in `user_1` kept for the target `work`, whose grant `served`
    /// honours: the launch reads it and refreshes.
    fn kept_at_launch(served: &Arc<Mutex<Served>>) -> Arc<MemoryKeychain> {
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key("work"), ("user_1".into(), b"kept".to_vec()));
        keychain
    }

    /// Signs `connection` in through the browser, its keychain write held: the sign-in is
    /// finishing when this returns, and sending on the returned sender lets the write land.
    fn sign_in_with_its_write_held(
        served: &Arc<Mutex<Served>>,
        keychain: &MemoryKeychain,
        connection: &Entity<Connection>,
        cx: &mut TestAppContext,
    ) -> oneshot::Sender<()> {
        let (release, gate) = oneshot::channel::<()>();
        *keychain.write_gate.lock().unwrap() = Some(gate.shared());
        let writes = keychain.writes.load(Ordering::SeqCst);
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(served, &cx.opened_url().unwrap());
        until(cx, "the new sign-in's write under way", |_| {
            keychain.writes.load(Ordering::SeqCst) > writes
        });
        browser.join().unwrap();
        release
    }

    /// Whether the fake server logged `line`: a token request, a revocation.
    fn logged(served: &Arc<Mutex<Served>>, line: &str) -> bool {
        served.lock().unwrap().log.iter().any(|l| l == line)
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

    /// A metadata-document client ID is an `https` URL with a path other than `/`, no query and
    /// no fragment; a sign-in service compares it as a string with the document's `client_id`.
    #[test]
    fn studio_s_client_id_is_a_metadata_document_url() {
        let id = METADATA_DOCUMENT.expect("Studio has a metadata document");
        let url = url::Url::parse(id).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_ne!(url.path(), "/");
        assert_eq!((url.query(), url.fragment()), (None, None));
        // Parsing removes dot segments, so an ID with one would not survive unchanged.
        assert_eq!(url.as_str(), id, "the client ID is sent as written");
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
        let offers = !served.offers_no_sign_in;
        let challenge = |refused: bool| {
            let error = if refused {
                r#"error="invalid_token", "#
            } else {
                ""
            };
            let value = if offers {
                format!(
                    r#"Bearer {error}resource_metadata="{SERVER}/.well-known/oauth-protected-resource/mcp""#
                )
            } else {
                format!("Bearer {}", error.trim_end_matches(", "))
                    .trim_end()
                    .to_owned()
            };
            vec![("www-authenticate", value)]
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
            ("GET", u)
                if u == format!("{SERVER}/.well-known/oauth-protected-resource/mcp")
                    && served.fails_metadata > 0 =>
            {
                served.fails_metadata -= 1;
                (503, vec![], json!({"error": "unavailable"}))
            }
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
                if served.fails_tokens > 0 {
                    served.fails_tokens -= 1;
                    return match served.fails_status {
                        0 => (503, vec![], json!({})),
                        status => (status, vec![], json!({"error": served.fails_error})),
                    };
                }
                if served.refuses_target && form["grant_type"] == "refresh_token" {
                    return (400, vec![], json!({"error": "invalid_target"}));
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
                let refresh = body.contains("grant_type=refresh_token");
                let (status, headers, page) =
                    answer(&mut served.lock().unwrap(), &method, &url, bearer, body);
                if url.ends_with("/token") {
                    while {
                        let served = served.lock().unwrap();
                        served.hold_token || (refresh && served.hold_refresh)
                    } {
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
        connection_in(served, context(served, keychain), cx)
    }

    /// What every connection to the fake server signs in with, its keychain turns among it.
    fn context(served: &Arc<Mutex<Served>>, keychain: Arc<MemoryKeychain>) -> SignInContext {
        SignInContext {
            elsewhere: fake(served.clone()),
            keychain,
            turns: KeychainTurns::default(),
            identities: identities(),
        }
    }

    /// A connection to the target `work` that signs in with `context`.
    fn connection_in(
        served: &Arc<Mutex<Served>>,
        context: SignInContext,
        cx: &mut TestAppContext,
    ) -> Entity<Connection> {
        let server = ServerEntry {
            name: "work".into(),
            url: SERVER.into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let http = fake(served.clone());
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

    /// Runs the app a while, `happened` staying false throughout: a revocation is sent in the
    /// background, so that none comes is checked over time, not once.
    fn never(cx: &mut TestAppContext, what: &str, happened: impl Fn(&mut TestAppContext) -> bool) {
        for _ in 0..30 {
            cx.run_until_parked();
            assert!(!happened(cx), "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
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

    /// What the connection says the keychain holds for its target.
    fn kept(connection: &Entity<Connection>, cx: &mut TestAppContext) -> Kept {
        connection.read_with(cx, |c, _| c.keychain())
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
        let served = Arc::new(Mutex::new(Served::default()));
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
        // The window's tabs say who asks and where the sign-in is, and offer it, in place of the
        // refused reads' 401s.
        let prompt = |cx: &mut TestAppContext| connection.read_with(cx, |c, _| c.sign_in_prompt());
        assert_eq!(
            prompt(cx),
            Some(SignInPrompt {
                server: "work".into(),
                says: "work asks you to sign in at id.test.".into(),
                offers: Some(PromptAction::SignIn),
            })
        );

        connection.update(cx, |c, cx| c.sign_in(cx));
        assert_eq!(
            sign_in_state(&connection, cx).1,
            SignIn::Waiting {
                service: "id.test".into()
            }
        );
        assert_eq!(
            prompt(cx),
            Some(SignInPrompt {
                server: "work".into(),
                says: "Finish signing in at id.test in your browser.".into(),
                offers: Some(PromptAction::Cancel),
            })
        );
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        assert_eq!(prompt(cx), None, "signed in, the tabs read again");
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
            }
        );
        assert_eq!(kept(&connection, cx), Kept::This);
        let key = key("work");
        assert_eq!(
            keychain.items.lock().unwrap()[&key],
            ("user_1".to_owned(), b"r1".to_vec())
        );

        // Signing out revokes the refresh token, forgets it, and the server asks again.
        connection.update(cx, |c, cx| c.sign_out(cx));
        until(cx, "the sign-out", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Unauthorized
        });
        until(cx, "the revocation", |_| logged(&served, "revoke r1"));
        assert!(keychain.items.lock().unwrap().is_empty());
        assert_eq!(
            sign_in_state(&connection, cx).1,
            SignIn::Offered {
                service: "id.test".into(),
                why: None
            }
        );
        assert_eq!(
            prompt(cx).and_then(|prompt| prompt.offers),
            Some(PromptAction::SignIn)
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
            (matches!(sign_in_state(&connection, cx).1, SignIn::SignedIn { .. })
                && matches!(kept(&connection, cx), Kept::Nothing))
                && sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        browser.join().unwrap();
        until(cx, "the replaced sign-in's revocation", |_| {
            logged(&served, "revoke r2")
        });
        assert!(keychain.items.lock().unwrap().is_empty());
        connection.update(cx, |c, cx| c.sign_out(cx));
        until(cx, "the sign-out", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Unauthorized
        });

        // A keychain that will not keep the sign-in leaves it good for this run, and says so.
        served.lock().unwrap().no_refresh = false;
        keychain.refuses_writes.store(true, Ordering::SeqCst);
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in not kept", |cx| {
            (matches!(sign_in_state(&connection, cx).1, SignIn::SignedIn { .. })
                && matches!(kept(&connection, cx), Kept::Nothing))
                && sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        browser.join().unwrap();
        assert!(keychain.items.lock().unwrap().is_empty());
        // Its sign-out finds nothing to delete, which is no refusal.
        let revoked = |served: &Arc<Mutex<Served>>| {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|l| l.starts_with("revoke"))
                .count()
        };
        let left = |cx: &mut TestAppContext| connection.read_with(cx, |c, _| c.keychain_left());
        let before = revoked(&served);
        connection.update(cx, |c, cx| c.sign_out(cx));
        until(cx, "the unkept sign-in's revocation", |_| {
            revoked(&served) > before
        });
        cx.run_until_parked();
        assert_eq!(
            sign_in_state(&connection, cx).1,
            SignIn::Offered {
                service: "id.test".into(),
                why: None
            }
        );
        assert_eq!(left(cx), None);

        // One that will not delete it leaves the person signed out for this run, and says so:
        // the next launch would read the sign-in back.
        keychain.refuses_writes.store(false, Ordering::SeqCst);
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in kept", |cx| {
            (matches!(sign_in_state(&connection, cx).1, SignIn::SignedIn { .. })
                && matches!(kept(&connection, cx), Kept::This))
        });
        browser.join().unwrap();
        keychain.refuses_deletes.store(true, Ordering::SeqCst);
        connection.update(cx, |c, cx| c.sign_out(cx));
        until(cx, "the keychain's refusal", |cx| left(cx).is_some());
        let (state, offer, _) = sign_in_state(&connection, cx);
        assert_eq!(state, ConnectionState::Unauthorized);
        assert_eq!(
            offer,
            SignIn::Offered {
                service: "id.test".into(),
                why: None
            }
        );
        assert_eq!(left(cx).as_deref(), Some("the keychain refused"));
        assert_eq!(keychain.items.lock().unwrap().len(), 1);
        // It is the connection's to say, not a message's: a sign-in begun and given up leaves it.
        connection.update(cx, |c, cx| c.sign_in(cx));
        connection.update(cx, |c, cx| c.cancel_sign_in(cx));
        assert_eq!(left(cx).as_deref(), Some("the keychain refused"));

        // A sign-in the keychain keeps nothing of, over an earlier one it will not delete, says
        // the earlier one is what the next launch reads.
        served.lock().unwrap().no_refresh = true;
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in over an earlier one", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::SignedIn { .. })
        });
        browser.join().unwrap();
        assert_eq!(
            kept(&connection, cx),
            Kept::Left("the keychain refused".into())
        );
        assert_eq!(left(cx).as_deref(), Some("the keychain refused"));
        assert_eq!(keychain.items.lock().unwrap().len(), 1);
        // And one whose write the keychain refuses deletes the earlier one rather than leave it.
        served.lock().unwrap().no_refresh = false;
        keychain.refuses_deletes.store(false, Ordering::SeqCst);
        keychain.refuses_writes.store(true, Ordering::SeqCst);
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the earlier sign-in deleted", |cx| {
            (matches!(sign_in_state(&connection, cx).1, SignIn::SignedIn { .. })
                && matches!(kept(&connection, cx), Kept::Nothing))
        });
        browser.join().unwrap();
        assert!(keychain.items.lock().unwrap().is_empty());
        assert_eq!(left(cx), None);

        // The CLI's credential for the target replaces a sign-in whose token the keychain does
        // not hold, over an earlier item it would not delete: the token, which nothing else
        // holds, is revoked, and the item stays said.
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key.clone(), ("user_9".into(), b"earlier".to_vec()));
        keychain.refuses_deletes.store(true, Ordering::SeqCst);
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "a sign-in over an earlier item", |cx| {
            (matches!(sign_in_state(&connection, cx).1, SignIn::SignedIn { .. })
                && matches!(kept(&connection, cx), Kept::Left(_)))
        });
        browser.join().unwrap();
        let before = revoked(&served);
        connection.update(cx, |c, cx| c.take_token_from(Some(Arc::new(Cli)), cx));
        until(cx, "the dropped sign-in's revocation", |_| {
            revoked(&served) > before
        });
        until(cx, "the item left said", |cx| left(cx).is_some());
        assert_eq!(left(cx).as_deref(), Some("the keychain refused"));
        assert_eq!(keychain.items.lock().unwrap()[&key].1, b"earlier".to_vec());
    }

    /// The target's keychain work takes its turns in order. A sign-in made while a sign-out's
    /// delete is under way reaches the keychain after it, so the delete cannot take the item the
    /// new sign-in kept. Given its tokens, a sign-in is finishing and not to be cancelled; one
    /// dropped then (the CLI's credential taking the target) is kept once the delete is done,
    /// then dropped as one held would be: its item stays for the target made anonymous again.
    #[gpui::test]
    async fn a_sign_in_made_during_a_sign_out_reaches_the_keychain_after_it(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        let key = key("work");
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
        let held = |keychain: &MemoryKeychain| {
            keychain
                .items
                .lock()
                .unwrap()
                .get(&key)
                .map(|(_, secret)| String::from_utf8(secret.clone()).unwrap())
        };
        assert_eq!(held(&keychain).as_deref(), Some("r1"), "rotated at launch");
        let exchanged = |served: &Arc<Mutex<Served>>| {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|line| line.starts_with("token authorization_code"))
                .count()
        };
        // Signs out with its delete held, then signs in: the sign-in is exchanged and waits.
        let sign_out_held_then_sign_in = |cx: &mut TestAppContext| {
            let (release, gate) = oneshot::channel::<()>();
            *keychain.delete_gate.lock().unwrap() = Some(gate.shared());
            connection.update(cx, |c, cx| c.sign_out(cx));
            until(cx, "the sign-out", |cx| {
                sign_in_state(&connection, cx).0 == ConnectionState::Unauthorized
            });
            let before = exchanged(&served);
            connection.update(cx, |c, cx| c.sign_in(cx));
            let browser = approve(&served, &cx.opened_url().unwrap());
            until(cx, "the code exchanged", |_| exchanged(&served) > before);
            browser.join().unwrap();
            cx.run_until_parked();
            assert!(
                matches!(sign_in_state(&connection, cx).1, SignIn::Finishing { .. }),
                "the sign-in, its tokens given, waits for the sign-out's delete"
            );
            release
        };

        // Let through, Cancel refused while it finishes: its own token, r2, is what the keychain
        // holds once the delete before it is done.
        let release = sign_out_held_then_sign_in(cx);
        connection.update(cx, |c, cx| c.cancel_sign_in(cx));
        assert!(
            matches!(sign_in_state(&connection, cx).1, SignIn::Finishing { .. }),
            "not cancelled once finishing"
        );
        assert_eq!(held(&keychain).as_deref(), Some("r1"), "nothing yet");
        release.send(()).unwrap();
        until(cx, "the new sign-in", |cx| {
            (matches!(sign_in_state(&connection, cx).1, SignIn::SignedIn { .. })
                && matches!(kept(&connection, cx), Kept::This))
        });
        assert_eq!(held(&keychain).as_deref(), Some("r2"));

        // The same, the sign-in dropped while it finishes as the CLI's credential takes the
        // target: not cancelled either, its token, r3, is kept after the delete, unrevoked.
        let release = sign_out_held_then_sign_in(cx);
        connection.update(cx, |c, cx| c.take_token_from(Some(Arc::new(Cli)), cx));
        assert_eq!(sign_in_state(&connection, cx).1, SignIn::NotOffered);
        // A window reads with the CLI's token, waiting for its answer, not with what the
        // sign-in's answers left.
        assert!(!connection.read_with(cx, |c, _| c.token_ready()));
        until(cx, "the CLI's answer", |cx| {
            connection.read_with(cx, |c, _| c.token_ready())
        });
        release.send(()).unwrap();
        until(cx, "the dropped sign-in kept", |_| {
            held(&keychain).as_deref() == Some("r3")
        });
        until(cx, "the dropped sign-in said as kept", |cx| {
            kept(&connection, cx) == Kept::This
        });
        never(cx, "the dropped sign-in kept, so not revoked", |_| {
            logged(&served, "revoke r3")
        });
    }

    /// A sign-in as someone else dropped while it finishes (the CLI's credential taking the
    /// target as the keychain takes its token) is kept all the same, then dropped as one held
    /// would be: the keychain holds its token for the target made anonymous again, and the
    /// sign-in it replaced ends as replaced, its token revoked. Before, the drop cancelled it
    /// mid-write: its token was revoked, and the keychain kept the replaced account's, revoked
    /// too where the service revokes and signed in with at the next launch where it does not.
    /// The target made anonymous again before it is kept, the server's answer once it is offers
    /// a sign-in, which signs in with it; before, no sign-in was offered until a restart.
    #[gpui::test]
    async fn a_sign_in_dropped_while_it_finishes_is_kept_then_dropped(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        let key = key("work");
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
        let held = || {
            keychain
                .items
                .lock()
                .unwrap()
                .get(&key)
                .map(|(_, secret)| String::from_utf8(secret.clone()).unwrap())
        };
        assert_eq!(held().as_deref(), Some("r1"), "rotated at launch");
        // Signs in as someone else, the keychain slow to take the new token.
        let (release, gate) = oneshot::channel::<()>();
        *keychain.write_gate.lock().unwrap() = Some(gate.shared());
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the replaced sign-in's revocation", |_| {
            logged(&served, "revoke r1")
        });
        browser.join().unwrap();
        assert!(matches!(
            sign_in_state(&connection, cx).1,
            SignIn::Finishing { .. }
        ));
        connection.update(cx, |c, cx| c.take_token_from(Some(Arc::new(Cli)), cx));
        assert_eq!(sign_in_state(&connection, cx).1, SignIn::NotOffered);
        // The CLI names no credential for the target again before the sign-in is kept, and the
        // server's answer, which would offer a sign-in, comes while it still finishes.
        let listed = || {
            served
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|request| request.ends_with("/graphs"))
                .count()
        };
        let before = listed();
        connection.update(cx, |c, cx| c.take_token_from(None, cx));
        until(cx, "the server's answer to the anonymous target", |_| {
            listed() > before
        });
        never(cx, "a sign-in offered while one finishes", |cx| {
            sign_in_state(&connection, cx).1 != SignIn::NotOffered
        });
        release.send(()).unwrap();
        until(cx, "signed in with the sign-in kept", |cx| {
            let (state, sign_in, _) = sign_in_state(&connection, cx);
            state == ConnectionState::Ready && matches!(sign_in, SignIn::SignedIn { .. })
        });
        assert!(!logged(&served, "revoke r2"), "kept, so not revoked");
        assert_eq!(kept(&connection, cx), Kept::This);
    }

    /// A sign-in cancelled while the service answers its code exchange leaves no token behind:
    /// the exchange runs to its end on its own, and the token it gives, which nobody takes, is
    /// revoked. Before, Cancel dropped the exchange mid-request, after the service had redeemed
    /// the code, so its refresh token was left with nobody.
    #[gpui::test]
    async fn a_sign_in_cancelled_during_its_exchange_revokes_its_token(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            hold_token: true,
            ..Served::default()
        }));
        let keychain = Arc::new(MemoryKeychain::default());
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the offer", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
        });
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the code exchanged", |_| {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|l| l.starts_with("token authorization_code"))
        });
        browser.join().unwrap();
        connection.update(cx, |c, cx| c.cancel_sign_in(cx));
        served.lock().unwrap().hold_token = false;
        until(cx, "the token nobody took revoked", |_| {
            logged(&served, "revoke r1")
        });
        assert!(keychain.items.lock().unwrap().is_empty());
        assert!(matches!(
            sign_in_state(&connection, cx).1,
            SignIn::Offered { why: None, .. }
        ));
    }

    /// Tokens Studio may not use (an access token living an hour, past the server's five
    /// minutes) are refused, and the refresh token they came with, which nothing will hold, is
    /// revoked: at a sign-in, and at a refresh that rotated the kept token. Before, both were
    /// dropped unrevoked.
    #[gpui::test]
    async fn tokens_studio_may_not_use_have_their_refresh_token_revoked(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let offered_why =
            |connection: &Entity<Connection>, cx: &mut TestAppContext| match connection
                .read_with(cx, |c, _| c.sign_in.clone())
            {
                SignIn::Offered { why, .. } => why,
                _ => None,
            };
        // At a sign-in.
        let served = Arc::new(Mutex::new(Served {
            lives: 3600,
            ..Served::default()
        }));
        let keychain = Arc::new(MemoryKeychain::default());
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the offer", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
        });
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in refused", |cx| {
            offered_why(&connection, cx).is_some()
        });
        browser.join().unwrap();
        assert!(
            offered_why(&connection, cx)
                .unwrap()
                .contains("lasts 3600 seconds")
        );
        until(cx, "its refresh token revoked", |_| {
            logged(&served, "revoke r1")
        });
        assert!(keychain.items.lock().unwrap().is_empty());

        // At a kept sign-in's refresh, which rotates the kept token.
        let served = Arc::new(Mutex::new(Served {
            lives: 3600,
            ..Served::default()
        }));
        let keychain = kept_at_launch(&served);
        let kept_one = connection_in(&served, context(&served, keychain.clone()), cx);
        kept_one.update(cx, |c, cx| c.connect(cx));
        until(cx, "the rotated token revoked", |_| {
            logged(&served, "revoke r1")
        });
        until(cx, "the sign-in refused", |cx| {
            offered_why(&kept_one, cx).is_some_and(|why| why.contains("lasts 3600 seconds"))
        });
        // The token from before, which the rotation spent, is forgotten, and not revoked.
        assert!(keychain.items.lock().unwrap().is_empty());
        assert_eq!(kept(&kept_one, cx), Kept::Nothing);
        never(cx, "the spent token revoked", |_| {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|l| l == "revoke kept")
        });
    }

    /// A sign-out deletes the keychain item at once while a refresh is in flight, so Studio
    /// quitting before that refresh answers leaves the next launch nothing to sign in with. The
    /// refresh, answered after, writes nothing, and its rotated token is revoked. Before, the
    /// delete waited for the refresh, up to its request's 15 seconds and a retry.
    #[gpui::test]
    async fn a_sign_out_deletes_at_once_over_a_refresh_in_flight(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            hold_token: true,
            ..Served::default()
        }));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the launch's refresh in flight", |_| {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|l| l.starts_with("token refresh_token"))
        });
        connection.update(cx, |c, cx| c.sign_out(cx));
        until(cx, "the item deleted", |_| {
            keychain.items.lock().unwrap().is_empty()
        });
        served.lock().unwrap().hold_token = false;
        until(cx, "the rotated token revoked", |_| {
            logged(&served, "revoke r1")
        });
        assert!(keychain.items.lock().unwrap().is_empty());
        assert_eq!(kept(&connection, cx), Kept::Nothing);
    }

    /// A sign-out while a refresh is writing the token it rotated leaves nothing in the
    /// keychain: its delete comes after that write, and the rotated token is revoked.
    #[gpui::test]
    async fn a_sign_out_during_a_refresh_write_leaves_nothing(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let (release, gate) = oneshot::channel::<()>();
        *keychain.write_gate.lock().unwrap() = Some(gate.shared());
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the rotated token's write under way", |_| {
            keychain.writes.load(Ordering::SeqCst) > 0
        });
        connection.update(cx, |c, cx| c.sign_out(cx));
        release.send(()).unwrap();
        until(cx, "the rotated token revoked", |_| {
            logged(&served, "revoke r1")
        });
        never(cx, "an item after the sign-out", |_| {
            !keychain.items.lock().unwrap().is_empty()
        });
    }

    /// The ask a read's 401 makes of the sign-in, under way when the person signs out or the CLI
    /// renames the target, is answered for nobody: the ended sign-in's answer neither puts a token
    /// back nor says the target gave none. Each change of source ends the old one's asks
    /// (`connection::tests::a_token_answered_after_its_source_was_replaced_is_not_taken` holds
    /// the CLI's credential to it).
    #[gpui::test]
    async fn an_ask_under_way_when_the_sign_in_ends_gives_nobody_its_token(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        for rename in [false, true] {
            let served = Arc::new(Mutex::new(Served::default()));
            let keychain = kept_at_launch(&served);
            let connection = connection(&served, keychain, cx);
            connection.update(cx, |c, cx| c.connect(cx));
            until(cx, "the kept sign-in", |cx| {
                sign_in_state(&connection, cx).0 == ConnectionState::Ready
            });
            let refreshes = || {
                served
                    .lock()
                    .unwrap()
                    .log
                    .iter()
                    .filter(|line| line.starts_with("token refresh_token"))
                    .count()
            };
            served.lock().unwrap().hold_token = true;
            connection.update(cx, |c, cx| c.read_refused(cx));
            until(cx, "the refresh the 401 asked for", |_| refreshes() == 2);
            connection.update(cx, |c, cx| {
                if rename {
                    c.rename("renamed".into(), cx);
                } else {
                    c.sign_out(cx);
                }
            });
            until(cx, "a sign-in offered", |cx| {
                matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
            });
            let answered = |cx: &mut TestAppContext| {
                connection.read_with(cx, |c, _| c.has_token() || c.token_unread.is_some())
            };
            assert!(!answered(cx));
            served.lock().unwrap().hold_token = false;
            never(cx, "the ended sign-in's answer taken", answered);
        }
    }

    /// A sign-in taken while the one it replaces is asked again after a refusal keeps its own
    /// token, and that ask is not the new one's: its answer comes for nobody, and a read refused
    /// under the new token asks the new sign-in again. Without a change of source ending the old
    /// one's asks, the new token was taken for the answer to the refusal, so a read refused
    /// under it asked nothing.
    #[gpui::test]
    async fn a_sign_in_taken_while_the_one_it_replaces_is_asked_keeps_its_token(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain, cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        served.lock().unwrap().hold_refresh = true;
        connection.update(cx, |c, cx| c.read_refused(cx));
        until(cx, "the refresh the 401 asked for", |_| {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|line| line.starts_with("token refresh_token"))
                .count()
                == 2
        });
        // A sign-in through the browser, whose code exchange is not held, is taken.
        let before = connection.read_with(cx, |c, _| c.client().credential_changes());
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the new sign-in taken", |cx| {
            let (state, sign_in, _) = sign_in_state(&connection, cx);
            state == ConnectionState::Ready
                && matches!(sign_in, SignIn::SignedIn { .. })
                && connection.read_with(cx, |c, _| c.client().credential_changes()) != before
        });
        browser.join().unwrap();
        let changes = connection.read_with(cx, |c, _| c.client().credential_changes());
        served.lock().unwrap().hold_refresh = false;
        never(cx, "the replaced sign-in's answer taken", |cx| {
            let (state, sign_in, _) = sign_in_state(&connection, cx);
            state != ConnectionState::Ready
                || !matches!(sign_in, SignIn::SignedIn { .. })
                || connection.read_with(cx, |c, _| c.client().credential_changes()) != changes
        });
        connection.update(cx, |c, cx| c.read_refused(cx));
        until(cx, "the new sign-in asked after the refusal", |_| {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|line| line.starts_with("token refresh_token"))
                .count()
                == 3
        });
    }

    /// A target the CLI renames is, to its sign-in, the old target gone and a new one at the same
    /// address, as its keychain item is the target's: the sign-in held is dropped, its item kept
    /// under the old name and its token not revoked, and the server offers a sign-in under the
    /// new name, whose item is kept under it. Before, the sign-in held went on under the new name
    /// while its refreshes wrote under the old one, which the next launch, reading the new name,
    /// did not read.
    #[gpui::test]
    async fn a_renamed_target_is_signed_in_to_under_its_new_name(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let held = |target: &str| {
            keychain
                .items
                .lock()
                .unwrap()
                .get(&key(target))
                .map(|(_, secret)| String::from_utf8(secret.clone()).unwrap())
        };
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        assert_eq!(held("work").as_deref(), Some("r1"), "rotated at launch");
        // A refresh in flight across the rename, so the old sign-in's ending, a drop that waits
        // for it, reports only after the new name's offer.
        {
            let mut served = served.lock().unwrap();
            served.hold_token = true;
            served.admits_nobody = true;
        }
        let refreshes = || {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|l| l.starts_with("token refresh_token"))
                .count()
        };
        let before = refreshes();
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "a refresh in flight", |_| refreshes() > before);
        served.lock().unwrap().admits_nobody = false;

        connection.update(cx, |c, cx| c.rename("job".into(), cx));
        until(cx, "a sign-in offered under the new name", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: None, .. }
            )
        });
        assert!(!connection.read_with(cx, |c, _| c.holds_sign_in()));
        served.lock().unwrap().hold_token = false;
        until(
            cx,
            "the old sign-in's rotation kept under the old name",
            |_| held("work").as_deref() == Some("r2"),
        );
        never(cx, "the old name's item said under the new name", |cx| {
            kept(&connection, cx) != Kept::Nothing
        });
        assert!(!logged(&served, "revoke r2"), "kept, so not revoked");

        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "signed in under the new name", |cx| {
            let (state, sign_in, _) = sign_in_state(&connection, cx);
            state == ConnectionState::Ready && matches!(sign_in, SignIn::SignedIn { .. })
        });
        browser.join().unwrap();
        assert_eq!(held("job").as_deref(), Some("r3"));
        assert_eq!(kept(&connection, cx), Kept::This);
        assert_eq!(held("work").as_deref(), Some("r2"));
    }

    /// A rename while a sign-in as someone else finishes, the one held ended as replaced (its
    /// token revoked) when the finishing one took its turn: the finishing one is kept, under the
    /// name it began with, then dropped as that name's; meanwhile the connection reads as signing
    /// in, not as asking for a token; and once it is kept, a sign-in is offered under the new
    /// name.
    #[gpui::test]
    async fn a_rename_while_a_sign_in_finishes_offers_one_under_the_new_name(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        let release = sign_in_with_its_write_held(&served, &keychain, &connection, cx);
        connection.update(cx, |c, cx| c.rename("job".into(), cx));
        until(cx, "the held sign-in, replaced, revoked", |_| {
            logged(&served, "revoke r1")
        });
        assert_eq!(
            connection.read_with(cx, |c, _| c.label()),
            "Signing in…",
            "kept before anything is offered"
        );
        release.send(()).unwrap();
        until(cx, "a sign-in offered under the new name", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: None, .. }
            )
        });
        let held = |target: &str| {
            keychain
                .items
                .lock()
                .unwrap()
                .get(&key(target))
                .map(|(_, secret)| String::from_utf8(secret.clone()).unwrap())
        };
        assert_eq!(
            held("work").as_deref(),
            Some("r2"),
            "kept under the name it began with"
        );
        assert_eq!(held("job"), None);
        assert_eq!(kept(&connection, cx), Kept::Nothing);
        assert!(!logged(&served, "revoke r2"));
    }

    /// While a sign-in as someone else finishes, the one it replaces has ended and answers so;
    /// the token it gave stands until the new one is taken, so reads go on with it. Before, the
    /// answer dropped the token and the connection read as refused for that moment.
    #[gpui::test]
    async fn the_token_of_a_sign_in_being_replaced_stands_while_the_new_one_finishes(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        // A sign-in as someone else, its keychain write held: it finishes.
        let release = sign_in_with_its_write_held(&served, &keychain, &connection, cx);
        // The one replaced is asked again, and answers that it ended.
        connection.update(cx, |c, cx| c.refresh_token(cx));
        never(
            cx,
            "the connection refused while the new sign-in finishes",
            |cx| sign_in_state(&connection, cx).0 != ConnectionState::Ready,
        );
        release.send(()).unwrap();
        until(cx, "the new sign-in taken", |cx| {
            let (state, sign_in, _) = sign_in_state(&connection, cx);
            state == ConnectionState::Ready && matches!(sign_in, SignIn::SignedIn { .. })
        });
    }

    /// A sign-in the service refused for good is dropped, its item kept for the next launch; a
    /// sign-in made after writes over that item, and revokes its token, which nothing holds once
    /// overwritten. Before, the earlier token was left with nobody, unrevoked.
    #[gpui::test]
    async fn a_sign_in_over_an_item_left_revokes_its_token(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        // Refused for good: dropped, its item kept, its token not revoked.
        {
            let mut served = served.lock().unwrap();
            served.refuses_target = true;
            served.admits_nobody = true;
        }
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the sign-in refused", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: Some(_), .. }
            ) && kept(&connection, cx) == Kept::This
        });
        never(cx, "the dropped sign-in's token revoked", |_| {
            logged(&served, "revoke r1")
        });
        {
            let mut served = served.lock().unwrap();
            served.refuses_target = false;
            served.admits_nobody = false;
        }
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the earlier token revoked", |_| {
            logged(&served, "revoke r1")
        });
        browser.join().unwrap();
        until(cx, "the new sign-in taken, and kept", |cx| {
            let (state, sign_in, _) = sign_in_state(&connection, cx);
            state == ConnectionState::Ready
                && matches!(sign_in, SignIn::SignedIn { .. })
                && kept(&connection, cx) == Kept::This
        });
        assert_eq!(
            keychain.items.lock().unwrap()[&key("work")].1,
            b"r2".to_vec()
        );
    }

    /// A sign-in over an earlier item the keychain will neither replace nor delete keeps that
    /// item's token, as the keychain still holds it; once its own token replaces the item (a
    /// rotation the keychain now takes), the earlier token, held by nothing, is revoked. Before,
    /// it was left with nobody.
    #[gpui::test]
    async fn an_earlier_token_is_revoked_once_the_item_no_longer_holds_it(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = Arc::new(MemoryKeychain::default());
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key("work"), ("user_9".into(), b"earlier".to_vec()));
        keychain.refuses_writes.store(true, Ordering::SeqCst);
        keychain.refuses_deletes.store(true, Ordering::SeqCst);
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        // The earlier item is read at launch, refreshed, and refused: the offer that follows.
        until(cx, "the earlier sign-in refused", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: Some(_), .. }
            )
        });
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "a sign-in over the earlier item", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
                && matches!(kept(&connection, cx), Kept::Left(_))
        });
        browser.join().unwrap();
        never(
            cx,
            "the earlier token revoked while the keychain holds it",
            |_| logged(&served, "revoke earlier"),
        );
        // The keychain takes writes again, and a refresh rotates the token into the item.
        keychain.refuses_writes.store(false, Ordering::SeqCst);
        keychain.refuses_deletes.store(false, Ordering::SeqCst);
        served.lock().unwrap().admits_nobody = true;
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the rotation kept", |_| {
            keychain.items.lock().unwrap()[&key("work")].1 == b"r2".to_vec()
        });
        until(cx, "the earlier token revoked", |_| {
            logged(&served, "revoke earlier")
        });
        assert_eq!(kept(&connection, cx), Kept::This);
    }

    /// A connection released while a sign-in finishes, retired first as when its target is gone
    /// from the CLI or not as when Studio quits, lets the sign-in's keychain work finish, which
    /// runs on its own: the write over an earlier item lands, and that item's token, held by
    /// nothing then, is revoked. The new sign-in is dropped as for a retired connection, kept for
    /// the target's return, and a connection to it later takes its turn at the item and signs in
    /// with it. Before, the release cut the work off mid-write, leaving the overwritten token with
    /// nobody.
    #[gpui::test]
    async fn a_connection_released_while_a_sign_in_finishes_lets_it_finish(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        for retired in [true, false] {
            let served = Arc::new(Mutex::new(Served::default()));
            let keychain = Arc::new(MemoryKeychain::default());
            // An earlier item the keychain will not let go of at launch: said as left.
            keychain
                .items
                .lock()
                .unwrap()
                .insert(key("work"), ("user_9".into(), b"earlier".to_vec()));
            keychain.refuses_deletes.store(true, Ordering::SeqCst);
            let context = context(&served, keychain.clone());
            let connection = connection_in(&served, context.clone(), cx);
            connection.update(cx, |c, cx| c.connect(cx));
            until(cx, "the earlier sign-in refused", |cx| {
                matches!(
                    sign_in_state(&connection, cx).1,
                    SignIn::Offered { why: Some(_), .. }
                )
            });
            keychain.refuses_deletes.store(false, Ordering::SeqCst);
            let release = sign_in_with_its_write_held(&served, &keychain, &connection, cx);
            // Retired, then released, as the registry does; or released alone, as at quitting.
            if retired {
                connection.update(cx, |c, cx| {
                    c.retire("The CLI no longer lists it".into(), cx)
                });
            }
            let released = connection.downgrade();
            drop(connection);
            cx.update(|_| {});
            cx.run_until_parked();
            assert!(released.upgrade().is_none(), "the connection released");
            release.send(()).unwrap();
            until(cx, "the earlier token revoked", |_| {
                logged(&served, "revoke earlier")
            });
            assert_eq!(
                keychain.items.lock().unwrap()[&key("work")].1,
                b"r1".to_vec()
            );
            // A connection to the target later reads the item in its turn, and signs in with it.
            let again = connection_in(&served, context, cx);
            again.update(cx, |c, cx| c.connect(cx));
            until(cx, "signed in with the sign-in kept", |cx| {
                let (state, sign_in, _) = sign_in_state(&again, cx);
                state == ConnectionState::Ready && matches!(sign_in, SignIn::SignedIn { .. })
            });
            assert!(
                !logged(&served, "revoke r1"),
                "kept for the target's return, so not revoked (retired: {retired})"
            );
        }
    }

    /// A sign-out while a sign-in as someone else finishes stands through the connection's
    /// retirement and release that follow (its target gone from the CLI): once kept, the new
    /// sign-in is signed out of, its item deleted and its token revoked, and a connection to the
    /// target later, which reads the item only after that deletion, is offered a sign-in.
    /// Before, the release dropped it, the keychain kept it, and the next connection signed in
    /// with it.
    #[gpui::test]
    async fn a_sign_out_while_a_sign_in_finishes_stands_when_the_connection_is_released(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let context = context(&served, keychain.clone());
        let connection = connection_in(&served, context.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        let (release_write, write_gate) = oneshot::channel::<()>();
        *keychain.write_gate.lock().unwrap() = Some(write_gate.shared());
        let writes = keychain.writes.load(Ordering::SeqCst);
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the new sign-in's write under way", |_| {
            keychain.writes.load(Ordering::SeqCst) > writes
        });
        browser.join().unwrap();
        connection.update(cx, |c, cx| c.sign_out(cx));
        // As the registry does: retired, then released.
        connection.update(cx, |c, cx| {
            c.retire("The CLI no longer lists it".into(), cx)
        });
        let released = connection.downgrade();
        drop(connection);
        cx.update(|_| {});
        cx.run_until_parked();
        assert!(released.upgrade().is_none(), "the connection released");
        let (release_delete, delete_gate) = oneshot::channel::<()>();
        *keychain.delete_gate.lock().unwrap() = Some(delete_gate.shared());
        release_write.send(()).unwrap();
        until(cx, "the new sign-in written", |_| {
            keychain.items.lock().unwrap()[&key("work")].1 == b"r2".to_vec()
        });
        let again = connection_in(&served, context, cx);
        again.update(cx, |c, cx| c.connect(cx));
        // Waiting for its turn at the item, or, given it too soon, signed in with it.
        until(cx, "the next connection reading how to sign in", |cx| {
            let (state, sign_in, _) = sign_in_state(&again, cx);
            sign_in == SignIn::Reading || state == ConnectionState::Ready
        });
        never(
            cx,
            "the next connection signing in with the item being deleted",
            |cx| sign_in_state(&again, cx).0 == ConnectionState::Ready,
        );
        release_delete.send(()).unwrap();
        until(cx, "the next connection offered a sign-in", |cx| {
            matches!(
                sign_in_state(&again, cx).1,
                SignIn::Offered { why: None, .. }
            )
        });
        assert!(keychain.items.lock().unwrap().is_empty());
        until(cx, "both tokens revoked", |_| {
            logged(&served, "revoke r1") && logged(&served, "revoke r2")
        });
    }

    /// A sign-out while a sign-in as someone else finishes: nothing is offered until that one is
    /// kept, as no sign-in can start before, then signed out of; then a sign-in is offered, and
    /// the keychain keeps neither. Before, Sign in was offered at once and did nothing. Another
    /// connection to the target reads the item only once the sign-out has deleted it, the
    /// sign-out ending it in the turn its finishing held. Before, it read the new sign-in's item
    /// between its write and the deletion, and signed in with a token about to be revoked.
    #[gpui::test]
    async fn a_sign_out_while_a_sign_in_finishes_offers_one_once_it_is_kept(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let context = context(&served, keychain.clone());
        let connection = connection_in(&served, context.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        let release = sign_in_with_its_write_held(&served, &keychain, &connection, cx);
        connection.update(cx, |c, cx| c.sign_out(cx));
        assert_eq!(sign_in_state(&connection, cx).1, SignIn::NotOffered);
        until(cx, "the server's answer without a token", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Unauthorized
        });
        assert_eq!(sign_in_state(&connection, cx).1, SignIn::NotOffered);
        assert_eq!(connection.read_with(cx, |c, _| c.label()), "Signing in…");
        let other = connection_in(&served, context, cx);
        other.update(cx, |c, cx| c.connect(cx));
        until(cx, "the other connection reading how to sign in", |cx| {
            sign_in_state(&other, cx).1 == SignIn::Reading
        });
        let (release_delete, delete_gate) = oneshot::channel::<()>();
        *keychain.delete_gate.lock().unwrap() = Some(delete_gate.shared());
        release.send(()).unwrap();
        until(cx, "the new sign-in written", |_| {
            keychain.items.lock().unwrap()[&key("work")].1 == b"r2".to_vec()
        });
        never(
            cx,
            "the other connection signing in with the item being deleted",
            |cx| sign_in_state(&other, cx).0 == ConnectionState::Ready,
        );
        release_delete.send(()).unwrap();
        for (which, connection) in [("this", &connection), ("the other", &other)] {
            until(
                cx,
                &format!("{which} connection offered a sign-in once that one is signed out of"),
                |cx| {
                    matches!(
                        sign_in_state(connection, cx).1,
                        SignIn::Offered { why: None, .. }
                    )
                },
            );
        }
        until(cx, "both tokens revoked", |_| {
            logged(&served, "revoke r1") && logged(&served, "revoke r2")
        });
        assert!(keychain.items.lock().unwrap().is_empty());
    }

    /// A sign-in as someone else that fails while the one held still stands says why, and what
    /// the one held is: not answering, when its service does not answer, not "Not admitted".
    /// And one that fails after the one held was refused says why that one ended too.
    #[gpui::test]
    async fn a_failed_sign_in_says_what_became_of_the_one_held(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        // The service down from launch: the kept sign-in stands, not answering.
        let served = Arc::new(Mutex::new(Served {
            token_down: true,
            ..Served::default()
        }));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the sign-in not answering", |cx| {
            sign_in_state(&connection, cx).2 == "Sign-in not answering"
        });
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in as someone else failed", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: Some(_), .. }
            )
        });
        browser.join().unwrap();
        assert!(connection.read_with(cx, |c, _| c.holds_sign_in()));
        assert_eq!(sign_in_state(&connection, cx).2, "Sign-in not answering");

        // The one held refused for good while another is in the browser, which then fails.
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let connection = connection_in(&served, context(&served, keychain), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        connection.update(cx, |c, cx| c.sign_in(cx));
        let opened = cx.opened_url().unwrap();
        {
            let mut served = served.lock().unwrap();
            served.refuses_target = true;
            served.admits_nobody = true;
        }
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the one held refused and revoked", |_| {
            logged(&served, "revoke r1")
        });
        served.lock().unwrap().token_down = true;
        let browser = approve(&served, &opened);
        until(cx, "the sign-in under way failed", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: Some(_), .. }
            )
        });
        browser.join().unwrap();
        let SignIn::Offered { why: Some(why), .. } = sign_in_state(&connection, cx).1 else {
            unreachable!()
        };
        assert!(why.contains("does not issue tokens"), "{why}");
    }

    /// The keychain's turns are its item's, whichever connection takes them: a connection made
    /// for a target while another's sign-out is still deleting its item (the target listed
    /// again) reads the keychain after that delete, and is offered a sign-in. Before, each
    /// connection kept its own turns, so the new one read the item being deleted and signed in.
    #[gpui::test]
    async fn a_connection_reads_the_keychain_after_another_signs_out(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let context = context(&served, keychain.clone());
        let first = connection_in(&served, context.clone(), cx);
        first.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&first, cx).0 == ConnectionState::Ready
        });
        let (release, gate) = oneshot::channel::<()>();
        *keychain.delete_gate.lock().unwrap() = Some(gate.shared());
        first.update(cx, |c, cx| c.sign_out(cx));
        let second = connection_in(&served, context, cx);
        second.update(cx, |c, cx| c.connect(cx));
        until(cx, "the second reading how to sign in", |cx| {
            sign_in_state(&second, cx).1 == SignIn::Reading
        });
        never(
            cx,
            "the second signing in with the item being deleted",
            |cx| sign_in_state(&second, cx).0 == ConnectionState::Ready,
        );
        release.send(()).unwrap();
        until(cx, "the second offered a sign-in", |cx| {
            matches!(
                sign_in_state(&second, cx).1,
                SignIn::Offered { why: None, .. }
            )
        });
        assert!(keychain.items.lock().unwrap().is_empty());
    }

    /// A sign-in the service refuses for good, not by ending its grant, is ended as one dropped:
    /// its refresh token, which the keychain does not hold and the service still honours, is
    /// revoked, as nothing else would hold it. Before, the connection let it go unrevoked.
    #[gpui::test]
    async fn a_sign_in_refused_for_good_revokes_a_token_nobody_holds(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = Arc::new(MemoryKeychain::default());
        keychain.refuses_writes.store(true, Ordering::SeqCst);
        let connection = connection(&served, keychain, cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the offer", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
        });
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in not kept", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
                && kept(&connection, cx) == Kept::Nothing
        });
        browser.join().unwrap();
        // The server refuses the token, and the refresh after it is refused for good.
        {
            let mut served = served.lock().unwrap();
            served.admits_nobody = true;
            served.refuses_target = true;
        }
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the refused sign-in's revocation", |_| {
            logged(&served, "revoke r1")
        });
        assert!(matches!(
            sign_in_state(&connection, cx).1,
            SignIn::Offered { why: Some(_), .. }
        ));
    }

    /// A sign-in the service ends for good while a connect and the ask after a refused read both
    /// wait on its refresh is said once: offered again, with why. The first answer drops the
    /// sign-in, which ends the other ask; without that, the second answer said the target gave
    /// no token, as if the CLI had given none.
    #[gpui::test]
    async fn a_sign_in_ended_while_two_asks_wait_on_it_is_said_once(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain, cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        {
            let mut served = served.lock().unwrap();
            served.refuses_target = true;
            served.hold_refresh = true;
        }
        let refreshes = || {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|line| line.starts_with("token refresh_token"))
                .count()
        };
        // The ask after a refused read starts the refresh; a connect's ask joins it.
        connection.update(cx, |c, cx| c.read_refused(cx));
        until(cx, "the refresh after the refusal", |_| refreshes() == 2);
        connection.update(cx, |c, cx| c.connect(cx));
        cx.run_until_parked();
        served.lock().unwrap().hold_refresh = false;
        until(cx, "the sign-in offered again", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: Some(_), .. }
            )
        });
        never(cx, "the target said to have given no token", |cx| {
            connection.read_with(cx, |c, _| c.token_unread.is_some())
        });
        assert_eq!(refreshes(), 2, "the second ask joined the refresh");
    }

    /// A sign-in under way keeps saying so while the one it replaces answers a renewal: Sign
    /// out, which would end that one mid-sign-in, is not offered.
    #[gpui::test]
    async fn a_sign_in_under_way_keeps_saying_so_as_the_one_it_replaces_renews(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        // Tokens that live two seconds are renewed after one.
        let served = Arc::new(Mutex::new(Served {
            lives: 2,
            ..Served::default()
        }));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain, cx);
        let _watch = connection.update(cx, |c, cx| c.watch(cx));
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        let issued = served.lock().unwrap().issued;
        let lease = connection.read_with(cx, |c, _| c.lease_ends());
        connection.update(cx, |c, cx| c.sign_in(cx));
        std::thread::sleep(Duration::from_millis(1100));
        cx.executor().advance_clock(Duration::from_secs(2));
        until(cx, "the renewal", |_| {
            served.lock().unwrap().issued > issued
        });
        until(cx, "the renewal taken", |cx| {
            connection
                .read_with(cx, |c, _| c.lease_ends())
                .is_some_and(|ends| Some(ends) != lease)
        });
        assert!(
            matches!(sign_in_state(&connection, cx).1, SignIn::Waiting { .. }),
            "{:?}",
            sign_in_state(&connection, cx).1
        );
    }

    /// A sign-in held that the service refuses for good while another is under way ends as the
    /// one replaced: its token is revoked, the state shown stays the sign-in under way's, and that
    /// one's token is what the keychain keeps. Before, it ended as dropped and kept its token,
    /// which the new sign-in's item then replaced, leaving a token the service honours with
    /// nobody.
    #[gpui::test]
    async fn a_sign_in_refused_while_another_is_under_way_is_replaced(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        let key = key("work");
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
        connection.update(cx, |c, cx| c.sign_in(cx));
        let opened = cx.opened_url().unwrap();
        {
            let mut served = served.lock().unwrap();
            served.admits_nobody = true;
            served.refuses_target = true;
        }
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the held sign-in's revocation", |_| {
            logged(&served, "revoke r1")
        });
        assert!(matches!(
            sign_in_state(&connection, cx).1,
            SignIn::Waiting { .. }
        ));
        {
            let mut served = served.lock().unwrap();
            served.admits_nobody = false;
            served.refuses_target = false;
        }
        let browser = approve(&served, &opened);
        until(cx, "the new sign-in", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::SignedIn { .. })
                && sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        browser.join().unwrap();
        assert_eq!(keychain.items.lock().unwrap()[&key].1, b"r2".to_vec());
        assert_eq!(kept(&connection, cx), Kept::This);
    }

    /// A sign-in held over an item the keychain would not delete, refused for good while another
    /// is under way, leaves that item said: the sign-in under way, cancelled, does not take it,
    /// and says why the held one ended.
    #[gpui::test]
    async fn an_item_left_stays_said_when_the_sign_in_replacing_it_is_cancelled(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = Arc::new(MemoryKeychain::default());
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the offer", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
        });
        // A sign-in the keychain keeps nothing of, over an earlier item it will not delete.
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key("work"), ("user_9".into(), b"earlier".to_vec()));
        keychain.refuses_writes.store(true, Ordering::SeqCst);
        keychain.refuses_deletes.store(true, Ordering::SeqCst);
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in over the earlier item", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
                && matches!(kept(&connection, cx), Kept::Left(_))
        });
        browser.join().unwrap();
        // Another sign-in waits on the browser while the held one is refused for good.
        connection.update(cx, |c, cx| c.sign_in(cx));
        assert!(matches!(
            sign_in_state(&connection, cx).1,
            SignIn::Waiting { .. }
        ));
        {
            let mut served = served.lock().unwrap();
            served.admits_nobody = true;
            served.refuses_target = true;
        }
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the held sign-in's revocation", |_| {
            logged(&served, "revoke r1")
        });
        let left = |cx: &mut TestAppContext| connection.read_with(cx, |c, _| c.keychain_left());
        assert!(matches!(
            sign_in_state(&connection, cx).1,
            SignIn::Waiting { .. }
        ));
        assert_eq!(left(cx).as_deref(), Some("the keychain refused"));
        connection.update(cx, |c, cx| c.cancel_sign_in(cx));
        assert_eq!(left(cx).as_deref(), Some("the keychain refused"));
        // And why the held sign-in ended, which the sign-in under way kept.
        assert!(matches!(
            sign_in_state(&connection, cx).1,
            SignIn::Offered { why: Some(why), .. } if why.contains("does not issue tokens")
        ));
    }

    /// A sign-in as someone else ends with a Cancel or a sign-out: Cancel says the sign-in still
    /// held again, and after a sign-out nothing of the sign-in under way finishes. Before, Cancel
    /// left the dialog offering a sign-in with no Sign out, and a sign-out left the sign-in under
    /// way to finish after it, its keychain work behind the sign-out's.
    #[gpui::test]
    async fn a_sign_in_as_someone_else_ends_with_a_cancel_or_a_sign_out(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain, cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });

        connection.update(cx, |c, cx| c.sign_in(cx));
        assert!(matches!(
            sign_in_state(&connection, cx).1,
            SignIn::Waiting { .. }
        ));
        connection.update(cx, |c, cx| c.cancel_sign_in(cx));
        assert!(
            matches!(sign_in_state(&connection, cx).1, SignIn::SignedIn { .. }),
            "{:?}",
            sign_in_state(&connection, cx).1
        );

        connection.update(cx, |c, cx| c.sign_in(cx));
        let opened = cx.opened_url().unwrap();
        connection.update(cx, |c, cx| c.sign_out(cx));
        cx.run_until_parked();
        // The browser answers after the sign-out: nobody listens any more.
        let browser = approve(&served, &opened);
        until(cx, "the browser's request", |_| browser.is_finished());
        browser.join().ok();
        cx.run_until_parked();
        assert!(
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. }),
            "{:?}",
            sign_in_state(&connection, cx).1
        );
        let exchanged = served
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|line| line.starts_with("token authorization_code"));
        assert!(
            !exchanged,
            "the sign-in under way finished after the sign-out"
        );
    }

    /// A window waiting for a kept sign-in's first token hears the token is ready when the person
    /// signs out meanwhile: the connection reads without one from then on. Before, the window
    /// waited until it was shown again.
    #[gpui::test]
    async fn a_sign_out_while_a_window_waits_says_the_token_is_ready(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            hold_token: true,
            ..Served::default()
        }));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain, cx);
        let readied = std::rc::Rc::new(std::cell::Cell::new(0));
        let heard = readied.clone();
        let _heard = cx.update(|cx| {
            cx.subscribe(
                &connection,
                move |_, event: &crate::connection::ConnectionEvent, _| {
                    if matches!(event, crate::connection::ConnectionEvent::TokenReady) {
                        heard.set(heard.get() + 1);
                    }
                },
            )
        });
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in's refresh", |_| {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|line| line.starts_with("token refresh_token"))
        });
        connection.update(cx, |c, cx| c.wait_for_token(cx));
        assert!(!connection.read_with(cx, |c, _| c.token_ready()));
        connection.update(cx, |c, cx| c.sign_out(cx));
        cx.run_until_parked();
        assert!(connection.read_with(cx, |c, _| c.token_ready()));
        assert_eq!(
            readied.get(),
            1,
            "the waiting window heard the token is ready"
        );
        served.lock().unwrap().hold_token = false;
        until(cx, "the sign-out's revocation", |_| {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|line| line.starts_with("revoke"))
        });
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
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the refresh", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        assert_eq!(cx.opened_url(), None);
        let key = key("work");
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
        let key = key("work");
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
        };
        let connection = connection(&served, keychain.clone(), cx);
        let _watch = connection.update(cx, |c, cx| c.watch(cx));
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the refresh and its retry", |_| refreshes() == 2);
        // The service logs the retry as it reads it; the connection takes its answer after,
        // over the socket, so the test waits for that answer and not for the log alone.
        until(cx, "the retry's answer", |cx| {
            connection.read_with(cx, |c, _| c.token_unanswered)
        });
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
        let served = Arc::new(Mutex::new(Served::default()));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        let key = key("work");
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

    /// A sign-in after one failed reads the sign-in service's details again before it opens the
    /// browser, as what they named may be what failed (spec 016, discovery), then signs in.
    /// Before, the details a failed sign-in used were used for every later one until Studio
    /// restarted.
    #[gpui::test]
    async fn a_sign_in_after_one_failed_reads_the_details_again(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            fails_tokens: 1,
            ..Served::default()
        }));
        let details = || {
            served
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|r| r.contains("oauth-protected-resource"))
                .count()
        };
        let connection = connection(&served, Arc::default(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the offer", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
        });
        assert_eq!(details(), 1);
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in failed", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: Some(_), .. }
            )
        });
        browser.join().unwrap();
        assert_eq!(
            details(),
            1,
            "a sign-in that failed reads nothing more by itself"
        );

        connection.update(cx, |c, cx| c.sign_in(cx));
        until(cx, "waiting for the browser", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Waiting { .. })
        });
        assert_eq!(details(), 2, "read again before the browser opened");
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        browser.join().unwrap();
    }

    /// The sign-in held that the service ends while the details are read again for a sign-in
    /// as someone else is said, should that sign-in not be taken: here, cancelled. Before, the
    /// reason was dropped when the details came, and the cancelled sign-in said nothing of it.
    #[gpui::test]
    async fn why_the_one_held_ended_while_the_details_are_read_again_is_said(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain, cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        // A sign-in as someone else fails, the one held standing.
        served.lock().unwrap().fails_tokens = 1;
        connection.update(cx, |c, cx| c.sign_in(cx));
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in as someone else failed", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: Some(_), .. }
            )
        });
        browser.join().unwrap();
        assert!(connection.read_with(cx, |c, _| c.holds_sign_in()));

        // The next reads the details again; meanwhile the service ends the one held.
        served.lock().unwrap().hold_metadata = true;
        connection.update(cx, |c, cx| c.sign_in(cx));
        assert_eq!(sign_in_state(&connection, cx).1, SignIn::Reading);
        served.lock().unwrap().refresh.clear();
        connection.update(cx, |c, cx| c.read_refused(cx));
        until(cx, "the one held ended", |cx| {
            !connection.read_with(cx, |c, _| c.holds_sign_in())
        });
        served.lock().unwrap().hold_metadata = false;
        until(cx, "waiting for the browser", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Waiting { .. })
        });
        connection.update(cx, |c, cx| c.cancel_sign_in(cx));
        match sign_in_state(&connection, cx).1 {
            SignIn::Offered { why: Some(why), .. } => {
                assert!(why.contains("Sign in again"), "{why}")
            }
            other => panic!("why the one held ended is not said: {other:?}"),
        }
    }

    /// A sign-in whose token ended while the machine slept, and whose service does not answer
    /// the renewal (the network not back yet), says it is not answering; the next health check,
    /// which comes as often as a ready server's, asks again, then less often while the service
    /// does not answer, and signs in once it does. Before, that check waited the two minutes of
    /// a failed one, and a check at the ready pace kept asking a service down for hours.
    #[gpui::test]
    async fn a_sign_in_not_answering_after_a_wake_is_asked_again_soon(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            lives: 2,
            ..Served::default()
        }));
        let keychain = kept_at_launch(&served);
        let connection = connection(&served, keychain, cx);
        let _watch = connection.update(cx, |c, cx| {
            c.connect(cx);
            c.watch(cx)
        });
        until(cx, "the kept sign-in", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        // Asleep past the token's end; awake before the network.
        served.lock().unwrap().token_down = true;
        std::thread::sleep(Duration::from_millis(2100));
        cx.executor().advance_clock(Duration::from_secs(2));
        until(cx, "the sign-in not answering", |cx| {
            sign_in_state(&connection, cx).2 == "Sign-in not answering"
        });
        let refreshes = || {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|l| l.starts_with("token refresh_token"))
                .count()
        };
        // Asked again a ready server's interval on, still not answered.
        let asked = refreshes();
        cx.executor()
            .advance_clock(crate::connection::DEFAULT_CHECK_INTERVAL);
        until(cx, "asked again", |cx| {
            refreshes() > asked && sign_in_state(&connection, cx).2 == "Sign-in not answering"
        });
        // Then twice as long: not asked at the same interval again.
        let asked = refreshes();
        cx.executor()
            .advance_clock(crate::connection::DEFAULT_CHECK_INTERVAL);
        for _ in 0..3 {
            never(cx, "asked again at the same interval", |_| {
                refreshes() > asked
            });
        }
        // The network is back: the check that comes then signs in again.
        served.lock().unwrap().token_down = false;
        cx.executor()
            .advance_clock(crate::connection::DEFAULT_CHECK_INTERVAL);
        until(cx, "signed in again", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
    }

    /// Sign-in details the server named that could not be read are read again: by Retry, which
    /// connects again, and by the health check that finds the server still asking for a
    /// sign-in. Before, "Sign-in unavailable" stood for the life of the connection, with no
    /// button, until the target was renamed or Studio restarted. Details read that Studio
    /// cannot use stand until Retry: before, every health check read them again, the status
    /// going through "Connecting" and "Reading how to sign in" each time.
    #[gpui::test]
    async fn sign_in_details_that_could_not_be_read_are_read_again(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            fails_metadata: 1,
            ..Served::default()
        }));
        let unavailable = |connection: &Entity<Connection>, cx: &mut TestAppContext| {
            matches!(sign_in_state(connection, cx).1, SignIn::Unavailable(_))
        };
        let offered = |connection: &Entity<Connection>, cx: &mut TestAppContext| {
            matches!(
                sign_in_state(connection, cx).1,
                SignIn::Offered { why: None, .. }
            )
        };

        // Retry.
        let retried = connection(&served, Arc::default(), cx);
        retried.update(cx, |c, cx| c.connect(cx));
        until(cx, "the details unavailable", |cx| {
            unavailable(&retried, cx)
        });
        retried.update(cx, |c, cx| c.connect(cx));
        until(cx, "a sign-in offered on Retry", |cx| offered(&retried, cx));

        // The health check.
        served.lock().unwrap().fails_metadata = 1;
        let checked = connection(&served, Arc::default(), cx);
        let _watch = checked.update(cx, |c, cx| {
            c.connect(cx);
            c.watch(cx)
        });
        until(cx, "the details unavailable", |cx| {
            unavailable(&checked, cx)
        });
        cx.executor()
            .advance_clock(crate::connection::FAILED_CHECK_INTERVAL);
        until(cx, "a sign-in offered at the next check", |cx| {
            offered(&checked, cx)
        });
        drop(_watch);

        // A sign-in service that does not accept Studio.
        let refusing = SignInContext {
            identities: ClientIdentities::default(),
            ..context(&served, Arc::default())
        };
        let lasting = connection_in(&served, refusing, cx);
        let _watch = lasting.update(cx, |c, cx| {
            c.connect(cx);
            c.watch(cx)
        });
        until(cx, "the sign-in refused", |cx| unavailable(&lasting, cx));
        let details_read = |served: &Arc<Mutex<Served>>| {
            served
                .lock()
                .unwrap()
                .requests
                .iter()
                .any(|request| request.contains("oauth-protected-resource"))
        };
        served.lock().unwrap().requests.clear();
        cx.executor()
            .advance_clock(crate::connection::FAILED_CHECK_INTERVAL);
        until(cx, "the next check", |_| {
            let served = served.lock().unwrap();
            served.requests.iter().any(|r| r.ends_with("/healthz"))
        });
        cx.run_until_parked();
        assert!(!details_read(&served), "the check reads nothing more");
        assert!(unavailable(&lasting, cx));
        lasting.update(cx, |c, cx| c.connect(cx));
        until(cx, "the details read again on Retry", |_| {
            details_read(&served)
        });
    }

    /// A sign-in read, or made in the browser, while the target's transport changes takes the
    /// new client: requests to the server's address, to a sign-in service there included, the
    /// code exchange of a browser sign-in under way among them, do not stay on the proxy or
    /// certificate authority the task started with. Before, the exchange went through the client
    /// the browser sign-in began with.
    #[gpui::test]
    async fn a_sign_in_finished_after_a_transport_change_takes_the_new_client(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
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
        until(cx, "the revocation", |_| logged(&served, "revoke r1"));
        let requests = served.lock().unwrap().requests.clone();
        // The exchange, under way when the transport changed, and what the sign-in kept take
        // the newest.
        assert!(
            requests.contains(&format!("third {SERVER}/oauth/token")),
            "{requests:?}"
        );
        assert!(
            requests.contains(&format!("third {SERVER}/oauth/revoke")),
            "{requests:?}"
        );
        for stale in [
            format!("first {SERVER}/oauth/token"),
            format!("next {SERVER}/oauth/token"),
            format!("first {SERVER}/oauth/revoke"),
            format!("next {SERVER}/oauth/revoke"),
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
            hold_token: true,
            ..Served::default()
        }));
        served.lock().unwrap().refresh.insert("kept".into());
        let service = discovered(&served);
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
        let key = key("work");
        assert_eq!(keychain.items.lock().unwrap()[&key].1, b"r1".to_vec());
    }

    /// The sign-in service the fake server names, found as a connection finds it.
    fn discovered(served: &Arc<Mutex<Served>>) -> Service {
        let http = fake(served.clone());
        let clients = Clients::new(url::Url::parse(SERVER).unwrap(), http.clone(), http);
        let identities = identities();
        let metadata = format!("{SERVER}/.well-known/oauth-protected-resource/mcp");
        smol::block_on(discover(clients, metadata, identities, None)).unwrap()
    }

    /// A delete is done when the next launch would read nothing at the key: macOS and Windows
    /// refuse to delete an item that is not there, and a launch takes a read that fails as
    /// nothing kept. Only an item still read back is a refusal.
    #[test]
    fn a_delete_is_done_when_the_next_launch_reads_nothing() {
        let keychain = MemoryKeychain::default();
        let forgot = |keychain: &MemoryKeychain| smol::block_on(forgotten(keychain, "key".into()));
        assert_eq!(forgot(&keychain), Kept::Nothing);
        keychain
            .items
            .lock()
            .unwrap()
            .insert("key".into(), ("user_1".into(), b"r1".to_vec()));
        keychain.refuses_deletes.store(true, Ordering::SeqCst);
        assert_eq!(
            forgot(&keychain),
            Kept::Left("the keychain refused".to_owned())
        );
        keychain.fails_reads.store(true, Ordering::SeqCst);
        assert_eq!(forgot(&keychain), Kept::Nothing);
        keychain.fails_reads.store(false, Ordering::SeqCst);
        keychain.refuses_deletes.store(false, Ordering::SeqCst);
        assert_eq!(forgot(&keychain), Kept::Nothing);
        assert!(keychain.items.lock().unwrap().is_empty());
    }

    /// A rotated refresh token the keychain will not keep: a sign-in it kept forgets this
    /// account's token from before and reads not kept, or says the token the keychain will not
    /// delete; an earlier sign-in it holds instead stays said, as the next launch reads that one.
    #[test]
    fn a_rotation_the_keychain_refuses_leaves_what_it_holds_said() {
        let served = Arc::new(Mutex::new(Served::default()));
        let service = discovered(&served);
        let keychain = Arc::new(MemoryKeychain::default());
        let key = key("work");
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key.clone(), ("user_1".into(), b"kept".to_vec()));
        keychain.refuses_writes.store(true, Ordering::SeqCst);
        let rotated_from = |source: OAuthSource, refresh: &str| {
            served.lock().unwrap().refresh.insert(refresh.to_owned());
            // The access token held is set aside, as after the server refused it: the next ask
            // refreshes.
            source.note_refused();
            let token = smol::block_on(source.token(Arc::default()));
            assert!(token.is_ok(), "{token:?}");
            assert!(
                !served.lock().unwrap().refresh.contains(refresh),
                "the service took {refresh} and rotated it"
            );
            source.kept_in_keychain()
        };

        let kept = OAuthSource::kept(
            service.clone(),
            "work",
            keychain.clone(),
            "user_1".into(),
            "kept".into(),
        );
        assert_eq!(rotated_from(kept, "kept"), Kept::Nothing);
        assert!(
            keychain.items.lock().unwrap().is_empty(),
            "the token from before is forgotten"
        );
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key.clone(), ("user_1".into(), b"kept-again".to_vec()));
        keychain.refuses_deletes.store(true, Ordering::SeqCst);
        let kept = OAuthSource::kept(
            service.clone(),
            "work",
            keychain.clone(),
            "user_1".into(),
            "kept-again".into(),
        );
        assert_eq!(
            rotated_from(kept, "kept-again"),
            Kept::Left("the keychain refused".into())
        );
        keychain.refuses_deletes.store(false, Ordering::SeqCst);

        let earlier = Kept::Left("the keychain refused".into());
        let over_earlier = OAuthSource::signed_in(
            service,
            "work",
            keychain.clone(),
            Tokens {
                access_token: "held".into(),
                refresh_token: Some("over-earlier".into()),
                expires_at: SystemTime::now() + Duration::from_secs(300),
                account: Account {
                    subject: "user_2".into(),
                    organization: None,
                },
            },
            earlier.clone(),
            None,
        );
        assert_eq!(rotated_from(over_earlier, "over-earlier"), earlier);
        assert_eq!(
            keychain.items.lock().unwrap()[&key].1,
            b"kept-again".to_vec()
        );
    }

    /// A sign-in whose grant gave no refresh token keeps its access token until that ends: its
    /// renewal, due before the end, has nothing to refresh with, so the token held stands, the
    /// next renewal due at its end, which ends the sign-in. Before, that renewal ended the
    /// sign-in while its token still held, up to a minute early.
    #[test]
    fn a_grant_without_a_refresh_token_stands_until_its_token_ends() {
        let served = Arc::new(Mutex::new(Served::default()));
        let expires_at = SystemTime::now() + Duration::from_secs(3);
        let source = OAuthSource::signed_in(
            discovered(&served),
            "work",
            Arc::new(MemoryKeychain::default()),
            Tokens {
                access_token: "held".into(),
                refresh_token: None,
                expires_at,
                account: Account {
                    subject: "user_1".into(),
                    organization: None,
                },
            },
            Kept::Nothing,
            None,
        );
        let ask = || smol::block_on(source.token(Arc::new(AtomicBool::new(false))));
        let first = ask().expect("the token given");
        let renew_at = first.lease().expect("a lease").renew_at;
        assert!(renew_at < expires_at, "renewed before it ends");
        std::thread::sleep(
            renew_at
                .duration_since(SystemTime::now())
                .unwrap_or_default()
                + Duration::from_millis(100),
        );
        assert!(
            SystemTime::now() < expires_at,
            "asked while the token holds"
        );
        let renewed = ask().expect("the token held stands until it ends");
        assert_eq!(renewed.lease().expect("a lease").renew_at, expires_at);
        assert_eq!(renewed.into_string(), "held");
        std::thread::sleep(
            expires_at
                .duration_since(SystemTime::now())
                .unwrap_or_default()
                + Duration::from_millis(100),
        );
        assert!(ask().is_err(), "ended with its token");
        assert!(
            served
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|r| !r.contains("token"))
        );
    }

    /// A grant the service ended forgets the keychain item, and one the keychain will not delete
    /// is said with the sign-out, as the next launch reads it.
    #[test]
    fn a_grant_the_service_ended_says_what_the_keychain_kept() {
        let served = Arc::new(Mutex::new(Served::default()));
        let service = discovered(&served);
        let keychain = Arc::new(MemoryKeychain::default());
        let key = key("work");
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key.clone(), ("user_1".into(), b"stale".to_vec()));
        keychain.refuses_deletes.store(true, Ordering::SeqCst);
        // The service no longer knows the kept token: its first ask is refused for good.
        let source = OAuthSource::kept(
            service,
            "work",
            keychain.clone(),
            "user_1".into(),
            "stale".into(),
        );
        let Err(ended) = smol::block_on(source.token(Arc::default())) else {
            panic!("the grant ended");
        };
        assert!(ended.why.starts_with("Signed out: "), "{}", ended.why);
        assert_eq!(
            source.kept_in_keychain(),
            Kept::Left("the keychain refused".into())
        );
        assert_eq!(keychain.items.lock().unwrap()[&key].1, b"stale".to_vec());
    }

    /// A sign-in dropped for the CLI's credential while a refresh is under way is decided once
    /// the refresh has settled: the drop gives the keychain the token the refresh rotated, so the
    /// next launch signs in with it; one the keychain will not take is forgotten, and the token,
    /// which nothing else holds, revoked. Before, the drop judged by what the keychain held before
    /// the refresh, and the rotated token was left with nobody.
    #[test]
    fn a_sign_in_dropped_during_a_refresh_is_decided_after_it() {
        // A drop while the service holds its answer to a refresh it has rotated: what the drop
        // says, and what the keychain and the service's log hold after.
        let dropped_during_refresh = |refuses_writes: bool| {
            let served = Arc::new(Mutex::new(Served {
                hold_token: true,
                ..Served::default()
            }));
            served.lock().unwrap().refresh.insert("kept".into());
            let service = discovered(&served);
            let keychain = Arc::new(MemoryKeychain::default());
            let key = key("work");
            keychain
                .items
                .lock()
                .unwrap()
                .insert(key.clone(), ("user_1".into(), b"kept".to_vec()));
            keychain
                .refuses_writes
                .store(refuses_writes, Ordering::SeqCst);
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
            let asked = smol::block_on(smol::future::or(
                async { Some(source.token(Arc::default()).await) },
                async {
                    while !rotated() {
                        smol::Timer::after(Duration::from_millis(10)).await;
                    }
                    None
                },
            ));
            assert!(asked.is_none(), "the ask was dropped mid-exchange");
            let dropped = source.end(End::Dropped, None);
            served.lock().unwrap().hold_token = false;
            let kept = smol::block_on(dropped);
            // A revocation, if any, follows in the background: waited for when one is due.
            let revoke_due = refuses_writes;
            for _ in 0..if revoke_due { 500 } else { 50 } {
                if served.lock().unwrap().log.contains(&"revoke r1".to_owned()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let held = keychain
                .items
                .lock()
                .unwrap()
                .get(&key)
                .map(|(_, secret)| String::from_utf8(secret.clone()).unwrap());
            let revoked = served.lock().unwrap().log.contains(&"revoke r1".to_owned());
            (kept, held, revoked)
        };
        // The keychain takes the rotated token: the next launch signs in with it.
        assert_eq!(
            dropped_during_refresh(false),
            (Kept::This, Some("r1".to_owned()), false)
        );
        // It refuses it: the token from before is forgotten, and the rotated one revoked.
        assert_eq!(dropped_during_refresh(true), (Kept::Nothing, None, true));
    }

    /// A sign-in replaced while its refresh is in flight ends at once, not after the service
    /// answers, and that refresh, answered after (`invalid_grant` here), leaves the item alone:
    /// it is the replacing sign-in's by then. Before, the replacement waited for the refresh (up
    /// to its request's 15 seconds and a retry) with the keychain's turn held, and a refresh the
    /// service ended deleted the item outside any turn, taking the new sign-in's.
    #[test]
    fn a_replaced_sign_in_waits_for_no_refresh_and_leaves_the_item_after() {
        let served = Arc::new(Mutex::new(Served {
            hold_token: true,
            ..Served::default()
        }));
        let keychain = Arc::new(MemoryKeychain::default());
        // "spent" is no grant the service holds: the refresh answers `invalid_grant`.
        let source = OAuthSource::kept(
            discovered(&served),
            "work",
            keychain.clone(),
            "user_1".into(),
            "spent".into(),
        );
        let key = source.key().to_owned();
        let asked = {
            let source = source.clone();
            smol::spawn(async move { source.token(Arc::default()).await })
        };
        while !served
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|line| line.starts_with("token refresh_token"))
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let ended = smol::block_on(smol::future::or(
            async { Some(source.end(End::Replaced, None).await) },
            async {
                smol::Timer::after(Duration::from_secs(2)).await;
                None
            },
        ));
        assert_eq!(
            ended,
            Some(Kept::This),
            "ended without waiting for the service"
        );
        // The sign-in replacing it keeps its own item; then the service answers.
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key.clone(), ("user_2".into(), b"theirs".to_vec()));
        served.lock().unwrap().hold_token = false;
        assert!(smol::block_on(asked).is_err());
        assert_eq!(
            keychain.items.lock().unwrap()[&key].1,
            b"theirs".to_vec(),
            "the item left to the sign-in replacing it"
        );
    }

    /// A drop decides by what the refresh in flight learns. One that answers `invalid_grant`, or
    /// rotates the token with tokens Studio refuses, leaves the item spent, and the drop forgets
    /// it rather than keep it for the next launch, which would be refused; before, the drop kept
    /// it. One that fails and is tried once more
    /// after the drop began still runs its retry, whose rotated token the drop keeps; before,
    /// the drop waited out the pause, the retry was skipped, and the drop kept the token from
    /// before.
    #[test]
    fn a_drop_decides_by_what_the_refresh_in_flight_learns() {
        let key = key("work");
        // The service's first answer is held until the drop has begun; `fails` fails it (503),
        // and the retry is answered at once.
        let dropped_during_refresh = |token: &str, granted: bool, fails: u32, lives: u64| {
            let served = Arc::new(Mutex::new(Served {
                lives,
                hold_token: true,
                fails_tokens: fails,
                ..Served::default()
            }));
            if granted {
                served.lock().unwrap().refresh.insert(token.to_owned());
            }
            let keychain = Arc::new(MemoryKeychain::default());
            keychain
                .items
                .lock()
                .unwrap()
                .insert(key.clone(), ("user_1".into(), token.as_bytes().to_vec()));
            let source = OAuthSource::kept(
                discovered(&served),
                "work",
                keychain.clone(),
                "user_1".into(),
                token.into(),
            );
            let asked = {
                let source = source.clone();
                smol::spawn(async move { source.token(Arc::default()).await })
            };
            while !served
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|line| line.starts_with("token refresh_token"))
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            let dropped = source.end(End::Dropped, None);
            served.lock().unwrap().hold_token = false;
            let kept = smol::block_on(dropped);
            assert!(smol::block_on(asked).is_err());
            let held = keychain
                .items
                .lock()
                .unwrap()
                .get(&key)
                .map(|(_, secret)| String::from_utf8(secret.clone()).unwrap());
            (kept, held)
        };
        // `invalid_grant`: the spent item is forgotten.
        assert_eq!(
            dropped_during_refresh("spent", false, 0, 300),
            (Kept::Nothing, None)
        );
        // A token rotated with tokens Studio refuses (living an hour): the spent item is
        // forgotten too.
        assert_eq!(
            dropped_during_refresh("kept", true, 0, 3600),
            (Kept::Nothing, None)
        );
        // A failed request, then the retry after the drop began: its rotation is kept.
        assert_eq!(
            dropped_during_refresh("kept", true, 1, 300),
            (Kept::This, Some("r1".to_owned()))
        );
    }

    /// A busy token endpoint (429) is a failure that may pass: the refresh is tried again, and
    /// the sign-in stands. Before, it was taken as a refusal for good and ended the sign-in.
    #[test]
    fn a_busy_token_endpoint_is_asked_again() {
        for (status, error) in [
            (429, "rate_limited"),
            (408, "request_timeout"),
            (400, "slow_down"),
            (400, "temporarily_unavailable"),
        ] {
            let served = Arc::new(Mutex::new(Served {
                fails_tokens: 1,
                fails_status: status,
                fails_error: error,
                ..Served::default()
            }));
            served.lock().unwrap().refresh.insert("kept".into());
            let source = OAuthSource::kept(
                discovered(&served),
                "work",
                Arc::new(MemoryKeychain::default()),
                "user_1".into(),
                "kept".into(),
            );
            assert!(
                smol::block_on(source.token(Arc::default())).is_ok(),
                "{status} {error}"
            );
            let refreshes = served
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|l| l.starts_with("token refresh_token"))
                .count();
            assert_eq!(
                refreshes, 2,
                "{status} {error}: the busy answer, then the retry"
            );
        }
    }

    /// An ask made while a refresh is under way joins it, and gets its answer: a refresh that
    /// fails is not begun again by every ask that waited for it, and one the server's refusal
    /// asked for meanwhile takes the new token the refresh under way gives. Before, a waiting
    /// ask could join only in the moment the refresh began; after it, it refreshed again.
    #[test]
    fn an_ask_joins_the_refresh_under_way() {
        let served = Arc::new(Mutex::new(Served {
            token_down: true,
            hold_token: true,
            ..Served::default()
        }));
        served.lock().unwrap().refresh.insert("kept".into());
        let source = OAuthSource::kept(
            discovered(&served),
            "work",
            Arc::new(MemoryKeychain::default()),
            "user_1".into(),
            "kept".into(),
        );
        let refreshes = || {
            served
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|l| l.starts_with("token refresh_token"))
                .count()
        };
        let first = {
            let source = source.clone();
            smol::spawn(async move { source.token(Arc::default()).await })
        };
        while refreshes() == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The second ask, polled once while the first's answer is held, so it is under way
        // before that refresh ends.
        let mut second = source.token(Arc::default());
        smol::block_on(async {
            assert!(futures::poll!(&mut second).is_pending());
        });
        served.lock().unwrap().hold_token = false;
        assert!(smol::block_on(first).is_err());
        assert!(smol::block_on(second).is_err());
        assert_eq!(refreshes(), 2, "one refresh and its retry, for both asks");

        // The server refuses the token held while a refresh is under way: an ask after that
        // takes the refresh's new token, and no second refresh begins.
        {
            let mut served = served.lock().unwrap();
            served.token_down = false;
            served.hold_token = true;
        }
        let first = {
            let source = source.clone();
            smol::spawn(async move { source.token(Arc::default()).await })
        };
        while refreshes() == 2 {
            std::thread::sleep(Duration::from_millis(5));
        }
        source.note_refused();
        let mut second = source.token(Arc::default());
        smol::block_on(async {
            assert!(futures::poll!(&mut second).is_pending());
        });
        served.lock().unwrap().hold_token = false;
        assert!(smol::block_on(first).is_ok());
        assert!(smol::block_on(second).is_ok());
        assert_eq!(
            refreshes(),
            3,
            "the second ask joined the refresh under way"
        );
    }

    /// A refresh the service answers after its sign-in was signed out keeps nothing in the
    /// keychain, so the sign-out's delete is its last word on it, and the sign-out revokes the
    /// rotated token, the one the service still honours. Before, the refresh, running while the
    /// sign-out waited for it, wrote its rotated token after the sign-out began, so a sign-in
    /// made meanwhile could find its own item replaced, then deleted.
    #[test]
    fn a_refresh_that_answers_after_its_sign_out_keeps_nothing() {
        let served = Arc::new(Mutex::new(Served {
            hold_token: true,
            ..Served::default()
        }));
        served.lock().unwrap().refresh.insert("kept".into());
        let service = discovered(&served);
        let keychain = Arc::new(MemoryKeychain::default());
        let key = key("work");
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key.clone(), ("user_1".into(), b"kept".to_vec()));
        // Deletes refused, so what the refresh wrote, if anything, stays to be seen.
        keychain.refuses_deletes.store(true, Ordering::SeqCst);
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
        // The ask is dropped once the service has rotated the token; its refresh runs on.
        let asked = smol::block_on(smol::future::or(
            async { Some(source.token(Arc::default()).await) },
            async {
                while !rotated() {
                    smol::Timer::after(Duration::from_millis(10)).await;
                }
                None
            },
        ));
        assert!(asked.is_none(), "the ask was dropped mid-exchange");
        let ending = source.end(End::SignOut, None);
        served.lock().unwrap().hold_token = false;
        assert_eq!(
            smol::block_on(ending),
            Kept::Left("the keychain refused".into())
        );
        assert_eq!(
            keychain.items.lock().unwrap()[&key].1,
            b"kept".to_vec(),
            "the rotated token was not written after the sign-out"
        );
        // The revocation follows, in the background.
        let revoked = || served.lock().unwrap().log.contains(&"revoke r1".to_owned());
        for _ in 0..500 {
            if revoked() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(revoked(), "{:?}", served.lock().unwrap().log);
        // It ends once: a second ending (the CLI's credential, say) answers with the first's.
        assert_eq!(
            smol::block_on(source.end(End::Dropped, None)),
            Kept::Left("the keychain refused".into())
        );
    }

    #[gpui::test]
    async fn a_sign_in_the_service_ended_or_the_server_does_not_admit_is_said(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        // A kept refresh token the service no longer honours: signed out, and forgotten.
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = Arc::new(MemoryKeychain::default());
        let key = key("work");
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
        let served = Arc::new(Mutex::new(Served::default()));
        let connection = connection(&served, Arc::default(), cx);
        connection.update(cx, |c, cx| {
            *c = std::mem::replace(
                c,
                Connection::new(c.server.clone(), None, fake(served.clone())).unwrap(),
            )
            .with_token_source(Some(Arc::new(Cli)));
            c.connect(cx);
        });
        cx.run_until_parked();
        let (state, offer, label) = sign_in_state(&connection, cx);
        assert_eq!(state, ConnectionState::Unauthorized);
        assert_eq!(offer, SignIn::NotOffered);
        assert_eq!(label, "Token refused");
    }

    /// A managed target's CLI login, as the CLI gives it: the data token it exchanges for the
    /// profile `work-profile`, which is in `organization`. Counts its asks.
    #[derive(Default)]
    struct Login {
        organization: Option<String>,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl TokenSource for Login {
        fn token(&self, _: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            futures::future::ready(Ok(Token::typed("from-the-cli".into()))).boxed()
        }

        fn login_profile(&self) -> Option<String> {
            Some("work-profile".into())
        }

        fn organization(&self) -> Option<String> {
            self.organization.clone()
        }
    }

    impl Login {
        fn in_organization(organization: &str) -> Arc<Self> {
            Arc::new(Self {
                organization: Some(organization.into()),
                ..Self::default()
            })
        }

        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    /// A connection to the managed target `work`, whose CLI login is `login`.
    fn managed(
        served: &Arc<Mutex<Served>>,
        keychain: Arc<MemoryKeychain>,
        login: Arc<Login>,
        cx: &mut TestAppContext,
    ) -> Entity<Connection> {
        let connection = connection(served, keychain, cx);
        connection.update(cx, |c, _| {
            *c = std::mem::replace(
                c,
                Connection::new(c.server.clone(), None, fake(served.clone())).unwrap(),
            )
            .with_token_source(Some(login));
        });
        connection
    }

    /// A managed cluster that offers a sign-in is signed in to as any server that offers one
    /// (spec 016, When Studio signs in): its first request carries no token, and the CLI's login
    /// is never asked, as Studio is a client of its own there. A kept sign-in is refreshed.
    #[gpui::test]
    async fn a_managed_target_signs_in_where_its_cluster_offers_it(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let login = Login::in_organization("org_1");
        let connection = managed(&served, kept_at_launch(&served), login.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the refresh", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        assert_eq!(login.asked(), 0, "the CLI's login is not asked");
        assert_eq!(cx.opened_url(), None);
        connection.read_with(cx, |c, _| {
            assert!(c.holds_sign_in());
            assert_eq!(
                c.login_profile(),
                None,
                "the connection is no CLI profile's"
            );
        });
        assert!(served.lock().unwrap().refused.is_empty());

        // With nothing kept, the cluster asks for a sign-in, and still not the CLI's login.
        let served = Arc::new(Mutex::new(Served::default()));
        let login = Login::in_organization("org_1");
        let connection = managed(&served, Arc::default(), login.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the offer", |cx| {
            matches!(sign_in_state(&connection, cx).1, SignIn::Offered { .. })
        });
        assert_eq!(sign_in_state(&connection, cx).2, "Sign in needed");
        assert_eq!(login.asked(), 0);
    }

    /// A managed cluster whose 401 offers no sign-in is reached with the CLI's login, the interim
    /// before clusters offer one (spec 016): its token is asked for once the server's answer says
    /// so, and from then on at once, without a request that carries none.
    #[gpui::test]
    async fn a_managed_target_uses_the_clis_login_where_its_cluster_offers_no_sign_in(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            offers_no_sign_in: true,
            accepted: HashSet::from(["from-the-cli".to_owned()]),
            ..Served::default()
        }));
        let login = Login::in_organization("org_1");
        let connection = managed(&served, Arc::default(), login.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the connect", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        assert_eq!(login.asked(), 1);
        let (_, offer, _) = sign_in_state(&connection, cx);
        assert_eq!(offer, SignIn::NotOffered);
        assert_eq!(
            connection.read_with(cx, |c, _| c.login_profile()),
            Some("work-profile".into())
        );
        let lists = |served: &Arc<Mutex<Served>>| {
            served
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|request| request.ends_with("/graphs"))
                .count()
        };
        assert_eq!(
            lists(&served),
            2,
            "one without a token, then one with the CLI's"
        );

        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the second connect", |cx| {
            sign_in_state(&connection, cx).0 == ConnectionState::Ready
        });
        assert_eq!(login.asked(), 2);
        assert_eq!(lists(&served), 3, "the CLI's token is asked for first now");
    }

    /// A sign-in to a managed target is in the organization its CLI profile names: a token for
    /// another is refused before it is used, and said.
    #[gpui::test]
    async fn a_sign_in_to_a_managed_target_must_be_in_its_organization(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let login = Login::in_organization("org_2");
        let connection = managed(&served, kept_at_launch(&served), login.clone(), cx);
        connection.update(cx, |c, cx| c.connect(cx));
        until(cx, "the refused refresh", |cx| {
            matches!(
                sign_in_state(&connection, cx).1,
                SignIn::Offered { why: Some(_), .. }
            )
        });
        let (state, offer, _) = sign_in_state(&connection, cx);
        assert_eq!(state, ConnectionState::Unauthorized);
        let SignIn::Offered { why: Some(why), .. } = offer else {
            unreachable!()
        };
        assert!(
            why.contains("The sign-in was for org_1, and this target is in org_2."),
            "{why}"
        );
        assert_eq!(login.asked(), 0);
        assert!(
            served.lock().unwrap().refused.is_empty(),
            "the token was never sent"
        );
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
            turns: KeychainTurns::default(),
            identities: identities(),
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
            matches!(&signed_in, SignIn::SignedIn { account, .. } if account == "user_alice"),
            "{signed_in:?}"
        );
        assert_eq!(kept(&connection, cx), Kept::This);
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
