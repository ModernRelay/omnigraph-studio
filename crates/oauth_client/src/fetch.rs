//! The requests a sign-in makes: bounded in time and in the bytes read, never redirected.

use std::time::Duration;

use futures::AsyncReadExt as _;
use http_client::http::{Method, Request, StatusCode};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy};
use url::Url;

/// The most of a metadata or token answer Studio reads (spec 016, Limits).
pub(crate) const MAX_ANSWER_BYTES: usize = 64 * 1024;
/// How long a metadata fetch may take.
pub(crate) const METADATA_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a token exchange or refresh may take.
pub(crate) const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a revocation may take.
pub(crate) const REVOKE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Answer {
    pub status: StatusCode,
    /// At most [`MAX_ANSWER_BYTES`]; `None` when the answer was longer, and the rest unread.
    pub body: Option<Vec<u8>>,
}

impl Answer {
    /// The body as a JSON object, or `None` when it is too long or not one.
    pub fn object(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        match serde_json::from_slice(self.body.as_deref()?) {
            Ok(serde_json::Value::Object(object)) => Some(object),
            _ => None,
        }
    }
}

pub(crate) async fn get(
    http: &dyn HttpClient,
    url: &Url,
    timeout: Duration,
) -> Result<Answer, String> {
    send(http, Method::GET, url, None, timeout).await
}

pub(crate) async fn post_form(
    http: &dyn HttpClient,
    url: &Url,
    form: &[(&str, &str)],
    timeout: Duration,
) -> Result<Answer, String> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    send(http, Method::POST, url, Some(body.into_bytes()), timeout).await
}

async fn send(
    http: &dyn HttpClient,
    method: Method,
    url: &Url,
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Answer, String> {
    let mut builder = Request::builder()
        .method(method)
        .uri(url.as_str())
        .header("accept", "application/json")
        .follow_redirects(RedirectPolicy::NoFollow)
        .timeout(timeout);
    if body.is_some() {
        builder = builder.header("content-type", "application/x-www-form-urlencoded");
    }
    let request = builder
        .body(body.map(AsyncBody::from).unwrap_or_else(AsyncBody::empty))
        .map_err(|error| error.to_string())?;
    let response = http
        .send(request)
        .await
        .map_err(|error| transport(&error))?;
    let status = response.status();
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8 * 1024];
    loop {
        let read = body
            .read(&mut buffer)
            .await
            .map_err(|error| transport(&error.into()))?;
        if read == 0 {
            break;
        }
        if bytes.len() + read > MAX_ANSWER_BYTES {
            return Ok(Answer { status, body: None });
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(Answer {
        status,
        body: Some(bytes),
    })
}

/// Why a request did not complete, in words: a class, never the URL (which can hold a code).
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
