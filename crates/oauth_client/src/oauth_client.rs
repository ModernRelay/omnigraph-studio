//! Studio as an OAuth public client of an Omnigraph server (spec 016): what the server says about
//! signing in, the authorization server it names, PKCE, the loopback callback the browser returns
//! to, and the token endpoint. No GPUI here; Studio's connections drive it (`graph_store`).
//!
//! Every fetch is bounded in time and size, nothing is retried here, and every refusal is a
//! sentence the person reads.

mod callback;
mod discovery;
mod fetch;
mod pkce;
mod token;

pub use callback::{Callback, CallbackError};
pub use discovery::{
    AuthorizationServer, ClientIdentities, Clients, ProtectedResource, authorization_server,
    protected_resource,
};
pub use pkce::{Pkce, random_token};
pub use token::{Account, Expected, TokenError, Tokens, exchange, refresh, revoke};

use url::{Host, Url};

/// How long the person has to finish signing in once the browser opens (spec 016, Limits).
pub const SIGN_IN_WAIT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// The longest access token Studio accepts, in seconds: an Omnigraph server refuses one that
/// lives longer (spec 016, Limits).
pub const MAX_ACCESS_SECONDS: u64 = 300;

/// The address the browser opens to sign in (RFC 6749 §4.1.1, RFC 7636 §4.3, RFC 8707): the
/// server's authorization endpoint with the sign-in's parameters added to any it has.
pub fn authorization_url(
    server: &AuthorizationServer,
    client_id: &str,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
    resource: &str,
) -> Url {
    let mut url = server.authorization_endpoint.clone();
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("resource", resource);
        if let Some(scope) = server.scope() {
            query.append_pair("scope", scope);
        }
    }
    url
}

/// A sign-in that cannot go on, in words the person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// Whether `url` names this machine: `127.0.0.1` and the rest of `127.0.0.0/8`, `[::1]`, or
/// `localhost`.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Whether Studio sends anything of a sign-in to `url`: HTTPS, or HTTP to this machine, with no
/// user name or password in it and no fragment.
fn reachable(url: &Url) -> bool {
    (url.scheme() == "https" || (url.scheme() == "http" && is_loopback(url)))
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

/// The host of `url` as the person reads it, with its port when it has one.
pub fn host_of(url: &str) -> String {
    match Url::parse(url) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_owned(),
            _ => url.to_string(),
        },
        Err(_) => url.to_owned(),
    }
}

/// A value from a sign-in service shown to the person: control characters dropped, and cut at
/// 300 characters (spec 016).
pub fn shown(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.chars().count() > 300 {
        clean.chars().take(299).chain(['…']).collect()
    } else {
        clean
    }
}
