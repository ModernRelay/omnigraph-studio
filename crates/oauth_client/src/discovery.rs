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

    /// The same clients, with `at_endpoint` for the server's origin: the CLI target's transport
    /// changed (another proxy or certificate authority).
    pub fn through(&self, at_endpoint: Arc<dyn HttpClient>) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            at_endpoint,
            elsewhere: self.elsewhere.clone(),
        }
    }

    /// The client for a request to `url`, to hold across an await.
    pub fn client_for(&self, url: &Url) -> Arc<dyn HttpClient> {
        if url.origin() == self.endpoint.origin() {
            self.at_endpoint.clone()
        } else {
            self.elsewhere.clone()
        }
    }

    /// The client for a request to `url`.
    pub fn for_url(&self, url: &Url) -> &dyn HttpClient {
        if url.origin() == self.endpoint.origin() {
            self.at_endpoint.as_ref()
        } else {
            self.elsewhere.as_ref()
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
            Refusal(format!(
                "The server named its sign-in details at {}, not on its own address, so Studio \
                 does not use them.",
                host_of(metadata_url)
            ))
        })?;
    let object = fetch_object(clients, &url, "The server's sign-in details").await?;
    let resource = object
        .get("resource")
        .and_then(Value::as_str)
        .ok_or_else(|| Refusal("The server's sign-in details name no resource.".into()))?;
    Url::parse(resource)
        .ok()
        .filter(|url| reachable(url) && url.origin() == endpoint.origin())
        .ok_or_else(|| {
            Refusal(format!(
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
        return Err(Refusal(if issuers.is_empty() {
            "The server's sign-in details name no sign-in service.".into()
        } else {
            "The server's sign-in details name several sign-in services; Studio signs in \
             through one."
                .into()
        }));
    };
    if Url::parse(issuer).map_or(true, |url| url.scheme() != "https" || !reachable(&url)) {
        return Err(Refusal(format!(
            "The server names a sign-in service at {issuer}, which is not an HTTPS address."
        )));
    }
    if let Some(methods) = object.get("bearer_methods_supported") {
        let header = methods
            .as_array()
            .is_some_and(|methods| methods.iter().any(|m| m.as_str() == Some("header")));
        if !header {
            return Err(Refusal(
                "The server does not take its token in a request header, as Studio sends it."
                    .into(),
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
        Refusal(format!(
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
        Refusal(format!(
            "The sign-in service's address, {issuer}, cannot be read."
        ))
    })?;
    let what = format!("The sign-in service at {}", host_of(issuer));
    let object = match fetch_answer(clients, &oauth, &what).await? {
        (status, _) if status != StatusCode::OK => fetch_object(clients, &openid, &what).await?,
        (_, object) => object.ok_or_else(|| {
            Refusal(format!(
                "{what} described itself in a way Studio cannot read."
            ))
        })?,
    };
    if object.get("issuer").and_then(Value::as_str) != Some(issuer) {
        return Err(Refusal(format!(
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
                    Refusal(format!(
                        "{what} names its {} at an address that is not HTTPS.",
                        name.replace('_', " ")
                    ))
                }),
        }
    };
    let authorization_endpoint = endpoint("authorization_endpoint")?
        .ok_or_else(|| Refusal(format!("{what} names no authorization endpoint.")))?;
    let token_endpoint = endpoint("token_endpoint")?
        .ok_or_else(|| Refusal(format!("{what} names no token endpoint.")))?;
    let revocation_endpoint = endpoint("revocation_endpoint")?;
    let s256 = object
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
        .is_some_and(|methods| methods.iter().any(|m| m.as_str() == Some("S256")));
    if !s256 {
        return Err(Refusal(format!(
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
    pub fn for_server(&self, server: &AuthorizationServer) -> Option<String> {
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

async fn fetch_answer(
    clients: &Clients,
    url: &Url,
    what: &str,
) -> Result<(StatusCode, Option<Map<String, Value>>), Refusal> {
    let answer = fetch::get(clients.for_url(url), url, METADATA_TIMEOUT)
        .await
        .map_err(|why| Refusal(format!("{what} could not be read: {why}.")))?;
    if answer.body.is_none() {
        return Err(Refusal(format!("{what} answered with more than 64 KiB.")));
    }
    Ok((answer.status, answer.object()))
}

async fn fetch_object(
    clients: &Clients,
    url: &Url,
    what: &str,
) -> Result<Map<String, Value>, Refusal> {
    match fetch_answer(clients, url, what).await? {
        (StatusCode::OK, Some(object)) => Ok(object),
        (StatusCode::OK, None) => Err(Refusal(format!(
            "{what} answered with something Studio cannot read."
        ))),
        (status, _) => Err(Refusal(format!(
            "{what} could not be read: the answer was {}.",
            status.as_u16()
        ))),
    }
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
            async move {
                Ok(match page {
                    Some(page) => Response::builder()
                        .status(200)
                        .body(AsyncBody::from(page.to_string()))?,
                    None => Response::builder()
                        .status(404)
                        .body(AsyncBody::from("{}".to_owned()))?,
                })
            }
        })
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
        let (clients, seen) = clients(vec![
            (
                METADATA.to_owned(),
                json!({"resource": "https://graphs.example/mcp", "authorization_servers": ["https://id.example/tenant"], "bearer_methods_supported": ["header"]}),
            ),
            (
                "https://id.example/.well-known/oauth-authorization-server/tenant".to_owned(),
                server_metadata("https://id.example/tenant"),
            ),
        ]);
        let resource = smol::block_on(protected_resource(&clients, METADATA)).unwrap();
        assert_eq!(resource.resource, "https://graphs.example/mcp");
        let server = smol::block_on(authorization_server(&clients, &resource.issuer)).unwrap();
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
        // A new client for the target replaces only the one for the server's own address.
        let moved = clients.through(client("next", Vec::new(), seen.clone()));
        for url in [METADATA, "https://id.example/token"] {
            let client = moved.client_for(&Url::parse(url).unwrap());
            smol::block_on(client.get(url, http_client::AsyncBody::default(), false)).ok();
        }
        // The server's own address through the target's client, the sign-in service through
        // Studio's.
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                format!("target {METADATA}"),
                "studio https://id.example/.well-known/oauth-authorization-server/tenant"
                    .to_owned(),
                format!("next {METADATA}"),
                "studio https://id.example/token".to_owned(),
            ]
        );
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
    }

    #[test]
    fn sign_in_details_that_point_elsewhere_are_refused() {
        let refused = |metadata: &str, page: Value| {
            let (clients, _) = clients(vec![(metadata.to_owned(), page)]);
            smol::block_on(protected_resource(&clients, metadata))
                .unwrap_err()
                .0
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
            smol::block_on(authorization_server(&clients, "https://id.example"))
                .unwrap_err()
                .0
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
    }
}
