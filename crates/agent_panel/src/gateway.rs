//! Studio's gateway for the agent's `omnigraph` CLI (spec 013): a loopback HTTPS server per
//! conversation, with a certificate of its own ([`crate::gateway_tls`]) that the conversation's
//! CLI home trusts alone. The CLI reaches it with a session token Studio wrote for this
//! conversation; the gateway checks that token and the contract header, applies
//! [`gateway_rules`], and forwards over [`GraphClient::forward`] with the person's token, which
//! never leaves Studio.
//!
//! Reads stream their answers back. A write waits for the panel's decision, is sent once, and
//! its answer is read whole (bounded) so the panel can show its outcome. Request bodies are
//! read whole too, at most [`MAX_BODY`]: a JSON write is small, and a larger load is refused
//! before anything is sent. One request per connection; the CLI opens a new one each time.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use futures::channel::{mpsc, oneshot};
use futures::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use futures_rustls::TlsAcceptor;
use futures_rustls::rustls::ServerConfig;
use graph_client::api::CommitKind;
use graph_client::{
    Answered, AsyncBody, CONTRACT, CONTRACT_HEADER, ForwardRequest, ForwardResponse, GraphClient,
    HeaderMap, Method, NoAnswer, StatusCode, Unauthorized,
};
use serde::Deserialize as _;
use serde_json::{Value, json};
use smol::net::{TcpListener, TcpStream};

use crate::gateway_rules::{self, Incoming, Sandbox, Verdict, WriteKind, WriteSummary};
use crate::requester::Starters;

/// The most connections the gateway's listener serves at once, so connections held open by
/// another process cannot use up Studio's file descriptors.
const MAX_CONNECTIONS: usize = 64;

/// How long a listener waits after a connection it could not take, before taking the next.
const ACCEPT_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// Whether a connection's peer proved itself to the listener (the gateway's token, checked
/// once a request's headers are in): its server says so, and a full listener lets go of
/// connections that have not first ([`serve_each`]).
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

/// Serves each connection `accept` takes on a task of its own, at most [`MAX_CONNECTIONS`] at
/// once, telling `serve` the connection's [`Standing`] for it to raise. A full listener closes
/// the oldest connection whose peer has not proved itself to take the next (its task is
/// dropped), so connections another local process holds open never keep out one that proves
/// itself; with every one proved, the next is closed unread. A connection it could not take
/// (out of descriptors, reset before it was taken) ends that connection, not the listener.
async fn serve_each<C, A, S>(mut accept: impl FnMut() -> A, serve: impl Fn(C, Standing) -> S)
where
    A: Future<Output = std::io::Result<C>>,
    S: Future<Output = ()> + Send + 'static,
{
    // Oldest first.
    let mut connections: Vec<(smol::Task<()>, Standing)> = Vec::new();
    loop {
        let Ok(connection) = accept().await else {
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
                    drop(connection);
                    continue;
                }
            }
        }
        let standing = Standing::default();
        connections.push((smol::spawn(serve(connection, standing.clone())), standing));
    }
}

/// The most a request's line and headers may take.
const MAX_HEAD: usize = 64 * 1024;
/// The most a request body may take; a larger load is refused with 413.
pub const MAX_BODY: usize = 64 * 1024 * 1024;
/// How long, from the connection's opening, its request may take to arrive whole, its body
/// included. Only a request with the session's token has a body read.
const READ_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);
/// How long, from the connection's opening, its TLS handshake and then its request's line and
/// headers, its token among them, may take together. Any local process can open a connection and
/// finish the handshake (it need not trust the certificate), so one that sends nothing, or sends
/// slowly, is let go by then: without the token it never holds a connection longer.
const HEAD_WITHIN: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_secs(3)
} else {
    std::time::Duration::from_secs(10)
};
/// The most a write's answer may take. A larger one is neither read for the card nor passed on:
/// its outcome is unknown, and the CLI hears no answer.
const MAX_WRITE_ANSWER: usize = 8 * 1024 * 1024;
/// Request headers passed on to the server; the gateway sets the token and contract itself.
/// `omnigraph-if-actor` (the CLI's `--as`) can only narrow a request: the server refuses it
/// unless it names the person's actor.
const FORWARDED_HEADERS: &[&str] = &[
    "accept",
    "content-type",
    "if-match",
    "if-none-match",
    "omnigraph-if-actor",
    "omnigraph-if-graph-commit",
    "range",
];

/// What the gateway tells the conversation.
#[derive(Debug)]
pub enum GatewayEvent {
    /// A write for the agent's branch waits for `decide`: `Some` sends it, with what records
    /// its outcome (constitution III).
    Write {
        id: u64,
        write: WriteSummary,
        decide: oneshot::Sender<Option<OnOutcome>>,
    },
    /// The CLI stopped waiting before the write was decided (its command timed out or was
    /// stopped): nothing was sent, and nothing will be.
    Abandoned { id: u64 },
    /// The server refused the token a request the gateway forwarded carried (a 401 that
    /// refused it, [`Unauthorized::TokenRefused`]): the session asks for the next token as for
    /// its own requests ([`graph_store::GraphSession::token_refused`]).
    TokenRefused,
}

/// What records an approved write's outcome. It is called once, with the outcome, however the
/// write ends, though the conversation that sent it stops meanwhile; dropped uncalled (Studio
/// quit while it was sent), it says nothing more.
pub type OnOutcome = Box<dyn FnOnce(&Seen) + Send>;

/// What an approved write that was not sent, because the CLI stopped waiting, records.
pub const NOT_SENT: &str = "The agent's CLI stopped waiting, so nothing was sent.";

/// What an approved write that was not sent, because the conversation stopped before the
/// gateway took the approval, records.
pub const NOT_SENT_STOPPED: &str = "The conversation stopped before the write was sent, so \
                                    nothing was sent.";

/// A forwarded write's outcome, as the gateway saw the server's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    /// The server answered with the write's receipt. `commit` is the commit it names, with the
    /// commit's kind as Studio's own writes read it: a commit of the agent's branch
    /// (`Published`), one no branch shows (`Unpublished`, `Unattached`: a stored mutation
    /// written `as commit` or `as unattached branch`, whose text the gateway never sees), or one
    /// of a kind this build does not know (`Other`), which leaves whether the branch moved
    /// unknown. `None` when the write published no commit (a branch created or deleted, a
    /// merge already up to date, a mutation that matched nothing).
    Written {
        commit: Option<(String, CommitKind)>,
        nodes: i64,
        edges: i64,
        /// A load's answer says it created its branch (`branch_created`).
        branch_created: bool,
    },
    /// The server answered a stored query with a read's rows: the caller did not say whether
    /// the query writes, and it reads. Nothing was written.
    Read,
    Refused(String),
    Unknown(String),
}

/// A bound gateway. Its port, token and certificate go in the conversation's CLI home
/// ([`Gateway::access`]); dropping the future [`Gateway::serve`] returns closes the listener.
pub struct Gateway {
    pub port: u16,
    pub token: String,
    /// The certificate it serves (PEM), made as it was bound; its key is in `tls` alone.
    pub certificate: String,
    tls: Arc<ServerConfig>,
    listener: Option<TcpListener>,
}

impl Gateway {
    pub fn bind() -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let listener = TcpListener::try_from(listener)?;
        let identity = crate::gateway_tls::identity()?;
        Ok(Self {
            port,
            token: crate::confined::random_hex(32)?,
            certificate: identity.certificate,
            tls: identity.config,
            listener: Some(listener),
        })
    }

    /// What the conversation's CLI home writes to reach this gateway.
    pub fn access(&self) -> crate::cli_home::GatewayAccess<'_> {
        crate::cli_home::GatewayAccess {
            port: self.port,
            token: &self.token,
            certificate: &self.certificate,
        }
    }

    pub fn serve(
        &mut self,
        client: GraphClient,
        sandbox: Sandbox,
        events: mpsc::UnboundedSender<GatewayEvent>,
    ) -> impl Future<Output = ()> + Send + 'static + use<> {
        let listener = self.listener.take();
        let acceptor = TlsAcceptor::from(self.tls.clone());
        let shared = Arc::new(Shared {
            client,
            sandbox,
            token: self.token.clone(),
            events,
            next_id: AtomicU64::new(0),
        });
        async move {
            let Some(listener) = listener else { return };
            serve_each(
                || listener.accept(),
                |(stream, _), standing| {
                    serve_connection(stream, acceptor.clone(), shared.clone(), standing)
                },
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

/// A connection's two ends, the CLI's and the gateway's, as TCP addresses: what finds the
/// processes a write's CLI was started under (`requester`).
type Ends = (Option<SocketAddr>, Option<SocketAddr>);

/// Serves one connection. Its [`Standing`] is raised once its request carries the session's
/// token: until then a full listener may close it for another.
async fn serve_connection(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    shared: Arc<Shared>,
    standing: Standing,
) {
    let ends = (stream.peer_addr().ok(), stream.local_addr().ok());
    // Both bounds count from here, so the handshake's time comes out of the head's: a local
    // process that opens a connection and sends slowly, or nothing, holds it no longer than
    // `HEAD_WITHIN` in all.
    let opened = Instant::now();
    let head_by = opened + HEAD_WITHIN;
    // A connection that does not finish its handshake (one that speaks plain HTTP among them)
    // is closed unanswered.
    let Some(Ok(stream)) = by(head_by, acceptor.accept(stream)).await else {
        return;
    };
    let (mut reader, mut writer) = stream.split();
    let read = by(
        opened + READ_WITHIN,
        read_request(&mut reader, &shared.token, head_by, &standing),
    )
    .await;
    match read.unwrap_or(Err(ReadFailure::Gone)) {
        Ok(request) => handle(&mut reader, &mut writer, ends, request, &shared).await,
        Err(ReadFailure::Gone) => return,
        Err(ReadFailure::Answer(status, code, message)) => {
            answer(&mut writer, status, code, &message).await;
        }
    }
    // The answer's end is its framing (a length, or the chunked stream's last chunk), never the
    // connection's: a cut answer stays cut though TLS closes cleanly here.
    writer.close().await.ok();
}

async fn handle(
    reader: &mut (impl AsyncRead + Unpin),
    stream: &mut (impl AsyncWrite + Unpin),
    (client, server): Ends,
    request: Request,
    shared: &Shared,
) {
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
            stream,
            400,
            "bad_request",
            "Studio: a request with a `#` or a repeated query parameter is not sent. Nothing \
             was sent.",
        )
        .await;
        return;
    }
    // The CLI probes `/healthz` with no token, no contract header and no body before every
    // request; one that declared a body was refused unread. Every other request carries the
    // session's token, checked before its body was read.
    if is_health(&request.method, &path) {
        let body = if request.method == "HEAD" { "" } else { "ok" };
        write_simple(stream, 200, "text/plain", body.as_bytes()).await;
        return;
    }
    let contracts = request.headers_named(CONTRACT_HEADER);
    if contracts != [CONTRACT] {
        let sent = if contracts.is_empty() {
            "none".to_owned()
        } else {
            contracts.join(", ")
        };
        answer(
            stream,
            400,
            "api_contract_mismatch",
            &format!(
                "Studio: this omnigraph CLI speaks HTTP API {sent}, and this graph speaks {}. Use \
                 the omnigraph CLI Studio named for this conversation.",
                CONTRACT
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
    // Judged: the body is held once from here, as it came, until it is sent.
    drop(json);
    match verdict {
        Verdict::Refuse(message) => answer(stream, 403, "forbidden", &message).await,
        Verdict::Read | Verdict::ReadWith(_) => {
            let body = match verdict {
                Verdict::ReadWith(body) => Some(serde_json::to_vec(&body).unwrap_or_default()),
                _ => None,
            };
            let method = request.method.clone();
            let request = forward_request(request, body);
            // A read the CLI stops waiting for (its shell's two-minute timeout) is dropped with
            // its server request, which would otherwise hold this connection's place, proved,
            // until the server answered.
            let response = smol::future::or(
                async { Some(forward(&shared.client, &shared.events, request).await) },
                async {
                    closed(reader).await;
                    None
                },
            )
            .await;
            if let Some(response) = response {
                stream_back(stream, &method, response).await;
            }
        }
        Verdict::Write(write) => {
            // A target already retired sends nothing: the agent is told so at once, as a read
            // is, rather than the person being asked to approve a write that cannot go. One that
            // retires while the person decides is refused when sent (`NoAnswer::Retired`).
            if let Some(why) = shared.client.retired() {
                answer(stream, 403, "forbidden", &retired_refusal(why)).await;
                return;
            }
            // Whoever ran the command, as they are now (`requester`): on Windows the CLI can
            // outlive a command stopped by its process id, its connection open.
            let starters = Starters::of(client, server).await;
            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
            let kind = write.kind;
            let (decide, decision) = oneshot::channel();
            let mut decision = Decision(decision);
            if shared
                .events
                .unbounded_send(GatewayEvent::Write { id, write, decide })
                .is_err()
            {
                answer(
                    stream,
                    403,
                    "forbidden",
                    "Studio: the conversation has ended. Nothing was sent.",
                )
                .await;
                return;
            }
            // The person may decide after the CLI has given up (its shell's two-minute timeout):
            // a write approved then would land with the agent told it failed. A closed
            // connection while waiting ends the write unsent, and so does the end of a process
            // the CLI was started under, since whoever ran it no longer waits.
            let (gone, stopped) = (
                futures::FutureExt::fuse(closed(reader)),
                futures::FutureExt::fuse(starters.ended()),
            );
            futures::pin_mut!(gone, stopped);
            let decided = futures::select! {
                decided = decision => Ok(decided),
                () = gone => Err(Undecided::Gone),
                () = stopped => Err(Undecided::Stopped),
            };
            let decided = match decided {
                Ok(decided) => decided,
                Err(undecided) => {
                    // An approval that came as the CLI gave up is not sent: it says so, rather
                    // than being dropped and taken for a write whose outcome is unknown.
                    decision.abandon();
                    shared
                        .events
                        .unbounded_send(GatewayEvent::Abandoned { id })
                        .ok();
                    // A CLI whose command was stopped still listens: it hears that nothing
                    // was sent, and ends.
                    if let Undecided::Stopped = undecided {
                        answer(
                            stream,
                            403,
                            "forbidden",
                            "Studio: the command that sent this write was stopped before the \
                             person decided. Nothing was sent.",
                        )
                        .await;
                    }
                    return;
                }
            };
            let Ok(Some(on_outcome)) = decided else {
                answer(
                    stream,
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
            let client = shared.client.clone();
            let events = shared.events.clone();
            let request = forward_request(request, None);
            let (reply_tx, reply_rx) = oneshot::channel();
            smol::spawn(async move {
                let response = forward(&client, &events, request).await;
                let (outcome, reply) = read_write_answer(response, kind).await;
                on_outcome(&outcome);
                reply_tx.send(reply).ok();
            })
            .detach();
            // When the server's answer was lost, the CLI's connection closes without one, and
            // the CLI reports the outcome as unknown, as it is.
            match reply_rx.await {
                Ok(Some(Reply::Server(status, headers, body))) => {
                    write_buffered(stream, status, &headers, &body).await
                }
                Ok(Some(Reply::NotSent(unsent))) => {
                    answer(stream, unsent.status, unsent.code, &unsent.message).await
                }
                _ => {}
            }
        }
    }
}

/// Ends when the CLI closes its side of the connection: it sends nothing after its request.
async fn closed(reader: &mut (impl AsyncRead + Unpin)) {
    let mut byte = [0u8; 1];
    while matches!(reader.read(&mut byte).await, Ok(1..)) {}
}

/// The person's decision on a write, as the gateway waits for it. Dropped with an approval it
/// never took (the conversation stopped as the person allowed the write), the approval records
/// that nothing was sent, rather than being dropped with it and taken for a write whose outcome
/// is unknown.
struct Decision(oneshot::Receiver<Option<OnOutcome>>);

impl Decision {
    /// Takes no decision after this: an approval sent before records that nothing was sent,
    /// and one sent after finds nobody to take it, and records so itself (the panel's
    /// `hand_over`).
    fn abandon(&mut self) {
        self.0.close();
        if let Ok(Some(Some(approval))) = self.0.try_recv() {
            approval(&Seen::Refused(NOT_SENT.into()));
        }
    }
}

impl Future for Decision {
    type Output = Result<Option<OnOutcome>, oneshot::Canceled>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

impl futures::future::FusedFuture for Decision {
    fn is_terminated(&self) -> bool {
        self.0.is_terminated()
    }
}

impl Drop for Decision {
    fn drop(&mut self) {
        if let Ok(Some(Some(approval))) = self.0.try_recv() {
            approval(&Seen::Refused(NOT_SENT_STOPPED.into()));
        }
    }
}

/// Why a write stopped waiting for the person's decision, unsent.
enum Undecided {
    /// The CLI closed its connection.
    Gone,
    /// A process the CLI was started under ended; the CLI still listens.
    Stopped,
}

/// Sends `request` on with Studio's token, and says the server refusing that token
/// ([`GatewayEvent::TokenRefused`]; a 401 without the contract header came from something in
/// between, and one that refused no token, one that never arrived, would refuse the next
/// alike), so the token is asked for again as for Studio's own requests: an agent working while
/// no window reads would otherwise be refused until one did.
async fn forward(
    client: &GraphClient,
    events: &mpsc::UnboundedSender<GatewayEvent>,
    request: ForwardRequest,
) -> Result<ForwardResponse, NoAnswer> {
    let response = client.forward(request).await;
    if let Ok(response) = &response
        && response.unauthorized == Some(Unauthorized::TokenRefused)
        && !response.token_replaced
        && graph_client::speaks_contract(
            response
                .headers
                .get(CONTRACT_HEADER)
                .and_then(|value| value.to_str().ok()),
        )
    {
        events.unbounded_send(GatewayEvent::TokenRefused).ok();
    }
    response
}

/// The request as the server is sent it, with `body` in place of the one that came. Taken whole,
/// so a large body moves into the forward rather than being copied.
fn forward_request(request: Request, body: Option<Vec<u8>>) -> ForwardRequest {
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
    let body = body.unwrap_or(request.body);
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

/// Reads a write's answer whole, and what it says happened to a write of `kind`.
async fn read_write_answer(
    response: Result<graph_client::ForwardResponse, NoAnswer>,
    kind: WriteKind,
) -> (Seen, Option<Reply>) {
    let mut response = match response {
        Ok(response) => response,
        Err(no) => {
            return match unsent_refusal(&no) {
                // Never sent (the target retired, the connection refused, or a request Studio
                // would not make): nothing was written, and the CLI is told so.
                Some(unsent) => (
                    Seen::Refused(graph_store::nothing_sent(&String::from(no))),
                    Some(Reply::NotSent(unsent)),
                ),
                None => (Seen::Unknown(String::from(no)), None),
            };
        }
    };
    let mut body = Vec::new();
    // One byte past the limit tells a whole answer from a cut one.
    let read = (&mut response.body)
        .take(MAX_WRITE_ANSWER as u64 + 1)
        .read_to_end(&mut body)
        .await;
    if let Err(error) = read {
        return (Seen::Unknown(error.to_string()), None);
    }
    if body.len() > MAX_WRITE_ANSWER {
        let reason = format!(
            "the server's answer is larger than the {} MiB Studio reads",
            MAX_WRITE_ANSWER / (1024 * 1024)
        );
        return (Seen::Unknown(reason), None);
    }
    // One contract header, as the server sends it; more than one confirms nothing.
    let mut contracts = response
        .headers
        .get_all(CONTRACT_HEADER)
        .iter()
        .filter_map(|value| value.to_str().ok());
    let contract = match (contracts.next(), contracts.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    };
    // Judged as Studio's own writes are, a success whose body cannot be read included: it
    // confirms nothing.
    let outcome = match graph_client::judge_write(response.status, contract, &body) {
        Answered::Committed(body) => match serde_json::from_slice::<Value>(body) {
            Ok(json) => seen_written(&json, kind),
            Err(error) => Seen::Unknown(format!("the success response could not be read: {error}")),
        },
        Answered::Refused(error) => Seen::Refused(error.body.error),
        Answered::Unknown { reason } => Seen::Unknown(reason),
    };
    (
        outcome,
        Some(Reply::Server(response.status, response.headers, body)),
    )
}

/// The CLI's refusal of a request to a retired target ([`NoAnswer::Retired`]).
fn retired_refusal(why: &str) -> String {
    format!(
        "Studio: {}. Tell the person what this says. Nothing was sent.",
        studio_ui::clause(why)
    )
}

/// How the CLI is answered for a request never sent ([`answer`]): its status, code and message.
#[derive(Debug)]
struct Unsent {
    status: u16,
    code: &'static str,
    message: String,
}

/// The CLI's answer to a request never sent, or `None` for one whose answer was lost: as every
/// gateway refusal does (spec 013), it says what to do instead and ends "Nothing was sent.",
/// which the agent reads as nothing written. A request Studio refused is a 403 `forbidden`, to
/// be fixed first; one whose server refused the connection is a 503 `service_unavailable`, to be
/// tried again once the server answers.
fn unsent_refusal(no: &NoAnswer) -> Option<Unsent> {
    let forbidden = |message| Unsent {
        status: 403,
        code: "forbidden",
        message,
    };
    match no {
        NoAnswer::Retired(why) => Some(forbidden(retired_refusal(why))),
        NoAnswer::Unsendable(why) => Some(forbidden(format!(
            "Studio: {}. Change the request, or tell the person what this says. Nothing was \
             sent.",
            studio_ui::clause(why)
        ))),
        NoAnswer::Unreached(why) => Some(Unsent {
            status: 503,
            code: "service_unavailable",
            message: format!(
                "Studio: the server could not be reached ({why}). Try again once it answers, or \
                 tell the person. Nothing was sent."
            ),
        }),
        NoAnswer::Lost(_) => None,
    }
}

/// What the CLI is answered with once a forwarded write ends: the server's answer, or the
/// refusal of one never sent.
enum Reply {
    Server(StatusCode, HeaderMap, Vec<u8>),
    NotSent(Unsent),
}

/// What a success answer to a write of `kind` says it did. A write's receipt names its commit
/// in `commit` (null when it published none: `ChangeOutput`, the loads', Blob and merge
/// receipts); a branch's create or delete answers with the branch's `name` and no commit. A
/// stored query that reads answers with a read's rows (`ReadOutput`), whose `graph_commit_id`
/// is the commit it read at, not one it made. Any other answer confirms nothing.
fn seen_written(json: &Value, kind: WriteKind) -> Seen {
    let receipt = match kind {
        WriteKind::BranchCreate | WriteKind::BranchDelete => json.get("name").is_some(),
        _ => json.get("commit").is_some(),
    };
    if !receipt {
        if kind == WriteKind::StoredQuery && json.get("rows").is_some() {
            return Seen::Read;
        }
        return Seen::Unknown("the success response is not the write's receipt".into());
    }
    let commit = match json.get("commit").filter(|commit| !commit.is_null()) {
        None => None,
        Some(commit) => match commit.get("graph_commit_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => {
                // Read as Studio's own writes read it: a kind this build does not know is
                // `Other`, and a commit without one is no 0.14 receipt.
                match commit.get("kind").map(CommitKind::deserialize) {
                    Some(Ok(kind)) => Some((id.to_owned(), kind)),
                    _ => {
                        return Seen::Unknown("the receipt names a commit without its kind".into());
                    }
                }
            }
            _ => {
                return Seen::Unknown("the receipt names a commit without its id".into());
            }
        },
    };
    Seen::Written {
        commit,
        nodes: count_of(json, "affected_nodes", "nodes"),
        edges: count_of(json, "affected_edges", "edges"),
        branch_created: json.get("branch_created").and_then(Value::as_bool) == Some(true),
    }
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

/// Reads into `buffer` up to the end of a request's headers, and where that is.
async fn read_head(
    stream: &mut (impl AsyncRead + Unpin),
    buffer: &mut Vec<u8>,
) -> Result<usize, ReadFailure> {
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

/// How a request's headers say its body is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// No body: no `transfer-encoding`, and no `content-length` or one of 0.
    None,
    Length(usize),
    Chunked,
    /// A body whose end cannot be told: a `transfer-encoding` that does not end with `chunked`,
    /// or a `content-length` that is not one number.
    Unreadable,
}

impl Framing {
    /// Read from the headers alone, before any of the body is.
    fn of(headers: &[(String, String)]) -> Self {
        let values = |name: &str| -> Vec<&str> {
            headers
                .iter()
                .filter(|(key, _)| key == name)
                .flat_map(|(_, value)| value.split(','))
                .map(str::trim)
                .collect()
        };
        let encodings = values("transfer-encoding");
        if !encodings.is_empty() {
            // A transfer coding decides over a length; only one that ends with `chunked` says
            // where a request's body ends.
            return match encodings.last() {
                Some(last) if last.eq_ignore_ascii_case("chunked") => Self::Chunked,
                _ => Self::Unreadable,
            };
        }
        let lengths = values("content-length");
        let Some(first) = lengths.first() else {
            return Self::None;
        };
        let digits = !first.is_empty() && first.bytes().all(|byte| byte.is_ascii_digit());
        match first.parse::<usize>() {
            Ok(0) if digits && lengths.iter().all(|other| other == first) => Self::None,
            Ok(length) if digits && lengths.iter().all(|other| other == first) => {
                Self::Length(length)
            }
            // A length past the largest number is still a body too large to read.
            Err(_) if digits => Self::Length(usize::MAX),
            _ => Self::Unreadable,
        }
    }
}

/// Reads one request, its body last. The health probe carries no token and may carry no body:
/// one that declares a body is refused without a byte of it read. Every other request's token
/// is checked once its headers are in, before its body (up to [`MAX_BODY`]) is read: only the
/// conversation's CLI makes Studio read that much, and its connection's `standing` is raised
/// then. Its line and headers must be in by `head_by`.
async fn read_request(
    stream: &mut (impl AsyncRead + Unpin),
    token: &str,
    head_by: Instant,
    standing: &Standing,
) -> Result<Request, ReadFailure> {
    let mut buffer = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];
    let head_end = by(head_by, read_head(stream, &mut buffer))
        .await
        .unwrap_or(Err(ReadFailure::Gone))?;
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
    let framing = Framing::of(&headers);
    if is_health(&method, path) {
        if framing != Framing::None {
            return Err(ReadFailure::Answer(
                400,
                "bad_request",
                "Studio: the health probe carries no body.".into(),
            ));
        }
        return Ok(Request {
            method,
            target,
            headers,
            body: Vec::new(),
        });
    }
    let authorized = headers
        .iter()
        .find(|(name, _)| name == "authorization")
        .and_then(|(_, value)| value.strip_prefix("Bearer "))
        .is_some_and(|sent| same(sent.as_bytes(), token.as_bytes()));
    if !authorized {
        return Err(ReadFailure::Answer(
            401,
            "unauthorized",
            "Studio: this token is not the one Studio wrote for this conversation.".into(),
        ));
    }
    standing.prove();
    let mut rest = buffer[head_end..].to_vec();
    let body = match framing {
        Framing::None => Vec::new(),
        Framing::Chunked => read_chunked(stream, rest).await?,
        Framing::Length(length) => {
            if length > MAX_BODY {
                return Err(too_large());
            }
            // Held at its length from the start: grown as it arrives, a body would take up to
            // twice its size.
            rest.reserve_exact(length.saturating_sub(rest.len()));
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
        }
        Framing::Unreadable => {
            return Err(ReadFailure::Answer(
                400,
                "bad_request",
                "Studio: the request's body could not be read.".into(),
            ));
        }
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
        "payload_too_large",
        format!(
            "Studio: a request through Studio may carry at most {} MiB. Load in smaller parts. \
             Nothing was sent.",
            MAX_BODY / (1024 * 1024)
        ),
    )
}

/// Decodes a chunked body, at most [`MAX_BODY`].
async fn read_chunked(
    stream: &mut (impl AsyncRead + Unpin),
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
        // The data goes straight into the body, as it arrives: gathered first, a large chunk
        // would be held twice.
        let buffered = size.min(pending.len());
        body.extend_from_slice(&pending[..buffered]);
        pending.drain(..buffered);
        let mut left = size - buffered;
        while left > 0 {
            let read = stream
                .read(&mut chunk)
                .await
                .map_err(|_| ReadFailure::Gone)?;
            if read == 0 {
                return Err(ReadFailure::Gone);
            }
            let data = read.min(left);
            body.extend_from_slice(&chunk[..data]);
            pending.extend_from_slice(&chunk[data..read]);
            left -= data;
        }
        // Its CRLF; a zero size ends the body after the trailer's CRLF.
        while pending.len() < 2 {
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
        pending.drain(..2);
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

/// A refusal or error the gateway makes itself, as Omnigraph's `ErrorOutput` with the contract
/// header, so the CLI shows it as a typed error. The gateway answers so only before it sends a
/// request on, so its disposition says nothing took effect: Studio failing to reach the server
/// (a 5xx) may be tried again, and anything else is fixed first.
async fn answer(stream: &mut (impl AsyncWrite + Unpin), status: u16, code: &str, message: &str) {
    let action = if status >= 500 { "retry" } else { "fix" };
    let body = serde_json::to_vec(&json!({
        "error": message,
        "code": code,
        "disposition": { "effects": "none", "action": action },
    }))
    .unwrap_or_default();
    write_simple(stream, status, "application/json", &body).await;
}

async fn write_simple(
    stream: &mut (impl AsyncWrite + Unpin),
    status: u16,
    content_type: &str,
    body: &[u8],
) {
    let head = format!(
        "HTTP/1.1 {status} {}\r\n{CONTRACT_HEADER}: {}\r\ncontent-type: {content_type}\r\n\
         content-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        reason(status),
        CONTRACT,
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
    stream: &mut (impl AsyncWrite + Unpin),
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
    stream: &mut (impl AsyncWrite + Unpin),
    method: &str,
    response: Result<graph_client::ForwardResponse, NoAnswer>,
) {
    let mut response = match response {
        Ok(response) => response,
        Err(no) => {
            match unsent_refusal(&no) {
                Some(unsent) => answer(stream, unsent.status, unsent.code, &unsent.message).await,
                None => {
                    let reason = String::from(no);
                    answer(
                        stream,
                        502,
                        "internal",
                        &format!("Studio could not reach the graph's server: {reason}"),
                    )
                    .await
                }
            }
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
    use std::time::Duration;

    use futures::StreamExt as _;
    use http_client::FakeHttpClient;
    use http_client::http::Response;

    use super::*;

    /// A connection as the listener test offers it: it notes its id when it is closed, whether
    /// served or closed unread.
    struct Offered {
        id: usize,
        proves: bool,
        closed: Arc<Mutex<Vec<usize>>>,
    }

    impl Drop for Offered {
        fn drop(&mut self) {
            self.closed.lock().unwrap().push(self.id);
        }
    }

    /// A full listener closes the oldest connection whose peer has not proved itself to take
    /// the next, so connections another local process holds open, idle within their bound,
    /// never keep out one that proves itself (the agent's CLI with its token). With every one
    /// proved, the next is closed unread.
    /// A full listener closes the oldest connection whose peer has not proved itself to take
    /// the next, so connections another local process holds open, idle within their bound,
    /// never keep out one that proves itself (the agent's CLI with its token). With every one
    /// proved, the next is closed unread.
    #[test]
    fn a_full_listener_lets_go_of_the_oldest_unproved_connection() {
        smol::block_on(async {
            let (offer, offered) = smol::channel::unbounded::<Offered>();
            let closed = Arc::new(Mutex::new(Vec::new()));
            let served = Arc::new(Mutex::new(Vec::new()));
            let listening = {
                let served = served.clone();
                smol::spawn(serve_each(
                    move || {
                        let offered = offered.clone();
                        async move { offered.recv().await.map_err(std::io::Error::other) }
                    },
                    move |connection: Offered, standing: Standing| {
                        served.lock().unwrap().push(connection.id);
                        if connection.proves {
                            standing.prove();
                        }
                        async move {
                            let _held = connection;
                            futures::future::pending::<()>().await;
                        }
                    },
                ))
            };
            let mut next = 0;
            let mut connect = |proves: bool| {
                let connection = Offered {
                    id: next,
                    proves,
                    closed: closed.clone(),
                };
                next += 1;
                offer.try_send(connection).unwrap();
            };
            async fn until(done: impl Fn() -> bool) {
                let by = Instant::now() + Duration::from_secs(5);
                while !done() {
                    assert!(Instant::now() < by, "the listener never took it");
                    smol::Timer::after(Duration::from_millis(1)).await;
                }
            }
            let taken = |n: usize| served.lock().unwrap().len() == n;
            // A dropped task's future is dropped on the executor, so in no set order.
            let closed_now = || {
                let mut closed = closed.lock().unwrap().clone();
                closed.sort_unstable();
                closed
            };

            // The agent's CLI, then connections another process opens and holds.
            connect(true);
            for _ in 1..MAX_CONNECTIONS {
                connect(false);
            }
            until(|| taken(MAX_CONNECTIONS)).await;
            assert!(closed_now().is_empty());
            // Full: the next, proved or not, takes the oldest unproved one's place.
            connect(false);
            until(|| taken(MAX_CONNECTIONS + 1) && closed_now() == [1]).await;
            connect(true);
            until(|| taken(MAX_CONNECTIONS + 2) && closed_now() == [1, 2]).await;
            // Proved ones take every unproved one's place, the one that took the first's
            // included, and the CLI's stays.
            for _ in 3..=MAX_CONNECTIONS {
                connect(true);
            }
            let expected: Vec<usize> = (1..=MAX_CONNECTIONS).collect();
            until(|| taken(2 * MAX_CONNECTIONS) && closed_now() == expected).await;
            // With every one proved, the next is closed unread.
            connect(false);
            until(|| closed_now().len() == MAX_CONNECTIONS + 1).await;
            assert_eq!(closed_now().last(), Some(&(2 * MAX_CONNECTIONS)));
            assert!(taken(2 * MAX_CONNECTIONS), "closed unread");
            drop(listening);
        });
    }

    const AGENT: &str = "agent/20261013-101500";

    /// A request's conditions reach the server, its actor assertion among them; its own token
    /// and every other header stay behind.
    #[test]
    fn a_forwarded_request_keeps_its_conditions_and_drops_its_credentials() {
        let request = Request {
            method: "POST".into(),
            target: "/graphs/knowledge/mutate".into(),
            headers: [
                ("authorization", "Bearer agent-token"),
                ("omnigraph-if-actor", "act-person"),
                ("omnigraph-if-graph-commit", "c1"),
                ("cookie", "x"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .into(),
            body: b"{}".to_vec(),
        };
        assert_eq!(
            forward_request(request, None).headers,
            [
                ("omnigraph-if-actor", "act-person"),
                ("omnigraph-if-graph-commit", "c1")
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
        );
    }

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
                    r#"{"branch":"agent/20261013-101500","query_name":"m","affected_nodes":2,"affected_edges":1,"commit":{"graph_commit_id":"c9","kind":"published"}}"#
                } else {
                    r#"{"rows":[{"n":200}]}"#
                };
                log.lock()
                    .unwrap()
                    .push((method, uri, auth, contract, body));
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
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

    /// A client of the gateway as the CLI is one: TLS to `127.0.0.1`, trusting only the
    /// certificates it is given.
    #[derive(Clone)]
    struct Cli {
        port: u16,
        tls: futures_rustls::TlsConnector,
    }

    impl Cli {
        /// One that trusts `gateway`'s certificate.
        fn of(gateway: &Gateway) -> Self {
            Self::trusting(gateway.port, &gateway.certificate)
        }

        fn trusting(port: u16, pem: &str) -> Self {
            Self {
                port,
                tls: crate::gateway_tls::client_trusting(pem).into(),
            }
        }

        /// A connection whose handshake is done, or why it failed.
        async fn connect(&self) -> std::io::Result<futures_rustls::client::TlsStream<TcpStream>> {
            let tcp = TcpStream::connect(("127.0.0.1", self.port)).await?;
            self.tls.connect(crate::gateway_tls::loopback(), tcp).await
        }
    }

    /// Reads `stream` until it ends, cleanly or not: what arrived.
    async fn read_all(mut stream: impl AsyncRead + Unpin) -> Vec<u8> {
        let mut answer = Vec::new();
        let mut buffer = [0u8; 8 * 1024];
        while let Ok(read @ 1..) = stream.read(&mut buffer).await {
            answer.extend_from_slice(&buffer[..read]);
        }
        answer
    }

    /// Sends raw HTTP to the gateway over TLS and returns the status, headers and body.
    async fn call(cli: Cli, raw: String) -> (u16, String, String) {
        let mut stream = cli.connect().await.unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let answer = read_all(stream).await;
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
                        .header(CONTRACT_HEADER, CONTRACT)
                        .header("content-type", "application/json")
                        .body(AsyncBody::from(
                            r#"{"branch":"agent/20261013-101500","query_name":"m","affected_nodes":1,"affected_edges":0,"commit":{"graph_commit_id":"c9","kind":"published"}}"#,
                        ))
                        .unwrap())
                }
            });
            let client =
                GraphClient::new("http://127.0.0.1:8787", Some("person-token".into()), http)
                    .unwrap();
            let mut gateway = Gateway::bind().unwrap();
            let (cli, token) = (Cli::of(&gateway), gateway.token.clone());
            let (events_tx, mut events) = mpsc::unbounded();
            let serve = smol::spawn(gateway.serve(client, sandbox(), events_tx));
            let write =
                r#"{"query":"query m() { … }","name":"m","branch":"agent/20261013-101500"}"#;
            let _cli = smol::spawn(call(
                cli,
                post("/graphs/knowledge/mutate", &token, Some(CONTRACT), write),
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
                super::Seen::Written {
                    commit: Some(("c9".into(), CommitKind::Published)),
                    nodes: 1,
                    edges: 0,
                    branch_created: false,
                }
            );
        });
    }

    /// On Windows the agent's wrapper is a batch file and the CLI runs under `cmd`: the agent
    /// stopping the command by the wrapper's process id ends `cmd` and leaves the CLI waiting,
    /// its connection open. The write it sent is given up unsent all the same, as when the CLI
    /// goes, so a person allowing it later sends nothing.
    #[cfg(windows)]
    #[test]
    fn a_write_whose_command_was_stopped_is_not_sent() {
        async fn next_within(
            events: &mut mpsc::UnboundedReceiver<GatewayEvent>,
        ) -> Option<GatewayEvent> {
            smol::future::or(events.next(), async {
                smol::Timer::after(std::time::Duration::from_secs(60)).await;
                None
            })
            .await
        }
        smol::block_on(async {
            let (client, seen) = upstream();
            let mut gateway = Gateway::bind().unwrap();
            let (port, token) = (gateway.port, gateway.token.clone());
            let (events_tx, mut events) = mpsc::unbounded();
            let _serve = smol::spawn(gateway.serve(client, sandbox(), events_tx));
            let write =
                r#"{"query":"query m() { … }","name":"m","branch":"agent/20261013-101500"}"#;
            let request = post("/graphs/knowledge/mutate", &token, Some(CONTRACT), write);
            let root =
                std::env::temp_dir().join(format!("studio-gateway-stopped-{}", std::process::id()));
            let (home, mut wrapper) = crate::requester::stand_in::start(
                &root,
                port,
                Some((request.as_bytes(), &gateway.certificate)),
            );
            let Some(GatewayEvent::Write { decide, id, .. }) = next_within(&mut events).await
            else {
                panic!("no write event")
            };
            wrapper.kill().unwrap();
            wrapper.wait().unwrap();
            let Some(GatewayEvent::Abandoned { id: abandoned }) = next_within(&mut events).await
            else {
                panic!("the gateway did not see the command stopped")
            };
            assert_eq!(abandoned, id);
            assert!(
                decide.send(Some(Box::new(|_: &super::Seen| {}))).is_err(),
                "nobody waits for the decision"
            );
            assert!(seen.lock().unwrap().is_empty(), "nothing was sent");
            drop(home);
            std::fs::remove_dir_all(&root).ok();
        });
    }

    /// A request the gateway forwards with Studio's token that the server refuses for it (a 401
    /// with `error="invalid_token"`) is said to the panel, so the token is asked for again as
    /// for Studio's own requests; any other answer says nothing, a 401 that refused no token
    /// (the one sent never arrived) included. Before, the gateway said nothing, and an agent
    /// working while no window read was refused once the token lapsed, until a window did.
    #[test]
    fn a_forwarded_401_refusing_the_token_is_said() {
        let answering = |status: u16, server: bool, refused: bool| {
            FakeHttpClient::create(move |_| async move {
                let mut response = Response::builder().status(status);
                if server {
                    response = response.header(CONTRACT_HEADER, CONTRACT);
                }
                if status == 401 {
                    let challenge = if refused {
                        r#"Bearer error="invalid_token""#
                    } else {
                        "Bearer"
                    };
                    response = response.header("www-authenticate", challenge);
                }
                Ok(response.body(AsyncBody::from("{}")).unwrap())
            })
        };
        smol::block_on(async {
            // A 401 the server sent refusing the token, one that found none, one from something
            // in between, and an answer.
            for (status, server, refused) in [
                (401, true, true),
                (401, true, false),
                (401, false, true),
                (200, true, false),
            ] {
                let client = GraphClient::new(
                    "http://127.0.0.1:8787",
                    Some("person-token".into()),
                    answering(status, server, refused),
                )
                .unwrap();
                let (events_tx, mut events) = mpsc::unbounded();
                let request = ForwardRequest {
                    method: Method::GET,
                    path_and_query: "graphs/knowledge/schema".into(),
                    headers: Vec::new(),
                    body: AsyncBody::empty(),
                };
                let response = super::forward(&client, &events_tx, request).await;
                assert_eq!(
                    response.map(|response| response.status.as_u16()).ok(),
                    Some(status)
                );
                drop(events_tx);
                let said = events.next().await;
                if status == 401 && server && refused {
                    assert!(matches!(said, Some(GatewayEvent::TokenRefused)));
                } else {
                    assert!(
                        said.is_none(),
                        "a {status} (the server's: {server}, refusing: {refused}) said nothing"
                    );
                }
            }
        });
    }

    /// A forwarded request's 401 to a token replaced while it was out says nothing: it refused
    /// a token no longer held, and asking about the one held could drop it.
    #[test]
    fn a_forwarded_401_to_a_replaced_token_says_nothing() {
        let held: Arc<std::sync::OnceLock<GraphClient>> = Arc::default();
        let replacing = held.clone();
        let http = FakeHttpClient::create(move |_| {
            if let Some(client) = replacing.get() {
                client.set_token(Some("next-token".into()));
            }
            async move {
                Ok(Response::builder()
                    .status(401)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .header("www-authenticate", r#"Bearer error="invalid_token""#)
                    .body(AsyncBody::from("{}"))
                    .unwrap())
            }
        });
        let client =
            GraphClient::new("http://127.0.0.1:8787", Some("person-token".into()), http).unwrap();
        held.set(client.clone()).ok();
        smol::block_on(async {
            let (events_tx, mut events) = mpsc::unbounded();
            let request = ForwardRequest {
                method: Method::GET,
                path_and_query: "graphs/knowledge/schema".into(),
                headers: Vec::new(),
                body: AsyncBody::empty(),
            };
            let response = super::forward(&client, &events_tx, request).await;
            assert!(response.is_ok_and(|response| response.status == StatusCode::UNAUTHORIZED));
            drop(events_tx);
            assert!(events.next().await.is_none(), "said for a replaced token");
        });
    }

    /// A write's success answer says what it did, as Studio's own writes read theirs: its
    /// commit with the commit's kind and its counts, whether a load created its branch, and a
    /// branch's create or delete, which publish none. A stored query that reads wrote nothing,
    /// though its rows name the commit they were read at. An answer that is no write's receipt,
    /// or a body that cannot be read, confirms nothing.
    #[test]
    fn a_writes_success_answer_says_what_it_did() {
        // A write the client never sent wrote nothing: its outcome says so, and the CLI is
        // answered that nothing was sent and what to do, rather than left to report it unknown.
        // One whose answer was lost stays unknown, with nothing to answer.
        let (seen, reply) = smol::block_on(super::read_write_answer(
            Err(NoAnswer::Retired("the target is gone".into())),
            WriteKind::Mutate,
        ));
        assert_eq!(
            seen,
            super::Seen::Refused("the target is gone, so nothing was sent.".into())
        );
        assert!(matches!(reply, Some(super::Reply::NotSent(unsent))
            if unsent.status == 403
                && unsent.message
                    == "Studio: the target is gone. Tell the person what this says. Nothing was sent."));
        // One Studio would not make as asked says to change it.
        let (seen, reply) = smol::block_on(super::read_write_answer(
            Err(NoAnswer::Unsendable("refused path \"../x\"".into())),
            WriteKind::Mutate,
        ));
        assert_eq!(
            seen,
            super::Seen::Refused("refused path \"../x\", so nothing was sent.".into())
        );
        assert!(matches!(reply, Some(super::Reply::NotSent(unsent))
            if unsent.status == 403
                && unsent.message == "Studio: refused path \"../x\". Change the request, or tell \
                                      the person what this says. Nothing was sent."));
        // One whose server refused the connection may be tried again once it answers: a 503
        // whose disposition says so (`effects: none`, `action: retry`), nothing having been sent.
        let (seen, reply) = smol::block_on(super::read_write_answer(
            Err(NoAnswer::Unreached("the connection was refused".into())),
            WriteKind::Mutate,
        ));
        assert_eq!(
            seen,
            super::Seen::Refused("the connection was refused, so nothing was sent.".into())
        );
        assert!(matches!(reply, Some(super::Reply::NotSent(unsent))
            if unsent.status == 503
                && unsent.code == "service_unavailable"
                && unsent.message.ends_with("Try again once it answers, or tell the person. \
                                             Nothing was sent.")));
        let (seen, reply) = smol::block_on(super::read_write_answer(
            Err(NoAnswer::Lost("the connection closed".into())),
            WriteKind::Mutate,
        ));
        assert_eq!(seen, super::Seen::Unknown("the connection closed".into()));
        assert!(reply.is_none());

        let answer = |kind: WriteKind, body: &str| {
            let mut headers = HeaderMap::new();
            headers.append(CONTRACT_HEADER, CONTRACT.parse().unwrap());
            let response = graph_client::ForwardResponse {
                status: StatusCode::OK,
                headers,
                body: AsyncBody::from(body.to_owned()),
                token_replaced: false,
                unauthorized: None,
            };
            smol::block_on(super::read_write_answer(Ok(response), kind)).0
        };
        let written = |commit: Option<(&str, CommitKind)>, nodes, edges, branch_created| {
            super::Seen::Written {
                commit: commit.map(|(id, kind)| (id.to_owned(), kind)),
                nodes,
                edges,
                branch_created,
            }
        };
        let mutation = |commit: &str| {
            format!(
                r#"{{"branch":"b","query_name":"m","affected_nodes":1,"affected_edges":0,
                    "commit":{commit}}}"#
            )
        };
        assert_eq!(
            answer(
                WriteKind::Mutate,
                r#"{"branch":"b","query_name":"m","affected_nodes":2,"affected_edges":1,
                    "commit":{"graph_commit_id":"c1","kind":"published","published":true}}"#
            ),
            written(Some(("c1", CommitKind::Published)), 2, 1, false)
        );
        for (created, expected) in [("true", true), ("false", false)] {
            assert_eq!(
                answer(
                    WriteKind::Load,
                    &format!(
                        r#"{{"branch":"b","branch_created":{created},"mode":"append",
                            "nodes":[{{"entities_loaded":3}},{{"entities_loaded":4}}],"edges":[],
                            "commit":{{"graph_commit_id":"c2","kind":"published"}}}}"#
                    )
                ),
                written(Some(("c2", CommitKind::Published)), 7, 0, expected)
            );
        }
        // A branch created or deleted through the API publishes no commit, and a mutation that
        // matched nothing names none.
        assert_eq!(
            answer(
                WriteKind::BranchCreate,
                r#"{"uri":"u","from":"main","name":"b","from_commit_id":"c0"}"#
            ),
            written(None, 0, 0, false)
        );
        assert_eq!(
            answer(WriteKind::BranchDelete, r#"{"uri":"u","name":"b"}"#),
            written(None, 0, 0, false)
        );
        assert_eq!(
            answer(WriteKind::Mutate, &mutation("null")),
            written(None, 1, 0, false)
        );
        // The commit's kind is read as Studio's own writes read it: a proposal, a snapshot, and
        // one of a kind this build does not know, said held or not. The kind decides over
        // `published`.
        for (commit, kind) in [
            (
                r#"{"graph_commit_id":"c3","kind":"unpublished","published":false}"#,
                CommitKind::Unpublished,
            ),
            (
                r#"{"graph_commit_id":"c3","kind":"unattached","published":false}"#,
                CommitKind::Unattached,
            ),
            (
                r#"{"graph_commit_id":"c3","kind":"pending","published":false}"#,
                CommitKind::Other,
            ),
            (
                r#"{"graph_commit_id":"c3","kind":"pending"}"#,
                CommitKind::Other,
            ),
            (
                r#"{"graph_commit_id":"c3","kind":"published","published":false}"#,
                CommitKind::Published,
            ),
        ] {
            assert_eq!(
                answer(WriteKind::StoredQuery, &mutation(commit)),
                written(Some(("c3", kind)), 1, 0, false),
                "{commit}"
            );
        }
        // A commit without a kind, or with one that is no name, is no 0.14 receipt.
        for commit in [
            r#"{"graph_commit_id":"c3","published":false}"#,
            r#"{"graph_commit_id":"c3","kind":7}"#,
        ] {
            assert!(
                matches!(
                    answer(WriteKind::StoredQuery, &mutation(commit)),
                    super::Seen::Unknown(_)
                ),
                "{commit}"
            );
        }
        // A stored query that reads answers with rows: nothing was written, whatever commit
        // they were read at.
        assert_eq!(
            answer(
                WriteKind::StoredQuery,
                r#"{"query_name":"people","target":{"branch":"agent/x"},"row_count":1,
                    "rows":[{"n":1}],"graph_commit_id":"c5"}"#
            ),
            super::Seen::Read
        );
        // Rows are no receipt of any other write, and an answer with neither confirms nothing.
        for (kind, body) in [
            (WriteKind::Mutate, r#"{"rows":[],"graph_commit_id":"c5"}"#),
            (WriteKind::StoredQuery, r#"{"graph_commit_id":"c5"}"#),
            (WriteKind::BranchCreate, r#"{"graph_commit_id":"c5"}"#),
            (WriteKind::Mutate, &mutation(r#"{"kind":"published"}"#)),
        ] {
            assert!(
                matches!(answer(kind, body), super::Seen::Unknown(_)),
                "{kind:?} {body}"
            );
        }
        for unreadable in ["", "done", "{\"commit\":"] {
            assert!(
                matches!(answer(WriteKind::Mutate, unreadable), super::Seen::Unknown(reason)
                    if reason.contains("could not be read")),
                "{unreadable:?}"
            );
        }
    }

    /// An approval the gateway never took, because the conversation stopped as the person
    /// allowed the write, records that nothing was sent; so does one taken as the CLI gave up.
    #[test]
    fn an_approval_the_gateway_never_took_records_that_nothing_was_sent() {
        let recorded = || {
            let heard: Arc<Mutex<Option<super::Seen>>> = Arc::default();
            let record = heard.clone();
            let approval: OnOutcome = Box::new(move |seen: &super::Seen| {
                *record.lock().unwrap() = Some(seen.clone());
            });
            (heard, approval)
        };
        let (heard, approval) = recorded();
        let (decide, decision) = oneshot::channel();
        let decision = Decision(decision);
        assert!(decide.send(Some(approval)).is_ok());
        drop(decision);
        assert_eq!(
            *heard.lock().unwrap(),
            Some(super::Seen::Refused(NOT_SENT_STOPPED.into()))
        );

        let (heard, approval) = recorded();
        let (decide, decision) = oneshot::channel();
        let mut decision = Decision(decision);
        assert!(decide.send(Some(approval)).is_ok());
        decision.abandon();
        drop(decision);
        assert_eq!(
            *heard.lock().unwrap(),
            Some(super::Seen::Refused(NOT_SENT.into())),
            "said once, as the CLI gave up"
        );
        // Abandoned first, a later approval finds nobody to take it.
        let (_, approval) = recorded();
        let (decide, decision) = oneshot::channel();
        let mut decision = Decision(decision);
        decision.abandon();
        assert!(decide.send(Some(approval)).is_err());
        // One the gateway took is not said again.
        let (heard, approval) = recorded();
        let (decide, decision) = oneshot::channel();
        let mut decision = Decision(decision);
        assert!(decide.send(Some(approval)).is_ok());
        let taken = smol::block_on(&mut decision);
        drop(decision);
        assert!(matches!(taken, Ok(Some(_))));
        assert_eq!(*heard.lock().unwrap(), None);
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
                token_replaced: false,
                unauthorized: None,
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

    /// A read the CLI stops waiting for (its shell's timeout) is dropped with its server
    /// request: it would otherwise hold its connection's place, proved, until the server
    /// answered.
    #[test]
    fn a_read_the_cli_gave_up_on_is_dropped_with_its_server_request() {
        /// Says when the server's handling of the request is dropped.
        struct Dropped(Option<oneshot::Sender<()>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                if let Some(dropped) = self.0.take() {
                    dropped.send(()).ok();
                }
            }
        }
        smol::block_on(async {
            let (arrived_tx, arrived) = oneshot::channel::<()>();
            let (dropped_tx, dropped) = oneshot::channel::<()>();
            let gates = Arc::new(Mutex::new(Some((arrived_tx, dropped_tx))));
            let http = FakeHttpClient::create(move |_request| {
                let gates = gates.clone();
                async move {
                    let (arrived, dropped) = gates.lock().unwrap().take().unwrap();
                    let _held = Dropped(Some(dropped));
                    arrived.send(()).ok();
                    futures::future::pending().await
                }
            });
            let client =
                GraphClient::new("http://127.0.0.1:8787", Some("person-token".into()), http)
                    .unwrap();
            let mut gateway = Gateway::bind().unwrap();
            let (cli, token) = (Cli::of(&gateway), gateway.token.clone());
            let (events_tx, _events) = mpsc::unbounded();
            let _serve = smol::spawn(gateway.serve(client, sandbox(), events_tx));
            let mut stream = cli.connect().await.unwrap();
            let read = post(
                "/graphs/knowledge/query",
                &token,
                Some(CONTRACT),
                r#"{"query":"q","branch":"main"}"#,
            );
            stream.write_all(read.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            arrived.await.unwrap();
            drop(stream);
            let dropped = smol::future::or(async { dropped.await.is_ok() }, async {
                smol::Timer::after(Duration::from_secs(30)).await;
                false
            })
            .await;
            assert!(dropped, "the server request outlived the CLI's connection");
        });
    }

    /// A reader that gives at most `step` bytes a read.
    struct Trickle {
        data: Vec<u8>,
        step: usize,
    }

    impl AsyncRead for Trickle {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buffer: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let given = buffer.len().min(self.step).min(self.data.len());
            buffer[..given].copy_from_slice(&self.data[..given]);
            self.data.drain(..given);
            std::task::Poll::Ready(Ok(given))
        }
    }

    /// A chunked body decodes the same however its bytes arrive: a chunk's data split across
    /// reads, its CRLF apart from it, the next size line in the same read as the data, and part
    /// of it read with the head.
    #[test]
    fn a_chunked_body_decodes_however_its_bytes_arrive() {
        let framed = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        for step in [1, 2, 3, 7, 8, 8 * 1024] {
            for with_head in [0, 4, 9] {
                let mut stream = Trickle {
                    data: framed[with_head..].to_vec(),
                    step,
                };
                let body = smol::block_on(read_chunked(&mut stream, framed[..with_head].to_vec()));
                assert!(
                    matches!(&body, Ok(body) if body == b"hello world"),
                    "step {step}, {with_head} bytes with the head"
                );
            }
        }
    }

    /// A connection that gives a request's line and headers in its first read, then as much body
    /// as it is asked for, counting what it gave.
    struct Declaring {
        head: Vec<u8>,
        body_read: usize,
    }

    impl AsyncRead for Declaring {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buffer: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if !self.head.is_empty() {
                let given = buffer.len().min(self.head.len());
                buffer[..given].copy_from_slice(&self.head[..given]);
                self.head.drain(..given);
                return std::task::Poll::Ready(Ok(given));
            }
            buffer.fill(b'x');
            self.body_read += buffer.len();
            std::task::Poll::Ready(Ok(buffer.len()))
        }
    }

    /// Studio reads a body only for a request with the session's token. The health probe, which
    /// carries none, and a request with no token or another, are refused once their headers are
    /// in, whatever body they declare, without a byte of it read: any local process can send
    /// them. The probe without a body is answered, and a request with the token has its body
    /// read. Only the token proves the connection, before its body is read, so a full
    /// listener keeps it.
    #[test]
    fn only_a_request_with_the_token_has_its_body_read() {
        let read = |head: &str| {
            let mut stream = Declaring {
                head: head.as_bytes().to_vec(),
                body_read: 0,
            };
            let standing = Standing::default();
            let read = smol::block_on(read_request(
                &mut stream,
                "the-token",
                Instant::now() + HEAD_WITHIN,
                &standing,
            ));
            let status = match read {
                Ok(_) => 200,
                Err(ReadFailure::Answer(status, ..)) => status,
                Err(ReadFailure::Gone) => 0,
            };
            (status, stream.body_read, standing.proved())
        };
        let large = format!("content-length: {MAX_BODY}");
        for (request, status) in [
            (
                format!("GET /healthz HTTP/1.1\r\nhost: x\r\n{large}\r\n\r\n"),
                400,
            ),
            (
                "HEAD /healthz HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n".into(),
                400,
            ),
            (
                format!(
                    "GET /healthz HTTP/1.1\r\nhost: x\r\nauthorization: Bearer the-token\r\n\
                     {large}\r\n\r\n"
                ),
                400,
            ),
            (
                "GET /healthz HTTP/1.1\r\nhost: x\r\ncontent-length: many\r\n\r\n".into(),
                400,
            ),
            (
                "GET /healthz HTTP/1.1\r\nhost: x\r\ncontent-length: 0\r\n\r\n".into(),
                200,
            ),
            ("GET /healthz HTTP/1.1\r\nhost: x\r\n\r\n".into(), 200),
            (
                format!("POST /graphs/knowledge/load HTTP/1.1\r\nhost: x\r\n{large}\r\n\r\n"),
                401,
            ),
            (
                format!(
                    "POST /graphs/knowledge/load HTTP/1.1\r\nhost: x\r\n\
                     authorization: Bearer wrong\r\n{large}\r\n\r\n"
                ),
                401,
            ),
            (
                "POST /graphs/knowledge/load HTTP/1.1\r\nhost: x\r\nauthorization: Bearer \
                 wrong\r\ntransfer-encoding: chunked\r\n\r\n"
                    .into(),
                401,
            ),
        ] {
            assert_eq!(read(&request), (status, 0, false), "{request}");
        }
        // What the stream counts is read: a request with the token has the body it declares
        // read, and one that declares a body it cannot frame is refused.
        let (status, body_read, proved) = read(
            "POST /graphs/knowledge/load HTTP/1.1\r\nhost: x\r\nauthorization: Bearer \
             the-token\r\ncontent-length: 100000\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert!(body_read >= 100_000, "{body_read}");
        assert!(proved);
        for framing in ["content-length: 5, 6", "transfer-encoding: gzip"] {
            let request = format!(
                "POST /graphs/knowledge/load HTTP/1.1\r\nhost: x\r\nauthorization: Bearer \
                 the-token\r\n{framing}\r\n\r\n"
            );
            assert_eq!(read(&request), (400, 0, true), "{framing}");
        }
    }

    /// One bound, from a connection's opening, covers its handshake and its request's head: a
    /// process that takes most of it to finish the handshake, then sends nothing, is let go when
    /// it ends, not a second bound later.
    #[test]
    fn a_connection_is_let_go_one_bound_after_it_opens() {
        smol::block_on(async {
            let (client, _) = upstream();
            let mut gateway = Gateway::bind().unwrap();
            let cli = Cli::of(&gateway);
            let (events_tx, _events) = mpsc::unbounded();
            let _serve = smol::spawn(gateway.serve(client, sandbox(), events_tx));
            let opened = Instant::now();
            let tcp = TcpStream::connect(("127.0.0.1", cli.port)).await.unwrap();
            smol::Timer::after(HEAD_WITHIN * 2 / 3).await;
            // The handshake, late, and then nothing: what ends is the gateway's close, or the
            // handshake itself when its time ran out first.
            let closed = by(opened + HEAD_WITHIN * 3, async {
                if let Ok(stream) = cli.tls.connect(crate::gateway_tls::loopback(), tcp).await {
                    read_all(stream).await;
                }
            })
            .await;
            let held = opened.elapsed();
            assert!(closed.is_some(), "the gateway never let it go");
            // A bound of its own for the head would have held it a whole one past the
            // handshake, five thirds of one in all.
            assert!(held < HEAD_WITHIN * 4 / 3, "held for {held:?}");
        });
    }

    #[test]
    fn the_gateway_checks_the_session_then_forwards_reads_and_approved_writes() {
        smol::block_on(async {
            let (client, seen) = upstream();
            let upstream_client = client.clone();
            let mut gateway = Gateway::bind().unwrap();
            let (cli, token) = (Cli::of(&gateway), gateway.token.clone());
            let (events_tx, mut events) = mpsc::unbounded();
            let _serve = smol::spawn(gateway.serve(client, sandbox(), events_tx));

            // The CLI's probe: no token, no contract header.
            let (status, head, _) = call(
                cli.clone(),
                "HEAD /healthz HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n".into(),
            )
            .await;
            assert_eq!(status, 200);
            assert!(
                head.contains(&format!("{CONTRACT_HEADER}: {CONTRACT}")),
                "{head}"
            );

            // A wrong token, and a CLI that sends no contract.
            let (status, head, body) = call(
                cli.clone(),
                post("/graphs/knowledge/query", "nope", Some(CONTRACT), "{}"),
            )
            .await;
            assert_eq!(status, 401);
            assert!(head.contains(&format!("{CONTRACT_HEADER}: {CONTRACT}")));
            assert!(body.contains("\"code\":\"unauthorized\""), "{body}");
            // Its body is never waited for: one that says it is large, and never comes, is
            // answered at once.
            let declared = "POST /graphs/knowledge/load HTTP/1.1\r\nhost: x\r\n\
                authorization: Bearer wrong\r\ncontent-length: 50000000\r\n\r\n"
                .to_owned();
            let soon = || Instant::now() + std::time::Duration::from_secs(5);
            let (status, _, _) = by(soon(), call(cli.clone(), declared))
                .await
                .expect("a refused request's body is not waited for");
            assert_eq!(status, 401);
            // Nor is a health probe's, which carries no token: one that declares a body, as large
            // as a load may carry, is refused at once, the body never read.
            for framing in [
                format!("content-length: {MAX_BODY}"),
                "transfer-encoding: chunked".to_owned(),
            ] {
                let probe = format!("GET /healthz HTTP/1.1\r\nhost: x\r\n{framing}\r\n\r\n");
                let (status, _, body) = by(soon(), call(cli.clone(), probe))
                    .await
                    .expect("the probe's body is not waited for");
                assert_eq!(status, 400, "{framing}: {body}");
                assert!(body.contains("carries no body"), "{body}");
            }
            let (status, _, body) = call(
                cli.clone(),
                post("/graphs/knowledge/query", &token, None, "{}"),
            )
            .await;
            assert_eq!(status, 400);
            assert!(body.contains("api_contract_mismatch"), "{body}");
            assert!(
                seen.lock().unwrap().is_empty(),
                "nothing reached the server"
            );

            // A read goes through with the person's token, and its answer comes back.
            let (status, _, body) = call(
                cli.clone(),
                post(
                    "/graphs/knowledge/query",
                    &token,
                    Some(CONTRACT),
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
                assert_eq!(contract, CONTRACT);
                assert_eq!(body, r#"{"query":"q","branch":"main"}"#);
            }

            // A write to main is refused before anything is sent.
            let (status, _, body) = call(
                cli.clone(),
                post(
                    "/graphs/knowledge/mutate",
                    &token,
                    Some(CONTRACT),
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
                    cli.clone(),
                    post(target, &token, Some(CONTRACT), "{}\n"),
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
                cli.clone(),
                post("/graphs/knowledge/mutate", &token, Some(CONTRACT), write),
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
                cli.clone(),
                post("/graphs/knowledge/mutate", &token, Some(CONTRACT), write),
            ));
            let Some(GatewayEvent::Write { decide, .. }) = events.next().await else {
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
            assert!(head.contains(&format!("{CONTRACT_HEADER}: {CONTRACT}")));
            assert!(body.contains("\"graph_commit_id\":\"c9\""), "{body}");
            // Its outcome was recorded before the answer went back to the CLI.
            assert_eq!(
                recorded.lock().unwrap().clone(),
                Some(super::Seen::Written {
                    commit: Some(("c9".into(), CommitKind::Published)),
                    nodes: 2,
                    edges: 1,
                    branch_created: false,
                })
            );
            assert_eq!(seen.lock().unwrap().len(), 2);

            // The CLI gives up before the person decides (its command timed out): the write is
            // never sent, even when allowed afterwards.
            let mut hung_up = cli.connect().await.unwrap();
            hung_up
                .write_all(
                    post("/graphs/knowledge/mutate", &token, Some(CONTRACT), write).as_bytes(),
                )
                .await
                .unwrap();
            hung_up.flush().await.unwrap();
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
                 {CONTRACT_HEADER}: {CONTRACT}\r\ncontent-type: application/json\r\n\
                 transfer-encoding: chunked\r\n\r\n5\r\n{{\"a\":\r\n2\r\n1}}\r\n0\r\n\r\n"
            );
            let (status, _, _) = call(cli.clone(), chunked).await;
            assert_eq!(status, 200);
            assert_eq!(seen.lock().unwrap()[2].4, r#"{"a":1}"#);

            // A chunk size near the largest number is refused, not added to.
            let huge = format!(
                "POST /graphs/knowledge/query HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {token}\r\n\
                 {CONTRACT_HEADER}: {CONTRACT}\r\ncontent-type: application/json\r\n\
                 transfer-encoding: chunked\r\n\r\n1\r\n{{\r\n{:x}\r\n",
                usize::MAX
            );
            let (status, _, body) = call(cli.clone(), huge).await;
            assert_eq!(status, 413, "{body}");
            // As the server's own errors do, it says what it did: nothing.
            let refused: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(refused["code"], "payload_too_large", "{body}");
            assert_eq!(
                refused["disposition"],
                json!({ "effects": "none", "action": "fix" }),
                "{body}"
            );
            assert_eq!(seen.lock().unwrap().len(), 3, "nothing was sent");

            // A stored query the CLI runs as a read on the agent's branch is forwarded as a
            // read: no card, and its answer streams back.
            let read = futures::FutureExt::fuse(call(
                cli.clone(),
                post(
                    "/graphs/knowledge/queries/people",
                    &token,
                    Some(CONTRACT),
                    r#"{"params":{},"branch":"agent/20261013-101500","expect_mutation":false}"#,
                ),
            ));
            let write = futures::FutureExt::fuse(events.next());
            futures::pin_mut!(read, write);
            let (status, _, body) = futures::select! {
                read = read => read,
                _ = write => panic!("a stored read was judged a write"),
            };
            assert_eq!(status, 200);
            assert_eq!(body, r#"{"rows":[{"n":200}]}"#);
            assert_eq!(seen.lock().unwrap().len(), 4);

            // The target retired (the CLI no longer lists it) while the person decides a write:
            // approved, it is answered that nothing was sent, as the gateway's own refusals
            // are, and its outcome is a refusal, not unknown. A read, and a write asked after,
            // are refused at once, the person not asked.
            let mutation =
                r#"{"query":"query m() { … }","name":"m","branch":"agent/20261013-101500"}"#;
            let allowed = smol::spawn(call(
                cli.clone(),
                post("/graphs/knowledge/mutate", &token, Some(CONTRACT), mutation),
            ));
            let Some(GatewayEvent::Write { decide, .. }) = events.next().await else {
                panic!("no write event")
            };
            upstream_client.retire("the target is gone".into());
            let refused_so = |status: u16, body: &str| {
                assert_eq!(status, 403, "{body}");
                let refused: Value = serde_json::from_str(body).unwrap();
                assert_eq!(refused["code"], "forbidden", "{body}");
                assert_eq!(
                    refused["disposition"],
                    json!({ "effects": "none", "action": "fix" }),
                    "{body}"
                );
                assert!(
                    body.contains(
                        "Studio: the target is gone. Tell the person what this says. Nothing was \
                         sent."
                    ),
                    "{body}"
                );
            };
            let (status, _, body) = call(
                cli.clone(),
                post(
                    "/graphs/knowledge/query",
                    &token,
                    Some(CONTRACT),
                    r#"{"query":"q","branch":"main"}"#,
                ),
            )
            .await;
            refused_so(status, &body);
            let after = smol::spawn(call(
                cli.clone(),
                post("/graphs/knowledge/mutate", &token, Some(CONTRACT), mutation),
            ));
            let futures::future::Either::Left(((status, _, body), _)) =
                futures::future::select(after, events.next()).await
            else {
                panic!("the person was asked to approve a write to a retired target")
            };
            refused_so(status, &body);
            let recorded: Arc<Mutex<Option<super::Seen>>> = Arc::default();
            let record = recorded.clone();
            decide
                .send(Some(Box::new(move |seen: &super::Seen| {
                    *record.lock().unwrap() = Some(seen.clone());
                })))
                .ok()
                .unwrap();
            let (status, _, body) = allowed.await;
            refused_so(status, &body);
            assert_eq!(
                recorded.lock().unwrap().clone(),
                Some(super::Seen::Refused(
                    "the target is gone, so nothing was sent.".into()
                ))
            );
            assert_eq!(seen.lock().unwrap().len(), 4, "nothing reached the server");
        });
    }

    /// The CLI reaches its gateway over HTTPS alone, with what its home holds: a client that
    /// trusts only `ca.pem` and sends the token from `token` is answered; one that sends no token
    /// is refused; one that trusts another certificate does not get through the handshake; and
    /// plain HTTP gets no HTTP answer at all. Only the one read with the token reaches the
    /// server, with the person's token.
    #[test]
    fn the_cli_reaches_the_gateway_over_https_with_what_its_home_holds() {
        smol::block_on(async {
            let (client, seen) = upstream();
            let mut gateway = Gateway::bind().unwrap();
            let cwd =
                std::env::temp_dir().join(format!("studio-gateway-tls-{}", std::process::id()));
            std::fs::remove_dir_all(&cwd).ok();
            std::fs::create_dir_all(&cwd).unwrap();
            let home =
                crate::cli_home::CliHome::write(&cwd, gateway.access(), "knowledge").unwrap();
            let port = gateway.port;
            let (events_tx, _events) = mpsc::unbounded();
            let _serve = smol::spawn(gateway.serve(client, sandbox(), events_tx));
            let ca = std::fs::read_to_string(home.dir.join("ca.pem")).unwrap();
            let token = std::fs::read_to_string(home.dir.join("token")).unwrap();
            let cli = Cli::trusting(port, &ca);
            let body = r#"{"query":"q","branch":"main"}"#;

            let (status, _, answer) = call(
                cli.clone(),
                post("/graphs/knowledge/query", &token, Some(CONTRACT), body),
            )
            .await;
            assert_eq!((status, answer.as_str()), (200, r#"{"rows":[{"n":200}]}"#));
            let tokenless = format!(
                "POST /graphs/knowledge/query HTTP/1.1\r\nhost: 127.0.0.1\r\n\
                 {CONTRACT_HEADER}: {CONTRACT}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\n\r\n{body}",
                body.len()
            );
            let (status, _, answer) = call(cli.clone(), tokenless).await;
            assert_eq!(status, 401, "{answer}");

            let stranger = crate::gateway_tls::identity().unwrap().certificate;
            assert!(Cli::trusting(port, &stranger).connect().await.is_err());

            let mut plain = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            plain
                .write_all(post("/graphs/knowledge/query", &token, Some(CONTRACT), body).as_bytes())
                .await
                .unwrap();
            let answer = by(
                Instant::now() + std::time::Duration::from_secs(30),
                read_all(plain),
            )
            .await
            .expect("the gateway closes a connection that does not speak TLS");
            assert!(
                !String::from_utf8_lossy(&answer).contains("HTTP/"),
                "{answer:?}"
            );

            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "only the read with the token was sent");
            assert_eq!(seen[0].2, "Bearer person-token");
            drop(home);
            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    /// A real omnigraph CLI that reads Omnigraph's version-1 operator configuration is chosen
    /// for the home Studio writes, and reaches the gateway over HTTPS with that home's token and
    /// certificate as the agent's shell runs it (the wrapper): the request reaches the server
    /// with the person's token. The fake
    /// server's answer need not be one the CLI reads; what reached it is what is checked.
    #[test]
    #[ignore = "needs: OMNIGRAPH_STUDIO_LIVE_CLI, an omnigraph CLI of Omnigraph v0.14.0 or later"]
    fn a_live_cli_reaches_the_gateway_with_what_its_home_holds() {
        let cli = std::env::var_os("OMNIGRAPH_STUDIO_LIVE_CLI")
            .map(std::path::PathBuf::from)
            .expect("set OMNIGRAPH_STUDIO_LIVE_CLI to an omnigraph CLI");
        smol::block_on(async {
            let (client, seen) = upstream();
            let mut gateway = Gateway::bind().unwrap();
            let cwd =
                std::env::temp_dir().join(format!("studio-gateway-live-{}", std::process::id()));
            std::fs::remove_dir_all(&cwd).ok();
            std::fs::create_dir_all(&cwd).unwrap();
            let home =
                crate::cli_home::CliHome::write(&cwd, gateway.access(), "knowledge").unwrap();
            let (events_tx, _events) = mpsc::unbounded();
            let _serve = smol::spawn(gateway.serve(client, sandbox(), events_tx));
            let chosen = crate::cli_home::choose_cli(&home, vec![cli]).await;
            if let Some(why) = chosen.refusal() {
                panic!("{why}");
            }
            let args = ["branch", "list", "--json"];
            let shell = smol::process::Command::new(home.command())
                .args(args)
                .stdin(std::process::Stdio::null())
                .output()
                .await
                .unwrap();
            let shell = String::from_utf8_lossy(&shell.stderr).into_owned();
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "shell: {shell}");
            for (method, uri, auth, contract, _) in seen.iter() {
                assert_eq!(
                    (method.as_str(), uri.as_str()),
                    ("GET", "http://127.0.0.1:8787/graphs/knowledge/branches")
                );
                assert_eq!(
                    (auth.as_str(), contract.as_str()),
                    ("Bearer person-token", CONTRACT)
                );
            }
            drop(home);
            std::fs::remove_dir_all(&cwd).ok();
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
        let output = Command::new(home.command())
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
    #[ignore = "needs a running Omnigraph server and an omnigraph CLI: set STUDIO_GATEWAY_E2E_*"]
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
        let home = CliHome::write(&cwd, gateway.access(), &graph).unwrap();
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
            while let Some(event) = events.next().await {
                match event {
                    GatewayEvent::Write { write, decide, .. } => {
                        let (kept, summary) = (kept.clone(), write.summary);
                        decide
                            .send(Some(Box::new(move |outcome: &super::Seen| {
                                kept.lock().unwrap().push((summary, outcome.clone()));
                            })))
                            .ok();
                    }
                    GatewayEvent::Abandoned { .. } | GatewayEvent::TokenRefused => {}
                }
            }
        });

        let candidates = match env("STUDIO_GATEWAY_E2E_CLI") {
            Some(cli) => vec![PathBuf::from(cli)],
            None => graph_store::cli::candidates(),
        };
        let chosen = smol::block_on(cli_home::choose_cli(&home, candidates));
        assert!(
            matches!(chosen, cli_home::CliChoice::Chosen { .. }),
            "no omnigraph CLI speaks the graph's contract: {chosen:?}"
        );

        // As the agent is told: no destination flags, since the home's config makes the gateway
        // and the graph the default, as a target or as a server.
        let with =
            |args: &[&str]| -> Vec<String> { args.iter().map(|arg| (*arg).to_owned()).collect() };
        let call = |args: Vec<String>| {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            run(&home, &args)
        };

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
            super::Seen::Written {
                commit: Some((_, CommitKind::Published)),
                nodes: 1,
                ..
            }
        ));
        drop(home);
        std::fs::remove_dir_all(&cwd).ok();
    }
}
