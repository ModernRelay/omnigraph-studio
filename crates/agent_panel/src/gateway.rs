//! Studio's gateway for the agent's `omnigraph` CLI (spec 013): a loopback HTTP server per
//! conversation. The CLI reaches it with a session token Studio wrote for this conversation;
//! the gateway checks that token and the contract header, applies [`gateway_rules`], and
//! forwards over [`GraphClient::forward`] with the person's token, which never leaves Studio.
//!
//! Reads stream their answers back. A write waits for the panel's decision, is sent once, and
//! its answer is read whole (bounded) so the panel can show its outcome. Request bodies are
//! read whole too, at most [`MAX_BODY`]: a JSON write is small, and a larger load is refused
//! before anything is sent. One request per connection; the CLI opens a new one each time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures::channel::{mpsc, oneshot};
use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use graph_client::{
    AsyncBody, CONTRACT_HEADER, ForwardRequest, GraphClient, HeaderMap, Method,
    SUPPORTED_CONTRACTS, StatusCode,
};
use serde_json::{Value, json};
use smol::net::{TcpListener, TcpStream};

use crate::gateway_rules::{self, Incoming, Sandbox, Verdict, WriteSummary};

/// The most a request's line and headers may take.
const MAX_HEAD: usize = 64 * 1024;
/// The most a request body may take; a larger load is refused with 413.
pub const MAX_BODY: usize = 64 * 1024 * 1024;
/// The most a write's answer may take before the gateway stops reading it for the card.
const MAX_WRITE_ANSWER: usize = 8 * 1024 * 1024;
/// Request headers passed on to the server; the gateway sets the token and contract itself.
const FORWARDED_HEADERS: &[&str] = &[
    "accept",
    "content-type",
    "if-match",
    "if-none-match",
    "omnigraph-if-graph-commit",
    "range",
];

/// What the gateway tells the conversation.
#[derive(Debug)]
pub enum GatewayEvent {
    /// The agent's CLI reached the gateway with this conversation's token.
    Reached,
    /// A write for the agent's branch waits for `decide` (true sends it).
    Write {
        id: u64,
        write: WriteSummary,
        decide: oneshot::Sender<bool>,
    },
    /// How a forwarded write ended (constitution III).
    Finished { id: u64, outcome: Seen },
}

/// A forwarded write's outcome, as the gateway saw the server's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    Committed {
        commit: Option<String>,
        nodes: i64,
        edges: i64,
    },
    Refused(String),
    Unknown(String),
}

/// A bound gateway. Its port and token go in the conversation's CLI home; dropping the future
/// [`Gateway::serve`] returns closes the listener.
pub struct Gateway {
    pub port: u16,
    pub token: String,
    listener: Option<TcpListener>,
}

impl Gateway {
    pub fn bind() -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let listener = TcpListener::try_from(listener)?;
        Ok(Self {
            port,
            token: random_token()?,
            listener: Some(listener),
        })
    }

    pub fn serve(
        &mut self,
        client: GraphClient,
        sandbox: Sandbox,
        events: mpsc::UnboundedSender<GatewayEvent>,
    ) -> impl Future<Output = ()> + Send + 'static + use<> {
        let listener = self.listener.take();
        let shared = Arc::new(Shared {
            client,
            sandbox,
            token: self.token.clone(),
            events,
            next_id: AtomicU64::new(0),
            reached: AtomicBool::new(false),
        });
        async move {
            let Some(listener) = listener else { return };
            let mut connections = Vec::new();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                connections.retain(|task: &smol::Task<()>| !task.is_finished());
                connections.push(smol::spawn(serve_connection(stream, shared.clone())));
            }
        }
    }
}

struct Shared {
    client: GraphClient,
    sandbox: Sandbox,
    token: String,
    events: mpsc::UnboundedSender<GatewayEvent>,
    next_id: AtomicU64,
    reached: AtomicBool,
}

/// 32 random bytes as hex.
fn random_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// One request, as read off the connection.
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn headers_named(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// Why a request could not be read.
enum ReadFailure {
    /// The client went away or sent nothing usable; nothing to answer.
    Gone,
    /// Answer with this status and message.
    Answer(u16, &'static str, String),
}

async fn serve_connection(mut stream: TcpStream, shared: Arc<Shared>) {
    let request = match read_request(&mut stream).await {
        Ok(request) => request,
        Err(ReadFailure::Gone) => return,
        Err(ReadFailure::Answer(status, code, message)) => {
            answer(&mut stream, status, code, &message).await;
            return;
        }
    };
    handle(stream, request, &shared).await;
}

async fn handle(mut stream: TcpStream, request: Request, shared: &Shared) {
    let (path, query) = match request.target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (request.target.clone(), None),
    };
    let pairs: Vec<(String, String)> = query
        .as_deref()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    // The CLI probes `/healthz` with no token and no contract header before every request.
    if matches!(request.method.as_str(), "GET" | "HEAD") && path == "/healthz" {
        let body = if request.method == "HEAD" { "" } else { "ok" };
        write_simple(&mut stream, 200, "text/plain", body.as_bytes()).await;
        return;
    }
    let authorized = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| same(token.as_bytes(), shared.token.as_bytes()));
    if !authorized {
        answer(
            &mut stream,
            401,
            "unauthorized",
            "Studio: this token is not the one Studio wrote for this conversation.",
        )
        .await;
        return;
    }
    let contracts = request.headers_named(CONTRACT_HEADER);
    if contracts != [SUPPORTED_CONTRACTS[0]] {
        let sent = if contracts.is_empty() {
            "none".to_owned()
        } else {
            contracts.join(", ")
        };
        answer(
            &mut stream,
            400,
            "api_contract_mismatch",
            &format!(
                "Studio: this omnigraph CLI speaks HTTP API {sent}, and this graph speaks {}. Use \
                 the omnigraph CLI Studio named for this conversation.",
                SUPPORTED_CONTRACTS[0]
            ),
        )
        .await;
        return;
    }
    if !shared.reached.swap(true, Ordering::Relaxed) {
        shared.events.unbounded_send(GatewayEvent::Reached).ok();
    }
    let json: Option<Value> = if request
        .header("content-type")
        .is_some_and(|value| value.starts_with("application/json"))
        && !request.body.is_empty()
    {
        serde_json::from_slice(&request.body).ok()
    } else {
        None
    };
    let verdict = gateway_rules::classify(
        &Incoming {
            method: &request.method,
            path: &path,
            query: &pairs,
            json: json.as_ref(),
        },
        &shared.sandbox,
    );
    match verdict {
        Verdict::Health => write_simple(&mut stream, 200, "text/plain", b"ok").await,
        Verdict::Refuse(message) => answer(&mut stream, 403, "forbidden", &message).await,
        Verdict::Read => {
            let response = shared.client.forward(forward_request(&request, None)).await;
            stream_back(&mut stream, &request.method, response).await;
        }
        Verdict::ReadWith(body) => {
            let body = serde_json::to_vec(&body).unwrap_or_default();
            let response = shared
                .client
                .forward(forward_request(&request, Some(body)))
                .await;
            stream_back(&mut stream, &request.method, response).await;
        }
        Verdict::Write(write) => {
            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
            let (decide, decision) = oneshot::channel();
            if shared
                .events
                .unbounded_send(GatewayEvent::Write { id, write, decide })
                .is_err()
            {
                answer(
                    &mut stream,
                    403,
                    "forbidden",
                    "Studio: the conversation has ended. Nothing was sent.",
                )
                .await;
                return;
            }
            if decision.await != Ok(true) {
                answer(
                    &mut stream,
                    403,
                    "forbidden",
                    "Studio: the person rejected this write. Nothing was sent.",
                )
                .await;
                return;
            }
            let response = shared.client.forward(forward_request(&request, None)).await;
            let (outcome, reply) = read_write_answer(response).await;
            shared
                .events
                .unbounded_send(GatewayEvent::Finished { id, outcome })
                .ok();
            // When the server's answer was lost, the CLI's connection closes without one, and
            // the CLI reports the outcome as unknown, as it is.
            if let Some((status, headers, body)) = reply {
                write_buffered(&mut stream, status, &headers, &body).await;
            }
        }
    }
}

fn forward_request(request: &Request, body: Option<Vec<u8>>) -> ForwardRequest {
    let headers = request
        .headers
        .iter()
        .filter(|(name, _)| FORWARDED_HEADERS.contains(&name.as_str()))
        .filter(|(name, _)| body.is_none() || name != "content-type")
        .cloned()
        .chain(
            body.is_some()
                .then(|| ("content-type".to_owned(), "application/json".to_owned())),
        )
        .collect();
    let body = body.unwrap_or_else(|| request.body.clone());
    ForwardRequest {
        method: Method::from_bytes(request.method.as_bytes()).unwrap_or(Method::GET),
        path_and_query: request.target.trim_start_matches('/').to_owned(),
        headers,
        body: if body.is_empty() {
            AsyncBody::empty()
        } else {
            AsyncBody::from(body)
        },
    }
}

/// Reads a write's answer whole, and what it says happened.
async fn read_write_answer(
    response: Result<graph_client::ForwardResponse, String>,
) -> (Seen, Option<(StatusCode, HeaderMap, Vec<u8>)>) {
    let mut response = match response {
        Ok(response) => response,
        Err(reason) => return (Seen::Unknown(reason), None),
    };
    let mut body = Vec::new();
    let read = (&mut response.body)
        .take(MAX_WRITE_ANSWER as u64)
        .read_to_end(&mut body)
        .await;
    if let Err(error) = read {
        return (Seen::Unknown(error.to_string()), None);
    }
    let confirmed = response
        .headers
        .get_all(CONTRACT_HEADER)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .eq([SUPPORTED_CONTRACTS[0]]);
    let json: Option<Value> = serde_json::from_slice(&body).ok();
    let outcome = if !confirmed {
        Seen::Unknown("the answer did not confirm the HTTP contract".into())
    } else if response.status.is_success() {
        let json = json.unwrap_or(Value::Null);
        Seen::Committed {
            commit: commit_of(&json),
            nodes: count_of(&json, "affected_nodes", "nodes"),
            edges: count_of(&json, "affected_edges", "edges"),
        }
    } else if response.status.is_client_error() {
        Seen::Refused(
            json.as_ref()
                .and_then(|json| json.get("error"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("HTTP {}", response.status.as_u16())),
        )
    } else {
        Seen::Unknown(format!("HTTP {}", response.status.as_u16()))
    };
    (outcome, Some((response.status, response.headers, body)))
}

/// The commit a write's answer names: a mutation's `commit.graph_commit_id`, or a top-level
/// `graph_commit_id`.
fn commit_of(json: &Value) -> Option<String> {
    json.get("commit")
        .and_then(|commit| commit.get("graph_commit_id"))
        .or_else(|| json.get("graph_commit_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// A mutation's affected count, or a load's entities loaded per type summed.
fn count_of(json: &Value, affected: &str, loaded: &str) -> i64 {
    if let Some(count) = json.get(affected).and_then(Value::as_i64) {
        return count;
    }
    json.get(loaded)
        .and_then(Value::as_array)
        .map(|types| {
            types
                .iter()
                .filter_map(|entry| entry.get("entities_loaded").and_then(Value::as_i64))
                .sum()
        })
        .unwrap_or(0)
}

async fn read_request(stream: &mut TcpStream) -> Result<Request, ReadFailure> {
    let mut buffer = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];
    let head_end = loop {
        if let Some(end) = find(&buffer, b"\r\n\r\n") {
            break end + 4;
        }
        if buffer.len() > MAX_HEAD {
            return Err(ReadFailure::Answer(
                431,
                "bad_request",
                "Studio: the request's headers are too large.".into(),
            ));
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|_| ReadFailure::Gone)?;
        if read == 0 {
            return Err(ReadFailure::Gone);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    match parsed.parse(&buffer[..head_end]) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => {
            return Err(ReadFailure::Answer(
                400,
                "bad_request",
                "Studio: the request could not be read.".into(),
            ));
        }
    }
    let method = parsed.method.unwrap_or("GET").to_owned();
    let target = parsed.path.unwrap_or("/").to_owned();
    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|header| {
            (
                header.name.to_ascii_lowercase(),
                String::from_utf8_lossy(header.value).trim().to_owned(),
            )
        })
        .collect();
    let mut rest = buffer[head_end..].to_vec();
    let chunked = headers.iter().any(|(name, value)| {
        name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
    });
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok());
    let too_large = || {
        ReadFailure::Answer(
            413,
            "bad_request",
            format!(
                "Studio: a request through Studio may carry at most {} MiB. Load in smaller \
                 parts. Nothing was sent.",
                MAX_BODY / (1024 * 1024)
            ),
        )
    };
    let body = if chunked {
        read_chunked(stream, rest, &chunk).await?
    } else if let Some(length) = length {
        if length > MAX_BODY {
            return Err(too_large());
        }
        while rest.len() < length {
            let read = stream
                .read(&mut chunk)
                .await
                .map_err(|_| ReadFailure::Gone)?;
            if read == 0 {
                return Err(ReadFailure::Gone);
            }
            rest.extend_from_slice(&chunk[..read]);
        }
        rest.truncate(length);
        rest
    } else {
        Vec::new()
    };
    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}

/// Decodes a chunked body, at most [`MAX_BODY`].
async fn read_chunked(
    stream: &mut TcpStream,
    mut pending: Vec<u8>,
    _scratch: &[u8],
) -> Result<Vec<u8>, ReadFailure> {
    let mut body = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        // The size line.
        let line_end = loop {
            if let Some(end) = find(&pending, b"\r\n") {
                break end;
            }
            if pending.len() > 1024 {
                return Err(ReadFailure::Answer(
                    400,
                    "bad_request",
                    "Studio: the request's body could not be read.".into(),
                ));
            }
            let read = stream
                .read(&mut chunk)
                .await
                .map_err(|_| ReadFailure::Gone)?;
            if read == 0 {
                return Err(ReadFailure::Gone);
            }
            pending.extend_from_slice(&chunk[..read]);
        };
        let size_text = String::from_utf8_lossy(&pending[..line_end]);
        let size_text = size_text.split(';').next().unwrap_or("").trim().to_owned();
        let size = usize::from_str_radix(&size_text, 16).map_err(|_| {
            ReadFailure::Answer(
                400,
                "bad_request",
                "Studio: the request's body could not be read.".into(),
            )
        })?;
        pending.drain(..line_end + 2);
        if body.len() + size > MAX_BODY {
            return Err(ReadFailure::Answer(
                413,
                "bad_request",
                format!(
                    "Studio: a request through Studio may carry at most {} MiB. Load in smaller \
                     parts. Nothing was sent.",
                    MAX_BODY / (1024 * 1024)
                ),
            ));
        }
        // The data and its CRLF; a zero size ends the body after the trailer's CRLF.
        while pending.len() < size + 2 {
            let read = stream
                .read(&mut chunk)
                .await
                .map_err(|_| ReadFailure::Gone)?;
            if read == 0 {
                return Err(ReadFailure::Gone);
            }
            pending.extend_from_slice(&chunk[..read]);
        }
        if size == 0 {
            return Ok(body);
        }
        body.extend_from_slice(&pending[..size]);
        pending.drain(..size + 2);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Compares two secrets in time that does not depend on where they differ.
fn same(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// A refusal or error the gateway makes itself, as OmniGraph's `ErrorOutput` with the contract
/// header, so the CLI shows it as a typed error.
async fn answer(stream: &mut TcpStream, status: u16, code: &str, message: &str) {
    let body = serde_json::to_vec(&json!({ "error": message, "code": code })).unwrap_or_default();
    write_simple(stream, status, "application/json", &body).await;
}

async fn write_simple(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} {}\r\n{CONTRACT_HEADER}: {}\r\ncontent-type: {content_type}\r\n\
         content-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        reason(status),
        SUPPORTED_CONTRACTS[0],
        body.len()
    );
    stream.write_all(head.as_bytes()).await.ok();
    stream.write_all(body).await.ok();
    stream.flush().await.ok();
}

fn reason(status: u16) -> &'static str {
    StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("")
}

/// The answer's headers, without hop-by-hop ones; the gateway sets length and framing.
fn end_to_end_headers(headers: &HeaderMap) -> String {
    let mut text = String::new();
    for (name, value) in headers {
        let name = name.as_str();
        if matches!(
            name,
            "connection"
                | "keep-alive"
                | "transfer-encoding"
                | "content-length"
                | "date"
                | "server"
        ) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    text
}

async fn write_buffered(
    stream: &mut TcpStream,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) {
    let head = format!(
        "HTTP/1.1 {} {}\r\n{}content-length: {}\r\nconnection: close\r\n\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        end_to_end_headers(headers),
        body.len()
    );
    stream.write_all(head.as_bytes()).await.ok();
    stream.write_all(body).await.ok();
    stream.flush().await.ok();
}

/// Streams a read's answer back as it arrives: with its own length when it has one, else
/// chunked. A HEAD answer keeps its length and carries no body.
async fn stream_back(
    stream: &mut TcpStream,
    method: &str,
    response: Result<graph_client::ForwardResponse, String>,
) {
    let mut response = match response {
        Ok(response) => response,
        Err(reason) => {
            answer(
                stream,
                502,
                "internal",
                &format!("Studio could not reach the graph's server: {reason}"),
            )
            .await;
            return;
        }
    };
    let length = response
        .headers
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let framing = match (method == "HEAD", length) {
        (true, Some(length)) | (false, Some(length)) => format!("content-length: {length}\r\n"),
        (true, None) => String::new(),
        (false, None) => "transfer-encoding: chunked\r\n".into(),
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\n{}{framing}connection: close\r\n\r\n",
        response.status.as_u16(),
        response.status.canonical_reason().unwrap_or(""),
        end_to_end_headers(&response.headers),
    );
    if stream.write_all(head.as_bytes()).await.is_err() || method == "HEAD" {
        stream.flush().await.ok();
        return;
    }
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = match response.body.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let written = if length.is_some() {
            stream.write_all(&buffer[..read]).await
        } else {
            let mut frame = format!("{read:x}\r\n").into_bytes();
            frame.extend_from_slice(&buffer[..read]);
            frame.extend_from_slice(b"\r\n");
            stream.write_all(&frame).await
        };
        if written.is_err() {
            return;
        }
    }
    if length.is_none() {
        stream.write_all(b"0\r\n\r\n").await.ok();
    }
    stream.flush().await.ok();
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::StreamExt as _;
    use http_client::FakeHttpClient;
    use http_client::http::Response;

    use super::*;

    const AGENT: &str = "agent/20261013-101500";

    /// What the fake server saw: method, URL, its authorization and contract headers, body.
    type Seen = Arc<Mutex<Vec<(String, String, String, String, String)>>>;

    fn upstream() -> (GraphClient, Seen) {
        let seen: Seen = Arc::default();
        let log = seen.clone();
        let http = FakeHttpClient::create(move |request| {
            let log = log.clone();
            async move {
                let header = |name: &str| {
                    request
                        .headers()
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_owned()
                };
                let (method, uri) = (request.method().to_string(), request.uri().to_string());
                let (auth, contract) = (header("authorization"), header(CONTRACT_HEADER));
                let mut body = String::new();
                request.into_body().read_to_string(&mut body).await.unwrap();
                let answer = if uri.ends_with("/mutate") {
                    r#"{"branch":"agent/20261013-101500","query_name":"m","affected_nodes":2,"affected_edges":1,"commit":{"graph_commit_id":"c9"}}"#
                } else {
                    r#"{"rows":[{"n":200}]}"#
                };
                log.lock()
                    .unwrap()
                    .push((method, uri, auth, contract, body));
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, "0.13")
                    .header("content-type", "application/json")
                    .body(AsyncBody::from(answer))
                    .unwrap())
            }
        });
        let client =
            GraphClient::new("http://127.0.0.1:8787", Some("person-token".into()), http).unwrap();
        (client, seen)
    }

    fn sandbox() -> Sandbox {
        Sandbox {
            graph: "knowledge".into(),
            branch: AGENT.into(),
            base: "main".into(),
        }
    }

    /// Sends raw HTTP to the gateway and returns the status, headers and body.
    async fn call(port: u16, raw: String) -> (u16, String, String) {
        let mut stream = smol::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await.unwrap();
        let text = String::from_utf8_lossy(&answer).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let head = head.to_ascii_lowercase();
        let body = if head.contains("transfer-encoding: chunked") {
            unchunk(body)
        } else {
            body.to_owned()
        };
        (status, head, body)
    }

    fn unchunk(mut framed: &str) -> String {
        let mut body = String::new();
        while let Some((size, rest)) = framed.split_once("\r\n") {
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            if size == 0 {
                break;
            }
            body.push_str(&rest[..size]);
            framed = &rest[size + 2..];
        }
        body
    }

    fn post(path: &str, token: &str, contract: Option<&str>, body: &str) -> String {
        let contract = contract
            .map(|value| format!("{CONTRACT_HEADER}: {value}\r\n"))
            .unwrap_or_default();
        format!(
            "POST {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nauthorization: Bearer {token}\r\n\
             {contract}content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn the_gateway_checks_the_session_then_forwards_reads_and_approved_writes() {
        smol::block_on(async {
            let (client, seen) = upstream();
            let mut gateway = Gateway::bind().unwrap();
            let (port, token) = (gateway.port, gateway.token.clone());
            let (events_tx, mut events) = mpsc::unbounded();
            let _serve = smol::spawn(gateway.serve(client, sandbox(), events_tx));

            // The CLI's probe: no token, no contract header.
            let (status, head, _) = call(
                port,
                "HEAD /healthz HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n".into(),
            )
            .await;
            assert_eq!(status, 200);
            assert!(head.contains("omnigraph-http-api: 0.13"), "{head}");

            // A wrong token, and a CLI that sends no contract.
            let (status, head, body) = call(
                port,
                post("/graphs/knowledge/query", "nope", Some("0.13"), "{}"),
            )
            .await;
            assert_eq!(status, 401);
            assert!(head.contains("omnigraph-http-api: 0.13"));
            assert!(body.contains("\"code\":\"unauthorized\""), "{body}");
            let (status, _, body) =
                call(port, post("/graphs/knowledge/query", &token, None, "{}")).await;
            assert_eq!(status, 400);
            assert!(body.contains("api_contract_mismatch"), "{body}");
            assert!(
                seen.lock().unwrap().is_empty(),
                "nothing reached the server"
            );

            // A read goes through with the person's token, and its answer comes back.
            let (status, _, body) = call(
                port,
                post(
                    "/graphs/knowledge/query",
                    &token,
                    Some("0.13"),
                    r#"{"query":"q","branch":"main"}"#,
                ),
            )
            .await;
            assert_eq!(status, 200);
            assert_eq!(body, r#"{"rows":[{"n":200}]}"#);
            assert!(matches!(events.next().await, Some(GatewayEvent::Reached)));
            {
                let seen = seen.lock().unwrap();
                let (method, uri, auth, contract, body) = &seen[0];
                assert_eq!(
                    (method.as_str(), uri.as_str()),
                    ("POST", "http://127.0.0.1:8787/graphs/knowledge/query")
                );
                assert_eq!(auth, "Bearer person-token");
                assert_eq!(contract, "0.13");
                assert_eq!(body, r#"{"query":"q","branch":"main"}"#);
            }

            // A write to main is refused before anything is sent.
            let (status, _, body) = call(
                port,
                post(
                    "/graphs/knowledge/mutate",
                    &token,
                    Some("0.13"),
                    r#"{"query":"q","branch":"main"}"#,
                ),
            )
            .await;
            assert_eq!(status, 403);
            assert!(body.contains("only to its own branch"), "{body}");
            assert_eq!(seen.lock().unwrap().len(), 1);

            // A write to the agent's branch waits for the person; rejected, nothing is sent.
            let write =
                r#"{"query":"query m() { … }","name":"m","branch":"agent/20261013-101500"}"#;
            let rejected = smol::spawn(call(
                port,
                post("/graphs/knowledge/mutate", &token, Some("0.13"), write),
            ));
            let Some(GatewayEvent::Write {
                decide,
                write: summary,
                ..
            }) = events.next().await
            else {
                panic!("no write event")
            };
            assert_eq!(summary.kind, gateway_rules::WriteKind::Mutate);
            decide.send(false).unwrap();
            let (status, _, body) = rejected.await;
            assert_eq!(status, 403);
            assert!(body.contains("rejected"), "{body}");
            assert_eq!(seen.lock().unwrap().len(), 1);

            // Allowed, it is sent once and its outcome reported.
            let allowed = smol::spawn(call(
                port,
                post("/graphs/knowledge/mutate", &token, Some("0.13"), write),
            ));
            let Some(GatewayEvent::Write { decide, id, .. }) = events.next().await else {
                panic!("no write event")
            };
            decide.send(true).unwrap();
            let (status, head, body) = allowed.await;
            assert_eq!(status, 200);
            assert!(head.contains("omnigraph-http-api: 0.13"));
            assert!(body.contains("\"graph_commit_id\":\"c9\""), "{body}");
            let Some(GatewayEvent::Finished {
                id: finished,
                outcome,
            }) = events.next().await
            else {
                panic!("no outcome")
            };
            assert_eq!(finished, id);
            assert_eq!(
                outcome,
                super::Seen::Committed {
                    commit: Some("c9".into()),
                    nodes: 2,
                    edges: 1
                }
            );
            assert_eq!(seen.lock().unwrap().len(), 2);

            // A chunked body is read whole before the rules see it.
            let chunked = format!(
                "POST /graphs/knowledge/query HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {token}\r\n\
                 {CONTRACT_HEADER}: 0.13\r\ncontent-type: application/json\r\n\
                 transfer-encoding: chunked\r\n\r\n5\r\n{{\"a\":\r\n2\r\n1}}\r\n0\r\n\r\n"
            );
            let (status, _, _) = call(port, chunked).await;
            assert_eq!(status, 200);
            assert_eq!(seen.lock().unwrap()[2].4, r#"{"a":1}"#);
        });
    }
}

/// The stock `omnigraph` CLI through the real gateway and CLI home, against a running server.
/// Needs `STUDIO_GATEWAY_E2E_SERVER` (for example `http://127.0.0.1:8787`, `script/dev-server`),
/// `STUDIO_GATEWAY_E2E_TOKEN` and `STUDIO_GATEWAY_E2E_GRAPH`, and an omnigraph CLI that speaks
/// the server's API on PATH or at `STUDIO_GATEWAY_E2E_CLI`. It creates and deletes a branch.
#[cfg(test)]
mod end_to_end {
    use std::path::PathBuf;
    use std::process::Command;

    use futures::StreamExt as _;

    use super::*;
    use crate::cli_home::{self, CliHome};

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }

    /// Runs the conversation's wrapper; returns success, stdout and stderr.
    fn run(home: &CliHome, args: &[&str]) -> (bool, String, String) {
        let output = Command::new(&home.command)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    #[test]
    #[ignore = "needs a running OmniGraph server and an omnigraph CLI: set STUDIO_GATEWAY_E2E_*"]
    fn the_stock_cli_reads_and_writes_its_branch_through_the_gateway() {
        let (Some(server), Some(token), Some(graph)) = (
            env("STUDIO_GATEWAY_E2E_SERVER"),
            env("STUDIO_GATEWAY_E2E_TOKEN"),
            env("STUDIO_GATEWAY_E2E_GRAPH"),
        ) else {
            panic!("set STUDIO_GATEWAY_E2E_SERVER, _TOKEN and _GRAPH");
        };
        let http: Arc<dyn http_client::HttpClient> =
            Arc::new(reqwest_client::ReqwestClient::user_agent("studio-gateway-e2e").unwrap());
        let client = GraphClient::new(&server, Some(token), http).unwrap();
        let branch = format!("agent/e2e-{}", std::process::id());
        let cwd = std::env::temp_dir().join(format!("studio-gateway-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();

        let mut gateway = Gateway::bind().unwrap();
        let home =
            CliHome::write(&cwd, gateway.port, &gateway.token, &graph, &branch, "main").unwrap();
        let (events_tx, mut events) = mpsc::unbounded();
        let serve = gateway.serve(
            client,
            Sandbox {
                graph: graph.clone(),
                branch: branch.clone(),
                base: "main".into(),
            },
            events_tx,
        );
        let _serve = smol::spawn(serve);
        // The person allows every write, and the outcomes are kept.
        let outcomes: Arc<std::sync::Mutex<Vec<(String, super::Seen)>>> = Arc::default();
        let kept = outcomes.clone();
        let _person = smol::spawn(async move {
            let mut summaries = std::collections::HashMap::new();
            while let Some(event) = events.next().await {
                match event {
                    GatewayEvent::Write { id, write, decide } => {
                        summaries.insert(id, write.summary);
                        decide.send(true).ok();
                    }
                    GatewayEvent::Finished { id, outcome } => {
                        let summary = summaries.remove(&id).unwrap_or_default();
                        kept.lock().unwrap().push((summary, outcome));
                    }
                    GatewayEvent::Reached => {}
                }
            }
        });

        let candidates = match env("STUDIO_GATEWAY_E2E_CLI") {
            Some(cli) => vec![PathBuf::from(cli)],
            None => cli_home::candidates(None),
        };
        let chosen = smol::block_on(cli_home::choose_cli(&home, candidates));
        assert!(
            chosen.is_some(),
            "no omnigraph CLI answered through the gateway"
        );

        let target = ["--server", "studio", "--graph", graph.as_str()];
        let with = |args: &[&str]| -> Vec<String> {
            args.iter()
                .chain(target.iter())
                .map(|arg| (*arg).to_owned())
                .collect()
        };
        let call = |args: Vec<String>| {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            run(&home, &args)
        };

        // The same CLI run from Studio for a sandboxed agent (`omnigraph_run`): a read returned
        // whole, and one saved to a file in the working directory.
        let read = [
            "query",
            "-e",
            "query q() { match { $p: Person } return { count($p) as n } }",
            "--json",
        ]
        .map(str::to_owned);
        let checked = cli_home::check_run_args(&read, &cwd, &graph).unwrap();
        let (text, failed) = smol::block_on(cli_home::run(&home, &cwd, checked.clone(), None));
        assert!(
            !failed && text.starts_with("exit 0") && text.contains("\"rows\""),
            "{text}"
        );
        let saved = cli_home::within(&cwd, "count.json", true).unwrap();
        let (text, failed) =
            smol::block_on(cli_home::run(&home, &cwd, checked, Some(saved.clone())));
        assert!(!failed && text.contains("output saved to"), "{text}");
        assert!(
            std::fs::read_to_string(&saved)
                .unwrap()
                .contains("\"rows\"")
        );

        // A read on main.
        let (ok, out, err) = call(with(&[
            "query",
            "-e",
            "query q() { match { $p: Person } return { count($p) as n } }",
            "--json",
        ]));
        assert!(ok, "{err}");
        assert!(out.contains("\"rows\""), "{out}");

        // Writes elsewhere are refused before anything is sent.
        let (ok, _, err) = call(with(&[
            "mutate",
            "--branch",
            "main",
            "-e",
            "query m() { update Person set { title: \"x\" } where @id = \"nobody\" }",
        ]));
        assert!(!ok && err.contains("only to its own branch"), "{err}");
        let (ok, _, err) = call(with(&["mutate", "-e", "branch create \"x\" from \"main\""]));
        assert!(!ok && err.contains("statements"), "{err}");

        // The agent creates its branch, writes it, and merges are refused into main.
        let (ok, _, err) = call(with(&["branch", "create", &branch, "--from", "main"]));
        assert!(ok, "{err}");
        let (ok, out, err) = call(with(&[
            "mutate",
            "--branch",
            &branch,
            "-e",
            "query m() { update Person set { title: \"Through the gateway\" } where @id = \"person-000002\" }",
            "--json",
        ]));
        assert!(ok, "{err}");
        assert!(out.contains("\"affected_nodes\": 1"), "{out}");
        let data = cwd.join("rows.jsonl");
        std::fs::write(
            &data,
            "{\"type\":\"Person\",\"data\":{\"slug\":\"gateway-e2e\",\"name\":\"Gateway E2E\"}}\n",
        )
        .unwrap();
        let (ok, _, err) = call(with(&[
            "load",
            "--data",
            data.to_str().unwrap(),
            "--mode",
            "append",
            "--branch",
            &branch,
        ]));
        assert!(ok, "{err}");
        let (ok, _, err) = call(with(&["branch", "merge", &branch, "--into", "main"]));
        assert!(
            !ok && err.contains("merge only into its own branch"),
            "{err}"
        );

        // The branch is the agent's to delete.
        let (ok, _, err) = call(with(&["branch", "delete", &branch, "--yes"]));
        assert!(ok, "{err}");

        let outcomes = outcomes.lock().unwrap().clone();
        let summaries: Vec<&str> = outcomes
            .iter()
            .map(|(summary, _)| summary.as_str())
            .collect();
        assert_eq!(
            summaries,
            [
                format!("Create {branch} from main").as_str(),
                "Mutation",
                format!("Load (append) into {branch}").as_str(),
                format!("Delete {branch}").as_str(),
            ]
        );
        assert!(matches!(
            outcomes[1].1,
            super::Seen::Committed {
                commit: Some(_),
                nodes: 1,
                ..
            }
        ));
        drop(home);
        std::fs::remove_dir_all(&cwd).ok();
    }
}
