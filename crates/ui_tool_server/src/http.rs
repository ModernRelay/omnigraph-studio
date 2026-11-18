//! Streamable HTTP for the tool server (spec 032, The server): `POST /mcp` with plain JSON
//! answers, on `127.0.0.1`. One request per connection, closed after its answer.
//!
//! Every request's `Origin` and bearer token are checked once its line and headers are in, and
//! before a byte of its body is read: a web page cannot reach the server through the browser,
//! and only a token Studio gave out makes it read a body, at most [`MAX_BODY`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::tools::{Answer, Caller, Target};
use futures::channel::{mpsc, oneshot};
use futures::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use gpui::SharedString;
use serde_json::{Value, json};
use smol::net::TcpStream;

use crate::mcp::{self, Incoming};

/// The most connections served at once, so connections another local process holds open
/// cannot use up Studio's file descriptors.
pub const MAX_CONNECTIONS: usize = 16;
/// The most a request's body may take.
pub const MAX_BODY: usize = 64 * 1024;
/// The most an answer's body may take.
pub const MAX_ANSWER: usize = 64 * 1024;
/// The most calls one token may have in flight: a fifth is refused with `busy`, so one agent
/// cannot queue a burst of tabs.
pub const MAX_IN_FLIGHT: usize = 4;
/// The most a request's line and headers may take.
const MAX_HEAD: usize = 16 * 1024;
/// How long, from the connection's opening, its line and headers may take. A local process
/// that opens a connection and sends nothing is let go by then.
const HEAD_WITHIN: Duration = Duration::from_secs(10);
/// How long, from the connection's opening, its whole request may take.
const READ_WITHIN: Duration = Duration::from_secs(30);
/// How long a listener waits after a connection it could not take.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// One call for the main thread, which runs it in its window and answers on `reply`.
pub struct Call {
    /// The token the call came with, by which the main thread marks the outside notice given.
    pub token: String,
    pub target: Target,
    pub caller: Caller,
    pub tool: String,
    pub input: Value,
    pub reply: oneshot::Sender<Answer>,
}

/// What Studio holds of one token it gave out.
pub struct TokenState {
    pub target: Target,
    /// An agent outside Studio's token, which the outside-agent switch gave out.
    pub outside: bool,
    /// The agent's name, from its `initialize`.
    pub agent: Option<SharedString>,
    /// Calls sent and not yet answered.
    pub in_flight: usize,
    /// The window it acted in was told an outside agent is there (once a run).
    pub announced: bool,
}

impl TokenState {
    pub fn new(target: Target, outside: bool) -> Self {
        Self {
            target,
            outside,
            agent: None,
            in_flight: 0,
            announced: false,
        }
    }
}

/// What every connection shares: the tokens, the tool lists, and the way to the main thread.
pub struct Shared {
    pub tokens: Mutex<HashMap<String, TokenState>>,
    /// `tools/list` for an outside agent's token, which names a window in each call.
    pub tools_any: Value,
    /// `tools/list` for a conversation's token, whose window is fixed.
    pub tools_bound: Value,
    pub calls: mpsc::UnboundedSender<Call>,
}

/// Whether a connection's peer showed a token Studio gave out: a full listener lets go of
/// connections that have not first.
#[derive(Clone, Default)]
struct Standing(Arc<AtomicBool>);

impl Standing {
    fn prove(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn proved(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Serves each connection the listener takes on a task of its own, at most
/// [`MAX_CONNECTIONS`] at once. A full listener closes the oldest connection whose peer has not
/// shown a token to take the next, as the gateway's does (spec 013), so connections another
/// local process holds open never keep out an agent; with every one proved, the next is closed
/// unread.
pub async fn serve(listener: smol::net::TcpListener, shared: Arc<Shared>) {
    let mut connections: Vec<(smol::Task<()>, Standing)> = Vec::new();
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            smol::Timer::after(ACCEPT_RETRY).await;
            continue;
        };
        connections.retain(|(task, _)| !task.is_finished());
        if connections.len() >= MAX_CONNECTIONS {
            match connections
                .iter()
                .position(|(_, standing)| !standing.proved())
            {
                Some(oldest) => drop(connections.remove(oldest)),
                None => {
                    drop(stream);
                    continue;
                }
            }
        }
        let standing = Standing::default();
        let task = smol::spawn(serve_connection(stream, shared.clone(), standing.clone()));
        connections.push((task, standing));
    }
}

/// `future`'s output, or `None` when it is not done by `deadline`.
async fn by<T>(deadline: Instant, future: impl Future<Output = T>) -> Option<T> {
    let future = futures::FutureExt::fuse(future);
    let late = futures::FutureExt::fuse(smol::Timer::at(deadline));
    futures::pin_mut!(future, late);
    futures::select! {
        output = future => Some(output),
        _ = late => None,
    }
}

/// An answer: status, extra headers, and a JSON body or none.
struct Reply {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: Option<Value>,
}

impl Reply {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Some(body),
        }
    }

    /// A refusal before the request reached MCP, as JSON a person or a client can read.
    fn refuse(status: u16, message: &str) -> Self {
        Self::json(status, json!({ "error": message }))
    }
}

async fn serve_connection(mut stream: TcpStream, shared: Arc<Shared>, standing: Standing) {
    let opened = Instant::now();
    let reply = by(
        opened + READ_WITHIN,
        answer(&mut stream, &shared, opened + HEAD_WITHIN, &standing),
    )
    .await
    .flatten();
    if let Some(reply) = reply {
        write_reply(&mut stream, reply).await;
    }
    stream.close().await.ok();
}

/// The head of a request: method, path and headers by lower-case name.
struct Head {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Reads up to the end of the request's headers: the head, and the body's bytes read with it.
async fn read_head(stream: &mut (impl AsyncRead + Unpin)) -> Result<(Head, Vec<u8>), Reply> {
    let mut buffer = Vec::with_capacity(4 * 1024);
    let mut chunk = [0u8; 4 * 1024];
    let end = loop {
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        if buffer.len() > MAX_HEAD {
            return Err(Reply::refuse(431, "The request's headers are too large."));
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|_| Reply::refuse(400, "The request could not be read."))?;
        if read == 0 {
            return Err(Reply::refuse(400, "The request ended before its headers."));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    if !matches!(
        parsed.parse(&buffer[..end]),
        Ok(httparse::Status::Complete(_))
    ) {
        return Err(Reply::refuse(400, "The request could not be read."));
    }
    let head = Head {
        method: parsed.method.unwrap_or_default().to_owned(),
        path: parsed
            .path
            .unwrap_or("/")
            .split('?')
            .next()
            .unwrap_or_default()
            .to_owned(),
        headers: parsed
            .headers
            .iter()
            .map(|header| {
                (
                    header.name.to_ascii_lowercase(),
                    String::from_utf8_lossy(header.value).trim().to_owned(),
                )
            })
            .collect(),
    };
    Ok((head, buffer[end..].to_vec()))
}

/// The answer to one request, or `None` when the client went away.
async fn answer(
    stream: &mut (impl AsyncRead + Unpin),
    shared: &Shared,
    head_by: Instant,
    standing: &Standing,
) -> Option<Reply> {
    let (head, mut body) = match by(head_by, read_head(stream)).await? {
        Ok(read) => read,
        Err(reply) => return Some(reply),
    };
    // A browser sends `Origin` with every cross-site request; neither agent sends one. A page
    // that rebinds a name to 127.0.0.1 is refused here, before its token or body are looked at.
    if head.header("origin").is_some_and(|origin| origin != "null") {
        return Some(Reply::refuse(
            403,
            "Studio's tool server takes no request from a web page.",
        ));
    }
    let token = head
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .and_then(|sent| known_token(shared, sent));
    let Some(token) = token else {
        let mut reply = Reply::refuse(
            401,
            "This token is not one Studio gave out this run. Copy Studio's command again from \
             Settings, Agent.",
        );
        reply.headers.push(("www-authenticate", "Bearer"));
        return Some(reply);
    };
    standing.prove();
    if head.path != "/mcp" {
        return Some(Reply::refuse(404, "Studio's tool server answers at /mcp."));
    }
    if head.method != "POST" {
        let mut reply = Reply::refuse(405, "Studio's tool server takes POST, and keeps no stream.");
        reply.headers.push(("allow", "POST"));
        return Some(reply);
    }
    if head
        .header("transfer-encoding")
        .is_some_and(|coding| !coding.eq_ignore_ascii_case("identity"))
    {
        return Some(Reply::refuse(411, "Send the request's length."));
    }
    let length = match head.header("content-length").map(str::parse::<usize>) {
        Some(Ok(length)) => length,
        None => 0,
        Some(Err(_)) => {
            return Some(Reply::refuse(
                400,
                "The request's length could not be read.",
            ));
        }
    };
    if length > MAX_BODY {
        return Some(Reply::refuse(
            413,
            &format!("A request may take at most {} KiB.", MAX_BODY / 1024),
        ));
    }
    let mut chunk = [0u8; 4 * 1024];
    while body.len() < length {
        let want = (length - body.len()).min(chunk.len());
        let read = stream.read(&mut chunk[..want]).await.ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Some(rpc(&body, &token, shared).await)
}

/// The token as Studio holds it, when `sent` is one it gave out, compared in time that does not
/// depend on where they differ.
fn known_token(shared: &Shared, sent: &str) -> Option<String> {
    let tokens = shared.tokens.lock().ok()?;
    tokens
        .keys()
        .find(|token| same(token.as_bytes(), sent.as_bytes()))
        .cloned()
}

fn same(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Answers one JSON-RPC message from `token`'s agent.
async fn rpc(body: &[u8], token: &str, shared: &Shared) -> Reply {
    match mcp::handle(body) {
        Incoming::Reply(reply) => Reply::json(200, reply),
        Incoming::Nothing => Reply {
            status: 202,
            headers: Vec::new(),
            body: None,
        },
        Incoming::Initialize {
            id,
            version,
            client,
        } => {
            if let Ok(mut tokens) = shared.tokens.lock()
                && let Some(state) = tokens.get_mut(token)
            {
                state.agent = client.map(|client| agent_name(&client));
            }
            Reply::json(200, mcp::initialize(id, version))
        }
        Incoming::ListTools { id } => {
            let bound = shared
                .tokens
                .lock()
                .ok()
                .and_then(|tokens| tokens.get(token).map(|state| state.target))
                .is_some_and(|target| matches!(target, Target::Bound(_)));
            let tools = if bound {
                &shared.tools_bound
            } else {
                &shared.tools_any
            };
            Reply::json(200, mcp::tools_list(id, tools))
        }
        Incoming::CallTool {
            id,
            name,
            arguments,
        } => {
            let answer = call(shared, token, name.clone(), arguments).await;
            log::info!(
                "UI tool {}: {} (window {})",
                logged_name(&name),
                answer.outcome,
                answer.json["window"]
                    .as_u64()
                    .map_or_else(|| "none".to_owned(), |id| id.to_string())
            );
            let reply = mcp::tool_reply(id.clone(), &answer.says, &answer.json, answer.is_error);
            if reply.to_string().len() > MAX_ANSWER {
                return Reply::json(
                    200,
                    mcp::tool_reply(
                        id,
                        "Studio's answer was larger than 64 KiB, so it was not sent.",
                        &json!({ "outcome": "too_large" }),
                        true,
                    ),
                );
            }
            Reply::json(200, reply)
        }
    }
}

/// A call sent to the main thread, counted in flight for its token until it is answered.
async fn call(shared: &Shared, token: &str, tool: String, input: Value) -> Answer {
    let (target, caller) = {
        let Ok(mut tokens) = shared.tokens.lock() else {
            return Answer::busy(MAX_IN_FLIGHT);
        };
        let Some(state) = tokens.get_mut(token) else {
            return Answer::busy(MAX_IN_FLIGHT);
        };
        if state.in_flight >= MAX_IN_FLIGHT {
            return Answer::busy(MAX_IN_FLIGHT);
        }
        state.in_flight += 1;
        // Owed until a window has shown the notice, which the first call of any kind does (with
        // no window open, the next). The main thread, which runs calls one at a time, checks
        // again and marks it ([`announce_once`]).
        let announce = state.outside && !state.announced;
        (
            state.target,
            Caller {
                agent: state.agent.clone(),
                announce,
            },
        )
    };
    // Counted down however the call ends, the connection dropped among them.
    struct InFlight<'a>(&'a Shared, &'a str);
    impl Drop for InFlight<'_> {
        fn drop(&mut self) {
            if let Ok(mut tokens) = self.0.tokens.lock()
                && let Some(state) = tokens.get_mut(self.1)
            {
                state.in_flight = state.in_flight.saturating_sub(1);
            }
        }
    }
    let _in_flight = InFlight(shared, token);
    // The window the call meant, for an answer Studio could not give.
    let window = match target {
        Target::Bound(own) => Some(own),
        Target::Any => input.get("window").and_then(Value::as_u64),
    };
    let (reply, answered) = oneshot::channel();
    let sent = shared.calls.unbounded_send(Call {
        token: token.to_owned(),
        target,
        caller,
        tool,
        input,
        reply,
    });
    match (sent, answered.await) {
        (Ok(()), Ok(answer)) => answer,
        _ => Answer::cancelled(window),
    }
}

/// The agent's name as the window's chip says it, from the name its client gives.
fn agent_name(client: &str) -> SharedString {
    match client {
        "claude-code" | "claude-agent-acp" => "Claude Code".into(),
        name if name.starts_with("codex") => "Codex".into(),
        name => name.chars().take(40).collect::<String>().into(),
    }
}

async fn write_reply(stream: &mut (impl AsyncWrite + Unpin), reply: Reply) {
    let mut status = reply.status;
    let mut body = reply
        .body
        .map(|body| serde_json::to_vec(&body).unwrap_or_default())
        .unwrap_or_default();
    if body.len() > MAX_ANSWER {
        status = 500;
        body = serde_json::to_vec(&mcp::error(
            Value::Null,
            -32603,
            "Studio's answer was larger than 64 KiB, so it was not sent",
        ))
        .unwrap_or_default();
    }
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\ncache-control: no-store\r\nconnection: close\r\n\
         content-length: {}\r\n",
        reason(status),
        body.len()
    );
    if !body.is_empty() {
        head.push_str("content-type: application/json\r\n");
    }
    for (name, value) in reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.ok();
    stream.write_all(&body).await.ok();
    stream.flush().await.ok();
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    }
}

/// Whether `call` should show the outside notice, asked on the main thread just before it runs:
/// calls sent together each owed it when they were read, and only the first to name a window
/// shows it.
pub fn still_owed(shared: &Shared, call: &Call) -> bool {
    call.caller.announce
        && shared
            .tokens
            .lock()
            .ok()
            .and_then(|tokens| tokens.get(&call.token).map(|state| !state.announced))
            .unwrap_or(false)
}

/// Marks the outside notice given for `token`, once a window showed it.
pub fn announce_once(shared: &Shared, token: &str) {
    if let Ok(mut tokens) = shared.tokens.lock()
        && let Some(state) = tokens.get_mut(token)
    {
        state.announced = true;
    }
}

/// A call's tool name as the log may say it: one of Studio's tools, else "unknown". The name is
/// the agent's text, which may hold anything, a query's text or ids among it.
pub fn logged_name(name: &str) -> &str {
    if crate::tools::is_tool(name) {
        name
    } else {
        "unknown"
    }
}
