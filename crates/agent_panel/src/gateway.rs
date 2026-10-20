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
use std::sync::atomic::{AtomicU64, Ordering};

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
/// How long a request may take to arrive whole, its body included.
const READ_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);
/// How long a request's line and headers, its token among them, may take: any local process
/// can open a connection, and one that sends nothing is let go before it holds one long.
const HEAD_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);
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
    /// A write for the agent's branch waits for `decide`: `Some` sends it, with what records
    /// its outcome.
    Write {
        id: u64,
        write: WriteSummary,
        decide: oneshot::Sender<Option<OnOutcome>>,
    },
    /// How a forwarded write ended (constitution III).
    Finished { id: u64, outcome: Seen },
    /// The CLI stopped waiting before the write was decided (its command timed out or was
    /// stopped): nothing was sent, and nothing will be.
    Abandoned { id: u64 },
}

/// What records an approved write's outcome. It is called once, with the outcome, however the
/// write ends, though the conversation that sent it stops meanwhile; dropped uncalled (Studio
/// quit while it was sent), it says nothing more.
pub type OnOutcome = Box<dyn FnOnce(&Seen) + Send>;

/// What an approved write that was not sent, because the CLI stopped waiting, records.
pub const NOT_SENT: &str = "The agent's CLI stopped waiting, so nothing was sent.";

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
            token: crate::confined::random_hex(32)?,
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
        });
        async move {
            let Some(listener) = listener else { return };
            crate::socket::serve_each(
                || listener.accept(),
                |(stream, _)| serve_connection(stream, shared.clone()),
            )
            .await;
        }
    }
}

struct Shared {
    client: GraphClient,
    sandbox: Sandbox,
    token: String,
    events: mpsc::UnboundedSender<GatewayEvent>,
    next_id: AtomicU64,
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
    // A local process that opens a connection and sends slowly, or nothing, holds it no longer
    // than this.
    let read = {
        let read = futures::FutureExt::fuse(read_request(&mut stream, &shared.token));
        let late = futures::FutureExt::fuse(smol::Timer::after(READ_WITHIN));
        futures::pin_mut!(read, late);
        futures::select! {
            read = read => read,
            _ = late => Err(ReadFailure::Gone),
        }
    };
    let request = match read {
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
    // The rules judge the target as split here, and the server must read the same request. A
    // `#` never travels in HTTP, and forwarding drops it with all that follows, so the rules
    // would judge a query the server never sees; a repeated key could be judged by one value
    // and read by the server as another.
    let mut keys = std::collections::HashSet::new();
    if request.target.contains('#') || !pairs.iter().all(|(key, _)| keys.insert(key.as_str())) {
        answer(
            &mut stream,
            400,
            "bad_request",
            "Studio: a request with a `#` or a repeated query parameter is not sent. Nothing \
             was sent.",
        )
        .await;
        return;
    }
    // The CLI probes `/healthz` with no token and no contract header before every request.
    // Every other request carries the session's token, checked before its body was read.
    if is_health(&request.method, &path) {
        let body = if request.method == "HEAD" { "" } else { "ok" };
        write_simple(&mut stream, 200, "text/plain", body.as_bytes()).await;
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
            // The person may decide after the CLI has given up (its shell's two-minute timeout):
            // a write approved then would land with the agent told it failed. A closed
            // connection while waiting ends the write unsent.
            let gone = {
                let mut watch = stream.clone();
                async move {
                    let mut byte = [0u8; 1];
                    while matches!(
                        futures::AsyncReadExt::read(&mut watch, &mut byte).await,
                        Ok(1..)
                    ) {}
                }
            };
            let (decision, gone) = (
                futures::FutureExt::fuse(decision),
                futures::FutureExt::fuse(gone),
            );
            futures::pin_mut!(decision, gone);
            let decided = futures::select! {
                decided = decision => Some(decided),
                () = gone => None,
            };
            let Some(decided) = decided else {
                // An approval that came as the CLI gave up is not sent: it says so, rather
                // than being dropped and taken for a write whose outcome is unknown.
                if let Some(Ok(Some(on_outcome))) =
                    futures::FutureExt::now_or_never(decision.as_mut())
                {
                    on_outcome(&Seen::Refused(NOT_SENT.into()));
                }
                shared
                    .events
                    .unbounded_send(GatewayEvent::Abandoned { id })
                    .ok();
                return;
            };
            let Ok(Some(on_outcome)) = decided else {
                answer(
                    &mut stream,
                    403,
                    "forbidden",
                    "Studio: the person rejected this write. Nothing was sent.",
                )
                .await;
                return;
            };
            // Once sent, a write runs to its end in a task of its own, and its outcome is
            // recorded: the conversation stopping, or the CLI going, would otherwise drop it
            // halfway with the server perhaps committing it unseen.
            let (client, events) = (shared.client.clone(), shared.events.clone());
            let forward = forward_request(&request, None);
            let (reply_tx, reply_rx) = oneshot::channel();
            smol::spawn(async move {
                let response = client.forward(forward).await;
                let (outcome, reply) = read_write_answer(response).await;
                on_outcome(&outcome);
                events
                    .unbounded_send(GatewayEvent::Finished { id, outcome })
                    .ok();
                reply_tx.send(reply).ok();
            })
            .detach();
            // When the server's answer was lost, the CLI's connection closes without one, and
            // the CLI reports the outcome as unknown, as it is.
            if let Ok(Some((status, headers, body))) = reply_rx.await {
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

/// The health probe, which carries no token.
fn is_health(method: &str, path: &str) -> bool {
    matches!(method, "GET" | "HEAD") && path == "/healthz"
}

/// Reads one request. Its token is checked once its headers are in, before its body (up to
/// [`MAX_BODY`]) is read: only the conversation's CLI makes Studio read that much.
/// Reads into `buffer` up to the end of a request's headers, and where that is.
async fn read_head(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<usize, ReadFailure> {
    let mut chunk = [0u8; 8 * 1024];
    loop {
        if let Some(end) = find(buffer, b"\r\n\r\n") {
            return Ok(end + 4);
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
    }
}

async fn read_request(stream: &mut TcpStream, token: &str) -> Result<Request, ReadFailure> {
    let mut buffer = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];
    let head_end = {
        let head = futures::FutureExt::fuse(read_head(stream, &mut buffer));
        let late = futures::FutureExt::fuse(smol::Timer::after(HEAD_WITHIN));
        futures::pin_mut!(head, late);
        futures::select! {
            end = head => end,
            _ = late => Err(ReadFailure::Gone),
        }
    }?;
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
    let path = target.split('?').next().unwrap_or_default();
    let authorized = headers
        .iter()
        .find(|(name, _)| name == "authorization")
        .and_then(|(_, value)| value.strip_prefix("Bearer "))
        .is_some_and(|sent| same(sent.as_bytes(), token.as_bytes()));
    if !authorized && !is_health(&method, path) {
        return Err(ReadFailure::Answer(
            401,
            "unauthorized",
            "Studio: this token is not the one Studio wrote for this conversation.".into(),
        ));
    }
    let mut rest = buffer[head_end..].to_vec();
    let chunked = headers.iter().any(|(name, value)| {
        name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
    });
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok());
    let body = if chunked {
        read_chunked(stream, rest).await?
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

/// The refusal of a body past [`MAX_BODY`].
fn too_large() -> ReadFailure {
    ReadFailure::Answer(
        413,
        "bad_request",
        format!(
            "Studio: a request through Studio may carry at most {} MiB. Load in smaller parts. \
             Nothing was sent.",
            MAX_BODY / (1024 * 1024)
        ),
    )
}

/// Decodes a chunked body, at most [`MAX_BODY`].
async fn read_chunked(
    stream: &mut TcpStream,
    mut pending: Vec<u8>,
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
        // Compared without adding to `size`, which the request chose and may be near the
        // largest number.
        if size > MAX_BODY - body.len() {
            return Err(too_large());
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
            Ok(0) => break,
            Ok(read) => read,
            // The answer broke off: the connection closes without the chunked stream's end
            // (or short of its length), so the CLI sees a cut answer, never a complete one.
            Err(_) => return,
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

    /// An approved write on its way when the gateway stops (the conversation restarted) runs to
    /// its end, and its outcome reaches what records it.
    #[test]
    fn an_approved_write_ends_and_reports_though_the_gateway_stops() {
        smol::block_on(async {
            let (arrived_tx, arrived) = oneshot::channel::<()>();
            let (release, released) = oneshot::channel::<()>();
            let gates = Arc::new(Mutex::new((Some(arrived_tx), Some(released))));
            let held = gates.clone();
            let http = FakeHttpClient::create(move |_request| {
                let held = held.clone();
                async move {
                    let (arrived, released) = {
                        let mut gates = held.lock().unwrap();
                        (gates.0.take(), gates.1.take())
                    };
                    if let Some(arrived) = arrived {
                        arrived.send(()).ok();
                    }
                    if let Some(released) = released {
                        released.await.ok();
                    }
                    Ok(Response::builder()
                        .status(200)
                        .header(CONTRACT_HEADER, "0.13")
                        .header("content-type", "application/json")
                        .body(AsyncBody::from(
                            r#"{"branch":"agent/20261013-101500","query_name":"m","affected_nodes":1,"affected_edges":0,"commit":{"graph_commit_id":"c9"}}"#,
                        ))
                        .unwrap())
                }
            });
            let client =
                GraphClient::new("http://127.0.0.1:8787", Some("person-token".into()), http)
                    .unwrap();
            let mut gateway = Gateway::bind().unwrap();
            let (port, token) = (gateway.port, gateway.token.clone());
            let (events_tx, mut events) = mpsc::unbounded();
            let serve = smol::spawn(gateway.serve(client, sandbox(), events_tx));
            let write =
                r#"{"query":"query m() { … }","name":"m","branch":"agent/20261013-101500"}"#;
            let _cli = smol::spawn(call(
                port,
                post("/graphs/knowledge/mutate", &token, Some("0.13"), write),
            ));
            let Some(GatewayEvent::Write { decide, .. }) = events.next().await else {
                panic!("no write event")
            };
            let (recorded, outcome) = oneshot::channel::<super::Seen>();
            decide
                .send(Some(Box::new(move |seen: &super::Seen| {
                    recorded.send(seen.clone()).ok();
                })))
                .ok()
                .unwrap();
            arrived.await.unwrap();
            drop(serve);
            release.send(()).unwrap();
            assert_eq!(
                outcome.await.unwrap(),
                super::Seen::Committed {
                    commit: Some("c9".into()),
                    nodes: 1,
                    edges: 0
                }
            );
        });
    }

    /// A read whose answer breaks off partway closes without the chunked stream's end: the CLI
    /// sees a cut answer, not a complete short one.
    #[test]
    fn a_read_cut_off_partway_does_not_end_as_complete() {
        struct Broken;
        impl futures::AsyncRead for Broken {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &mut [u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::ErrorKind::ConnectionReset.into()))
            }
        }
        smol::block_on(async {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let reader = smol::spawn(async move {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                let mut raw = Vec::new();
                stream.read_to_end(&mut raw).await.unwrap();
                String::from_utf8(raw).unwrap()
            });
            let (mut stream, _) = listener.accept().await.unwrap();
            let body = futures::io::Cursor::new(br#"{"rows":["#.to_vec()).chain(Broken);
            let response = graph_client::ForwardResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: AsyncBody::from_reader(body),
            };
            stream_back(&mut stream, "POST", Ok(response)).await;
            drop(stream);
            let raw = reader.await;
            assert!(raw.contains("transfer-encoding: chunked"), "{raw}");
            assert!(
                raw.contains(r#"{"rows":["#),
                "what arrived is passed on: {raw}"
            );
            assert!(!raw.ends_with("0\r\n\r\n"), "no end of the stream: {raw}");
        });
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
            // Its body is never waited for: one that says it is large, and never comes, is
            // answered at once.
            let declared = "POST /graphs/knowledge/load HTTP/1.1\r\nhost: x\r\n\
                authorization: Bearer wrong\r\ncontent-length: 50000000\r\n\r\n"
                .to_owned();
            let (status, _, _) = call(port, declared).await;
            assert_eq!(status, 401);
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

            // A target the rules would read differently from the server is refused: forwarding
            // drops a `#` and what follows it, so this load would land on main.
            for target in [
                "/graphs/knowledge/load/ndjson?mode=overwrite#&branch=agent%2F20261013-101500",
                "/graphs/knowledge/load/ndjson?branch=agent%2F20261013-101500&branch=main&mode=append",
            ] {
                let answer = futures::FutureExt::fuse(call(
                    port,
                    post(target, &token, Some("0.13"), "{}\n"),
                ));
                let write = futures::FutureExt::fuse(events.next());
                futures::pin_mut!(answer, write);
                let (status, _, body) = futures::select! {
                    answer = answer => answer,
                    _ = write => panic!("{target} was judged a write to the agent's branch"),
                };
                assert_eq!(status, 400, "{target}: {body}");
                assert!(body.contains("Nothing was sent"), "{body}");
            }
            assert_eq!(seen.lock().unwrap().len(), 1, "nothing reached the server");

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
            decide.send(None).ok().unwrap();
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
            let recorded: Arc<Mutex<Option<super::Seen>>> = Arc::default();
            let record = recorded.clone();
            decide
                .send(Some(Box::new(move |seen: &super::Seen| {
                    *record.lock().unwrap() = Some(seen.clone());
                })))
                .ok()
                .unwrap();
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
            assert_eq!(recorded.lock().unwrap().as_ref(), Some(&outcome));
            assert_eq!(seen.lock().unwrap().len(), 2);

            // The CLI gives up before the person decides (its command timed out): the write is
            // never sent, even when allowed afterwards.
            let mut hung_up = smol::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            hung_up
                .write_all(post("/graphs/knowledge/mutate", &token, Some("0.13"), write).as_bytes())
                .await
                .unwrap();
            let Some(GatewayEvent::Write { decide, id, .. }) = events.next().await else {
                panic!("no write event")
            };
            drop(hung_up);
            let Some(GatewayEvent::Abandoned { id: abandoned }) = events.next().await else {
                panic!("the gateway did not see the CLI go")
            };
            assert_eq!(abandoned, id);
            assert!(
                decide.send(Some(Box::new(|_: &super::Seen| {}))).is_err(),
                "nobody waits for the decision"
            );
            assert_eq!(seen.lock().unwrap().len(), 2, "nothing was sent");

            // A chunked body is read whole before the rules see it.
            let chunked = format!(
                "POST /graphs/knowledge/query HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {token}\r\n\
                 {CONTRACT_HEADER}: 0.13\r\ncontent-type: application/json\r\n\
                 transfer-encoding: chunked\r\n\r\n5\r\n{{\"a\":\r\n2\r\n1}}\r\n0\r\n\r\n"
            );
            let (status, _, _) = call(port, chunked).await;
            assert_eq!(status, 200);
            assert_eq!(seen.lock().unwrap()[2].4, r#"{"a":1}"#);

            // A chunk size near the largest number is refused, not added to.
            let huge = format!(
                "POST /graphs/knowledge/query HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {token}\r\n\
                 {CONTRACT_HEADER}: 0.13\r\ncontent-type: application/json\r\n\
                 transfer-encoding: chunked\r\n\r\n1\r\n{{\r\n{:x}\r\n",
                usize::MAX
            );
            let (status, _, body) = call(port, huge).await;
            assert_eq!(status, 413, "{body}");
            assert_eq!(seen.lock().unwrap().len(), 3, "nothing was sent");
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
        let home = CliHome::write(
            &cwd,
            &cwd.join("private"),
            gateway.port,
            &gateway.token,
            &graph,
            &branch,
            "main",
        )
        .unwrap();
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
                        decide.send(Some(Box::new(|_: &super::Seen| {}))).ok();
                    }
                    GatewayEvent::Finished { id, outcome } => {
                        let summary = summaries.remove(&id).unwrap_or_default();
                        kept.lock().unwrap().push((summary, outcome));
                    }
                    GatewayEvent::Abandoned { .. } => {}
                }
            }
        });

        let candidates = match env("STUDIO_GATEWAY_E2E_CLI") {
            Some(cli) => vec![PathBuf::from(cli)],
            None => cli_home::candidates(None),
        };
        let chosen = smol::block_on(cli_home::choose_cli(
            &home,
            candidates,
            graph_client::SUPPORTED_CONTRACTS[0],
        ));
        assert!(
            matches!(chosen, cli_home::CliChoice::Chosen { .. }),
            "no omnigraph CLI speaks the graph's contract: {chosen:?}"
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
        let mut plan = cli_home::check_run_args(&read, &cwd, &graph).unwrap();
        let (text, failed) = smol::block_on(cli_home::run(&home, &cwd, plan.clone()));
        assert!(
            !failed && text.starts_with("exit 0") && text.contains("\"rows\""),
            "{text}"
        );
        plan.save_to = Some(cli_home::RunFile {
            parts: cli_home::inside(&cwd, "count.json").unwrap(),
            output: true,
        });
        let (text, failed) = smol::block_on(cli_home::run(&home, &cwd, plan));
        assert!(!failed && text.contains("output saved to"), "{text}");
        assert!(
            std::fs::read_to_string(cwd.join("count.json"))
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
