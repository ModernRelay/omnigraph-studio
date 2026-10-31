//! What the server publishes about signing in (RFC 9728), and the authorization server it names
//! (RFC 8414, else OpenID Connect discovery), each checked before anything is sent to it.

use std::sync::Arc;

use http_client::HttpClient;
use http_client::http::StatusCode;
use serde_json::{Map, Value};
use url::Url;

use crate::fetch::{self, METADATA_TIMEOUT};
use crate::{Refusal, host_of, reachable};

/// The clients a sign-in's requests go through: the target's own (its proxy and certificate
/// authority, spec 003) for anything at the endpoint's origin, and Studio's ordinary client for
/// any other host. The Omnigraph CLI's rule is the same: a target's transport is for its data
/// endpoint, and identity-provider traffic does not inherit it (`docs/user/cli/targets.md`,
/// Transport and capability boundaries).
#[derive(Clone)]
pub struct Clients {
    endpoint: Url,
    at_endpoint: Arc<dyn HttpClient>,
    elsewhere: Arc<dyn HttpClient>,
}

impl Clients {
    pub fn new(
        endpoint: Url,
        at_endpoint: Arc<dyn HttpClient>,
        elsewhere: Arc<dyn HttpClient>,
    ) -> Self {
        Self {
            endpoint,
            at_endpoint,
            elsewhere,
        }
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// The client for a request to `url`, to hold across an await.
    pub fn client_for(&self, url: &Url) -> Arc<dyn HttpClient> {
        if url.origin() == self.endpoint.origin() {
            self.at_endpoint.clone()
        } else {
            self.elsewhere.clone()
        }
    }
}

/// The resource a server's tokens are issued for, and the one authorization server that issues
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedResource {
    /// Exactly as the metadata writes it: tokens are requested for this string.
    pub resource: String,
    pub issuer: String,
}

/// Fetches and checks the protected-resource metadata at `metadata_url`, the URL a 401's
/// `resource_metadata` names. Both it and the resource must be on the endpoint's origin: a data
/// host cannot have Studio fetch a token for someone else's resource (spec 016, Discovery).
pub async fn protected_resource(
    clients: &Clients,
    metadata_url: &str,
) -> Result<ProtectedResource, Refusal> {
    let endpoint = clients.endpoint();
    let url = Url::parse(metadata_url)
        .ok()
        .filter(|url| reachable(url) && url.origin() == endpoint.origin())
        .ok_or_else(|| {
            Refusal::lasting(format!(
                "The server named its sign-in details at {}, not on its own address, so Studio \
                 does not use them.",
                host_of(metadata_url)
            ))
        })?;
    let what = "The server's sign-in details";
    let object = object_of(fetch_answer(clients, &url, what).await, what)?;
    let resource = object
        .get("resource")
        .and_then(Value::as_str)
        .ok_or_else(|| Refusal::lasting("The server's sign-in details name no resource."))?;
    Url::parse(resource)
        .ok()
        .filter(|url| reachable(url) && url.origin() == endpoint.origin())
        .ok_or_else(|| {
            Refusal::lasting(format!(
                "The server asks for tokens issued for {}, which is not the server's own \
                 address, so Studio does not sign in for it.",
                host_of(resource)
            ))
        })?;
    let issuers: Vec<&str> = object
        .get("authorization_servers")
        .and_then(Value::as_array)
        .map(|servers| servers.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let [issuer] = issuers.as_slice() else {
        return Err(Refusal::lasting(if issuers.is_empty() {
            "The server's sign-in details name no sign-in service."
        } else {
            "The server's sign-in details name several sign-in services; Studio signs in \
             through one."
        }));
    };
    if Url::parse(issuer).map_or(true, |url| url.scheme() != "https" || !reachable(&url)) {
        return Err(Refusal::lasting(format!(
            "The server names a sign-in service at {issuer}, which is not an HTTPS address."
        )));
    }
    if let Some(methods) = object.get("bearer_methods_supported") {
        let header = methods
            .as_array()
            .is_some_and(|methods| methods.iter().any(|m| m.as_str() == Some("header")));
        if !header {
            return Err(Refusal::lasting(
                "The server does not take its token in a request header, as Studio sends it.",
            ));
        }
    }
    Ok(ProtectedResource {
        resource: resource.to_owned(),
        issuer: (*issuer).to_owned(),
    })
}

/// An authorization server, as its metadata describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationServer {
    pub issuer: String,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub revocation_endpoint: Option<Url>,
    /// It accepts a Client ID Metadata Document's URL as a client ID.
    pub metadata_documents: bool,
    pub scopes: Vec<String>,
    /// Described by OpenID Connect's document in place of RFC 8414's, which failed for now: that
    /// one may answer next time and say otherwise, so a refusal of this description may pass.
    pub standing_in: bool,
}

impl AuthorizationServer {
    /// The scope Studio asks for: `offline_access` when the server lists it, so that it issues
    /// a refresh token; otherwise none, and the server's default (spec 016, open question 3).
    pub fn scope(&self) -> Option<&'static str> {
        self.scopes
            .iter()
            .any(|scope| scope == "offline_access")
            .then_some("offline_access")
    }
}

/// Fetches and checks the metadata of `issuer`: RFC 8414's well-known path, inserted after the
/// host, else OpenID Connect discovery's, appended to the issuer. Its `issuer` must be exactly
/// the one the resource named.
pub async fn authorization_server(
    clients: &Clients,
    issuer: &str,
) -> Result<AuthorizationServer, Refusal> {
    let named = Url::parse(issuer).map_err(|_| {
        Refusal::lasting(format!(
            "The sign-in service's address, {issuer}, cannot be read."
        ))
    })?;
    let path = named.path().trim_end_matches('/');
    let mut oauth = named.clone();
    oauth.set_path(&format!("/.well-known/oauth-authorization-server{path}"));
    let openid = Url::parse(&format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    ))
    .map_err(|_| {
        Refusal::lasting(format!(
            "The sign-in service's address, {issuer}, cannot be read."
        ))
    })?;
    let what = format!("The sign-in service at {}", host_of(issuer));
    // OpenID Connect's document stands in for RFC 8414's whenever that one gives no description:
    // a 404 or another refusal of the path (Cognito answers 400), or a 200 that is no
    // description Studio can read (a page every path answers with, one too long to read, or an
    // object that names no issuer). The stand-in is read alongside, so it costs no wait, and is
    // dropped unawaited once RFC 8414's describes the service. One that failed for now (no
    // answer, or [`fetch::failed_for_now`]) may answer next time and say otherwise, so a refusal
    // of what the stand-in says may pass, and a stand-in that cannot be read is that failure.
    let first = fetch_answer(clients, &oauth, &what);
    let second = fetch_answer(clients, &openid, &what);
    futures::pin_mut!(first, second);
    let (first, second_answered) = match futures::future::select(first, second.as_mut()).await {
        futures::future::Either::Left((first, _)) => (first, None),
        futures::future::Either::Right((second, first)) => (first.await, Some(second)),
    };
    let described_first = first
        .as_ref()
        .ok()
        .filter(|answer| answer.status == StatusCode::OK)
        .and_then(fetch::Answer::object)
        .filter(|object| object.contains_key("issuer"));
    let (object, standing_in) = match described_first {
        Some(object) => (object, false),
        None => {
            // Why RFC 8414's failed for now, as reading it as an object says, when it did.
            let failed = object_of(first, &what)
                .err()
                .filter(|refusal| refusal.passing)
                .map(|refusal| refusal.why);
            let second = match second_answered {
                Some(second) => second,
                None => second.await,
            };
            let object = object_of(second, &what).map_err(|refusal| match &failed {
                Some(why) => Refusal::passing(why.clone()),
                None => refusal,
            })?;
            (object, failed.is_some())
        }
    };
    let mut server = described(issuer, &what, &object).map_err(|refusal| {
        if standing_in {
            Refusal::passing(refusal.why)
        } else {
            refusal
        }
    })?;
    server.standing_in = standing_in;
    Ok(server)
}

/// Checks what the sign-in service `issuer` says of itself in `object`, its metadata.
fn described(
    issuer: &str,
    what: &str,
    object: &Map<String, Value>,
) -> Result<AuthorizationServer, Refusal> {
    if object.get("issuer").and_then(Value::as_str) != Some(issuer) {
        return Err(Refusal::lasting(format!(
            "{what} describes itself as another service, so Studio does not sign in through it."
        )));
    }
    let endpoint = |name: &str| -> Result<Option<Url>, Refusal> {
        match object.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_str()
                .and_then(|url| Url::parse(url).ok())
                .filter(reachable)
                .map(Some)
                .ok_or_else(|| {
                    Refusal::lasting(format!(
                        "{what} names its {} at an address that is not HTTPS.",
                        name.replace('_', " ")
                    ))
                }),
        }
    };
    let authorization_endpoint = endpoint("authorization_endpoint")?
        .ok_or_else(|| Refusal::lasting(format!("{what} names no authorization endpoint.")))?;
    let token_endpoint = endpoint("token_endpoint")?
        .ok_or_else(|| Refusal::lasting(format!("{what} names no token endpoint.")))?;
    let revocation_endpoint = endpoint("revocation_endpoint")?;
    let s256 = object
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
        .is_some_and(|methods| methods.iter().any(|m| m.as_str() == Some("S256")));
    if !s256 {
        return Err(Refusal::lasting(format!(
            "{what} does not say it supports PKCE with S256, which Studio requires."
        )));
    }
    let scopes = object
        .get("scopes_supported")
        .and_then(Value::as_array)
        .map(|scopes| {
            scopes
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(AuthorizationServer {
        issuer: issuer.to_owned(),
        authorization_endpoint,
        token_endpoint,
        revocation_endpoint,
        metadata_documents: object
            .get("client_id_metadata_document_supported")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        scopes,
        standing_in: false,
    })
}

/// Which client ID Studio signs in with at a server: one registered for its issuer, else one
/// named for development, else its Client ID Metadata Document's URL when the server accepts
/// one. Studio never registers itself (spec 016, Client identity).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientIdentities {
    /// The URL of Studio's Client ID Metadata Document.
    pub metadata_document: Option<String>,
    /// Client IDs registered for Studio, by issuer.
    pub registered: Vec<(String, String)>,
    /// A client ID used at any sign-in service: for development, against a service of one's
    /// own that Studio is not registered with.
    pub development: Option<String>,
}

impl ClientIdentities {
    /// The client ID Studio signs in with at `server`, or that the service does not accept
    /// Studio: a refusal that may pass where its description stands in for one that failed.
    pub fn client_id(&self, server: &AuthorizationServer) -> Result<String, Refusal> {
        self.for_server(server).ok_or_else(|| {
            let why = format!(
                "The sign-in service at {} does not accept Omnigraph Studio.",
                host_of(&server.issuer)
            );
            if server.standing_in {
                Refusal::passing(why)
            } else {
                Refusal::lasting(why)
            }
        })
    }

    fn for_server(&self, server: &AuthorizationServer) -> Option<String> {
        self.registered
            .iter()
            .find(|(issuer, _)| *issuer == server.issuer)
            .map(|(_, id)| id.clone())
            .or_else(|| self.development.clone())
            .or_else(|| {
                self.metadata_document
                    .clone()
                    .filter(|_| server.metadata_documents)
            })
    }
}

/// What `url` answered, or why nothing could be read from it.
async fn fetch_answer(clients: &Clients, url: &Url, what: &str) -> Result<fetch::Answer, Refusal> {
    fetch::get(clients.client_for(url).as_ref(), url, METADATA_TIMEOUT)
        .await
        .map_err(|why| Refusal::passing(format!("{what} could not be read: {why}.")))
}

/// The JSON object an answer holds. No answer, or one that failed for now, may be another next
/// time; any other answer, a page or one too long to read among them, is the server's.
fn object_of(
    answer: Result<fetch::Answer, Refusal>,
    what: &str,
) -> Result<Map<String, Value>, Refusal> {
    let answer = answer?;
    if answer.status != StatusCode::OK {
        let why = format!(
            "{what} could not be read: the answer was {}.",
            answer.status.as_u16()
        );
        return Err(if fetch::failed_for_now(answer.status) {
            Refusal::passing(why)
        } else {
            Refusal::lasting(why)
        });
    }
    if answer.body.is_none() {
        return Err(Refusal::lasting(format!(
            "{what} answered with more than 64 KiB."
        )));
    }
    answer.object().ok_or_else(|| {
        Refusal::lasting(format!(
            "{what} answered with something Studio cannot read."
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};
    use serde_json::json;

    use super::*;

    /// A fake answering `pages` by URL, 404 otherwise, and noting which client each request
    /// went through.
    fn client(
        name: &'static str,
        pages: Vec<(String, Value)>,
        seen: Arc<Mutex<Vec<String>>>,
    ) -> Arc<dyn HttpClient> {
        FakeHttpClient::create(move |request| {
            let url = request.uri().to_string();
            seen.lock().unwrap().push(format!("{name} {url}"));
            let page = pages
                .iter()
                .find(|(at, _)| *at == url)
                .map(|(_, page)| page.clone());
            // `{"wait_for": url, "page": page}` answers `page` once `url` was requested too.
            let wait_for = page
                .as_ref()
                .and_then(|page| page.get("wait_for"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let page = match &wait_for {
                Some(_) => page.and_then(|page| page.get("page").cloned()),
                None => page,
            };
            let seen = seen.clone();
            async move {
                if let Some(wait_for) = wait_for {
                    while !seen.lock().unwrap().iter().any(|r| r.ends_with(&wait_for)) {
                        smol::future::yield_now().await;
                    }
                }
                Ok(match page {
                    // A page that is only a status answers with it; status 0 gives no answer, and
                    // status 1 never answers.
                    Some(page) => match page.get("status").and_then(Value::as_u64) {
                        Some(0) if page.as_object().is_some_and(|page| page.len() == 1) => {
                            anyhow::bail!("timed out")
                        }
                        Some(1) if page.as_object().is_some_and(|page| page.len() == 1) => {
                            futures::future::pending::<()>().await;
                            unreachable!()
                        }
                        Some(status) if page.as_object().is_some_and(|page| page.len() == 1) => {
                            Response::builder()
                                .status(status as u16)
                                .body(AsyncBody::from("{}".to_owned()))?
                        }
                        _ => Response::builder()
                            .status(200)
                            .body(AsyncBody::from(page.to_string()))?,
                    },
                    None => Response::builder()
                        .status(404)
                        .body(AsyncBody::from("{}".to_owned()))?,
                })
            }
        })
    }

    /// `read`'s answer, or `None` if it takes five seconds: a read that would wait for ever fails.
    fn within_5s<T>(read: impl std::future::Future<Output = T>) -> Option<T> {
        smol::block_on(smol::future::or(async { Some(read.await) }, async {
            smol::Timer::after(std::time::Duration::from_secs(5)).await;
            None
        }))
    }

    fn server_metadata(issuer: &str) -> Value {
        json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "code_challenge_methods_supported": ["S256"],
            "client_id_metadata_document_supported": true,
            "scopes_supported": ["openid", "offline_access"],
        })
    }

    fn clients(pages: Vec<(String, Value)>) -> (Clients, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let clients = Clients::new(
            Url::parse("https://graphs.example").unwrap(),
            client("target", pages.clone(), seen.clone()),
            client("studio", pages, seen.clone()),
        );
        (clients, seen)
    }

    const METADATA: &str = "https://graphs.example/.well-known/oauth-protected-resource/mcp";

    #[test]
    fn the_resource_and_its_sign_in_service_are_read_through_the_right_clients() {
        let openid = "https://id.example/tenant/.well-known/openid-configuration";
        let (clients, seen) = clients(vec![
            (
                METADATA.to_owned(),
                json!({"resource": "https://graphs.example/mcp", "authorization_servers": ["https://id.example/tenant"], "bearer_methods_supported": ["header"]}),
            ),
            // Answered once the stand-in is requested too, so its client is seen.
            (
                "https://id.example/.well-known/oauth-authorization-server/tenant".to_owned(),
                json!({"wait_for": openid, "page": server_metadata("https://id.example/tenant")}),
            ),
        ]);
        let resource = smol::block_on(protected_resource(&clients, METADATA)).unwrap();
        assert_eq!(resource.resource, "https://graphs.example/mcp");
        let server = within_5s(authorization_server(&clients, &resource.issuer))
            .expect("the stand-in was not read alongside")
            .unwrap();
        assert_eq!(
            server.token_endpoint.as_str(),
            "https://id.example/tenant/token"
        );
        assert_eq!(server.scope(), Some("offline_access"));
        let identities = ClientIdentities {
            metadata_document: Some("https://studio.example/client.json".into()),
            registered: vec![("https://other.example".into(), "studio".into())],
            development: None,
        };
        assert_eq!(
            identities.for_server(&server).as_deref(),
            Some("https://studio.example/client.json")
        );
        // The server's own address through the target's client, the sign-in service through
        // Studio's, its stand-in included.
        let mut seen = seen.lock().unwrap().clone();
        seen.sort();
        assert_eq!(
            seen,
            [
                "studio https://id.example/.well-known/oauth-authorization-server/tenant"
                    .to_owned(),
                format!("studio {openid}"),
                format!("target {METADATA}"),
            ]
        );
    }

    #[test]
    fn rfc_8414s_description_does_not_wait_for_openid_connects() {
        let (clients, _) = clients(vec![
            (
                "https://id.example/.well-known/oauth-authorization-server/tenant".to_owned(),
                server_metadata("https://id.example/tenant"),
            ),
            (
                "https://id.example/tenant/.well-known/openid-configuration".to_owned(),
                json!({"status": 1}),
            ),
        ]);
        let read = within_5s(authorization_server(&clients, "https://id.example/tenant"));
        let server = read.expect("RFC 8414's description waited for the stand-in");
        assert_eq!(server.unwrap().issuer, "https://id.example/tenant");

        // The two are in flight together: RFC 8414's answers only once the stand-in was asked.
        let openid = "https://id.example/tenant/.well-known/openid-configuration";
        let (together, _) = self::clients(vec![
            (
                "https://id.example/.well-known/oauth-authorization-server/tenant".to_owned(),
                json!({"wait_for": openid, "page": server_metadata("https://id.example/tenant")}),
            ),
            (
                openid.to_owned(),
                server_metadata("https://id.example/tenant"),
            ),
        ]);
        let read = within_5s(authorization_server(&together, "https://id.example/tenant"));
        let server = read.expect("the stand-in was not read alongside");
        assert!(!server.unwrap().standing_in);
    }

    #[test]
    fn openid_discovery_stands_in_when_the_oauth_metadata_is_missing() {
        let (clients, _) = clients(vec![(
            "https://id.example/tenant/.well-known/openid-configuration".to_owned(),
            server_metadata("https://id.example/tenant"),
        )]);
        let server =
            smol::block_on(authorization_server(&clients, "https://id.example/tenant")).unwrap();
        assert_eq!(server.issuer, "https://id.example/tenant");
        let none = ClientIdentities::default();
        assert_eq!(none.for_server(&server), None);

        // A page every path answers with is no description: OpenID Connect's stands in.
        let (catch_all, _) = self::clients(vec![
            (
                "https://id.example/.well-known/oauth-authorization-server/tenant".to_owned(),
                json!("<html>Not found</html>"),
            ),
            (
                "https://id.example/tenant/.well-known/openid-configuration".to_owned(),
                server_metadata("https://id.example/tenant"),
            ),
        ]);
        let server = smol::block_on(authorization_server(
            &catch_all,
            "https://id.example/tenant",
        ))
        .unwrap();
        assert_eq!(server.issuer, "https://id.example/tenant");
        assert!(!server.standing_in);
        // So is an object that names no issuer, as a catch-all that answers `{}`.
        let (empty, _) = self::clients(vec![
            (
                "https://id.example/.well-known/oauth-authorization-server/tenant".to_owned(),
                json!({}),
            ),
            (
                "https://id.example/tenant/.well-known/openid-configuration".to_owned(),
                server_metadata("https://id.example/tenant"),
            ),
        ]);
        let server =
            smol::block_on(authorization_server(&empty, "https://id.example/tenant")).unwrap();
        assert_eq!(server.issuer, "https://id.example/tenant");
        // So is one too long to read.
        let (oversize, _) = self::clients(vec![
            (
                "https://id.example/.well-known/oauth-authorization-server/tenant".to_owned(),
                json!("x".repeat(70 * 1024)),
            ),
            (
                "https://id.example/tenant/.well-known/openid-configuration".to_owned(),
                server_metadata("https://id.example/tenant"),
            ),
        ]);
        let server =
            smol::block_on(authorization_server(&oversize, "https://id.example/tenant")).unwrap();
        assert_eq!(server.issuer, "https://id.example/tenant");
    }

    #[test]
    fn sign_in_details_that_point_elsewhere_are_refused() {
        let refused = |metadata: &str, page: Value| {
            let (clients, _) = clients(vec![(metadata.to_owned(), page)]);
            let refusal = smol::block_on(protected_resource(&clients, metadata)).unwrap_err();
            assert!(!refusal.passing, "{refusal}");
            refusal.why
        };
        let ok_issuer = json!(["https://id.example"]);
        assert!(
            refused(
                "https://evil.example/.well-known/oauth-protected-resource",
                json!({})
            )
            .contains("not on its own address")
        );
        assert!(
            refused(
                METADATA,
                json!({"resource": "https://evil.example/mcp", "authorization_servers": ok_issuer})
            )
            .contains("not the server's own address")
        );
        assert!(refused(METADATA, json!({"resource": "https://graphs.example/mcp", "authorization_servers": ["https://a.example", "https://b.example"]})).contains("several"));
        assert!(refused(METADATA, json!({"resource": "https://graphs.example/mcp", "authorization_servers": ["http://id.example"]})).contains("not an HTTPS address"));
        assert!(refused(METADATA, json!({"resource": "https://graphs.example/mcp", "authorization_servers": ok_issuer, "bearer_methods_supported": ["body"]})).contains("request header"));

        let server_refused = |page: Value| {
            let (clients, _) = clients(vec![(
                "https://id.example/.well-known/oauth-authorization-server".to_owned(),
                page,
            )]);
            let refusal =
                smol::block_on(authorization_server(&clients, "https://id.example")).unwrap_err();
            assert!(!refusal.passing, "{refusal}");
            refusal.why
        };
        assert!(
            server_refused(server_metadata("https://other.example")).contains("another service")
        );
        let mut plain = server_metadata("https://id.example");
        plain["token_endpoint"] = json!("http://id.example/token");
        assert!(server_refused(plain).contains("not HTTPS"));
        let mut no_pkce = server_metadata("https://id.example");
        no_pkce["code_challenge_methods_supported"] = json!(["plain"]);
        assert!(server_refused(no_pkce).contains("S256"));

        // Details that could not be read for now may be, asked again; ones not there are not.
        let (nothing, _) = clients(Vec::new());
        let missing = smol::block_on(protected_resource(&nothing, METADATA)).unwrap_err();
        assert!(
            !missing.passing && missing.why.contains("the answer was 404"),
            "{missing}"
        );
        // Nor is a page where the details should be.
        let (page, _) = clients(vec![(METADATA.to_owned(), json!("<html></html>"))]);
        let page = smol::block_on(protected_resource(&page, METADATA)).unwrap_err();
        assert!(!page.passing, "{page}");
        let busy = |url: &str| {
            let (clients, _) = clients(vec![(url.to_owned(), json!({"status": 503}))]);
            clients
        };
        let unread = smol::block_on(protected_resource(&busy(METADATA), METADATA)).unwrap_err();
        assert!(
            unread.passing && unread.why.contains("could not be read"),
            "{unread}"
        );
        let unread = smol::block_on(authorization_server(
            &busy("https://id.example/.well-known/openid-configuration"),
            "https://id.example",
        ));
        assert!(unread.unwrap_err().passing);

        // OpenID Connect's document in place of RFC 8414's: what it says lasts where that one is
        // not there (404), and may pass where it failed for now.
        let oauth = "https://id.example/.well-known/oauth-authorization-server".to_owned();
        let openid = "https://id.example/.well-known/openid-configuration".to_owned();
        let mut no_pkce = server_metadata("https://id.example");
        no_pkce["code_challenge_methods_supported"] = json!(["plain"]);
        let refused = |pages| {
            let (clients, _) = clients(pages);
            smol::block_on(authorization_server(&clients, "https://id.example")).unwrap_err()
        };
        assert!(!refused(vec![(openid.clone(), no_pkce.clone())]).passing);
        let failed = refused(vec![
            (oauth.clone(), json!({"status": 503})),
            (openid.clone(), no_pkce.clone()),
        ]);
        assert!(failed.passing && failed.why.contains("S256"), "{failed}");
        // A path refused another way (Cognito answers 400) is not there either.
        let refused_path = refused(vec![
            (oauth.clone(), json!({"status": 400})),
            (openid.clone(), no_pkce),
        ]);
        assert!(!refused_path.passing, "{refused_path}");
        // RFC 8414's gave no answer: the stand-in describes the service, and its refusals may
        // pass; with none there either, that failure is why.
        let silent = |stand_in: Value| {
            let (clients, _) = clients(vec![
                (oauth.clone(), json!({"status": 0})),
                (openid.clone(), stand_in),
            ]);
            smol::block_on(authorization_server(&clients, "https://id.example"))
        };
        assert!(
            silent(server_metadata("https://id.example"))
                .unwrap()
                .standing_in
        );
        let unanswered = silent(json!({"status": 404})).unwrap_err();
        assert!(
            unanswered.passing && unanswered.why.contains("timed out"),
            "{unanswered}"
        );
        // RFC 8414's failed for now and no stand-in there: that failure is why, and may pass.
        let both = refused(vec![(oauth.clone(), json!({"status": 503}))]);
        assert!(both.passing && both.why.contains("503"), "{both}");
        // So may a service that does not accept Studio, as the stand-in describes it.
        let mut no_documents = server_metadata("https://id.example");
        no_documents["client_id_metadata_document_supported"] = json!(false);
        let accepts = ClientIdentities {
            metadata_document: Some("https://studio.example/client.json".into()),
            ..ClientIdentities::default()
        };
        let not_accepted = |pages| {
            let (clients, _) = clients(pages);
            let server =
                smol::block_on(authorization_server(&clients, "https://id.example")).unwrap();
            accepts.client_id(&server).unwrap_err()
        };
        assert!(!not_accepted(vec![(openid.clone(), no_documents.clone())]).passing);
        let failed = not_accepted(vec![
            (oauth, json!({"status": 503})),
            (openid, no_documents),
        ]);
        assert!(
            failed.passing && failed.why.contains("does not accept"),
            "{failed}"
        );
    }
}
