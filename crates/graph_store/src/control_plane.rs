//! A managed control plane, as a CLI profile names it (spec 021): its sign-in, which Studio makes
//! as its own OAuth client with the control plane as the resource, and the organization's
//! clusters, read while something shows them.
//!
//! One per distinct route and organization: two profiles on the same pair share it. Nothing is
//! sent to the route until a sign-in is held; the list is read when asked, never on a timer, and
//! held only until it is read again.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use control_client::{
    ClusterDetail, ClusterList, ClusterStatus, ControlClient, ControlError, Envelope,
};
use gpui::{App, AppContext as _, Context, Entity, Task};
use http_client::HttpClient;
use oauth_client::{Clients, Refusal};

use crate::cli_tokens::TokenSource as _;
use crate::control_token::PlaneHandle;
use crate::sign_in::{self, End, Kept, OAuthSource, Service, SignIn, SignInContext};

/// What the control plane last answered for the organization's clusters.
#[derive(Debug, Clone, PartialEq)]
pub enum Listing {
    /// Being read.
    Reading,
    Listed(Envelope<ClusterList>),
    Failed(ControlError),
}

pub struct ControlPlane {
    /// Its origin, `owner.route` as the CLI says it.
    pub route: String,
    /// The organization a CLI profile pins it to; empty for Omnigraph Cloud's default, whose
    /// sign-in's token names one ([`Self::organization`]).
    pub organization: String,
    /// Omnigraph Cloud, offered whether or not a CLI profile names it (`omnigraph_cloud`).
    is_default: bool,
    /// The CLI profiles that name it.
    pub profiles: Vec<String>,
    context: Option<SignInContext>,
    service: Option<Service>,
    sign_in: SignIn,
    oauth: Option<OAuthSource>,
    /// The sign-in, or a launch's read of its keychain item, under way.
    signing_in: Option<Task<()>>,
    /// The next sign-in asks which organization (`prompt=login`): the last one failed (another
    /// organization's token among the reasons) or was signed out of. AuthKit asks only as the
    /// person signs in, so otherwise a signed-in browser gives its organization at once.
    ask_organization: bool,
    listing: Option<Listing>,
    reading: Option<Task<()>>,
    /// What its clusters' token sources read its sign-in through.
    handle: Option<PlaneHandle>,
}

impl ControlPlane {
    /// The control plane at `route` for `organization`, signing in with `context`. A sign-in the
    /// keychain kept is taken up at once; nothing is sent to the route before one is held.
    pub fn new(
        route: String,
        organization: String,
        profiles: Vec<String>,
        context: Option<SignInContext>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            sign_in: SignIn::Offered {
                service: oauth_client::host_of(&route),
                why: None,
            },
            route,
            organization,
            is_default: false,
            profiles,
            context,
            service: None,
            oauth: None,
            signing_in: None,
            ask_organization: false,
            listing: None,
            reading: None,
            handle: None,
        };
        this.handle = this
            .context
            .as_ref()
            .map(|context| PlaneHandle::new(this.route.clone(), context.elsewhere.clone(), true));
        this.take_up_kept(cx);
        this
    }

    /// What a cluster's data token source reads its sign-in through ([`ControlTokenSource`]):
    /// `None` without a sign-in context, as in tests of other crates.
    ///
    /// [`ControlTokenSource`]: crate::control_token::ControlTokenSource
    pub fn handle(&self) -> Option<PlaneHandle> {
        self.handle.clone()
    }

    /// Holds `source` as its sign-in, for itself and its clusters' token sources.
    fn hold(&mut self, source: Option<OAuthSource>) {
        if let Some(handle) = &self.handle {
            handle.hold(source.clone());
        }
        self.oauth = source;
    }

    /// Omnigraph Cloud's control plane at `route`, which no profile pins to an organization: its
    /// sign-in's token names one (spec 021, Omnigraph Cloud by default).
    pub fn cloud(route: String, context: Option<SignInContext>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(route, String::new(), Vec::new(), None, cx);
        this.is_default = true;
        this.context = context;
        this.handle = this
            .context
            .as_ref()
            .map(|context| PlaneHandle::new(this.route.clone(), context.elsewhere.clone(), false));
        this.take_up_kept(cx);
        this
    }

    /// Whether the next sign-in asks which organization, at the cost of a sign-in on the web.
    pub fn asks_organization(&self) -> bool {
        self.ask_organization
    }

    /// Whether it is Omnigraph Cloud's default.
    pub fn is_default(&self) -> bool {
        self.is_default
    }

    /// Its organization: the one a profile pins, else the one its sign-in's token names.
    pub fn organization(&self) -> Option<String> {
        if !self.organization.is_empty() {
            return Some(self.organization.clone());
        }
        self.oauth.as_ref().and_then(OAuthSource::organization)
    }

    /// The keychain item of its sign-in (spec 021, Signing in): one per route and organization,
    /// and one for Omnigraph Cloud's default.
    pub fn key(&self) -> String {
        if self.is_default {
            cloud_key(&self.route)
        } else {
            key(&self.route, &self.organization)
        }
    }

    pub fn sign_in_state(&self) -> &SignIn {
        &self.sign_in
    }

    pub fn holds_sign_in(&self) -> bool {
        self.oauth.is_some()
    }

    /// The clusters last read, while something shows them.
    pub fn listing(&self) -> Option<&Listing> {
        self.listing.as_ref()
    }

    /// What the keychain holds for it.
    pub fn keychain(&self) -> Kept {
        self.oauth
            .as_ref()
            .map_or(Kept::Nothing, OAuthSource::kept_in_keychain)
    }

    /// Reads the item a sign-in left at launch, in its keychain turn, and holds it: the details
    /// it needs are read only once there is one.
    fn take_up_kept(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self.context.clone() else {
            return;
        };
        let key = self.key();
        let route = self.route.clone();
        let organization = self.organization.clone();
        self.signing_in = Some(cx.spawn(async move |this, cx| {
            let (before, done) = context.turns.next(&key);
            if let Some(before) = before {
                before.await;
            }
            let item = context.keychain.read(key.clone()).await.ok().flatten();
            done.send(()).ok();
            let Some((subject, secret)) = item else {
                return;
            };
            let Ok(refresh) = String::from_utf8(secret) else {
                return;
            };
            let found = discover(&context, &route, pinned(&organization)).await;
            this.update(cx, |this, cx| {
                this.signing_in = None;
                match found {
                    Ok(service) => {
                        let source = OAuthSource::kept_at(
                            service.clone(),
                            key,
                            context.keychain.clone(),
                            subject.clone(),
                            refresh,
                        );
                        this.sign_in = SignIn::SignedIn {
                            service: service.host(),
                            account: subject,
                        };
                        this.service = Some(service);
                        this.hold(Some(source));
                    }
                    Err(refusal) => {
                        this.sign_in = SignIn::Offered {
                            service: oauth_client::host_of(&this.route),
                            why: Some(refusal.why),
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Signs in through the person's browser, as they asked (Sign in on the profile's row):
    /// reads the control plane's sign-in details first when none are held.
    pub fn sign_in(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.sign_in,
            SignIn::Reading | SignIn::Waiting { .. } | SignIn::Finishing { .. }
        ) {
            return;
        }
        let Some(context) = self.context.clone() else {
            return;
        };
        let Some(service) = self.service.clone() else {
            self.sign_in = SignIn::Reading;
            cx.notify();
            let route = self.route.clone();
            let organization = self.organization.clone();
            self.signing_in = Some(cx.spawn(async move |this, cx| {
                let found = discover(&context, &route, pinned(&organization)).await;
                this.update(cx, |this, cx| {
                    this.signing_in = None;
                    match found {
                        Ok(service) => {
                            this.service = Some(service);
                            this.sign_in = SignIn::Offered {
                                service: oauth_client::host_of(&this.route),
                                why: None,
                            };
                            this.sign_in(cx);
                        }
                        Err(refusal) => {
                            this.sign_in = SignIn::Unavailable(refusal);
                            cx.notify();
                        }
                    }
                })
                .ok();
            }));
            return;
        };
        // Asking which organization costs a full sign-in on the web: only when it is needed.
        let service = Service {
            prompt: self.ask_organization.then_some("login"),
            ..service
        };
        let browser = match sign_in::browser(
            &service,
            oauth_client::host_of(&self.route),
            cx.background_executor(),
        ) {
            Ok(browser) => browser,
            Err(why) => {
                self.sign_in = SignIn::Offered {
                    service: service.host(),
                    why: Some(why),
                };
                cx.notify();
                return;
            }
        };
        cx.open_url(browser.page.as_str());
        self.sign_in = SignIn::Waiting {
            service: service.host(),
        };
        cx.notify();
        let key = self.key();
        self.signing_in = Some(cx.spawn(async move |this, cx| {
            let tokens = browser.tokens.await;
            let (tokens, unclaimed) = match tokens {
                Ok(taken) => taken,
                Err(why) => {
                    this.update(cx, |this, cx| {
                        this.signing_in = None;
                        // What the details named may be what failed: the next sign-in reads them
                        // again, and asks which organization, as this one may have been another's.
                        this.service = None;
                        this.ask_organization = true;
                        this.sign_in = SignIn::Offered {
                            service: service.host(),
                            why: Some(why),
                        };
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            // Finishing from here, not to be cancelled: the sign-in it replaces ends, and the
            // keychain keeps this one, in the item's turn.
            let Ok((before, done, previous)) = this.update(cx, |this, cx| {
                this.sign_in = SignIn::Finishing {
                    service: service.host(),
                };
                cx.notify();
                let (before, done) = context.turns.next(&key);
                let previous = this.oauth.clone();
                this.hold(None);
                (before, done, previous)
            }) else {
                return;
            };
            if let Some(before) = before {
                before.await;
            }
            let (source, let_go) = sign_in::keep(
                context.keychain.clone(),
                key,
                previous,
                service.clone(),
                tokens,
                unclaimed,
            )
            .await;
            done.send(()).ok();
            if let Some(token) = let_go {
                source.revoke(token);
            }
            let account = source.account();
            let taken = this.update(cx, |this, cx| {
                this.signing_in = None;
                this.ask_organization = false;
                this.sign_in = SignIn::SignedIn {
                    service: service.host(),
                    account,
                };
                this.hold(Some(source.clone()));
                // What was listed was another account's.
                this.listing = None;
                cx.notify();
            });
            // Gone before it took the sign-in: nothing holds it, so it ends as a drop.
            if taken.is_err() {
                drop(source.end(End::Dropped, None));
            }
        }));
    }

    /// Stops a sign-in waiting for the browser; one finishing runs to its end.
    pub fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.sign_in, SignIn::Waiting { .. } | SignIn::Reading) {
            return;
        }
        self.signing_in = None;
        self.sign_in = SignIn::Offered {
            service: oauth_client::host_of(&self.route),
            why: None,
        };
        cx.notify();
    }

    /// Signs out: the keychain item is deleted and the refresh token revoked where the service
    /// publishes a revocation endpoint (spec 016, Custody), in the item's turn.
    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.end(End::SignOut, cx);
    }

    /// The CLI no longer names it: its sign-in stays in the keychain for the next launch.
    pub(crate) fn retire(&mut self, cx: &mut Context<Self>) {
        self.end(End::Dropped, cx);
    }

    fn end(&mut self, how: End, cx: &mut Context<Self>) {
        self.signing_in = None;
        // Signed out, the next sign-in may be to another organization: it asks.
        if how == End::SignOut {
            self.ask_organization = true;
        }
        self.reading = None;
        self.listing = None;
        let held = self.oauth.clone();
        self.hold(None);
        if let Some(source) = held {
            let before = self
                .context
                .as_ref()
                .and_then(|context| context.turns.next(&self.key()).0);
            drop(source.end(how, before));
        }
        self.sign_in = SignIn::Offered {
            service: oauth_client::host_of(&self.route),
            why: None,
        };
        cx.notify();
    }

    /// Reads the organization's clusters (the Open dialog opening, or Refresh): once, with one
    /// silent refresh of the access token after a 401 (spec 021, Errors). Without a sign-in held
    /// nothing is sent.
    pub fn read_clusters(&mut self, cx: &mut Context<Self>) {
        let (Some(source), Some(context)) = (self.oauth.clone(), self.context.clone()) else {
            return;
        };
        self.listing = Some(Listing::Reading);
        cx.notify();
        let route = self.route.clone();
        let http = context.elsewhere.clone();
        self.reading = Some(cx.spawn(async move |this, cx| {
            let mut answer = list(&http, &route, &source).await;
            if matches!(answer, Err(ControlError::Unauthenticated(_))) {
                source.note_refused();
                answer = list(&http, &route, &source).await;
            }
            this.update(cx, |this, cx| {
                this.reading = None;
                this.listing = Some(match answer {
                    Ok(listed) => Listing::Listed(listed),
                    Err(error) => Listing::Failed(error),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    /// Lets the list go: nothing shows it any more (spec 021, Listing clusters).
    pub fn forget_clusters(&mut self, cx: &mut Context<Self>) {
        if self.listing.take().is_some() {
            self.reading = None;
            cx.notify();
        }
    }
}

/// How often a cluster's status is read while its card shows (spec 021, Limits).
const STATUS_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// A cluster's state and latest delivery, read while the connection chip's card shows it: once,
/// then every 30 seconds, until the card closes and lets this go (spec 021, A cluster's state).
pub struct ClusterCard {
    pub cluster_id: String,
    detail: Option<Result<Envelope<ClusterDetail>, ControlError>>,
    status: Option<Result<Envelope<ClusterStatus>, ControlError>>,
    _reading: Task<()>,
}

impl ClusterCard {
    /// The card's lines, as the person reads them: the cluster's state, then its latest
    /// delivery and when the control plane observed it ("as of …" when its answer is stale).
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        match &self.detail {
            Some(Ok(detail)) => lines.push(format!(
                "{} · {}",
                detail.data.cluster.name,
                sentence_case(&detail.data.cluster.state)
            )),
            Some(Err(error)) => lines.push(error.to_string()),
            None => lines.push("Reading the cluster's state…".to_owned()),
        }
        match &self.status {
            Some(Ok(status)) => {
                let observed = ago(&status.meta.observed_at);
                let line = match &status.data.delivery {
                    Some(delivery) => format!(
                        "Latest deployment {} · observed {observed}",
                        delivery.delivery.replace('_', " ")
                    ),
                    None => format!("No deployment yet · observed {observed}"),
                };
                lines.push(if status.meta.stale {
                    format!("{line} (as of {})", status.meta.observed_at)
                } else {
                    line
                });
            }
            Some(Err(error)) if self.detail.as_ref().is_some_and(Result::is_ok) => {
                lines.push(error.to_string())
            }
            _ => {}
        }
        lines
    }
}

impl ControlPlane {
    /// A card reading the cluster `cluster_id`'s state while it is held; `None` without a
    /// sign-in, so nothing is sent.
    pub fn card(
        plane: &Entity<Self>,
        cluster_id: String,
        cx: &mut App,
    ) -> Option<Entity<ClusterCard>> {
        let (source, route, http) = {
            let plane = plane.read(cx);
            (
                plane.oauth.clone()?,
                plane.route.clone(),
                plane.context.as_ref()?.elsewhere.clone(),
            )
        };
        Some(cx.new(|cx: &mut Context<ClusterCard>| {
            let id = cluster_id.clone();
            let reading = cx.spawn(async move |card, cx| {
                let detail = with_access(&http, &route, &source, |client| {
                    let id = id.clone();
                    async move { client.cluster(&id).await }
                })
                .await;
                if card
                    .update(cx, |card, cx| {
                        card.detail = Some(detail);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                loop {
                    let status = with_access(&http, &route, &source, |client| {
                        let id = id.clone();
                        async move { client.status(&id).await }
                    })
                    .await;
                    if card
                        .update(cx, |card, cx| {
                            card.status = Some(status);
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                    cx.background_executor().timer(STATUS_EVERY).await;
                }
            });
            ClusterCard {
                cluster_id,
                detail: None,
                status: None,
                _reading: reading,
            }
        }))
    }
}

/// One request with the sign-in's access token, and one silent refresh after a 401.
async fn with_access<T, F, Fut>(
    http: &Arc<dyn HttpClient>,
    route: &str,
    source: &OAuthSource,
    request: F,
) -> Result<T, ControlError>
where
    F: Fn(ControlClient) -> Fut,
    Fut: std::future::Future<Output = Result<T, ControlError>>,
{
    let mut refreshed = false;
    loop {
        let token = source
            .token(Arc::new(AtomicBool::new(false)))
            .await
            .map_err(|no| {
                if no.passing {
                    ControlError::Unavailable {
                        status: None,
                        why: no.why,
                    }
                } else {
                    ControlError::Unauthenticated(None)
                }
            })?;
        let answer = request(ControlClient::new(
            http.clone(),
            route,
            token.into_string(),
        )?)
        .await;
        match answer {
            Err(ControlError::Unauthenticated(_)) if !refreshed => {
                refreshed = true;
                source.note_refused();
            }
            answer => return answer,
        }
    }
}

/// `state` as the person reads it: the first letter up, `_` as a space.
fn sentence_case(state: &str) -> String {
    let words = state.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Unknown state".to_owned(),
    }
}

/// How long ago the RFC 3339 time `at` was, in words: "just now", "12 min ago", "3 h ago", or
/// the time as given when it does not parse.
fn ago(at: &str) -> String {
    let Ok(then) = chrono::DateTime::parse_from_rfc3339(at) else {
        return at.to_owned();
    };
    let seconds = (chrono::Utc::now() - then.with_timezone(&chrono::Utc)).num_seconds();
    match seconds {
        s if s < 60 => "just now".to_owned(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s => format!("{} d ago", s / 86_400),
    }
}

/// The keychain item of the sign-in to the control plane at `route` for `organization`.
pub fn key(route: &str, organization: &str) -> String {
    format!("omnigraph-control:{route}#{organization}")
}

/// The organization a sign-in must be in: a profile's, or none for Omnigraph Cloud's default.
fn pinned(organization: &str) -> Option<&str> {
    (!organization.is_empty()).then_some(organization)
}

/// The keychain item of Omnigraph Cloud's default sign-in at `route`.
pub fn cloud_key(route: &str) -> String {
    format!("omnigraph-control:{route}")
}

/// The clusters, read with the sign-in's access token.
async fn list(
    http: &Arc<dyn HttpClient>,
    route: &str,
    source: &OAuthSource,
) -> Result<Envelope<ClusterList>, ControlError> {
    let token = source
        .token(Arc::new(AtomicBool::new(false)))
        .await
        .map_err(|no| {
            if no.passing {
                ControlError::Unavailable {
                    status: None,
                    why: no.why,
                }
            } else {
                ControlError::Unauthenticated(None)
            }
        })?;
    ControlClient::new(http.clone(), route, token.into_string())?
        .clusters()
        .await
}

/// Reads the control plane's sign-in details (CP1.1: its protected-resource metadata, which names
/// itself as the resource) and its sign-in service's, through Studio's own client: a token from
/// it must be in `organization`.
async fn discover(
    context: &SignInContext,
    route: &str,
    organization: Option<&str>,
) -> Result<Service, Refusal> {
    let at = url::Url::parse(route).map_err(|error| Refusal::lasting(error.to_string()))?;
    let metadata = format!(
        "{}/.well-known/oauth-protected-resource",
        route.trim_end_matches('/')
    );
    let clients = Clients::new(at, context.elsewhere.clone(), context.elsewhere.clone());
    let mut service = sign_in::discover(
        clients,
        metadata,
        context.identities.clone(),
        organization.map(str::to_owned),
    )
    .await?;
    if service.resource.resource.trim_end_matches('/') != route.trim_end_matches('/') {
        return Err(Refusal::lasting(format!(
            "The control plane at {} names another resource ({}), so Studio does not sign in there.",
            oauth_client::host_of(route),
            oauth_client::shown(&service.resource.resource)
        )));
    }
    service.organization_of = "this profile";
    Ok(service)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use std::time::Duration;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use futures::AsyncReadExt as _;
    use gpui::{Entity, TestAppContext};
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};
    use oauth_client::ClientIdentities;
    use serde_json::{Value, json};

    use super::*;
    use crate::sign_in::{KeychainTurns, MemoryKeychain};

    const ROUTE: &str = "https://control.test";
    const ISSUER: &str = "https://id.test";
    const CLIENT: &str = "https://studio.test/client.json";
    const ORG: &str = "org_1";

    /// The control plane and its sign-in service: codes and refresh tokens are single-use, and
    /// each refresh rotates.
    #[derive(Default)]
    struct Served {
        codes: HashSet<String>,
        refresh: HashSet<String>,
        accepted: HashSet<String>,
        issued: u32,
        /// The organization the service signs people in to (`ORG` when empty).
        org: String,
        /// The resource the control plane's metadata names (`ROUTE` when empty).
        resource: String,
        requests: Vec<String>,
        log: Vec<String>,
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn jwt(claims: Value) -> String {
        format!(
            "{}.{}.sig",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    fn issue(served: &mut Served) -> Value {
        served.issued += 1;
        let org = if served.org.is_empty() {
            ORG
        } else {
            &served.org
        };
        let access = jwt(json!({
            "iss": ISSUER, "aud": ROUTE, "client_id": CLIENT, "sub": "user_1",
            "org_id": org, "iat": now(), "exp": now() + 300, "n": served.issued,
        }));
        served.accepted.insert(access.clone());
        let refresh = format!("r{}", served.issued);
        served.refresh.insert(refresh.clone());
        json!({"token_type": "Bearer", "access_token": access, "expires_in": 300, "refresh_token": refresh})
    }

    fn cluster() -> Value {
        json!({
            "cluster_id": "01C", "name": "acme", "incarnation": "01I",
            "canonical_root": "s3://b/c", "binding": {"mode": "github", "repository": "acme/g"},
            "state": "ready", "cell": {"namespace": "ns", "cluster": "cell"},
            "created_by": "user_1", "created_at": "2026-11-01T00:00:00Z",
            "data_endpoint": "https://acme.graphs.test",
        })
    }

    fn answer(
        served: &mut Served,
        method: &str,
        url: &str,
        bearer: Option<String>,
        body: String,
    ) -> (u16, Value) {
        let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        match (method, url) {
            ("GET", u) if u == format!("{ROUTE}/.well-known/oauth-protected-resource") => {
                let resource = if served.resource.is_empty() {
                    ROUTE.to_owned()
                } else {
                    served.resource.clone()
                };
                (
                    200,
                    json!({"resource": resource, "authorization_servers": [ISSUER], "bearer_methods_supported": ["header"]}),
                )
            }
            ("GET", u) if u == format!("{ISSUER}/.well-known/oauth-authorization-server") => (
                200,
                json!({
                    "issuer": ISSUER, "authorization_endpoint": format!("{ISSUER}/authorize"),
                    "token_endpoint": format!("{ISSUER}/token"),
                    "code_challenge_methods_supported": ["S256"], "client_id_metadata_document_supported": true,
                }),
            ),
            ("POST", u) if u == format!("{ISSUER}/token") => {
                served.log.push(format!(
                    "token {} resource={}",
                    form["grant_type"],
                    form.get("resource").map_or("-", String::as_str)
                ));
                let granted = match form["grant_type"].as_str() {
                    "authorization_code" => served.codes.remove(&form["code"]),
                    _ => served.refresh.remove(&form["refresh_token"]),
                };
                if granted
                    && form.get("resource").map(String::as_str) == Some(ROUTE)
                    && form["client_id"] == CLIENT
                {
                    (200, issue(served))
                } else {
                    (400, json!({"error": "invalid_grant"}))
                }
            }
            ("POST", u) if u == format!("{ROUTE}/v1/clusters/01C/tokens") => match bearer {
                Some(token) if served.accepted.contains(&token) => {
                    served.log.push(format!("data token {body}"));
                    let expires =
                        (chrono::Utc::now() + chrono::Duration::seconds(900)).to_rfc3339();
                    (
                        200,
                        json!({
                            "data": {"version": 2, "token": "data-token", "endpoint": "https://acme.graphs.test",
                                     "expires_at": expires, "kid": "k", "actor": "user_1"},
                            "meta": {"provenance": "service_db", "assurance": "observed", "stale": false, "observed_at": "2026-11-05T00:00:00Z"},
                        }),
                    )
                }
                _ => (
                    401,
                    json!({"type": "unauthenticated", "title": "Unauthenticated", "status": 401}),
                ),
            },
            ("GET", u)
                if u == format!("{ROUTE}/v1/clusters/01C")
                    && bearer.as_ref().is_some_and(|t| served.accepted.contains(t)) =>
            {
                let mut detail = cluster();
                detail["service_observation"] = json!({
                    "cluster_id": "01C", "incarnation": "01I", "lifecycle_provenance": "service_db",
                    "lifecycle_state": "ready", "latest_operation_id": null, "latest_operation_kind": null,
                    "latest_operation_state": null, "latest_operation_created_at": null, "deletion_id": null,
                    "undo_deadline": null, "cleanup_requires_zero_retention": null,
                    "health_provenance": "telemetry", "observed_at": null, "freshness": "fresh",
                    "serving_health": "ready", "backup": null, "observability_url": null,
                });
                served.log.push("detail".into());
                (
                    200,
                    json!({"data": detail, "meta": {"provenance": "service_db", "assurance": "observed", "stale": false, "observed_at": "2026-11-05T00:00:00Z"}}),
                )
            }
            ("GET", u)
                if u == format!("{ROUTE}/v1/clusters/01C/status")
                    && bearer.as_ref().is_some_and(|t| served.accepted.contains(t)) =>
            {
                served.log.push("status".into());
                (
                    200,
                    json!({
                        "data": {"cluster_id": "01C", "incarnation": "01I", "source": {}, "requested": null, "observed": null,
                                 "delivery": {"deployment_id": "d", "native_deployment_id": "n", "preview_id": "p",
                                              "delivery": "converged", "archive": "a", "first_attempt_deadline": "2026-11-05T00:00:00Z", "observation": {}}},
                        "meta": {"provenance": "service_db", "assurance": "observed", "stale": true, "observed_at": "2026-11-05T00:00:00Z"},
                    }),
                )
            }
            ("GET", u) if u == format!("{ROUTE}/v1/clusters") => match bearer {
                Some(token) if served.accepted.contains(&token) => (
                    200,
                    json!({
                        "data": {"clusters": [cluster()]},
                        "meta": {"provenance": "service_db", "assurance": "observed", "stale": false, "observed_at": "2026-11-05T00:00:00Z"},
                    }),
                ),
                _ => (
                    401,
                    json!({"type": "unauthenticated", "title": "Unauthenticated", "status": 401}),
                ),
            },
            _ => (404, json!({})),
        }
    }

    fn fake(served: Arc<Mutex<Served>>) -> Arc<dyn HttpClient> {
        FakeHttpClient::create(move |request| {
            let served = served.clone();
            async move {
                let method = request.method().to_string();
                let url = request.uri().to_string();
                let bearer = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .map(str::to_owned);
                let mut body = String::new();
                request.into_body().read_to_string(&mut body).await?;
                let (status, page) = {
                    let mut served = served.lock().unwrap();
                    served.requests.push(format!("{method} {url}"));
                    answer(&mut served, &method, &url, bearer, body)
                };
                Ok(Response::builder()
                    .status(status)
                    .body(AsyncBody::from(page.to_string()))?)
            }
        })
    }

    fn context(served: &Arc<Mutex<Served>>, keychain: Arc<MemoryKeychain>) -> SignInContext {
        SignInContext {
            elsewhere: fake(served.clone()),
            keychain,
            turns: KeychainTurns::default(),
            identities: ClientIdentities {
                metadata_document: Some(CLIENT.into()),
                registered: Vec::new(),
                development: None,
            },
        }
    }

    fn plane(context: SignInContext, cx: &mut TestAppContext) -> Entity<ControlPlane> {
        cx.new(|cx| {
            ControlPlane::new(
                ROUTE.into(),
                ORG.into(),
                vec!["work".into()],
                Some(context),
                cx,
            )
        })
    }

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

    /// Plays the browser: checks what Studio asked for, and returns to its listener.
    fn approve(served: &Arc<Mutex<Served>>, opened: &str) -> std::thread::JoinHandle<()> {
        let opened = url::Url::parse(opened).unwrap();
        let query: HashMap<String, String> = opened.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], CLIENT);
        assert_eq!(query["resource"], ROUTE);
        assert_eq!(query["code_challenge_method"], "S256");
        served.lock().unwrap().codes.insert("c1".into());
        let callback = url::Url::parse(&query["redirect_uri"]).unwrap();
        let path = format!("/callback?code=c1&state={}&iss={ISSUER}", query["state"]);
        let port = callback.port().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(stream, "GET {path} HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n").unwrap();
            let mut page = String::new();
            stream.read_to_string(&mut page).ok();
        })
    }

    fn signed_in(plane: &Entity<ControlPlane>, cx: &mut TestAppContext) -> bool {
        plane.read_with(cx, |p, _| {
            matches!(p.sign_in_state(), SignIn::SignedIn { .. })
        })
    }

    #[gpui::test]
    fn nothing_is_sent_to_the_control_plane_before_a_sign_in(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let plane = plane(context(&served, Arc::new(MemoryKeychain::default())), cx);
        cx.run_until_parked();
        plane.update(cx, |p, cx| p.read_clusters(cx));
        cx.run_until_parked();
        assert!(served.lock().unwrap().requests.is_empty());
        plane.read_with(cx, |p, _| {
            assert!(matches!(
                p.sign_in_state(),
                SignIn::Offered { why: None, .. }
            ));
            assert_eq!(p.listing(), None);
        });
    }

    #[gpui::test]
    fn a_sign_in_lists_the_organizations_clusters_and_is_kept(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let keychain = Arc::new(MemoryKeychain::default());
        let plane = plane(context(&served, keychain.clone()), cx);
        plane.update(cx, |p, cx| p.sign_in(cx));
        until(cx, "the browser", |cx| cx.opened_url().is_some());
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in", |cx| signed_in(&plane, cx));
        browser.join().unwrap();
        let item = keychain
            .items
            .lock()
            .unwrap()
            .get(&key(ROUTE, ORG))
            .cloned();
        assert_eq!(item, Some(("user_1".into(), b"r1".to_vec())));

        plane.update(cx, |p, cx| p.read_clusters(cx));
        until(cx, "the list", |cx| {
            plane.read_with(cx, |p, _| matches!(p.listing(), Some(Listing::Listed(_))))
        });
        plane.read_with(cx, |p, _| {
            let Some(Listing::Listed(listed)) = p.listing() else {
                unreachable!()
            };
            assert_eq!(listed.data.clusters[0].name, "acme");
        });
        assert!(
            served
                .lock()
                .unwrap()
                .log
                .contains(&format!("token authorization_code resource={ROUTE}"))
        );

        plane.update(cx, |p, cx| p.forget_clusters(cx));
        plane.read_with(cx, |p, _| assert_eq!(p.listing(), None));
    }

    #[gpui::test]
    fn a_sign_in_to_another_organization_is_refused_in_the_profiles_words(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            org: "org_2".into(),
            ..Default::default()
        }));
        let keychain = Arc::new(MemoryKeychain::default());
        let plane = plane(context(&served, keychain.clone()), cx);
        plane.update(cx, |p, cx| p.sign_in(cx));
        until(cx, "the browser", |cx| cx.opened_url().is_some());
        // A first sign-in asks nothing: a signed-in browser gives its organization at once.
        assert_eq!(prompt_of(&cx.opened_url().unwrap()), None);
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the refusal", |cx| {
            plane.read_with(cx, |p, _| {
                matches!(p.sign_in_state(), SignIn::Offered { why: Some(_), .. })
            })
        });
        browser.join().unwrap();
        plane.read_with(cx, |p, _| {
            let SignIn::Offered { why: Some(why), .. } = p.sign_in_state() else {
                unreachable!()
            };
            assert_eq!(
                why,
                "The sign-in was for org_2, and this profile is in org_1."
            );
            assert!(!p.holds_sign_in());
            assert!(
                p.asks_organization(),
                "the next sign-in asks which organization"
            );
        });
        assert!(keychain.items.lock().unwrap().is_empty());

        // The next asks which organization, and the person chooses the profile's.
        served.lock().unwrap().org = ORG.into();
        plane.update(cx, |p, cx| p.sign_in(cx));
        until(cx, "the browser again", |cx| {
            cx.opened_url().is_some_and(|url| prompt_of(&url).is_some())
        });
        let asked = cx.opened_url().unwrap();
        assert_eq!(prompt_of(&asked).as_deref(), Some("login"));
        let browser = approve(&served, &asked);
        until(cx, "the sign-in", |cx| signed_in(&plane, cx));
        browser.join().unwrap();
        plane.read_with(cx, |p, _| assert!(!p.asks_organization()));
    }

    /// The `prompt` a sign-in's page sends: none unless the organization is asked for.
    fn prompt_of(opened: &str) -> Option<String> {
        url::Url::parse(opened)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "prompt")
            .map(|(_, value)| value.into_owned())
    }

    #[gpui::test]
    fn a_control_plane_naming_another_resource_is_not_signed_in_to(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            resource: "https://elsewhere.test".into(),
            ..Default::default()
        }));
        let plane = plane(context(&served, Arc::new(MemoryKeychain::default())), cx);
        plane.update(cx, |p, cx| p.sign_in(cx));
        until(cx, "the refusal", |cx| {
            plane.read_with(cx, |p, _| {
                matches!(p.sign_in_state(), SignIn::Unavailable(_))
            })
        });
        assert!(cx.opened_url().is_none());
    }

    #[gpui::test]
    fn a_kept_sign_in_lists_after_a_silent_refresh(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key(ROUTE, ORG), ("user_1".into(), b"kept".to_vec()));
        let plane = plane(context(&served, keychain.clone()), cx);
        until(cx, "the kept sign-in", |cx| signed_in(&plane, cx));
        plane.update(cx, |p, cx| p.read_clusters(cx));
        until(cx, "the list", |cx| {
            plane.read_with(cx, |p, _| matches!(p.listing(), Some(Listing::Listed(_))))
        });
        // The kept token was rotated, and the keychain holds the new one.
        let item = keychain
            .items
            .lock()
            .unwrap()
            .get(&key(ROUTE, ORG))
            .cloned();
        assert_eq!(item, Some(("user_1".into(), b"r1".to_vec())));
    }

    /// The CLI's export, as a managed target's login.
    struct Cli;

    impl crate::cli_tokens::TokenSource for Cli {
        fn token(
            &self,
            _: Arc<AtomicBool>,
        ) -> futures::future::BoxFuture<
            'static,
            Result<crate::cli_tokens::Token, crate::cli_tokens::NoToken>,
        > {
            use futures::FutureExt as _;
            futures::future::ready(Ok(crate::cli_tokens::Token::typed("from-the-cli".into())))
                .boxed()
        }
    }

    #[gpui::test]
    fn a_held_sign_in_gives_the_clusters_data_tokens(cx: &mut TestAppContext) {
        use crate::control_token::ControlTokenSource;
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key(ROUTE, ORG), ("user_1".into(), b"kept".to_vec()));
        let plane = plane(context(&served, keychain), cx);
        until(cx, "the kept sign-in", |cx| signed_in(&plane, cx));
        let handle = plane.read_with(cx, |p, _| p.handle()).unwrap();
        let source = ControlTokenSource::new(
            Arc::new(Cli),
            vec![handle.clone()],
            "01C".into(),
            "https://acme.graphs.test".into(),
        );
        let token = smol::block_on(source.token(Arc::default())).unwrap();
        assert_eq!(token.lease().unwrap().holder, "user_1");
        assert_eq!(token.into_string(), "data-token");
        assert!(
            served
                .lock()
                .unwrap()
                .log
                .contains(&r#"data token {"version":2,"ttl_seconds":900}"#.to_owned())
        );

        // A token for another endpoint than the cluster's is dropped.
        let elsewhere = ControlTokenSource::new(
            Arc::new(Cli),
            vec![handle.clone()],
            "01C".into(),
            "https://other.graphs.test".into(),
        );
        let refused = smol::block_on(elsewhere.token(Arc::default())).unwrap_err();
        assert!(refused.why.contains("another address"), "{}", refused.why);

        // Refused by the cluster, the next is not asked for within the minute.
        source.note_refused();
        let paused = smol::block_on(source.token(Arc::default())).unwrap_err();
        assert!(paused.passing);

        // Signed out, the CLI's export is the source again.
        plane.update(cx, |p, cx| p.sign_out(cx));
        cx.run_until_parked();
        let token = smol::block_on(source.token(Arc::default())).unwrap();
        assert_eq!(token.into_string(), "from-the-cli");
    }

    #[gpui::test]
    fn a_clusters_card_reads_its_state_and_latest_deployment(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        let unsigned = plane(context(&served, Arc::new(MemoryKeychain::default())), cx);
        cx.run_until_parked();
        assert!(
            cx.update(|cx| ControlPlane::card(&unsigned, "01C".into(), cx))
                .is_none()
        );
        assert!(served.lock().unwrap().requests.is_empty());

        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key(ROUTE, ORG), ("user_1".into(), b"kept".to_vec()));
        let plane = plane(context(&served, keychain), cx);
        until(cx, "the kept sign-in", |cx| signed_in(&plane, cx));
        let card = cx
            .update(|cx| ControlPlane::card(&plane, "01C".into(), cx))
            .unwrap();
        until(cx, "the card", |cx| {
            card.read_with(cx, |card, _| card.lines().len() == 2)
        });
        let lines = card.read_with(cx, |card, _| card.lines());
        assert_eq!(lines[0], "acme · Ready");
        assert!(
            lines[1].starts_with("Latest deployment converged · observed "),
            "{}",
            lines[1]
        );
        assert!(
            lines[1].ends_with("(as of 2026-11-05T00:00:00Z)"),
            "{}",
            lines[1]
        );
        let log = served.lock().unwrap().log.clone();
        assert_eq!(log.iter().filter(|l| *l == "detail").count(), 1);
        assert_eq!(log.iter().filter(|l| *l == "status").count(), 1);
    }

    /// The CLI's export for a target in `0`'s organization.
    struct CliIn(&'static str);

    impl crate::cli_tokens::TokenSource for CliIn {
        fn token(
            &self,
            _: Arc<AtomicBool>,
        ) -> futures::future::BoxFuture<
            'static,
            Result<crate::cli_tokens::Token, crate::cli_tokens::NoToken>,
        > {
            use futures::FutureExt as _;
            futures::future::ready(Ok(crate::cli_tokens::Token::typed("from-the-cli".into())))
                .boxed()
        }

        fn login_profile(&self) -> Option<String> {
            Some("work".into())
        }

        fn organization(&self) -> Option<String> {
            Some(self.0.into())
        }
    }

    fn cloud(context: SignInContext, cx: &mut TestAppContext) -> Entity<ControlPlane> {
        cx.new(|cx| ControlPlane::cloud(ROUTE.into(), Some(context), cx))
    }

    /// Omnigraph Cloud's default pins no organization: its sign-in, in whichever one, is kept
    /// under the route alone, and the token names the organization.
    #[gpui::test]
    fn the_clouds_sign_in_learns_its_organization(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served {
            org: "org_2".into(),
            ..Default::default()
        }));
        let keychain = Arc::new(MemoryKeychain::default());
        let plane = cloud(context(&served, keychain.clone()), cx);
        cx.run_until_parked();
        assert!(served.lock().unwrap().requests.is_empty());
        plane.read_with(cx, |p, _| {
            assert!(p.is_default());
            assert_eq!(p.organization(), None);
            assert_eq!(p.key(), cloud_key(ROUTE));
        });
        plane.update(cx, |p, cx| p.sign_in(cx));
        until(cx, "the browser", |cx| cx.opened_url().is_some());
        let browser = approve(&served, &cx.opened_url().unwrap());
        until(cx, "the sign-in", |cx| signed_in(&plane, cx));
        browser.join().unwrap();
        plane.read_with(cx, |p, _| {
            assert_eq!(p.organization().as_deref(), Some("org_2"))
        });
        let item = keychain
            .items
            .lock()
            .unwrap()
            .get(&cloud_key(ROUTE))
            .cloned();
        assert_eq!(item, Some(("user_1".into(), b"r1".to_vec())));
    }

    /// A kept cloud sign-in is taken up at launch, and names its organization once refreshed.
    #[gpui::test]
    fn a_kept_cloud_sign_in_names_its_organization_once_refreshed(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        keychain
            .items
            .lock()
            .unwrap()
            .insert(cloud_key(ROUTE), ("user_1".into(), b"kept".to_vec()));
        let plane = cloud(context(&served, keychain), cx);
        until(cx, "the kept sign-in", |cx| signed_in(&plane, cx));
        plane.update(cx, |p, cx| p.read_clusters(cx));
        until(cx, "the list", |cx| {
            plane.read_with(cx, |p, _| matches!(p.listing(), Some(Listing::Listed(_))))
        });
        plane.read_with(cx, |p, _| {
            assert_eq!(p.organization().as_deref(), Some(ORG))
        });
    }

    /// A managed target takes the cloud sign-in's data token only in its own organization; in
    /// another, the CLI's export stands.
    #[gpui::test]
    fn the_cloud_gives_data_tokens_only_in_the_targets_organization(cx: &mut TestAppContext) {
        use crate::control_token::ControlTokenSource;
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        keychain
            .items
            .lock()
            .unwrap()
            .insert(cloud_key(ROUTE), ("user_1".into(), b"kept".to_vec()));
        let plane = cloud(context(&served, keychain), cx);
        until(cx, "the kept sign-in", |cx| signed_in(&plane, cx));
        let handle = plane.read_with(cx, |p, _| p.handle()).unwrap();
        let source = |org: &'static str| {
            ControlTokenSource::new(
                Arc::new(CliIn(org)),
                vec![handle.clone()],
                "01C".into(),
                "https://acme.graphs.test".into(),
            )
        };
        let theirs = smol::block_on(source("org_other").token(Arc::default())).unwrap();
        assert_eq!(theirs.into_string(), "from-the-cli");
        let ours = smol::block_on(source(ORG).token(Arc::default())).unwrap();
        assert_eq!(ours.into_string(), "data-token");
    }

    #[gpui::test]
    fn signing_out_forgets_the_sign_in_and_the_list(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let served = Arc::new(Mutex::new(Served::default()));
        served.lock().unwrap().refresh.insert("kept".into());
        let keychain = Arc::new(MemoryKeychain::default());
        keychain
            .items
            .lock()
            .unwrap()
            .insert(key(ROUTE, ORG), ("user_1".into(), b"kept".to_vec()));
        let plane = plane(context(&served, keychain.clone()), cx);
        until(cx, "the kept sign-in", |cx| signed_in(&plane, cx));
        plane.update(cx, |p, cx| p.sign_out(cx));
        until(cx, "the item deleted", |_| {
            keychain.items.lock().unwrap().is_empty()
        });
        plane.read_with(cx, |p, _| {
            assert!(!p.holds_sign_in());
            assert_eq!(p.listing(), None);
        });
        let before = served.lock().unwrap().requests.len();
        plane.update(cx, |p, cx| p.read_clusters(cx));
        cx.run_until_parked();
        assert_eq!(served.lock().unwrap().requests.len(), before);
    }
}
