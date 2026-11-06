//! One client per control plane and sign-in: its route and the access token it sends there.

use std::sync::Arc;
use std::time::Duration;

use futures::AsyncReadExt as _;
use http_client::http::{Method, Request, StatusCode};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy};
use serde::de::DeserializeOwned;
use url::{Host, Url};

use crate::api::{
    ClusterDetail, ClusterList, ClusterStatus, DataCredential, Envelope, IdentityRequest,
    OrganizationList, Problem,
};

/// The header a request names the organization it acts in: the token's consent names one
/// organization at a time, so a person's other organizations are named here.
pub const ORGANIZATION_HEADER: &str = "omnigraph-organization";

/// How long a data token Studio asks for lives, in seconds (spec 021, Decisions): removing a
/// person's admission does not retract a token, so the shorter it lives the sooner it holds.
pub const DATA_TOKEN_SECONDS: u32 = 900;

/// How long a request may take and how much of its answer is read (spec 021, Limits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl Limits {
    /// The cluster list, a cluster's detail and its status.
    pub const READ: Self = Self {
        timeout: Duration::from_secs(10),
        max_bytes: 2 * 1024 * 1024,
    };
    /// A data token.
    pub const TOKEN: Self = Self {
        timeout: Duration::from_secs(15),
        max_bytes: 64 * 1024,
    };
}

/// Why a request to the control plane gave no answer Studio uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// The route is not one Studio sends a token to.
    Route(String),
    /// 401: the token is expired or refused; a sign-in is needed.
    Unauthenticated(Option<Problem>),
    /// 403: the person's role does not allow this (`scope_missing`, …).
    Forbidden(Option<Problem>),
    /// 404: the cluster is not in the organization (any more).
    NotFound(Option<Problem>),
    /// Another refusal (4xx), as the control plane described it.
    Refused {
        status: u16,
        problem: Option<Problem>,
    },
    /// No answer, a server error, a timeout or too many requests: it may answer another time.
    /// Nothing here asks again; a data token whose answer was lost is asked for only by the
    /// next read that needs one.
    Unavailable { status: Option<u16>, why: String },
    /// A 2xx that is not `{data, meta}` as Studio reads it, an answer over its size cap, or a
    /// redirect, which Studio does not follow.
    Unreadable(String),
    /// A data token for another endpoint than the cluster's. The token was dropped.
    EndpointMismatch { expected: String, answered: String },
}

impl ControlError {
    /// Asked again later, it may go on.
    pub fn is_passing(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = |problem: &Option<Problem>| {
            problem
                .as_ref()
                .and_then(|p| p.detail.clone())
                .map(|d| format!(": {}", shown(&d)))
                .unwrap_or_default()
        };
        match self {
            Self::Route(why) => write!(f, "Studio does not send a sign-in to this control plane: {why}"),
            Self::Unauthenticated(_) => f.write_str("Sign in needed"),
            Self::Forbidden(problem) => {
                write!(f, "Your role in this organization does not allow this{}", detail(problem))
            }
            Self::NotFound(_) => f.write_str("This cluster is no longer in the organization"),
            Self::Refused { status, problem } => match problem {
                Some(problem) => write!(f, "The control plane refused this ({status}): {}", shown(&problem.title)),
                None => write!(f, "The control plane refused this ({status})"),
            },
            Self::Unavailable { .. } => f.write_str("Control plane unreachable"),
            Self::Unreadable(_) => {
                f.write_str("The control plane answered in a form Studio does not read")
            }
            Self::EndpointMismatch { .. } => f.write_str(
                "The control plane gave a token for another address than the cluster's; Studio dropped it",
            ),
        }
    }
}

impl std::error::Error for ControlError {}

/// A control plane's API, called with one access token.
#[derive(Clone)]
pub struct ControlClient {
    http: Arc<dyn HttpClient>,
    route: Url,
    access_token: String,
    /// The organization its requests act in, when they name one.
    organization: Option<String>,
}

impl std::fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlClient")
            .field("route", &self.route.as_str())
            .field("access_token", &"<withheld>")
            .field("organization", &self.organization)
            .finish()
    }
}

impl ControlClient {
    /// A client for the control plane at `route` (a profile's `owner.route`), sending
    /// `access_token`, a control-plane token and never a graph token. The route must be HTTPS,
    /// or HTTP on this machine, with no user, password, query or fragment.
    pub fn new(
        http: Arc<dyn HttpClient>,
        route: &str,
        access_token: String,
    ) -> Result<Self, ControlError> {
        let route = Url::parse(route).map_err(|error| ControlError::Route(error.to_string()))?;
        if !sendable(&route) || route.query().is_some() {
            return Err(ControlError::Route(
                "it must be an HTTPS address, or HTTP on this machine".into(),
            ));
        }
        Ok(Self {
            http,
            route,
            access_token,
            organization: None,
        })
    }

    /// Its requests act in `organization`, one the signed-in person is a member of, rather
    /// than the one the token's consent names.
    pub fn in_organization(mut self, organization: impl Into<String>) -> Self {
        self.organization = Some(organization.into());
        self
    }

    /// `GET /v1/organizations`: the organizations the signed-in person is an active member of,
    /// with their names.
    pub async fn organizations(&self) -> Result<Envelope<OrganizationList>, ControlError> {
        self.request(Method::GET, &["v1", "organizations"], None, Limits::READ)
            .await
    }

    pub fn route(&self) -> &Url {
        &self.route
    }

    /// `GET /v1/clusters`: the organization's clusters the person may see the status of.
    pub async fn clusters(&self) -> Result<Envelope<ClusterList>, ControlError> {
        self.request(Method::GET, &["v1", "clusters"], None, Limits::READ)
            .await
    }

    /// `GET /v1/clusters/{id}`: one cluster and what the service last observed of it.
    pub async fn cluster(&self, cluster_id: &str) -> Result<Envelope<ClusterDetail>, ControlError> {
        self.request(
            Method::GET,
            &["v1", "clusters", cluster_id],
            None,
            Limits::READ,
        )
        .await
    }

    /// `GET /v1/clusters/{id}/status`: the source, what was requested and the latest delivery.
    pub async fn status(&self, cluster_id: &str) -> Result<Envelope<ClusterStatus>, ControlError> {
        self.request(
            Method::GET,
            &["v1", "clusters", cluster_id, "status"],
            None,
            Limits::READ,
        )
        .await
    }

    /// `POST /v1/clusters/{id}/tokens`: a data token for the cluster, living
    /// [`DATA_TOKEN_SECONDS`]. Sent once and never again here. The answer's `endpoint` must be
    /// `data_endpoint`, the cluster's as the list answered it; otherwise the token is dropped.
    pub async fn data_token(
        &self,
        cluster_id: &str,
        data_endpoint: &str,
    ) -> Result<Envelope<DataCredential>, ControlError> {
        let body = serde_json::to_vec(&IdentityRequest {
            version: 2,
            ttl_seconds: Some(DATA_TOKEN_SECONDS.into()),
        })
        .map_err(|error| ControlError::Unreadable(error.to_string()))?;
        let answer: Envelope<DataCredential> = self
            .request(
                Method::POST,
                &["v1", "clusters", cluster_id, "tokens"],
                Some(body),
                Limits::TOKEN,
            )
            .await?;
        if !same_endpoint(&answer.data.endpoint, data_endpoint) {
            return Err(ControlError::EndpointMismatch {
                expected: data_endpoint.to_owned(),
                answered: shown(&answer.data.endpoint),
            });
        }
        Ok(answer)
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.route.clone();
        url.set_fragment(None);
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        url
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Vec<u8>>,
        limits: Limits,
    ) -> Result<Envelope<T>, ControlError> {
        let url = self.url(segments);
        let mut builder = Request::builder()
            .method(method)
            .uri(url.as_str())
            .header("accept", "application/json")
            .header("authorization", format!("Bearer {}", self.access_token))
            .follow_redirects(RedirectPolicy::NoFollow)
            .timeout(limits.timeout);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        if let Some(organization) = &self.organization {
            builder = builder.header(ORGANIZATION_HEADER, organization.as_str());
        }
        let request = builder
            .body(body.map(AsyncBody::from).unwrap_or_else(AsyncBody::empty))
            .map_err(|error| ControlError::Unreadable(error.to_string()))?;
        let response =
            self.http
                .send(request)
                .await
                .map_err(|error| ControlError::Unavailable {
                    status: None,
                    why: transport(&error),
                })?;
        let status = response.status();
        let bytes = read_capped(response.into_body(), limits.max_bytes).await?;
        if status.is_success() {
            let bytes = bytes.ok_or_else(|| {
                ControlError::Unreadable(format!("the answer is over {} bytes", limits.max_bytes))
            })?;
            return serde_json::from_slice(&bytes)
                .map_err(|error| ControlError::Unreadable(error.to_string()));
        }
        if status.is_redirection() {
            return Err(ControlError::Unreadable(
                "a redirect, which Studio does not follow".into(),
            ));
        }
        let problem = bytes
            .as_deref()
            .and_then(|bytes| serde_json::from_slice::<Problem>(bytes).ok());
        Err(match status {
            StatusCode::UNAUTHORIZED => ControlError::Unauthenticated(problem),
            StatusCode::FORBIDDEN => ControlError::Forbidden(problem),
            StatusCode::NOT_FOUND => ControlError::NotFound(problem),
            status
                if status.is_server_error()
                    || status == StatusCode::REQUEST_TIMEOUT
                    || status == StatusCode::TOO_MANY_REQUESTS =>
            {
                ControlError::Unavailable {
                    status: Some(status.as_u16()),
                    why: format!("it answered {}", status.as_u16()),
                }
            }
            status => ControlError::Refused {
                status: status.as_u16(),
                problem,
            },
        })
    }
}

/// The body, or `None` when it is longer than `max_bytes`, the rest unread.
async fn read_capped(
    mut body: AsyncBody,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, ControlError> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8 * 1024];
    loop {
        let read = body
            .read(&mut buffer)
            .await
            .map_err(|error| ControlError::Unavailable {
                status: None,
                why: transport(&error.into()),
            })?;
        if read == 0 {
            return Ok(Some(bytes));
        }
        if bytes.len() + read > max_bytes {
            return Ok(None);
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

/// Whether `url` names this machine.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// HTTPS, or HTTP to this machine, with no user name, password or fragment.
fn sendable(url: &Url) -> bool {
    (url.scheme() == "https" || (url.scheme() == "http" && is_loopback(url)))
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

/// Whether a token's `endpoint` is the cluster's `data_endpoint`: the same sendable origin and
/// path, a trailing `/` aside.
fn same_endpoint(answered: &str, expected: &str) -> bool {
    match (Url::parse(answered), Url::parse(expected)) {
        (Ok(answered), Ok(expected)) => {
            sendable(&answered)
                && answered.origin() == expected.origin()
                && answered.path().trim_end_matches('/') == expected.path().trim_end_matches('/')
                && answered.query() == expected.query()
        }
        _ => false,
    }
}

/// A value from the control plane shown to the person: control characters dropped, cut at 300
/// characters.
fn shown(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.chars().count() > 300 {
        clean.chars().take(299).chain(['…']).collect()
    } else {
        clean
    }
}

/// Why a request did not complete, as a class: never the request, which holds a token.
fn transport(error: &anyhow::Error) -> String {
    let all = error
        .chain()
        .map(|layer| layer.to_string())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if all.contains("timed out") || all.contains("timeout") {
        "the request timed out"
    } else if all.contains("connection refused") {
        "the connection was refused"
    } else if all.contains("dns error")
        || all.contains("lookup address")
        || all.contains("nodename nor servname")
        || all.contains("name or service not known")
    {
        "the name could not be resolved"
    } else if all.contains("certificate") || all.contains("tls") || all.contains("ssl") {
        "the secure connection failed"
    } else {
        "the connection failed"
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use http_client::FakeHttpClient;
    use http_client::http::Response;
    use serde_json::{Value, json};

    use super::*;

    const ROUTE: &str = "https://control.example";
    const ENDPOINT: &str = "https://acme.graphs.example";

    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        url: String,
        authorization: Option<String>,
        organization: Option<String>,
        body: Vec<u8>,
    }

    /// A fake answering every request with `status` and `body`, noting each request.
    fn client(status: u16, body: Value) -> (ControlClient, Arc<Mutex<Vec<Seen>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let noted = seen.clone();
        let http = FakeHttpClient::create(move |request| {
            let noted = noted.clone();
            let body = body.clone();
            async move {
                let method = request.method().to_string();
                let url = request.uri().to_string();
                let authorization = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let organization = request
                    .headers()
                    .get(ORGANIZATION_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let mut sent = Vec::new();
                request.into_body().read_to_end(&mut sent).await?;
                noted.lock().unwrap().push(Seen {
                    method,
                    url,
                    authorization,
                    organization,
                    body: sent,
                });
                Ok(Response::builder()
                    .status(status)
                    .body(AsyncBody::from(body.to_string()))?)
            }
        });
        (
            ControlClient::new(http, ROUTE, "access-token".into()).unwrap(),
            seen,
        )
    }

    fn meta() -> Value {
        json!({
            "provenance": "service_db",
            "assurance": "observed",
            "stale": false,
            "observed_at": "2026-11-05T00:00:00Z",
        })
    }

    fn cluster() -> Value {
        json!({
            "cluster_id": "01CLUSTER",
            "name": "acme",
            "incarnation": "01INC",
            "canonical_root": "s3://b/c",
            "binding": {"mode": "github", "repository": "acme/graph"},
            "state": "ready",
            "cell": {"namespace": "ns", "cluster": "cell"},
            "created_by": "user_1",
            "created_at": "2026-11-01T00:00:00Z",
            "data_endpoint": ENDPOINT,
            "mcp_url": format!("{ENDPOINT}/mcp"),
        })
    }

    fn credential(endpoint: &str) -> Value {
        json!({"data": {
            "version": 2,
            "token": "data-token",
            "endpoint": endpoint,
            "expires_at": "2026-11-05T00:15:00Z",
            "kid": "k1",
            "actor": "user_1",
        }, "meta": meta()})
    }

    #[test]
    fn the_list_sends_the_control_plane_token_to_its_route() {
        let (client, seen) = client(
            200,
            json!({"data": {"clusters": [cluster()]}, "meta": meta()}),
        );
        let answer = smol::block_on(client.clusters()).unwrap();
        assert_eq!(answer.data.clusters[0].name, "acme");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].url, "https://control.example/v1/clusters");
        assert_eq!(
            seen[0].authorization.as_deref(),
            Some("Bearer access-token")
        );
    }

    #[test]
    fn a_request_names_its_organization_only_when_told() {
        let (client, seen) = client(
            200,
            json!({"data": {"organizations": [
                {"organization_id": "org_1", "name": "Acme"},
                {"organization_id": "org_2", "name": null},
            ]}, "meta": meta()}),
        );
        let listed = smol::block_on(client.organizations()).unwrap();
        assert_eq!(listed.data.organizations[0].name.as_deref(), Some("Acme"));
        assert_eq!(listed.data.organizations[1].name, None);
        assert_eq!(
            seen.lock().unwrap()[0].url,
            "https://control.example/v1/organizations"
        );
        assert_eq!(seen.lock().unwrap()[0].organization, None);
        let in_two = client.in_organization("org_2");
        let _ = smol::block_on(in_two.clusters());
        assert_eq!(
            seen.lock().unwrap()[1].organization.as_deref(),
            Some("org_2")
        );
    }

    #[test]
    fn a_cluster_id_is_one_path_segment() {
        let (client, seen) = client(404, json!({}));
        let _ = smol::block_on(client.status("a/../b?c"));
        assert_eq!(
            seen.lock().unwrap()[0].url,
            "https://control.example/v1/clusters/a%2F..%2Fb%3Fc/status"
        );
    }

    #[test]
    fn a_data_token_is_asked_for_once_with_studios_lifetime() {
        let (client, seen) = client(200, credential(ENDPOINT));
        let answer = smol::block_on(client.data_token("01CLUSTER", ENDPOINT)).unwrap();
        assert_eq!(answer.data.token, "data-token");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "POST");
        assert_eq!(
            seen[0].url,
            "https://control.example/v1/clusters/01CLUSTER/tokens"
        );
        let body: Value = serde_json::from_slice(&seen[0].body).unwrap();
        assert_eq!(body, json!({"version": 2, "ttl_seconds": 900}));
    }

    #[test]
    fn a_data_token_for_another_endpoint_is_dropped() {
        for answered in [
            "https://elsewhere.example",
            "http://acme.graphs.example",
            "https://acme.graphs.example/other",
            "not a url",
        ] {
            let (client, _) = client(200, credential(answered));
            let error = smol::block_on(client.data_token("01CLUSTER", ENDPOINT)).unwrap_err();
            assert!(
                matches!(error, ControlError::EndpointMismatch { .. }),
                "{answered}: {error:?}"
            );
            assert!(!format!("{error:?}").contains("data-token"));
        }
        let (client, _) = client(200, credential("https://acme.graphs.example/"));
        assert!(smol::block_on(client.data_token("01CLUSTER", ENDPOINT)).is_ok());
    }

    #[test]
    fn refusals_say_what_to_do() {
        let problem = |kind: &str, status: u16| json!({"type": kind, "title": "Refused", "status": status, "detail": "needs cluster_read"});
        /// A status, its body, and what it must be read as.
        type Case = (u16, Value, fn(&ControlError) -> bool);
        let cases: [Case; 6] = [
            (401, problem("unauthenticated", 401), |e| {
                matches!(e, ControlError::Unauthenticated(Some(_)))
            }),
            (
                403,
                problem("scope_missing", 403),
                |e| matches!(e, ControlError::Forbidden(Some(p)) if p.r#type == "scope_missing"),
            ),
            (404, problem("not_found", 404), |e| {
                matches!(e, ControlError::NotFound(_))
            }),
            (409, problem("conflict", 409), |e| {
                matches!(
                    e,
                    ControlError::Refused {
                        status: 409,
                        problem: Some(_)
                    }
                )
            }),
            (503, json!("not json"), |e| e.is_passing()),
            (429, json!({}), |e| e.is_passing()),
        ];
        for (status, body, expected) in cases {
            let (client, _) = client(status, body);
            let error = smol::block_on(client.clusters()).unwrap_err();
            assert!(expected(&error), "{status}: {error:?}");
        }
        let (client, _) = client(403, problem("scope_missing", 403));
        let error = smol::block_on(client.clusters()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Your role in this organization does not allow this: needs cluster_read"
        );
    }

    #[test]
    fn an_answer_studio_does_not_read_is_said() {
        for body in [json!({"clusters": []}), json!({"data": {"clusters": []}})] {
            let (client, _) = client(200, body);
            let error = smol::block_on(client.clusters()).unwrap_err();
            assert!(matches!(error, ControlError::Unreadable(_)), "{error:?}");
        }
        let (client, _) = client(302, json!({}));
        let error = smol::block_on(client.clusters()).unwrap_err();
        assert!(matches!(error, ControlError::Unreadable(_)), "{error:?}");
    }

    #[test]
    fn an_answer_over_its_cap_is_not_read() {
        let large = "x".repeat(Limits::TOKEN.max_bytes);
        let (client, _) = client(200, json!({"data": large, "meta": meta()}));
        let error = smol::block_on(client.data_token("01CLUSTER", ENDPOINT)).unwrap_err();
        assert!(matches!(error, ControlError::Unreadable(_)), "{error:?}");
    }

    #[test]
    fn no_answer_is_not_asked_again() {
        let attempts = Arc::new(Mutex::new(0));
        let counted = attempts.clone();
        let http = FakeHttpClient::create(move |_| {
            *counted.lock().unwrap() += 1;
            async move { anyhow::bail!("operation timed out") }
        });
        let client = ControlClient::new(http, ROUTE, "access-token".into()).unwrap();
        let error = smol::block_on(client.data_token("01CLUSTER", ENDPOINT)).unwrap_err();
        assert_eq!(
            error,
            ControlError::Unavailable {
                status: None,
                why: "the request timed out".into()
            }
        );
        assert_eq!(*attempts.lock().unwrap(), 1);
    }

    #[test]
    fn only_a_sendable_route_is_given_a_token() {
        let http = FakeHttpClient::with_404_response();
        for route in [
            "http://control.example",
            "https://user:pass@control.example",
            "https://control.example/#x",
            "https://control.example/?a=b",
            "ftp://control.example",
            "not a url",
        ] {
            assert!(
                ControlClient::new(http.clone(), route, "t".into()).is_err(),
                "{route}"
            );
        }
        for route in [
            "https://control.example",
            "http://127.0.0.1:8080",
            "http://localhost:3000",
        ] {
            assert!(
                ControlClient::new(http.clone(), route, "t".into()).is_ok(),
                "{route}"
            );
        }
        let client = ControlClient::new(http, ROUTE, "secret-access".into()).unwrap();
        assert!(!format!("{client:?}").contains("secret-access"));
    }
}
