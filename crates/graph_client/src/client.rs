//! The HTTP client for one Omnigraph server (spec 003).
//!
//! Reads return `Result<T, ReadError>`. Writes return a [`WriteOutcome`] and are never retried
//! here (constitution III).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures::AsyncReadExt;
use http_client::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use http_client::{AsyncBody, HttpClient, HttpRequestExt, RedirectPolicy};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use crate::api::{
    AccessOutput, BlobWriteOutput, BranchCreateOutput, BranchCreateRequest, BranchDeleteOutput,
    BranchListOutput, BranchMergeOutput, BranchMergeRequest, ChangeFeedOutput, ChangeOutput,
    ChangeRequest, CommitChanges, CommitListOutput, DiffOutput, DispositionEffects, ErrorOutput,
    GraphListResponse, HealthOutput, InvokeStoredQueryRequest, MergeBaseOutput,
    QueriesCatalogOutput, QueryRequest, ReadOutput, ReadinessOutput, SchemaOutput, SnapshotOutput,
};
use crate::{
    Answered, ApiError, CONTRACT, CONTRACT_HEADER, IF_GRAPH_COMMIT_HEADER, WriteOutcome,
    speaks_contract,
};

const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a read of changes waits for its answer: a page of a branch's change feed, of a diff,
/// of one commit's changes, and a merge's dry run. The server works such a page out whole before
/// it answers and sets no deadline of its own, and the work grows with the tables the changes
/// touch, not with the page (spec 012, E35 to E38): on the hosted personal graph (Omnigraph
/// `0.14.1-edge`, 2026-10-25) one page of a branch's review took 37 to 60 s, so a 60 s read
/// failed about as often as it answered. As long as a write, which the server also works out
/// whole. A read is still sent once, unless refused with 429.
pub const CHANGE_READ_TIMEOUT: Duration = Duration::from_secs(300);
const WRITE_TIMEOUT: Duration = Duration::from_secs(300);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// How many times a read refused with 429 is tried again (spec 003). Writes never are.
const READ_RETRIES: usize = 2;
/// The longest `Retry-After` a read waits; a longer one is shown as the refusal.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// The wait when a 429 names none.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);
/// How long a request whose token ended waits for the next before it sends what it holds: a
/// check of a server that could not be reached (5 seconds), a sign-in's refresh (15), the pause
/// before its one retry (5) and the retry (15), and a margin.
const ENDED_TOKEN_WAIT: Duration = Duration::from_secs(45);
/// How often a request waiting for the next token tells the holder again.
const ENDED_TOKEN_TELL: Duration = Duration::from_secs(1);
/// How many reads one connection has on the wire at once (spec 003). A server admits a bounded
/// number of reads across all its clients (it refuses past its read ingress cap, 64 by
/// default), so a view that asks for many counts at once queues here rather than being refused
/// there.
pub const READ_CONCURRENCY: usize = 8;

/// Why a read failed.
#[derive(Debug, Clone)]
pub enum ReadError {
    /// The request did not complete: connection refused, timeout, TLS, DNS; or something
    /// between Studio and the server (a proxy, a gateway) answered it with an error that lacks
    /// the contract header, which says nothing of the server or the graph.
    Transport(String),
    /// The server answered a success without the contract header, or answered naming a
    /// contract Studio does not speak.
    ContractMismatch { served: Option<String> },
    /// The server refused the read.
    Refused(ApiErrorSummary),
    /// The body did not match the contract.
    Decode(String),
    /// The answer is longer than the `limit` bytes its read takes; none of it was decoded.
    TooLarge { limit: usize },
}

/// A cloneable copy of a refusal, for showing in the UI.
#[derive(Debug, Clone)]
pub struct ApiErrorSummary {
    pub status: u16,
    pub message: String,
    pub code: Option<crate::api::ErrorCode>,
    /// The server's diagnostic for a refused query: where in the text, and what (spec 007).
    pub diagnostic: Option<Value>,
    /// Why a commit's changes could not be compared with its parent's (a 409), and the type
    /// at a schema boundary. Boxed, so every read's error stays small.
    pub change_diff_refusal: Option<Box<crate::api::ChangeDiffRefusalOutput>>,
    /// The `Bearer` challenge a 401 carried (`WWW-Authenticate`), which a server sends with
    /// every 401 since Omnigraph #1122; `None` otherwise, and from an older server.
    pub challenge: Option<Box<Challenge>>,
}

/// A `WWW-Authenticate` challenge: its scheme and its parameters, as RFC 9110 §11.6.1 writes
/// them. Omnigraph sends `Bearer`, with `error="invalid_token"` when it refused the token a
/// request carried, and `resource_metadata="<url>"` when it takes OAuth sign-in (Omnigraph RFC
/// 1017, A1). A scheme's token68 payload (`Negotiate abc==`), which some schemes take in place
/// of parameters, is read past and not kept: Studio speaks `Bearer` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub scheme: String,
    /// Each parameter's name, lower-cased, and its value, unquoted.
    pub params: Vec<(String, String)>,
}

impl Challenge {
    /// The value of the parameter `name`, which is matched in any case.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(param, _)| param.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether the server refused the token the request carried (`error="invalid_token"`),
    /// rather than finding none.
    pub fn token_refused(&self) -> bool {
        self.param("error") == Some("invalid_token")
    }

    /// Whether a 401 whose challenge is `challenge` refused the token its request carried: as
    /// the challenge says, or, from a server that sends none (an older one), whether one was
    /// `sent`. One that refused none though a token was sent means it never arrived (a proxy
    /// dropped it); with none sent, the server asks for one.
    pub fn refuses(challenge: Option<&Challenge>, sent: bool) -> bool {
        challenge.map_or(sent, Challenge::token_refused)
    }

    /// Where the server's protected-resource metadata is, when it takes OAuth sign-in.
    pub fn resource_metadata(&self) -> Option<&str> {
        self.param("resource_metadata")
    }

    /// The challenges in one `WWW-Authenticate` value, in order. A parameter is `name=token` or
    /// `name="quoted"`; a word followed only by `=`s is a token68 payload, read past so the
    /// challenge after it is not lost; a word with no `=` after it starts the next challenge.
    pub fn parse_all(value: &str) -> Vec<Challenge> {
        let bytes = value.as_bytes();
        let mut at = 0;
        let skip = |at: &mut usize, also_commas: bool| {
            while *at < bytes.len()
                && (bytes[*at] == b' '
                    || bytes[*at] == b'\t'
                    || (also_commas && bytes[*at] == b','))
            {
                *at += 1;
            }
        };
        let word = |at: &mut usize| {
            let start = *at;
            while *at < bytes.len() && !b" \t,=".contains(&bytes[*at]) {
                *at += 1;
            }
            value[start..*at].to_owned()
        };
        let mut challenges = Vec::new();
        loop {
            skip(&mut at, true);
            let scheme = word(&mut at);
            if scheme.is_empty() {
                break;
            }
            let mut challenge = Challenge {
                scheme,
                params: Vec::new(),
            };
            loop {
                skip(&mut at, true);
                let before = at;
                let name = word(&mut at);
                skip(&mut at, false);
                if name.is_empty() || bytes.get(at) != Some(&b'=') {
                    at = before;
                    break;
                }
                at += 1;
                // `word=`, `word==`: a token68, whose `=`s are padding, not a value.
                if matches!(bytes.get(at), None | Some(b'=' | b',')) {
                    while bytes.get(at) == Some(&b'=') {
                        at += 1;
                    }
                    continue;
                }
                skip(&mut at, false);
                let value = if bytes.get(at) == Some(&b'"') {
                    at += 1;
                    let mut unquoted = String::new();
                    let mut chars = value[at..].char_indices();
                    let mut end = value.len() - at;
                    while let Some((index, c)) = chars.next() {
                        match c {
                            '"' => {
                                end = index + 1;
                                break;
                            }
                            '\\' => {
                                if let Some((_, escaped)) = chars.next() {
                                    unquoted.push(escaped);
                                }
                            }
                            c => unquoted.push(c),
                        }
                    }
                    at += end;
                    unquoted
                } else {
                    word(&mut at)
                };
                challenge.params.push((name.to_ascii_lowercase(), value));
            }
            challenges.push(challenge);
            if at >= bytes.len() {
                break;
            }
        }
        challenges
    }

    /// The `Bearer` challenge among a response's `WWW-Authenticate` values, if any.
    fn bearer(headers: &HeaderMap) -> Option<Challenge> {
        headers
            .get_all(http_client::http::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(Challenge::parse_all)
            .find(|challenge| challenge.scheme.eq_ignore_ascii_case("bearer"))
    }
}

/// What a read at a commit was refused for because the commit predates the current schema: a
/// historical read is written against the current schema, and a type or property the commit
/// does not hold under its current name is refused, named, rather than read as null.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoricalGap {
    /// The node or edge type did not exist at the commit.
    Type { name: String },
    Property {
        owner: String,
        property: String,
        then: PropertyThen,
    },
}

/// What a property was at a commit a read of it was refused at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertyThen {
    /// It did not exist: added since, or dropped and added again.
    Absent,
    /// It had this name; a historical read does not follow a rename.
    Named(String),
    /// Its column carries no property identity (data written before Omnigraph 0.10), so the
    /// server cannot tell which property it was.
    Unwitnessed,
}

impl ApiErrorSummary {
    /// The type or property a historical read was refused for. The server says it only in the
    /// message (`omnigraph-core`'s `missing_graph_type_at_snapshot`,
    /// `missing_graph_property_at_snapshot`, `renamed_graph_property_at_snapshot`,
    /// `unwitnessed_graph_property_at_snapshot`).
    pub fn historical_gap(&self) -> Option<HistoricalGap> {
        if self.status != 400 {
            return None;
        }
        let message = self.message.trim();
        let (subject, then) =
            if let Some(subject) = message.strip_suffix(" does not exist at this snapshot") {
                (subject, PropertyThen::Absent)
            } else if let Some(subject) =
                message.strip_suffix(" has no persisted property-lifetime witness at this snapshot")
            {
                (subject, PropertyThen::Unwitnessed)
            } else {
                let (head, _) = message.split_once(" at this snapshot; ")?;
                let (subject, named) = head.rsplit_once(" is named ")?;
                (subject, PropertyThen::Named(quoted(named)?))
            };
        match subject.strip_prefix("property ") {
            Some(rest) => {
                let (property, owner) = rest.split_once(" of ")?;
                let owner = owner
                    .strip_prefix("node type ")
                    .or_else(|| owner.strip_prefix("edge type "))?;
                Some(HistoricalGap::Property {
                    owner: quoted(owner)?,
                    property: quoted(property)?,
                    then,
                })
            }
            None if then == PropertyThen::Absent => {
                let name = subject
                    .strip_prefix("node type ")
                    .or_else(|| subject.strip_prefix("edge type "))?;
                Some(HistoricalGap::Type {
                    name: quoted(name)?,
                })
            }
            None => None,
        }
    }
}

/// `'name'` without its quotes.
fn quoted(text: &str) -> Option<String> {
    let inner = text.strip_prefix('\'')?.strip_suffix('\'')?;
    (!inner.is_empty() && !inner.contains('\'')).then(|| inner.to_owned())
}

impl ReadError {
    /// Whether the server took the request and had not answered when Studio stopped waiting
    /// for it: a timeout once the connection was open, not one opening it.
    pub fn timed_out(&self) -> bool {
        matches!(self, ReadError::Transport(cause) if cause == REQUEST_TIMED_OUT)
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Transport(message) => write!(f, "Could not reach the server: {message}"),
            ReadError::ContractMismatch {
                served: Some(served),
            } => write!(
                f,
                "The server speaks contract {served}; Studio speaks {}",
                CONTRACT
            ),
            ReadError::ContractMismatch { served: None } => {
                write!(f, "The server did not say which contract it speaks")
            }
            ReadError::Refused(summary) => write!(f, "{} ({})", summary.message, summary.status),
            ReadError::Decode(message) => write!(f, "Unexpected response: {message}"),
            ReadError::TooLarge { limit } if *limit >= 1024 * 1024 => write!(
                f,
                "The answer is over {} MB, the most Studio reads at once",
                limit / (1024 * 1024)
            ),
            ReadError::TooLarge { limit } => write!(
                f,
                "The answer is over {limit} bytes, the most Studio reads at once"
            ),
        }
    }
}

impl std::error::Error for ReadError {}

/// What `HEAD`/`GET /healthz` says about a server.
#[derive(Debug, Clone)]
pub struct ServerHealth {
    pub contract: Option<String>,
    pub health: HealthOutput,
}

impl ServerHealth {
    pub fn contract_supported(&self) -> bool {
        speaks_contract(self.contract.as_deref())
    }
}

struct RawResponse {
    /// The token the request carried: a refusal of one replaced since is not this client's
    /// last word ([`GraphClient::fetch_reading`]).
    carried: Option<Arc<str>>,
    status: StatusCode,
    contract: Option<String>,
    /// `Retry-After` in seconds; the HTTP-date form is not used by Omnigraph.
    retry_after: Option<Duration>,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// One Blob cell (spec 004): a property of a node or an edge.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobCell {
    pub edge: bool,
    pub type_name: String,
    pub id: String,
    pub property: String,
}

/// Where a Blob is read: a branch head, or a commit.
#[derive(Debug, Clone, Copy)]
pub enum BlobAt<'a> {
    Branch(&'a str),
    Snapshot(&'a str),
}

/// What a Blob cell holds, without its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobMeta {
    Null,
    /// Bytes the graph owns.
    Managed {
        size: u64,
        etag: Option<String>,
    },
    /// A reference to an object elsewhere, which Studio never fetches.
    External {
        uri: String,
    },
}

/// The most bytes [`GraphClient::blob_bytes`] reads in one request.
const BLOB_READ_MAX: u64 = 16 * 1024 * 1024;

/// The most of a query's answer a read takes ([`GraphClient::query`], a stored read). A query
/// written in the editor bounds its rows only by its own `limit`, and its answer is decoded
/// whole into JSON values before a row is shown: 13.8 MB of node rows decoded to 164 MB
/// (11.9 times, serde_json with `preserve_order` as Studio builds it), so an answer this long
/// holds about 400 MB once read. A page Studio asks for itself stays far below it: a type
/// table's longest page, 1,000 rows, would need rows of 33 KB each to reach it.
const QUERY_ANSWER_MAX: usize = 32 * 1024 * 1024;

/// The most of any other read's answer a read takes ([`GraphClient::read`]), as a query's: a
/// change-feed page (up to 1,000 changes, each with its images), a commit's changes or a diff
/// page is decoded whole too. The pages Studio asks for stay far below it; a feed read stopped
/// by it leaves its later commits unknown, and the views over them read again. Every other
/// answer is bounded by it too (a write's, a merge preview's, health): one cut short does not
/// decode, which for a write is an unknown outcome.
const READ_ANSWER_MAX: usize = QUERY_ANSWER_MAX;

/// A server address as typed, made into the root URL a client is built on: trimmed, without a
/// trailing slash, and with `http://` (loopback hosts) or `https://` (any other host) when no
/// scheme was given. Refused unless it is an `http` or `https` URL with a host.
pub fn server_root(input: &str) -> Result<String, String> {
    let text = input.trim().trim_end_matches('/');
    if text.is_empty() {
        return Err("Enter the server's address, for example https://graph.example.com".into());
    }
    let text = if text.contains("://") {
        text.to_owned()
    } else {
        // The host as a URL reads it: a name that only begins like a loopback address
        // (`127.example.com`, `[::ffff:203.0.113.7]`) is another host, reached over https.
        let loopback = Url::parse(&format!("http://{text}"))
            .ok()
            .is_some_and(|url| match url.host() {
                Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
                Some(url::Host::Ipv4(address)) => address.is_loopback() || address.is_unspecified(),
                Some(url::Host::Ipv6(address)) => address.is_loopback(),
                None => false,
            });
        format!("{}://{text}", if loopback { "http" } else { "https" })
    };
    let url = Url::parse(&text).map_err(|error| format!("“{input}” is not an address: {error}"))?;
    check_root(&url)?;
    Ok(text)
}

/// Whether a URL can be a server's root: `http` or `https`, with a host, and no user name or
/// password in it (a token is given separately, never in the address).
fn check_root(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() || url.host().is_none() {
        return Err(format!(
            "“{url}” is not a server address: it needs http:// or https:// and a host"
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "A server address cannot hold a user name or password; give the token separately"
                .into(),
        );
    }
    Ok(())
}

/// A path segment with its `%XX` escapes read, as the server reads a graph id from it: the
/// client escapes each segment it sends, and a forwarded request names its graph as its sender
/// wrote it, escaped or not. A `%` not followed by two hex digits stays as it is.
fn decode_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = bytes
            .get(at + 1..at + 3)
            .filter(|hex| bytes[at] == b'%' && hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                at += 3;
            }
            None => {
                decoded.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Where a change-feed read starts: `now`, `beginning` or `after:<commit_id>`; a cursor a
/// terminal page returned; or a page token, to continue one read.
#[derive(Debug, Clone, Copy)]
pub enum FeedAt<'a> {
    Start(&'a str),
    Cursor(&'a str),
    Page(&'a str),
}

/// Why a request has no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoAnswer {
    /// Never sent, as its client is retired ([`GraphClient::retire`]): nothing it asked for
    /// was done, and nothing that client sends will be.
    Retired(String),
    /// Never sent, as Studio would not make it as asked: a path that leaves the server's root,
    /// or a request that could not be built. Nothing it asked for was done.
    Unsendable(String),
    /// Never reached the server, as it refused the connection: a request is written only on a
    /// connection made (an earlier attempt a client retries is one the server did not process),
    /// so nothing it asked for was done.
    Unreached(String),
    /// It was sent, or may have been, and its answer was lost: a write may have landed.
    Lost(String),
}

impl From<NoAnswer> for String {
    fn from(no: NoAnswer) -> Self {
        match no {
            NoAnswer::Retired(reason)
            | NoAnswer::Unsendable(reason)
            | NoAnswer::Unreached(reason)
            | NoAnswer::Lost(reason) => reason,
        }
    }
}

/// The server's transport, which a changed target replaces ([`GraphClient::set_http`]): a client
/// that sends each request through the one current when it is sent, so everything holding it
/// moves to a new one together, and a request under way finishes through the one it began with.
struct Transport(std::sync::RwLock<Arc<dyn HttpClient>>);

impl Transport {
    fn current(&self) -> Arc<dyn HttpClient> {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl HttpClient for Transport {
    // Nothing reads these of the server's transport: each transport puts its own agent and proxy
    // on what it sends.
    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&http_client::Url> {
        None
    }

    fn send(
        &self,
        request: Request<AsyncBody>,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<http_client::Response<AsyncBody>>> {
        self.current().send(request)
    }
}

/// A token as a client holds it: a blank one is none, as a server reads `Bearer ` with nothing
/// after it as no credential (an empty `OMNIGRAPH_STUDIO_TOKEN`, say).
fn held(token: Option<String>) -> Option<Arc<str>> {
    token
        .filter(|token| !token.trim().is_empty())
        .map(Into::into)
}

/// The token a client holds, and when it stops being accepted, as the lease it came with says
/// ([`GraphClient::set_token_ending`]): one lock, so no request reads one without the other.
struct HeldToken {
    value: Option<Arc<str>>,
    /// `None` for a token that does not end, or not known to.
    ends: Option<SystemTime>,
    /// Moves each time the value does ([`GraphClient::token_serial`]).
    serial: u64,
}

/// A client for one server. Cheap to clone.
#[derive(Clone)]
pub struct GraphClient {
    base: Url,
    /// Shared by every clone, as the connection's one token: a token replaced after the server
    /// refused the old one reaches every holder, a running agent's gateway among them.
    token: Arc<std::sync::RwLock<HeldToken>>,
    /// The holder told when a request finds the token held ended, once one listens
    /// ([`Self::token_ended`]). Shared by every clone.
    ended: Arc<std::sync::Mutex<Option<smol::channel::Sender<()>>>>,
    /// How many requests have begun waiting for the next token, each numbered by it
    /// ([`Self::waits_begun`]). Shared by every clone.
    waits_begun: Arc<std::sync::atomic::AtomicU64>,
    /// The number up to which the holder released the requests waiting
    /// ([`Self::release_waits`]). Shared by every clone.
    waits_released: Arc<std::sync::atomic::AtomicU64>,
    /// Wakes the requests waiting (for the next token; for a read slot or out a 429, should the
    /// client be retired) when what they wait on changes: each waits on the channel current
    /// when it looked, which a change replaces, closing it ([`Self::wake_waits`]). Shared by
    /// every clone.
    waits: Arc<std::sync::Mutex<(smol::channel::Sender<()>, smol::channel::Receiver<()>)>>,
    /// The server's transport, one for every clone and for whatever else reaches the server's
    /// address ([`Self::transport`]): a target whose transport changed is reached through the
    /// new one by every holder ([`Self::set_http`]).
    http: Arc<Transport>,
    /// Why the client was stopped for good ([`Self::retire`]): no request is sent after.
    /// Shared by every clone.
    retired: Arc<std::sync::OnceLock<String>>,
    /// Shared by every clone, so the bound holds for the whole connection.
    read_slots: Arc<smol::lock::Semaphore>,
    /// How often the token was replaced by another ([`Self::credential_changes`]), and how often
    /// the server refused a request on each of its graphs with 403, by graph id
    /// ([`Self::refusals`]). Shared by every clone.
    credential_changes: Arc<std::sync::atomic::AtomicU64>,
    refusals: Arc<std::sync::Mutex<HashMap<String, u64>>>,
}

impl std::fmt::Debug for GraphClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the token.
        f.debug_struct("GraphClient")
            .field("base", &self.base.as_str())
            .finish()
    }
}

/// A request for [`GraphClient::forward`]. The caller drops the incoming request's own
/// `authorization`, contract and hop-by-hop headers; this client adds its own.
pub struct ForwardRequest {
    pub method: Method,
    pub path_and_query: String,
    pub headers: Vec<(String, String)>,
    pub body: AsyncBody,
}

/// What [`GraphClient::forward`] got back, with the body still streaming.
pub struct ForwardResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: AsyncBody,
}

impl GraphClient {
    /// `base` is the server root, for example `http://127.0.0.1:8787`.
    pub fn new(
        base: &str,
        token: Option<String>,
        http: Arc<dyn HttpClient>,
    ) -> anyhow::Result<Self> {
        let mut base = Url::parse(base)?;
        check_root(&base).map_err(anyhow::Error::msg)?;
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Ok(Self {
            base,
            token: Arc::new(std::sync::RwLock::new(HeldToken {
                value: held(token),
                ends: None,
                serial: 0,
            })),
            ended: Arc::default(),
            waits_begun: Arc::default(),
            waits_released: Arc::default(),
            waits: Arc::new(std::sync::Mutex::new(smol::channel::bounded(1))),
            http: Arc::new(Transport(std::sync::RwLock::new(http))),
            retired: Arc::default(),
            read_slots: Arc::new(smol::lock::Semaphore::new(READ_CONCURRENCY)),
            credential_changes: Arc::default(),
            refusals: Arc::default(),
        })
    }

    /// A count that moves whenever the token is replaced by another, the transport it is sent
    /// through ([`Self::set_http`]), or the client is retired ([`Self::retire`]): everything
    /// read with the old one, and what it was allowed to do, is to be read again with the new,
    /// which may see less, or (a new certificate authority) reach what the old one could not.
    pub fn credential_changes(&self) -> u64 {
        self.credential_changes
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A count that moves whenever the server refuses a request on graph `graph` with 403: the
    /// graph's policy may have changed, so what the credential may do there is to be read
    /// again. A refusal on another graph of the server says nothing of this one, whose answers
    /// stand. A 403 from the server's own routes is not counted: listing or discovering its
    /// graphs is a server action most tokens may not take, and says nothing about a graph.
    pub fn refusals(&self, graph: &str) -> u64 {
        self.refusals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(graph)
            .copied()
            .unwrap_or(0)
    }

    /// Counts a response's 403 to a request on a graph (`graphs/{id}/…` under the root) when
    /// the server sent it (it carries the contract Studio speaks), against the graph its path
    /// names.
    fn note_refusal(&self, url: &Url, status: StatusCode, headers: &HeaderMap) {
        let contract = headers
            .get(CONTRACT_HEADER)
            .and_then(|value| value.to_str().ok());
        if status != StatusCode::FORBIDDEN || !speaks_contract(contract) {
            return;
        }
        let below = url.path().strip_prefix(self.base.path()).unwrap_or("");
        let mut segments = below.split('/');
        if segments.next() != Some("graphs") {
            return;
        }
        // The id, then at least one segment of a graph's route under it.
        let (Some(id), Some(_)) = (segments.next(), segments.next()) else {
            return;
        };
        *self
            .refusals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(decode_segment(id))
            .or_default() += 1;
    }

    /// A request to the server as every one is made: with the contract this build speaks and
    /// this client's token, following no redirect, and the token it carries. No route answers
    /// with a redirect to follow; a Blob's external reference answers 302 with a URI Studio
    /// shows and never fetches.
    fn request(&self, method: Method, url: &Url) -> (http_client::Builder, Option<Arc<str>>) {
        let builder = Request::builder()
            .method(method)
            .uri(url.as_str())
            .header(CONTRACT_HEADER, CONTRACT)
            .follow_redirects(RedirectPolicy::NoFollow);
        let carried = self.token();
        let builder = match &carried {
            Some(token) => builder.header("authorization", format!("Bearer {token}")),
            None => builder,
        };
        (builder, carried)
    }

    /// Sends `request`, made by [`Self::request`] for `url`, and counts a refusal of it
    /// ([`Self::note_refusal`]).
    async fn dispatch(
        &self,
        url: &Url,
        request: Request<AsyncBody>,
    ) -> Result<http_client::Response<AsyncBody>, NoAnswer> {
        // A retired client sends nothing: each request fails with why it was retired, as given,
        // not read as a transport's failure.
        if let Some(why) = self.retired.get() {
            return Err(NoAnswer::Retired(why.clone()));
        }
        let response = self
            .http
            .current()
            .send(request)
            .await
            .map_err(|error| unanswered_by_transport(&error))?;
        self.note_refusal(url, response.status(), response.headers());
        Ok(response)
    }

    /// The server's transport as one client, for requests to the server's address that are not
    /// this client's (a sign-in's): each goes through the transport current when it is sent
    /// ([`Self::set_http`]). Retiring the client does not stop them, so a sign-in's last
    /// revocation still reaches its service.
    pub fn transport(&self) -> Arc<dyn HttpClient> {
        self.http.clone()
    }

    /// Replaces the server's transport for this client, every clone of it and every holder of
    /// [`Self::transport`], as when the CLI target's proxy or certificate authority changed;
    /// requests sent after this go through it, and one under way finishes through the one it
    /// started with.
    pub fn set_http(&self, http: Arc<dyn HttpClient>) {
        // One that sent through itself would recurse on the next request.
        assert!(
            !std::ptr::addr_eq(Arc::as_ptr(&http), Arc::as_ptr(&self.http)),
            "a transport that sends through itself"
        );
        *self
            .http
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = http;
        // What was read, or failed, through the old transport is read again through this one.
        self.credential_changes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Why the client was retired ([`Self::retire`]), once it was: it sends nothing.
    pub fn retired(&self) -> Option<&str> {
        self.retired.get().map(String::as_str)
    }

    /// Stops the client and every clone of it for good, as when its CLI target is gone or names
    /// a transport Studio cannot use: every request after fails with `why`, and nothing is sent,
    /// a request waiting for the next token, a read slot or out a 429 included, at once. What
    /// was read is read again, and fails so. The first reason given stands.
    pub fn retire(&self, why: String) {
        if self.retired.set(why).is_ok() {
            self.credential_changes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.wake_waits();
        }
    }

    /// Replaces the token for this client and every clone of it; requests sent after this use
    /// it. Whether it differs from the token it replaced, which counts as a credential change.
    pub fn set_token(&self, token: Option<String>) -> bool {
        self.set_token_ending(token, None)
    }

    /// [`Self::set_token`], with when the token stops being accepted, as the lease it came with
    /// says (`None` for one not known to end), set with it, so no request reads one without the
    /// other. A request that would send it after then waits for the next
    /// ([`Self::token_ended`]).
    pub fn set_token_ending(&self, token: Option<String>, ends: Option<SystemTime>) -> bool {
        let token = held(token);
        let changed = {
            let mut held = self.held_mut();
            let changed = held.value != token;
            let serial = held.serial + u64::from(changed);
            *held = HeldToken {
                value: token,
                ends,
                serial,
            };
            changed
        };
        self.wake_waits();
        if changed {
            self.credential_changes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        changed
    }

    /// Replaces the token with a renewal of it: the same account's next short-lived token, as
    /// an OAuth refresh gives (spec 016). Nothing read with the old one is read again, so it is
    /// not counted as another token ([`Self::credential_changes`]). Whether it differs from the
    /// token it replaced.
    pub fn renew_token(&self, token: String, ends: Option<SystemTime>) -> bool {
        let token: Arc<str> = token.into();
        let renewed = {
            let mut held = self.held_mut();
            let renewed = held.value.as_deref() != Some(&*token);
            let serial = held.serial + u64::from(renewed);
            *held = HeldToken {
                value: Some(token),
                ends,
                serial,
            };
            renewed
        };
        self.wake_waits();
        renewed
    }

    fn held_mut(&self) -> std::sync::RwLockWriteGuard<'_, HeldToken> {
        self.token
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// When the token held stops being accepted, for a test: as a sleep past its renewal would
    /// leave it, or `None` for one that does not end. A token is given its end with it
    /// ([`Self::set_token_ending`], [`Self::renew_token`]).
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_token_ends(&self, ends: Option<SystemTime>) {
        self.held_mut().ends = ends;
        self.wake_waits();
    }

    /// Told when a request finds the token held ended, as when the machine slept past its
    /// renewal, and again each second it waits: the holder asks for the next, which the request
    /// waits for, a while ([`ENDED_TOKEN_WAIT`]); or releases the requests waiting
    /// ([`Self::release_waits`]). One holder listens; with none listening (never, or no
    /// longer), a request sends what it holds.
    pub fn token_ended(&self) -> smol::channel::Receiver<()> {
        let (tell, told) = smol::channel::bounded(1);
        *self
            .ended
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(tell);
        told
    }

    /// The number of the last request to begin waiting for the next token: those up to it are
    /// the ones waiting when the holder looked.
    pub fn waits_begun(&self) -> u64 {
        self.waits_begun.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The holder will give no next token for the requests waiting up to `through`
    /// ([`Self::waits_begun`] when it decided so, as when its server could not be reached then):
    /// they send what is held. One that began waiting after waits on, and the end stands, so a
    /// request after this tells the holder again.
    pub fn release_waits(&self, through: u64) {
        self.waits_released
            .fetch_max(through, std::sync::atomic::Ordering::SeqCst);
        self.wake_waits();
    }

    /// The channel a wait sleeps on, which [`Self::wake_waits`] closes. Taken before the wait
    /// looks at what it waits for, so a change made after the look wakes it.
    fn wake_channel(&self) -> smol::channel::Receiver<()> {
        self.waits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .1
            .clone()
    }

    /// Wakes the requests waiting (for the next token, for a read slot, out a 429) to look
    /// again, once what they wait on changed: the channel each waits on closes as this replaces
    /// it.
    fn wake_waits(&self) {
        *self
            .waits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = smol::channel::bounded(1);
    }

    /// Names the token held: it moves each time the token is replaced by another, a renewal
    /// included, and with nothing else (a transport change or a retire leaves it).
    pub fn token_serial(&self) -> u64 {
        self.token
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .serial
    }

    /// Whether a token is held, so the next request sends one.
    pub fn holds_token(&self) -> bool {
        self.token
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .value
            .is_some()
    }

    /// Whether the token held has ended, as its lease says. One in its last moments is sent: its
    /// renewal came before, and a refusal of it asks again.
    pub fn holds_ended_token(&self) -> bool {
        let held = self
            .token
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        held.value.is_some() && held.ends.is_some_and(|ends| SystemTime::now() >= ends)
    }

    /// Waits, while the token held has ended, a holder listens, and for at most
    /// [`ENDED_TOKEN_WAIT`], for the next, telling the holder ([`Self::token_ended`]) at once
    /// and each second after; a request then carries whatever it holds, which a server that
    /// refuses it says so as before. A change to the token, its end, a release or the client
    /// retired wakes it ([`Self::wake_waits`]); a holder gone is noticed at the next tell.
    /// Whether the token held is live as it returns.
    async fn fresh_token(&self) -> bool {
        if !self.holds_ended_token() {
            return true;
        }
        let until = Instant::now() + ENDED_TOKEN_WAIT;
        // Numbered before it tells, so a release the holder decides on its tell covers it.
        let number = self
            .waits_begun
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let released = || {
            self.waits_released
                .load(std::sync::atomic::Ordering::SeqCst)
                >= number
        };
        let mut told: Option<Instant> = None;
        loop {
            let woken = self.wake_channel();
            if !self.holds_ended_token() {
                return true;
            }
            if Instant::now() >= until || released() {
                return false;
            }
            let Some(holder) = self.listening_holder() else {
                return false;
            };
            let now = Instant::now();
            let told_at = match told {
                Some(at) if now < at + ENDED_TOKEN_TELL => at,
                _ => {
                    holder.try_send(()).ok();
                    now
                }
            };
            told = Some(told_at);
            let next = (told_at + ENDED_TOKEN_TELL).min(until);
            smol::future::or(
                async {
                    woken.recv().await.ok();
                },
                async {
                    smol::Timer::at(next).await;
                },
            )
            .await;
        }
    }

    /// Why the client was retired, once it is ([`Self::retire`], which wakes the waits).
    async fn retired_now(&self) -> String {
        loop {
            let woken = self.wake_channel();
            if let Some(why) = self.retired.get() {
                return why.clone();
            }
            woken.recv().await.ok();
        }
    }

    /// The holder told when a request finds the token held ended, while one listens and the
    /// client is not retired: whom a wait for the next token can be answered by.
    fn listening_holder(&self) -> Option<smol::channel::Sender<()>> {
        if self.retired.get().is_some() {
            return None;
        }
        self.ended
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .filter(|holder| holder.receiver_count() > 0)
    }

    /// When the token held stops being accepted, for a test.
    #[cfg(any(test, feature = "test-support"))]
    pub fn token_ends(&self) -> Option<SystemTime> {
        self.token
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ends
    }

    fn token(&self) -> Option<Arc<str>> {
        self.token
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .value
            .clone()
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = self.base.clone();
        // `new` admits only http(s) roots with a host, which always take path segments.
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        url
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        headers: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<RawResponse, NoAnswer> {
        self.fresh_token().await;
        self.send_now(
            method,
            url,
            body,
            headers,
            timeout,
            BodyLimit::First(READ_ANSWER_MAX),
        )
        .await
    }

    /// A `GET` of a public route (`/healthz`, `/readyz`), which takes no token, so never waits
    /// for one ([`Self::fresh_token`]).
    async fn send_public(&self, url: Url) -> Result<RawResponse, String> {
        self.send_now(
            Method::GET,
            url,
            None,
            &[],
            HEALTH_TIMEOUT,
            BodyLimit::First(READ_ANSWER_MAX),
        )
        .await
        .map_err(String::from)
    }

    /// Sends with the token held now, keeping only what `limit` says of the body: the rest is
    /// never read. A caller sending a token waits for a fresh one first ([`Self::fresh_token`]),
    /// once per attempt, and a read again should its token end while it queued for a slot and a
    /// wait can be answered; the public routes take none.
    async fn send_now(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        headers: &[(&str, &str)],
        timeout: Duration,
        limit: BodyLimit,
    ) -> Result<RawResponse, NoAnswer> {
        let head = method == Method::HEAD;
        let (builder, carried) = self.request(method, &url);
        let mut builder = builder.timeout(timeout);
        // A body is JSON unless the caller names its type: a Blob put's is raw bytes.
        if body.is_some()
            && !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        {
            builder = builder.header("content-type", "application/json");
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(body.map(AsyncBody::from).unwrap_or_else(AsyncBody::empty))
            .map_err(|error| NoAnswer::Unsendable(error.to_string()))?;
        let response = self.dispatch(&url, request).await?;
        let status = response.status();
        let contract = response
            .headers()
            .get(CONTRACT_HEADER)
            .and_then(|value: &HeaderValue| value.to_str().ok())
            .map(str::to_owned);
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value: &HeaderValue| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let headers = response.headers().clone();
        let (skip, keep) = match limit {
            // An answer that is not the value is read from its start, so its message reads
            // whole however little of the value was asked for.
            BodyLimit::First(keep) | BodyLimit::Range { len: keep, .. } if !status.is_success() => {
                (0, keep.max(ERROR_BODY_BYTES))
            }
            BodyLimit::First(keep) => (0, keep),
            BodyLimit::Range { start, len } if status == StatusCode::OK => (start, len),
            BodyLimit::Range { len, .. } => (0, len),
        };
        let length = body_length(head, status, &headers);
        let bytes = read_body(response.into_body(), skip, keep, length)
            .await
            .map_err(|error| NoAnswer::Lost(describe_transport(&error.into())))?;
        Ok(RawResponse {
            carried,
            status,
            contract,
            retry_after,
            headers,
            body: bytes,
        })
    }

    /// Sends a read once one of the connection's read slots is free, waiting up to `timeout`
    /// for its answer; one refused with 429 waits as asked, without its slot, and goes again. A
    /// retire ends the wait for a slot or out a 429 at once.
    async fn fetch(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        headers: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<RawResponse, ReadError> {
        self.fetch_reading(
            method,
            url,
            body,
            headers,
            BodyLimit::First(READ_ANSWER_MAX),
            timeout,
        )
        .await
    }

    /// [`Self::fetch`], keeping only what `limit` says of the body.
    async fn fetch_reading(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        headers: &[(&str, &str)],
        limit: BodyLimit,
        timeout: Duration,
    ) -> Result<RawResponse, ReadError> {
        let mut retries = 0;
        let mut read_again = false;
        loop {
            // Before a slot: reads queued for one join the wait for the next token, so one renewal
            // or check of a server away serves them all, not a slot's worth in turn.
            let live = self.fresh_token().await;
            let raw = {
                // Refused at once if the client is retired while the read queues, not when a slot
                // frees behind reads still under way.
                let slot = smol::future::or(async { Ok(self.read_slots.acquire().await) }, async {
                    Err(self.retired_now().await)
                })
                .await;
                let _slot = slot.map_err(ReadError::Transport)?;
                // A token live when the read queued that ended while it did (a sleep across a long
                // queue) is waited for again, the slot let go meanwhile, while a holder listens
                // to answer the wait ([`Self::listening_holder`]); one that cannot give the next
                // releases it at once, and the read queues once more.
                if live && self.holds_ended_token() && self.listening_holder().is_some() {
                    continue;
                }
                self.send_now(
                    method.clone(),
                    url.clone(),
                    body.clone(),
                    headers,
                    timeout,
                    limit,
                )
                .await
                .map_err(|no| ReadError::Transport(no.into()))?
            };
            // A read changes nothing, so a rate-limited one waits as asked and goes again.
            if raw.status == StatusCode::TOO_MANY_REQUESTS && retries < READ_RETRIES {
                let wait = raw.retry_after.unwrap_or(DEFAULT_RETRY_AFTER);
                if wait <= MAX_RETRY_AFTER {
                    retries += 1;
                    // Cut short by a retire, whose refusal the next attempt meets.
                    smol::future::or(
                        async {
                            smol::Timer::after(wait).await;
                        },
                        async {
                            self.retired_now().await;
                        },
                    )
                    .await;
                    continue;
                }
            }
            // Refused for a token replaced while the read was under way (a renewal, a refresh
            // after another read's refusal): read again, once, with the one held now, rather
            // than say a refusal of a token already gone, which would ask for yet another.
            if raw.status == StatusCode::UNAUTHORIZED && !read_again && raw.carried != self.token()
            {
                read_again = true;
                continue;
            }
            return Ok(raw);
        }
    }

    /// [`Self::read_capped`] at [`READ_ANSWER_MAX`].
    async fn read<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<T, ReadError> {
        self.read_capped(method, segments, query, body, READ_ANSWER_MAX)
            .await
    }

    /// [`Self::read`] of changes: a `GET` that waits up to [`CHANGE_READ_TIMEOUT`].
    async fn read_changes<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<T, ReadError> {
        self.read_within(
            Method::GET,
            segments,
            query,
            None,
            READ_ANSWER_MAX,
            CHANGE_READ_TIMEOUT,
        )
        .await
    }

    /// [`Self::read`] of an answer of at most `cap` bytes. One byte past `cap` is read, to know
    /// the answer goes on, and none after it: a longer answer is [`ReadError::TooLarge`], never
    /// a part of one decoded as if it were whole.
    async fn read_capped<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
        cap: usize,
    ) -> Result<T, ReadError> {
        self.read_within(method, segments, query, body, cap, READ_TIMEOUT)
            .await
    }

    /// [`Self::read_capped`], waiting up to `timeout` for the answer.
    async fn read_within<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
        cap: usize,
        timeout: Duration,
    ) -> Result<T, ReadError> {
        let url = self.url(segments, query);
        let limit = BodyLimit::First(cap.saturating_add(1));
        let raw = self
            .fetch_reading(method, url, body, &[], limit, timeout)
            .await?;
        if !raw.status.is_success() {
            return Err(refusal(&raw));
        }
        check_contract(&raw)?;
        if raw.body.len() > cap {
            return Err(ReadError::TooLarge { limit: cap });
        }
        serde_json::from_slice(&raw.body).map_err(|error| ReadError::Decode(error.to_string()))
    }

    fn blob_url(&self, graph: &str, cell: &BlobCell, at: BlobAt<'_>) -> Url {
        let entity = if cell.edge { "edge" } else { "node" };
        let (key, value) = match at {
            BlobAt::Branch(branch) => ("branch", branch),
            BlobAt::Snapshot(commit) => ("snapshot", commit),
        };
        self.url(
            &["graphs", graph, "blob"],
            &[
                ("entity", entity),
                ("type", &cell.type_name),
                ("id", &cell.id),
                ("property", &cell.property),
                (key, value),
            ],
        )
    }

    /// `HEAD /graphs/{id}/blob`: what a cell holds, without its bytes. The server answers a null
    /// cell and a missing row alike with 404; callers ask about rows they have read, so a 404
    /// is null. Every answer must carry the contract header: a 404 or a redirect from something
    /// between Studio and the server says nothing about the cell, and a write is bound by it.
    pub async fn blob_head(
        &self,
        graph: &str,
        cell: &BlobCell,
        at: BlobAt<'_>,
    ) -> Result<BlobMeta, ReadError> {
        let url = self.blob_url(graph, cell, at);
        let raw = self
            .fetch(Method::HEAD, url, None, &[], READ_TIMEOUT)
            .await?;
        let header = |name: &str| {
            raw.headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        match raw.status.as_u16() {
            200 => {
                check_contract(&raw)?;
                let size = header("content-length")
                    .and_then(|length| length.parse().ok())
                    .ok_or_else(|| ReadError::Decode("a Blob without a length".into()))?;
                Ok(BlobMeta::Managed {
                    size,
                    etag: header("etag"),
                })
            }
            301 | 302 | 303 | 307 | 308 => {
                check_contract(&raw)?;
                match header("location") {
                    Some(uri) => Ok(BlobMeta::External { uri }),
                    None => Err(ReadError::Decode("a redirect without a location".into())),
                }
            }
            404 => {
                check_contract(&raw)?;
                Ok(BlobMeta::Null)
            }
            _ => Err(refusal(&raw)),
        }
    }

    /// `GET /graphs/{id}/blob` with a `Range`: bytes `range` of a managed value, at most 16 MiB
    /// (`BLOB_READ_MAX`). An external reference is refused; Studio never fetches it.
    pub async fn blob_bytes(
        &self,
        graph: &str,
        cell: &BlobCell,
        at: BlobAt<'_>,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<u8>, ReadError> {
        let end = range.end.min(range.start + BLOB_READ_MAX);
        if end <= range.start {
            return Ok(Vec::new());
        }
        let url = self.blob_url(graph, cell, at);
        let bytes = format!("bytes={}-{}", range.start, end - 1);
        // A 206 is the range alone. A server that ignores Range sends the whole value, read
        // through the range's start without keeping it; what follows the range is never read.
        let len = (end - range.start) as usize;
        let raw = self
            .fetch_reading(
                Method::GET,
                url,
                None,
                &[("range", &bytes)],
                BodyLimit::Range {
                    start: range.start,
                    len,
                },
                READ_TIMEOUT,
            )
            .await?;
        match raw.status.as_u16() {
            200 => {
                check_contract(&raw)?;
                Ok(raw.body)
            }
            206 => {
                check_contract(&raw)?;
                // Bytes from elsewhere in the value would show as the ones asked for.
                let served = raw
                    .headers
                    .get("content-range")
                    .and_then(|value| value.to_str().ok());
                if !served.is_some_and(|served| content_range_starts_at(served, range.start)) {
                    return Err(ReadError::Decode(format!(
                        "the server sent a range other than the one asked for ({})",
                        served.unwrap_or("no Content-Range")
                    )));
                }
                let mut body = raw.body;
                body.truncate(len);
                Ok(body)
            }
            301 | 302 | 303 | 307 | 308 => {
                check_contract(&raw)?;
                Err(ReadError::Refused(ApiErrorSummary {
                    status: raw.status.as_u16(),
                    message: "The value is a reference to an object outside the graph; Studio \
                              does not fetch it."
                        .into(),
                    code: None,
                    diagnostic: None,
                    change_diff_refusal: None,
                    challenge: None,
                }))
            }
            _ => Err(refusal(&raw)),
        }
    }

    /// `PUT /graphs/{id}/blob`: stores `bytes` as the cell's value on `branch`, sent once like
    /// every write. `if_match` binds it to the value it replaces: that value's ETag, or `*` for
    /// any value at all. A cell read as null has no validator, so its put goes unbound. When the
    /// binding no longer holds the server changes nothing and refuses with 412, its
    /// `blob_precondition_failure` naming the cell's current ETag. Once the server admits the
    /// write it finishes it: a response lost after that leaves the outcome unknown.
    pub async fn blob_put(
        &self,
        graph: &str,
        branch: &str,
        cell: &BlobCell,
        if_match: Option<&str>,
        bytes: Vec<u8>,
    ) -> WriteOutcome<BlobWriteOutput> {
        let url = self.blob_url(graph, cell, BlobAt::Branch(branch));
        let mut headers = vec![("content-type", "application/octet-stream")];
        headers.extend(if_match.map(|tag| ("if-match", tag)));
        self.send_blob_write(Method::PUT, url, Some(bytes), &headers)
            .await
    }

    /// `DELETE /graphs/{id}/blob`: sets a nullable cell to null on `branch`, bound by `if_match`
    /// as [`Self::blob_put`] is. A cell that was already null publishes nothing: its receipt has
    /// no commit.
    pub async fn blob_clear(
        &self,
        graph: &str,
        branch: &str,
        cell: &BlobCell,
        if_match: Option<&str>,
    ) -> WriteOutcome<BlobWriteOutput> {
        let url = self.blob_url(graph, cell, BlobAt::Branch(branch));
        let headers: Vec<(&str, &str)> =
            if_match.map(|tag| ("if-match", tag)).into_iter().collect();
        self.send_blob_write(Method::DELETE, url, None, &headers)
            .await
    }

    async fn send_blob_write(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        headers: &[(&str, &str)],
    ) -> WriteOutcome<BlobWriteOutput> {
        match self.send(method, url, body, headers, WRITE_TIMEOUT).await {
            Ok(raw) => classify_write(raw),
            Err(no) => unanswered(no),
        }
    }

    async fn write<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
        headers: &[(&str, &str)],
    ) -> WriteOutcome<T> {
        let url = self.url(segments, query);
        let raw = match self.send(method, url, body, headers, WRITE_TIMEOUT).await {
            Ok(raw) => raw,
            Err(no) => return unanswered(no),
        };
        classify_write(raw)
    }

    // ---- public reads ----

    /// Sends a request on as it came (spec 013): the agent's CLI speaks to Studio's gateway,
    /// which forwards here with this client's token and contract header. Bodies stream both
    /// ways and there is no total timeout, as the CLI sets none; a write is sent once and never
    /// retried (constitution III). `path_and_query` is below the server root, without a leading
    /// slash; a path that would leave the root is refused.
    pub async fn forward(&self, request: ForwardRequest) -> Result<ForwardResponse, NoAnswer> {
        let path = request
            .path_and_query
            .split(['?', '#'])
            .next()
            .unwrap_or("");
        if request.path_and_query.starts_with('/')
            || request.path_and_query.contains("://")
            || path
                .split('/')
                .any(|segment| segment == ".." || segment == ".")
            || path.contains('\\')
        {
            return Err(NoAnswer::Unsendable(format!(
                "refused path {:?}",
                request.path_and_query
            )));
        }
        let url = self
            .base
            .join(&request.path_and_query)
            .map_err(|error| NoAnswer::Unsendable(error.to_string()))?;
        // The server receives exactly the path judged here: a join that resolves `%2e%2e`,
        // drops a tab or encodes a byte would send another.
        if !url.as_str().starts_with(self.base.as_str())
            || url.path() != format!("{}{path}", self.base.path())
        {
            return Err(NoAnswer::Unsendable(format!(
                "refused path {:?}",
                request.path_and_query
            )));
        }
        self.fresh_token().await;
        let (mut builder, _) = self.request(request.method, &url);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = builder
            .body(request.body)
            .map_err(|error| NoAnswer::Unsendable(error.to_string()))?;
        let response = self.dispatch(&url, request).await?;
        Ok(ForwardResponse {
            status: response.status(),
            headers: response.headers().clone(),
            body: response.into_body(),
        })
    }

    /// `GET /healthz`. Public; used to discover the server's contract, so a success is returned
    /// with whatever contract it names, for the caller to judge.
    pub async fn health(&self) -> Result<ServerHealth, ReadError> {
        let url = self.url(&["healthz"], &[]);
        let raw = self.send_public(url).await.map_err(ReadError::Transport)?;
        if !raw.status.is_success() {
            return Err(refusal(&raw));
        }
        let health =
            serde_json::from_slice(&raw.body).map_err(|e| ReadError::Decode(e.to_string()))?;
        Ok(ServerHealth {
            contract: raw.contract,
            health,
        })
    }

    /// `GET /readyz`. Public. A 503 still carries a readiness body, and is the server's only
    /// with the contract header, as a 200 is.
    pub async fn readiness(&self) -> Result<ReadinessOutput, ReadError> {
        let url = self.url(&["readyz"], &[]);
        let raw = self.send_public(url).await.map_err(ReadError::Transport)?;
        if !raw.status.is_success() && raw.status != StatusCode::SERVICE_UNAVAILABLE {
            return Err(refusal(&raw));
        }
        check_contract(&raw)?;
        serde_json::from_slice(&raw.body).map_err(|e| ReadError::Decode(e.to_string()))
    }

    // ---- protected reads ----

    pub async fn graphs(&self) -> Result<GraphListResponse, ReadError> {
        self.read(Method::GET, &["graphs"], &[], None).await
    }

    /// The graphs this credential may know of, for one that may not list the server's
    /// (`graph_list` is a server policy): an identity credential's, by id only.
    pub async fn discover_graphs(&self) -> Result<crate::api::GraphDiscoveryResponse, ReadError> {
        self.read(Method::GET, &["graphs", "discovery"], &[], None)
            .await
    }

    pub async fn schema(&self, graph: &str) -> Result<SchemaOutput, ReadError> {
        self.read(Method::GET, &["graphs", graph, "schema"], &[], None)
            .await
    }

    pub async fn snapshot(&self, graph: &str, branch: &str) -> Result<SnapshotOutput, ReadError> {
        self.read(
            Method::GET,
            &["graphs", graph, "snapshot"],
            &[("branch", branch)],
            None,
        )
        .await
    }

    /// `POST /graphs/{id}/query`, its answer read up to [`QUERY_ANSWER_MAX`] bytes.
    pub async fn query(
        &self,
        graph: &str,
        request: &QueryRequest,
    ) -> Result<ReadOutput, ReadError> {
        self.read_capped(
            Method::POST,
            &["graphs", graph, "query"],
            &[],
            Some(json(request)),
            QUERY_ANSWER_MAX,
        )
        .await
    }

    /// The graph's stored queries (`GET /graphs/{id}/queries`, gated by `read`).
    pub async fn stored_queries(&self, graph: &str) -> Result<QueriesCatalogOutput, ReadError> {
        self.read(Method::GET, &["graphs", graph, "queries"], &[], None)
            .await
    }

    /// Runs a stored read, its answer read up to [`QUERY_ANSWER_MAX`] bytes as a query's is.
    /// `request.expect_mutation` is set to `false`, so a stored query that writes is refused by
    /// the server rather than run as a read.
    pub async fn invoke_stored_read(
        &self,
        graph: &str,
        name: &str,
        request: &InvokeStoredQueryRequest,
    ) -> Result<ReadOutput, ReadError> {
        let request = InvokeStoredQueryRequest {
            expect_mutation: Some(false),
            ..request.clone()
        };
        self.read_capped(
            Method::POST,
            &["graphs", graph, "queries", name],
            &[],
            Some(json(&request)),
            QUERY_ANSWER_MAX,
        )
        .await
    }

    /// Runs a stored mutation on `request.branch`, sent once like every write. With `if_commit`
    /// it lands only while the branch head is that commit.
    pub async fn invoke_stored_write(
        &self,
        graph: &str,
        name: &str,
        request: &InvokeStoredQueryRequest,
        if_commit: Option<&str>,
    ) -> WriteOutcome<ChangeOutput> {
        let request = InvokeStoredQueryRequest {
            expect_mutation: Some(true),
            snapshot: None,
            ..request.clone()
        };
        self.write_bound(
            &["graphs", graph, "queries", name],
            json(&request),
            if_commit,
        )
        .await
    }

    /// `GET /graphs/{id}/access`: what this client's credential may do on the graph, and with
    /// `branch` on that branch; with `target_branch` as well, also creating it from `branch` and
    /// merging `branch` into it. A target without a branch is not asked about. The answer names
    /// actions, never the rules that allow them, and reads no graph data.
    pub async fn access(
        &self,
        graph: &str,
        branch: Option<&str>,
        target_branch: Option<&str>,
    ) -> Result<AccessOutput, ReadError> {
        let mut query = Vec::new();
        if let Some(branch) = branch {
            query.push(("branch", branch));
            if let Some(target) = target_branch {
                query.push(("target_branch", target));
            }
        }
        self.read(Method::GET, &["graphs", graph, "access"], &query, None)
            .await
    }

    pub async fn branches(&self, graph: &str) -> Result<BranchListOutput, ReadError> {
        self.read(Method::GET, &["graphs", graph, "branches"], &[], None)
            .await
    }

    /// A page of `branch`'s history, newest first: its newest `limit` commits, or from
    /// `page_token` the next ones. The page names the head it resolved and where it goes on.
    /// The one way Studio reads a history: without `limit` the server answers the whole of it,
    /// every commit since the graph began, in one body.
    pub async fn commit_page(
        &self,
        graph: &str,
        branch: &str,
        limit: u32,
        page_token: Option<&str>,
    ) -> Result<CommitListOutput, ReadError> {
        let limit = limit.to_string();
        let mut query = vec![("branch", branch), ("limit", limit.as_str())];
        if let Some(token) = page_token {
            query.push(("page_token", token));
        }
        self.read(Method::GET, &["graphs", graph, "commits"], &query, None)
            .await
    }

    /// One commit by its id, whichever branch's history holds it: its parents and creation
    /// metadata, which never change.
    pub async fn commit(
        &self,
        graph: &str,
        commit_id: &str,
    ) -> Result<crate::api::CommitOutput, ReadError> {
        self.read(
            Method::GET,
            &["graphs", graph, "commits", commit_id],
            &[],
            None,
        )
        .await
    }

    /// One page of a commit's changes against its first parent.
    pub async fn commit_changes(
        &self,
        graph: &str,
        commit_id: &str,
        page_token: Option<&str>,
        limit: u32,
    ) -> Result<CommitChanges, ReadError> {
        let limit = limit.to_string();
        let mut query = vec![("limit", limit.as_str())];
        if let Some(token) = page_token {
            query.push(("page_token", token));
        }
        self.read_changes(&["graphs", graph, "commits", commit_id, "changes"], &query)
            .await
    }

    /// `GET /graphs/{id}/diff`: one page of the net changes from commit `from` to commit
    /// `to`; pass `page_token` from the previous page to continue.
    pub async fn diff(
        &self,
        graph: &str,
        from: &str,
        to: &str,
        page_token: Option<&str>,
        limit: u32,
    ) -> Result<DiffOutput, ReadError> {
        let limit = limit.to_string();
        let mut query = vec![("from", from), ("to", to), ("limit", limit.as_str())];
        if let Some(token) = page_token {
            query.push(("page_token", token));
        }
        self.read_changes(&["graphs", graph, "diff"], &query).await
    }

    /// `GET /graphs/{id}/branches/merge-base`: what a merge of `source` into `target` would
    /// classify against, with both heads.
    pub async fn merge_base(
        &self,
        graph: &str,
        source: &str,
        target: &str,
    ) -> Result<MergeBaseOutput, ReadError> {
        self.read(
            Method::GET,
            &["graphs", graph, "branches", "merge-base"],
            &[("source", source), ("target", target)],
            None,
        )
        .await
    }

    /// `POST /graphs/{id}/branches/merge` with `dry_run`: what merging `source` into `target`
    /// would do now, or the conflicts that would stop it. Nothing is written; the server reads
    /// both branches, without the merge's write gate or its grant.
    pub async fn merge_preview(
        &self,
        graph: &str,
        source: &str,
        target: &str,
    ) -> Result<MergePreview, ReadError> {
        let url = self.url(&["graphs", graph, "branches", "merge"], &[]);
        let request = BranchMergeRequest {
            source: source.to_owned(),
            target: Some(target.to_owned()),
            delete_branch: false,
            dry_run: true,
        };
        let raw = self
            .fetch(
                Method::POST,
                url,
                Some(json(&request)),
                &[],
                CHANGE_READ_TIMEOUT,
            )
            .await?;
        check_contract(&raw)?;
        if raw.status.is_success() {
            let output: BranchMergeOutput = serde_json::from_slice(&raw.body)
                .map_err(|error| ReadError::Decode(error.to_string()))?;
            return Ok(MergePreview::Merges(output.outcome));
        }
        if raw.status == StatusCode::CONFLICT
            && let Ok(error) = serde_json::from_slice::<ErrorOutput>(&raw.body)
            && let Some(conflicts) = error.merge_conflicts.filter(|c| !c.is_empty())
        {
            return Ok(MergePreview::Conflicts(conflicts));
        }
        Err(refusal(&raw))
    }

    /// One page of a branch's change feed, from `at`.
    pub async fn changes(
        &self,
        graph: &str,
        branch: &str,
        at: FeedAt<'_>,
        types: &[String],
        limit: u32,
    ) -> Result<ChangeFeedOutput, ReadError> {
        let limit = limit.to_string();
        let mut query = vec![("branch", branch), ("limit", limit.as_str())];
        query.push(match at {
            FeedAt::Start(start) => ("start", start),
            FeedAt::Cursor(cursor) => ("cursor", cursor),
            FeedAt::Page(token) => ("page_token", token),
        });
        query.extend(types.iter().map(|name| ("type", name.as_str())));
        self.read_changes(&["graphs", graph, "changes"], &query)
            .await
    }

    // ---- writes ----

    /// `POST /mutate`, or `/mutate/if-graph-commit` when `if_commit` is set.
    pub async fn mutate(
        &self,
        graph: &str,
        request: &ChangeRequest,
        if_commit: Option<&str>,
    ) -> WriteOutcome<ChangeOutput> {
        self.write_bound(&["graphs", graph, "mutate"], json(request), if_commit)
            .await
    }

    /// A `POST` to `segments`, or, bound to `if_commit`, to its `if-graph-commit` route with
    /// the commit in the header: it changes nothing unless the branch is still at that commit.
    async fn write_bound<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        body: Vec<u8>,
        if_commit: Option<&str>,
    ) -> WriteOutcome<T> {
        match if_commit {
            Some(commit) => {
                let bound: Vec<&str> = segments
                    .iter()
                    .copied()
                    .chain(["if-graph-commit"])
                    .collect();
                self.write(
                    Method::POST,
                    &bound,
                    &[],
                    Some(body),
                    &[(IF_GRAPH_COMMIT_HEADER, commit)],
                )
                .await
            }
            None => {
                self.write(Method::POST, segments, &[], Some(body), &[])
                    .await
            }
        }
    }

    pub async fn create_branch(
        &self,
        graph: &str,
        request: &BranchCreateRequest,
    ) -> WriteOutcome<BranchCreateOutput> {
        self.write(
            Method::POST,
            &["graphs", graph, "branches"],
            &[],
            Some(json(request)),
            &[],
        )
        .await
    }

    pub async fn delete_branch(
        &self,
        graph: &str,
        branch: &str,
    ) -> WriteOutcome<BranchDeleteOutput> {
        self.write(
            Method::DELETE,
            &["graphs", graph, "branches", branch],
            &[],
            None,
            &[],
        )
        .await
    }

    pub async fn merge(
        &self,
        graph: &str,
        request: &BranchMergeRequest,
    ) -> WriteOutcome<BranchMergeOutput> {
        self.write(
            Method::POST,
            &["graphs", graph, "branches", "merge"],
            &[],
            Some(json(request)),
            &[],
        )
        .await
    }
}

/// What a merge would do now ([`GraphClient::merge_preview`]).
#[derive(Debug, Clone)]
pub enum MergePreview {
    Merges(crate::api::BranchMergeOutcome),
    /// The conflicts that would stop it, as a merge would report them.
    Conflicts(Vec<crate::api::MergeConflictOutput>),
}

fn json<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("request types serialize")
}

/// Whether a read's answer is the server's: it carries the contract Studio speaks. Omnigraph
/// stamps the header on every response it sends, errors included, as [`judge_write`] relies on
/// too. A success without it is a server that does not say which contract it speaks. An error
/// without it came from something between Studio and the server (a proxy, a gateway): it says
/// nothing of the server or the graph, so it is no refusal, and the read fails as one that did
/// not reach the server. An answer naming another contract is a server Studio cannot read.
fn check_contract(raw: &RawResponse) -> Result<(), ReadError> {
    match raw.contract.as_deref() {
        served if speaks_contract(served) => Ok(()),
        None if !raw.status.is_success() => Err(ReadError::Transport(format!(
            "something between Studio and the server answered {}",
            raw.status
        ))),
        served => Err(ReadError::ContractMismatch {
            served: served.map(str::to_owned),
        }),
    }
}

/// An answer that is not a success, as a read's error: the server's refusal only when the
/// server sent it ([`check_contract`]).
fn refusal(raw: &RawResponse) -> ReadError {
    match check_contract(raw) {
        Ok(()) => ReadError::Refused(summary(raw)),
        Err(error) => error,
    }
}

/// A refusal's summary: its body, and a 401's challenge.
fn summary(raw: &RawResponse) -> ApiErrorSummary {
    let status = raw.status;
    let challenge = (status == StatusCode::UNAUTHORIZED)
        .then(|| Challenge::bearer(&raw.headers))
        .flatten()
        .map(Box::new);
    match serde_json::from_slice::<ErrorOutput>(&raw.body) {
        Ok(error) => ApiErrorSummary {
            status: status.as_u16(),
            message: error.error,
            code: error.code,
            diagnostic: error.diagnostic,
            change_diff_refusal: error.change_diff_refusal.map(Box::new),
            challenge,
        },
        Err(_) => ApiErrorSummary {
            status: status.as_u16(),
            message: status
                .canonical_reason()
                .unwrap_or("Request failed")
                .to_owned(),
            code: None,
            diagnostic: None,
            change_diff_refusal: None,
            challenge,
        },
    }
}

/// What a write's answer says happened (constitution III, spec 003), its success body not yet
/// read as any type: Studio's own writes and the agent's, forwarded by its gateway, are judged
/// alike.
///
/// - 2xx with the contract header: committed, with the body.
/// - An error with the contract header and the server's error body that states its
///   disposition (every 0.14.0 server does): the disposition decides. Effects `none` is a
///   refusal whatever the status, a 500 included; `unknown` or `partial` is unknown whatever
///   the status, a 503 included, since some or all of the write may have happened.
/// - Such an error without a disposition (a server before Omnigraph 0.14.0), or with an effect
///   this build does not know: unknown, since nothing rules the change out.
/// - Anything else (a lost contract header, an error without the server's body): unknown.
///   Omnigraph stamps the header on every response it sends, errors included, so an error
///   without it came from something in between (a proxy, a gateway), which cannot say whether
///   the server applied the write.
pub fn judge_write<'a>(
    status: StatusCode,
    contract: Option<&str>,
    body: &'a [u8],
) -> Answered<&'a [u8]> {
    let contract_ok = speaks_contract(contract);
    if status.is_success() {
        if !contract_ok {
            return Answered::Unknown {
                reason: "the response did not carry the contract header".into(),
            };
        }
        return Answered::Committed(body);
    }
    if contract_ok && let Ok(stated) = serde_json::from_slice::<Stated>(body) {
        let effects = stated.disposition.map(|disposition| disposition.effects);
        // The rest of the body, as far as this build reads it: a refusal stays one when a detail
        // does not (a conflict of a kind it does not know).
        let body = serde_json::from_slice::<ErrorOutput>(body).unwrap_or(ErrorOutput {
            error: stated.error,
            code: stated.code,
            disposition: stated.disposition,
            ..ErrorOutput::default()
        });
        return match effects {
            Some(DispositionEffects::None) => Answered::Refused(ApiError {
                status: status.as_u16(),
                body,
            }),
            Some(DispositionEffects::Unknown | DispositionEffects::Partial) => Answered::Unknown {
                reason: format!(
                    "the server answered {status} and could not say whether the change was \
                         made: {}",
                    body.error
                ),
            },
            // An effect this build does not know, or none: the contract states one on every
            // error, so an answer without one cannot rule the change out.
            Some(DispositionEffects::Other) | None => Answered::Unknown {
                reason: format!(
                    "the server answered {status} without saying whether the change was made: {}",
                    body.error
                ),
            },
        };
    }
    Answered::Unknown {
        reason: format!("the server answered {status}"),
    }
}

/// What of an error's body decides a write's outcome ([`judge_write`]), read on its own: a detail
/// this build reads otherwise cannot turn a refusal into an unknown outcome.
#[derive(serde::Deserialize)]
struct Stated {
    error: String,
    #[serde(default)]
    code: Option<crate::api::ErrorCode>,
    #[serde(default)]
    disposition: Option<crate::api::DispositionOutput>,
}

/// [`judge_write`], with a committed write's body read as `T`; one that cannot be read is
/// unknown.
fn classify_write<T: DeserializeOwned>(raw: RawResponse) -> WriteOutcome<T> {
    match judge_write(raw.status, raw.contract.as_deref(), &raw.body) {
        Answered::Committed(body) => match serde_json::from_slice(body) {
            Ok(value) => WriteOutcome::Committed(value),
            Err(error) => WriteOutcome::Unknown {
                reason: format!("the success response could not be read: {error}"),
            },
        },
        Answered::Refused(error) => WriteOutcome::Refused(error),
        Answered::Unknown { reason } => WriteOutcome::Unknown { reason },
    }
}

/// Why a request the transport failed has no answer, by the error's kind and never its words:
/// a connection the server refused carried no request ([`NoAnswer::Unreached`]); any other
/// failure may have come after the request went out ([`NoAnswer::Lost`]).
fn unanswered_by_transport(error: &anyhow::Error) -> NoAnswer {
    let refused = error.chain().any(|layer| {
        layer
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
    });
    let why = describe_transport(error);
    if refused {
        NoAnswer::Unreached(why)
    } else {
        NoAnswer::Lost(why)
    }
}

/// A write with no answer: never sent, so nothing was written, or lost, so it may have been.
fn unanswered<T>(no: NoAnswer) -> WriteOutcome<T> {
    match no {
        NoAnswer::Retired(reason) | NoAnswer::Unsendable(reason) | NoAnswer::Unreached(reason) => {
            WriteOutcome::NotSent { reason }
        }
        NoAnswer::Lost(reason) => WriteOutcome::Unknown { reason },
    }
}

/// The least of an error answer's body a read keeps, whatever it asked for of the value.
const ERROR_BODY_BYTES: usize = 64 * 1024;

/// How much of a response body is kept.
#[derive(Debug, Clone, Copy)]
enum BodyLimit {
    /// Its first bytes, this many at most.
    First(usize),
    /// `len` bytes from `start` of a value asked for by range: a 206 is the range itself, a
    /// 200 the whole value, read through `start` without keeping it.
    Range { start: u64, len: usize },
}

/// Whether a 206's `Content-Range` (`bytes 2-4/10`) starts where the request asked.
fn content_range_starts_at(served: &str, start: u64) -> bool {
    served
        .trim()
        .strip_prefix("bytes ")
        .and_then(|range| range.split_once('-'))
        .and_then(|(first, _)| first.trim().parse::<u64>().ok())
        == Some(start)
}

/// How long a response's body says it is: its `Content-Length`, unless no body follows (an
/// answer to HEAD, a 204, a 304), where the header names what a GET would have sent.
fn body_length(head: bool, status: StatusCode, headers: &HeaderMap) -> Option<u64> {
    if head || status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED {
        return None;
    }
    headers
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// A response body: `skip` bytes read and dropped, then at most `limit` kept, the rest left
/// unread. What is kept is read straight into the answer, made as large as the body's
/// `Content-Length` says from the start (within `limit`), so a whole range is not grown to twice
/// its size at its end; without one, the answer grows as it is read.
async fn read_body(
    mut body: AsyncBody,
    skip: u64,
    limit: usize,
    length: Option<u64>,
) -> std::io::Result<Vec<u8>> {
    if skip > 0 {
        let skipped = futures::io::BufReader::with_capacity(64 * 1024, (&mut body).take(skip));
        futures::io::copy_buf(skipped, &mut futures::io::sink()).await?;
    }
    let kept = length.map_or(0, |length| {
        usize::try_from(length.saturating_sub(skip)).map_or(limit, |left| left.min(limit))
    });
    // A byte over, where reading to the end finds that nothing follows.
    let mut bytes = Vec::with_capacity(kept.saturating_add(1));
    body.take(limit as u64).read_to_end(&mut bytes).await?;
    Ok(bytes)
}

/// A request the server took and had not answered within its timeout ([`ReadError::timed_out`]).
const REQUEST_TIMED_OUT: &str = "the request timed out";
/// A connection that did not open within its timeout: no server was reached.
const CONNECT_TIMED_OUT: &str = "the connection timed out";

/// What went wrong reaching the server, in words: a class (refused, timed out, name not found,
/// TLS, connection closed) or else the innermost cause, with any URL removed. The client's own
/// text names the whole request URL, which can hold a Blob row's key, and its outer layer
/// ("error sending request") hides the reason; the cause is what helps.
fn describe_transport(error: &anyhow::Error) -> String {
    let layers: Vec<String> = error
        .chain()
        .map(|layer| without_urls(&layer.to_string()))
        .collect();
    let all = layers.join(" ").to_lowercase();
    let class = if all.contains("timed out") || all.contains("timeout") {
        // A connection that never opened reached no server (`client error (Connect)` is
        // hyper's layer for it); a request that did was waiting for the server's answer.
        if all.contains("(connect)") {
            Some(CONNECT_TIMED_OUT)
        } else {
            Some(REQUEST_TIMED_OUT)
        }
    } else if all.contains("connection refused") {
        Some("the connection was refused")
    } else if all.contains("dns error")
        || all.contains("lookup address")
        || all.contains("nodename nor servname")
        || all.contains("name or service not known")
    {
        Some("the server's name could not be resolved")
    } else if all.contains("certificate") || all.contains("tls") || all.contains("ssl") {
        Some("the secure connection failed")
    } else if all.contains("connection reset")
        || all.contains("broken pipe")
        || all.contains("connection closed")
        || all.contains("unexpected eof")
    {
        Some("the connection was closed")
    } else {
        None
    };
    match class {
        Some(class) => class.to_owned(),
        None => {
            let cause = layers.last().cloned().unwrap_or_default();
            if cause.chars().count() > 200 {
                cause.chars().take(199).chain(['…']).collect()
            } else {
                cause
            }
        }
    }
}

/// `text` without `for url (…)` and with every URL replaced by `…`.
fn without_urls(text: &str) -> String {
    let mut text = text.to_owned();
    while let Some(at) = text.find(" for url (") {
        let end = text[at..].find(')').map_or(text.len(), |end| at + end + 1);
        text.replace_range(at..end, "");
    }
    text.split_whitespace()
        .map(|word| if word.contains("://") { "…" } else { word })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::executor::block_on;
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};

    use super::*;
    use crate::api::{AccessAction, ParamKind};

    fn respond(
        status: u16,
        contract: Option<&'static str>,
        body: &'static str,
    ) -> Arc<dyn HttpClient> {
        FakeHttpClient::create(move |_request| async move {
            let mut builder = Response::builder().status(status);
            if let Some(contract) = contract {
                builder = builder.header(CONTRACT_HEADER, contract);
            }
            Ok(builder.body(AsyncBody::from(body)).unwrap())
        })
    }

    fn client(http: Arc<dyn HttpClient>) -> GraphClient {
        GraphClient::new("http://127.0.0.1:8787", Some("t".into()), http).unwrap()
    }

    /// A blank token is none, given at the start or later: sent, a server would read `Bearer `
    /// as no credential, and say it got none.
    #[test]
    fn a_blank_token_is_none() {
        let http = FakeHttpClient::create(|_| async { unreachable!() });
        let blank = GraphClient::new("http://127.0.0.1:8787", Some(String::new()), http).unwrap();
        assert!(!blank.holds_token());
        let held = client(FakeHttpClient::create(|_| async { unreachable!() }));
        assert!(
            held.set_token(Some("  ".into())),
            "the token held is dropped"
        );
        assert!(!held.holds_token());
    }

    const CHANGE: &str =
        r#"{"branch":"main","query_name":"u","affected_nodes":1,"affected_edges":0,"commit":null}"#;

    /// A 403 on a graph's route counts as a refusal of that graph on every clone, read or
    /// write, from the gateway too, by the id its path names, escaped or not; one on another
    /// graph does not count for it. One on the server's own routes (listing and discovering
    /// graphs, which most tokens may not), or one without the contract Studio speaks, does not
    /// count. Neither moves the credential count.
    #[test]
    fn a_refusal_on_a_graph_is_counted_for_that_graph_alone() {
        let held = GraphClient::new(
            "http://127.0.0.1:8787/og",
            Some("t".into()),
            FakeHttpClient::create(|request| {
                let path = request.uri().path().to_owned();
                async move {
                    let mut builder = Response::builder().status(403);
                    if path.ends_with("/older/schema") {
                        builder = builder.header(CONTRACT_HEADER, "0.13");
                    } else if !path.ends_with("/plain/schema") {
                        builder = builder.header(CONTRACT_HEADER, CONTRACT);
                    }
                    Ok(builder
                        .body(AsyncBody::from(r#"{"error":"denied"}"#))
                        .unwrap())
                }
            }),
        )
        .unwrap();
        let gateway = held.clone();
        block_on(held.graphs()).ok();
        block_on(held.discover_graphs()).ok();
        block_on(held.schema("plain")).ok();
        block_on(held.schema("older")).ok();
        for graph in ["plain", "older", "discovery", "knowledge"] {
            assert_eq!(
                held.refusals(graph),
                0,
                "the server's own routes, a 403 from elsewhere and one in another contract"
            );
        }
        block_on(held.schema("discovery")).ok();
        assert_eq!(
            held.refusals("discovery"),
            1,
            "a graph named like a route counts"
        );
        let change = ChangeRequest {
            query: "query q() {}".into(),
            name: None,
            params: None,
            branch: Some("main".into()),
        };
        block_on(held.mutate("knowledge", &change, None));
        block_on(gateway.forward(ForwardRequest {
            method: Method::GET,
            path_and_query: "graphs/knowledge/branches".into(),
            headers: Vec::new(),
            body: AsyncBody::empty(),
        }))
        .unwrap();
        assert_eq!(gateway.refusals("knowledge"), 2, "a write and a forward");
        assert_eq!(held.refusals("discovery"), 1, "another graph's refusals");
        // An id the client escapes, and one a forwarded request escaped otherwise: one graph.
        block_on(held.schema("a b")).ok();
        block_on(gateway.forward(ForwardRequest {
            method: Method::GET,
            path_and_query: "graphs/a%20b/branches".into(),
            headers: Vec::new(),
            body: AsyncBody::empty(),
        }))
        .unwrap();
        assert_eq!(held.refusals("a b"), 2);
        assert_eq!(held.refusals("knowledge"), 2);
        assert_eq!(held.credential_changes(), 0);
    }

    /// A path segment reads as the server reads it: each escape decoded, anything else kept.
    #[test]
    fn a_path_segment_reads_its_escapes() {
        assert_eq!(decode_segment("knowledge"), "knowledge");
        assert_eq!(decode_segment("a%20b"), "a b");
        assert_eq!(decode_segment("caf%C3%A9"), "café");
        assert_eq!(decode_segment("100%"), "100%");
        assert_eq!(decode_segment("%zz%4"), "%zz%4");
    }

    /// A 0.14.0 server states every error's effects, and the disposition decides a write's
    /// outcome over the status: nothing done is a refusal even on a 500, and effects the server
    /// cannot rule out are unknown even on a 503. Without one, nothing rules the change out.
    #[test]
    fn a_disposition_decides_a_write_outcome_over_its_status() {
        let error = |effects: Option<&str>| match effects {
            Some(effects) => format!(
                r#"{{"error":"no","code":"internal","disposition":{{"action":"retry","effects":"{effects}"}}}}"#
            ),
            None => r#"{"error":"no","code":"internal"}"#.to_owned(),
        };
        let judge = |status: u16, effects: Option<&str>| {
            let body = error(effects);
            match judge_write(
                StatusCode::from_u16(status).unwrap(),
                Some(crate::CONTRACT),
                body.as_bytes(),
            ) {
                Answered::Committed(_) => "committed",
                Answered::Refused(_) => "refused",
                Answered::Unknown { .. } => "unknown",
            }
        };
        assert_eq!(judge(500, Some("none")), "refused", "nothing was done");
        assert_eq!(judge(409, Some("none")), "refused");
        assert_eq!(
            judge(503, Some("unknown")),
            "unknown",
            "it may have been done"
        );
        assert_eq!(
            judge(409, Some("partial")),
            "unknown",
            "some of it was done"
        );
        assert_eq!(
            judge(409, Some("later")),
            "unknown",
            "an effect this build does not know"
        );
        assert_eq!(
            judge(409, None),
            "unknown",
            "no disposition: the status rules nothing out"
        );
        assert_eq!(judge(503, None), "unknown");
        // A detail this build does not read leaves the disposition to decide.
        let unread = r#"{"error":"no","code":"conflict","disposition":{"action":"fix","effects":"none"},"merge_conflicts":[{"entity_kind":"hyperedge","type_name":"T","kind":"later","message":"m"}]}"#;
        match judge_write(
            StatusCode::CONFLICT,
            Some(crate::CONTRACT),
            unread.as_bytes(),
        ) {
            Answered::Refused(refusal) => assert_eq!(refusal.body.error, "no"),
            _ => panic!("a refusal whose details this build does not read"),
        }
        // Without the contract header the server did not answer, whatever the body says.
        let body = error(Some("none"));
        assert!(matches!(
            judge_write(StatusCode::INTERNAL_SERVER_ERROR, None, body.as_bytes()),
            Answered::Unknown { .. }
        ));
    }

    /// Every route the client calls is one the contract serves, with that method: each public
    /// call is made once, a conditional write both ways, against a server that records what it
    /// was sent, and each request's path is matched to the contract's paths, a `{…}` standing for
    /// one segment. The agent's forwarded requests are the CLI's own and not counted here.
    #[test]
    fn every_route_the_client_calls_is_in_the_contract() {
        let sent: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
        let log = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            log.lock().unwrap().push((
                request.method().as_str().to_ascii_lowercase(),
                request.uri().path().to_owned(),
            ));
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from("{}"))
                    .unwrap())
            }
        });
        let at = client(http);
        let graph = "knowledge";
        let cell = BlobCell {
            edge: false,
            type_name: "Person".into(),
            id: "p1".into(),
            property: "photo".into(),
        };
        let query = QueryRequest {
            query: "query q() {}".into(),
            name: None,
            params: None,
            branch: Some("main".into()),
            snapshot: None,
        };
        let change = ChangeRequest {
            query: "query m() {}".into(),
            name: None,
            params: None,
            branch: Some("main".into()),
        };
        let stored = InvokeStoredQueryRequest {
            params: None,
            branch: Some("main".into()),
            snapshot: None,
            expect_mutation: None,
        };
        let create = BranchCreateRequest {
            name: "feature".into(),
            from: Some("main".into()),
            from_commit: None,
        };
        let merge = BranchMergeRequest {
            source: "feature".into(),
            target: Some("main".into()),
            delete_branch: true,
            dry_run: false,
        };
        block_on(async {
            at.health().await.ok();
            at.readiness().await.ok();
            at.graphs().await.ok();
            at.discover_graphs().await.ok();
            at.schema(graph).await.ok();
            at.snapshot(graph, "main").await.ok();
            at.query(graph, &query).await.ok();
            at.stored_queries(graph).await.ok();
            at.invoke_stored_read(graph, "find", &stored).await.ok();
            let _ = at.invoke_stored_write(graph, "find", &stored, None).await;
            let _ = at
                .invoke_stored_write(graph, "find", &stored, Some("c1"))
                .await;
            at.access(graph, Some("main"), Some("feature")).await.ok();
            at.branches(graph).await.ok();
            at.commit_page(graph, "main", 1, Some("t")).await.ok();
            at.commit(graph, "c1").await.ok();
            at.commit_changes(graph, "c1", None, 10).await.ok();
            at.diff(graph, "c1", "c2", None, 10).await.ok();
            at.merge_base(graph, "feature", "main").await.ok();
            at.merge_preview(graph, "feature", "main").await.ok();
            at.changes(graph, "main", FeedAt::Start("now"), &[], 10)
                .await
                .ok();
            let _ = at.mutate(graph, &change, None).await;
            let _ = at.mutate(graph, &change, Some("c1")).await;
            let _ = at.create_branch(graph, &create).await;
            let _ = at.delete_branch(graph, "feature").await;
            let _ = at.merge(graph, &merge).await;
            at.blob_head(graph, &cell, BlobAt::Branch("main"))
                .await
                .ok();
            at.blob_bytes(graph, &cell, BlobAt::Snapshot("c1"), 0..10)
                .await
                .ok();
            let _ = at.blob_put(graph, "main", &cell, None, vec![1]).await;
            let _ = at.blob_clear(graph, "main", &cell, None).await;
        });

        let contract: Value =
            serde_json::from_str(include_str!("../openapi/omnigraph-0.14.0.json")).unwrap();
        let paths = contract["paths"].as_object().expect("paths");
        let same_route = |template: &str, path: &str| {
            let (template, path) = (template.split('/'), path.split('/'));
            template.clone().count() == path.clone().count()
                && template
                    .zip(path)
                    .all(|(part, segment)| part.starts_with('{') || part == segment)
        };
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 29, "one request per call: {sent:?}");
        for (method, path) in sent.iter() {
            assert!(
                paths
                    .iter()
                    .any(|(template, operations)| same_route(template, path)
                        && operations.get(method).is_some()),
                "{} {path} is not a route of the contract",
                method.to_uppercase()
            );
        }
    }

    /// A read of changes, which the server works out whole whatever the page, waits as long as a
    /// write; every other read waits a minute, as before.
    #[test]
    fn a_read_of_changes_waits_longer_than_other_reads() {
        /// Each request's method and path, and the timeout it was sent with.
        type Sent = Vec<(String, Option<Duration>)>;
        let sent: Arc<std::sync::Mutex<Sent>> = Arc::default();
        let log = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            log.lock().unwrap().push((
                format!("{} {}", request.method(), request.uri().path()),
                request
                    .extensions()
                    .get::<http_client::RequestTimeout>()
                    .map(|timeout| timeout.0),
            ));
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from("{}"))
                    .unwrap())
            }
        });
        let at = client(http);
        let graph = "knowledge";
        let cell = BlobCell {
            edge: false,
            type_name: "Person".into(),
            id: "p1".into(),
            property: "photo".into(),
        };
        let query = QueryRequest {
            query: "query q() {}".into(),
            name: None,
            params: None,
            branch: Some("main".into()),
            snapshot: None,
        };
        let taken = || std::mem::take(&mut *sent.lock().unwrap());
        block_on(async {
            at.changes(graph, "main", FeedAt::Start("now"), &[], 10)
                .await
                .ok();
            at.diff(graph, "c1", "c2", None, 10).await.ok();
            at.commit_changes(graph, "c1", None, 10).await.ok();
            at.merge_preview(graph, "feature", "main").await.ok();
        });
        let changes = taken();
        block_on(async {
            at.graphs().await.ok();
            at.schema(graph).await.ok();
            at.snapshot(graph, "main").await.ok();
            at.query(graph, &query).await.ok();
            at.stored_queries(graph).await.ok();
            at.access(graph, Some("main"), None).await.ok();
            at.branches(graph).await.ok();
            at.commit_page(graph, "main", 1, None).await.ok();
            at.commit(graph, "c1").await.ok();
            at.merge_base(graph, "feature", "main").await.ok();
            at.blob_head(graph, &cell, BlobAt::Branch("main"))
                .await
                .ok();
            at.blob_bytes(graph, &cell, BlobAt::Snapshot("c1"), 0..10)
                .await
                .ok();
        });
        let others = taken();
        assert!(CHANGE_READ_TIMEOUT > READ_TIMEOUT);
        assert_eq!(CHANGE_READ_TIMEOUT, WRITE_TIMEOUT);
        assert_eq!(changes.len(), 4, "{changes:?}");
        for (route, timeout) in &changes {
            assert_eq!(*timeout, Some(CHANGE_READ_TIMEOUT), "{route}");
        }
        assert_eq!(others.len(), 12, "{others:?}");
        for (route, timeout) in &others {
            assert_eq!(*timeout, Some(READ_TIMEOUT), "{route}");
        }
    }

    /// A token replaced on the client reaches every clone, as the one a running agent's gateway
    /// holds: its next forwarded request and its next read send the new token. Only a token that
    /// differs from the one held counts as a credential change.
    #[test]
    fn a_replaced_token_reaches_every_clone() {
        let sent: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let log = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            let log = log.clone();
            async move {
                let token = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                log.lock().unwrap().push(token);
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from("{}"))
                    .unwrap())
            }
        });
        let held = client(http);
        let gateway = held.clone();
        assert!(!held.set_token(Some("t".into())), "the token held");
        assert_eq!(gateway.credential_changes(), 0);
        assert!(held.set_token(Some("new".into())));
        assert_eq!(gateway.credential_changes(), 1);
        block_on(gateway.forward(ForwardRequest {
            method: Method::GET,
            path_and_query: "graphs".into(),
            headers: Vec::new(),
            body: AsyncBody::empty(),
        }))
        .unwrap();
        block_on(gateway.schema("g")).ok();
        assert_eq!(*sent.lock().unwrap(), ["Bearer new", "Bearer new"]);
    }

    /// A request that would send a token its lease says has ended waits for the next, telling
    /// the token's holder to ask for it, and carries the next, woken as it comes. A token set is
    /// not known to end, the one held set again included. With no holder listening, and for the
    /// public routes, which take no token, nothing waits. Before, the ended token was sent, and
    /// refused, after the machine slept past its renewal.
    #[test]
    fn a_request_waits_for_the_next_token_once_the_one_held_ended() {
        let sent: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let log = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            let log = log.clone();
            async move {
                let token = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                log.lock().unwrap().push(token);
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from("{}"))
                    .unwrap())
            }
        });
        let held = client(http);
        let quick = |what: &str, request: &dyn Fn()| {
            let started = Instant::now();
            request();
            assert!(started.elapsed() < Duration::from_secs(1), "{what} waited");
        };

        // No holder listens: sent as held.
        held.set_token_ends(Some(SystemTime::now()));
        quick("a request nobody would answer", &|| {
            block_on(held.schema("g")).ok();
        });
        assert_eq!(sent.lock().unwrap().pop().unwrap(), "Bearer t");

        // A holder listens: told, it renews, and the request carries the next.
        let ended = held.token_ended();
        let holder = held.clone();
        // The holder, told, renews; not told within a while, it does nothing.
        let renewed = std::thread::spawn(move || {
            let told = block_on(smol::future::or(
                async { ended.recv().await.is_ok() },
                async {
                    smol::Timer::after(Duration::from_secs(5)).await;
                    false
                },
            ));
            if told {
                holder.renew_token(
                    "next".into(),
                    Some(SystemTime::now() + Duration::from_secs(300)),
                );
            }
            ended
        });
        held.set_token_ends(Some(SystemTime::now()));
        let started = Instant::now();
        block_on(held.schema("g")).ok();
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "woken by the renewal, not the next tell: {:?}",
            started.elapsed()
        );
        let ended = renewed.join().unwrap();
        assert_eq!(sent.lock().unwrap().pop().unwrap(), "Bearer next");

        // The public routes never wait, the token ended or not.
        held.set_token_ends(Some(SystemTime::now()));
        quick("the health check", &|| {
            block_on(held.health()).ok();
        });
        assert!(ended.try_recv().is_err(), "nobody told");

        // A token set, the one held again included, is not known to end.
        held.set_token(Some("next".into()));
        quick("the token held set again", &|| {
            block_on(held.schema("g")).ok();
        });
        assert_eq!(sent.lock().unwrap().pop().unwrap(), "Bearer next");

        // The holder gives no next token now: the request waiting sends what is held, and the
        // end stands, so the next request tells the holder again.
        drop(ended);
        let again = held.token_ended();
        held.set_token_ends(Some(SystemTime::now()));
        let holder = held.clone();
        let first_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let renews = first_sent.clone();
        // Releases each tell until the first request went, then renews at the next.
        let answering = std::thread::spawn(move || {
            loop {
                assert!(told_within(&again), "told again");
                if renews.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                holder.release_waits(holder.waits_begun());
            }
            holder.renew_token(
                "third".into(),
                Some(SystemTime::now() + Duration::from_secs(300)),
            );
            again
        });
        let started = Instant::now();
        block_on(held.schema("g")).ok();
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "woken by the release, not the next tell: {:?}",
            started.elapsed()
        );
        assert_eq!(
            sent.lock().unwrap().pop().unwrap(),
            "Bearer next",
            "sent as held"
        );
        first_sent.store(true, std::sync::atomic::Ordering::SeqCst);
        block_on(held.schema("g")).ok();
        let again = answering.join().unwrap();
        assert_eq!(
            sent.lock().unwrap().pop().unwrap(),
            "Bearer third",
            "told again"
        );

        // A release is for the requests waiting when the holder decided it: one that began
        // waiting after waits on, and goes with the next token.
        held.set_token_ends(Some(SystemTime::now()));
        let request = |held: &GraphClient| {
            let held = held.clone();
            std::thread::spawn(move || block_on(held.schema("g")).ok())
        };
        let first = request(&held);
        assert!(told_within(&again), "told by the first");
        let through = held.waits_begun();
        let second = request(&held);
        let started = Instant::now();
        while held.waits_begun() == through {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the second waiting"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        held.release_waits(through);
        first.join().unwrap();
        assert_eq!(sent.lock().unwrap().pop().unwrap(), "Bearer third");
        // The second tells at once, then again a second on: still waiting, as a request
        // released would tell no more.
        assert!(told_within(&again), "told by the second");
        assert!(told_within(&again), "the second waits on, telling again");
        assert!(!second.is_finished(), "the second waits on");
        held.renew_token(
            "fourth".into(),
            Some(SystemTime::now() + Duration::from_secs(300)),
        );
        second.join().unwrap();
        assert_eq!(sent.lock().unwrap().pop().unwrap(), "Bearer fourth");

        // The holder no longer listens (its connection gone): sent as held.
        drop(again);
        held.set_token_ends(Some(SystemTime::now()));
        quick("a request after the holder stopped listening", &|| {
            block_on(held.schema("g")).ok();
        });

        // Retired while a request waits for the next token: it stops waiting at once, refused,
        // and sends nothing.
        let again = held.token_ended();
        held.set_token_ends(Some(SystemTime::now()));
        let before = sent.lock().unwrap().len();
        let waiting = request(&held);
        assert!(told_within(&again), "told by the waiting request");
        let started = Instant::now();
        held.retire("the target is gone".into());
        assert!(waiting.join().unwrap().is_none(), "refused");
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(sent.lock().unwrap().len(), before, "nothing sent");
    }

    /// Whether the holder was told within five seconds, so a test that stops telling fails
    /// rather than hangs.
    fn told_within(told: &smol::channel::Receiver<()>) -> bool {
        block_on(smol::future::or(
            async { told.recv().await.is_ok() },
            async {
                smol::Timer::after(Duration::from_secs(5)).await;
                false
            },
        ))
    }

    /// A read refused with 401 for a token replaced while it was under way is read again, once,
    /// with the token held now, and its answer is that read's. Before, the refusal of a token
    /// already gone was said, and its holder asked for yet another token.
    #[test]
    fn a_read_refused_for_a_token_replaced_meanwhile_is_read_again() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let sent: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let hold = Arc::new(AtomicBool::new(true));
        let (log, held_back) = (sent.clone(), hold.clone());
        let http = FakeHttpClient::create(move |request| {
            let (log, hold) = (log.clone(), held_back.clone());
            async move {
                let token = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                log.lock().unwrap().push(token.clone());
                // The first answer comes once the token is replaced.
                while token == "Bearer t" && hold.load(Ordering::SeqCst) {
                    smol::Timer::after(Duration::from_millis(5)).await;
                }
                let status = if token == "Bearer t" { 401 } else { 200 };
                Ok(Response::builder()
                    .status(status)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from(
                        r#"{"schema_source": "node A {\n  slug: String @key\n}\n"}"#,
                    ))
                    .unwrap())
            }
        });
        let held = client(http);
        let reader = held.clone();
        let read = std::thread::spawn(move || block_on(reader.schema("g")));
        while sent.lock().unwrap().is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
        held.renew_token("next".into(), None);
        hold.store(false, Ordering::SeqCst);
        assert!(
            read.join().unwrap().is_ok(),
            "read again with the token held now"
        );
        assert_eq!(*sent.lock().unwrap(), ["Bearer t", "Bearer next"]);
    }

    /// Reads queued behind the read slots wait for the next token before taking one, so all of
    /// them join one wait (one renewal, one check of a server away). Before, only a slot's worth
    /// waited at a time, each batch in turn.
    #[test]
    fn reads_queued_for_a_slot_join_the_wait_for_the_next_token() {
        let http = FakeHttpClient::create(|_| async {
            Ok(Response::builder()
                .status(200)
                .header(CONTRACT_HEADER, CONTRACT)
                .body(AsyncBody::from("{}"))
                .unwrap())
        });
        let held = client(http);
        let _holder = held.token_ended();
        held.set_token_ends(Some(SystemTime::now()));
        let reads = READ_CONCURRENCY * 2 + 1;
        let waiting: Vec<_> = (0..reads)
            .map(|_| {
                let held = held.clone();
                std::thread::spawn(move || block_on(held.schema("g")).ok())
            })
            .collect();
        let started = Instant::now();
        while held.waits_begun() < reads as u64 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{} of {reads} waiting",
                held.waits_begun()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        held.release_waits(held.waits_begun());
        for read in waiting {
            read.join().unwrap();
        }
    }

    /// A read that queued for a slot while its token was live, and has a slot only once the token
    /// ended (a sleep across a long queue), waits for the next before it sends. Before, it sent
    /// the ended one.
    #[test]
    fn a_read_whose_token_ended_while_it_queued_waits_for_the_next() {
        use std::sync::atomic::AtomicBool;
        let (held, slots, sent) = slots_taken();
        let told = held.token_ended();
        // Polled by hand: queued for a slot with a live token, which then ends.
        let woken = Arc::new(Woken(AtomicBool::new(false)));
        let waker = futures::task::waker(woken.clone());
        let mut context = std::task::Context::from_waker(&waker);
        let mut queued = Box::pin(held.schema("g"));
        assert!(queued.as_mut().poll(&mut context).is_pending(), "queued");
        held.set_token_ends(Some(SystemTime::now()));
        slots.release();
        // With a slot, it finds its token ended, and waits for the next, telling the holder.
        assert!(queued.as_mut().poll(&mut context).is_pending(), "waits");
        assert!(told.try_recv().is_ok(), "the holder told");
        woken.0.store(false, std::sync::atomic::Ordering::SeqCst);
        held.renew_token(
            "next".into(),
            Some(SystemTime::now() + Duration::from_secs(300)),
        );
        assert!(
            woken.0.load(std::sync::atomic::Ordering::SeqCst),
            "woken by the renewal"
        );
        assert!(queued.as_mut().poll(&mut context).is_ready(), "sent");
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), READ_CONCURRENCY + 1, "{sent:?}");
        assert_eq!(sent.last().map(String::as_str), Some("Bearer next"));
    }

    /// A client whose read slots are all taken by reads its server answers only once the guard
    /// returned lets go, and the token each request carried, in order.
    fn slots_taken() -> (GraphClient, SlotsTaken, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let hold = Arc::new(AtomicBool::new(true));
        let sent: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let (holding, log) = (hold.clone(), sent.clone());
        let http = FakeHttpClient::create(move |request| {
            let (holding, log) = (holding.clone(), log.clone());
            async move {
                let token = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                log.lock().unwrap().push(token);
                while holding.load(Ordering::SeqCst) {
                    smol::Timer::after(Duration::from_millis(5)).await;
                }
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from("{}"))
                    .unwrap())
            }
        });
        let held = client(http);
        let filling = (0..READ_CONCURRENCY)
            .map(|_| {
                let held = held.clone();
                std::thread::spawn(move || {
                    block_on(held.schema("g")).ok();
                })
            })
            .collect();
        let started = Instant::now();
        while sent.lock().unwrap().len() < READ_CONCURRENCY {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the slots filled"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        (held, SlotsTaken { hold, filling }, sent)
    }

    /// The reads holding a client's read slots ([`slots_taken`]): let go when released, or when
    /// dropped, so a test that fails does not leave them waiting.
    struct SlotsTaken {
        hold: Arc<std::sync::atomic::AtomicBool>,
        filling: Vec<std::thread::JoinHandle<()>>,
    }

    impl SlotsTaken {
        /// Lets the reads be answered, and waits until they are.
        fn release(mut self) {
            self.hold.store(false, std::sync::atomic::Ordering::SeqCst);
            for read in self.filling.drain(..) {
                read.join().unwrap();
            }
        }
    }

    impl Drop for SlotsTaken {
        fn drop(&mut self) {
            self.hold.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A waker that notes it was woken, for a request polled by hand.
    struct Woken(std::sync::atomic::AtomicBool);

    impl futures::task::ArcWake for Woken {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A read queued for a slot is refused as soon as its client is retired, not when a slot
    /// frees behind reads still under way: polled by hand, it is woken by the retire itself.
    #[test]
    fn a_read_queued_when_its_client_is_retired_is_refused_at_once() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::Poll;
        let (held, slots, sent) = slots_taken();
        let woken = Arc::new(Woken(AtomicBool::new(false)));
        let waker = futures::task::waker(woken.clone());
        let mut context = std::task::Context::from_waker(&waker);
        let mut queued = Box::pin(held.schema("g"));
        assert!(queued.as_mut().poll(&mut context).is_pending(), "queued");
        assert!(!woken.0.load(Ordering::SeqCst));
        held.retire("the target is gone".into());
        assert!(woken.0.load(Ordering::SeqCst), "woken by the retire");
        let Poll::Ready(Err(ReadError::Transport(why))) = queued.as_mut().poll(&mut context) else {
            panic!("refused at once");
        };
        assert_eq!(why, "the target is gone");
        assert_eq!(
            sent.lock().unwrap().len(),
            READ_CONCURRENCY,
            "nothing more sent"
        );
        slots.release();
    }

    /// A read sleeping out a 429's Retry-After is refused as soon as its client is retired, not
    /// when the sleep ends, and is not sent again.
    #[test]
    fn a_read_sleeping_out_a_429_is_refused_when_its_client_is_retired() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::task::Poll;
        let sent = Arc::new(AtomicUsize::new(0));
        let count = sent.clone();
        let http = FakeHttpClient::create(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(Response::builder()
                    .status(429)
                    .header("retry-after", "30")
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from("{}"))
                    .unwrap())
            }
        });
        let held = client(http);
        let woken = Arc::new(Woken(AtomicBool::new(false)));
        let waker = futures::task::waker(woken.clone());
        let mut context = std::task::Context::from_waker(&waker);
        let mut read = Box::pin(held.schema("g"));
        assert!(
            read.as_mut().poll(&mut context).is_pending(),
            "sleeping out the 429"
        );
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        assert!(!woken.0.load(Ordering::SeqCst));
        held.retire("the target is gone".into());
        assert!(woken.0.load(Ordering::SeqCst), "woken by the retire");
        let Poll::Ready(Err(ReadError::Transport(why))) = read.as_mut().poll(&mut context) else {
            panic!("refused at once");
        };
        assert_eq!(why, "the target is gone");
        assert_eq!(sent.load(Ordering::SeqCst), 1, "not sent again");
    }

    /// An HTTP client replaced on the client, as when the CLI target's certificate authority
    /// changed, is the one every clone, and every holder of its transport (a sign-in's requests
    /// to the server's address), sends through next: none keeps the old trust, and the change
    /// counts as a credential change, so what was read is read again. A retired client sends
    /// nothing, while its transport still does, for a sign-in's last revocation.
    #[test]
    fn a_replaced_http_client_reaches_every_clone() {
        let answering = |name: &'static str, seen: Arc<std::sync::Mutex<Vec<&'static str>>>| {
            FakeHttpClient::create(move |_| {
                seen.lock().unwrap().push(name);
                async move {
                    Ok(Response::builder()
                        .status(200)
                        .header(CONTRACT_HEADER, CONTRACT)
                        .body(AsyncBody::from("{}"))
                        .unwrap())
                }
            })
        };
        let seen: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::default();
        let held = client(answering("old", seen.clone()));
        let gateway = held.clone();
        let sign_in = held.transport();
        let to_service = || {
            let request = Request::builder()
                .uri("http://127.0.0.1:8787/oauth/token")
                .body(AsyncBody::empty())
                .unwrap();
            block_on(sign_in.send(request)).is_ok()
        };
        block_on(gateway.schema("g")).ok();
        assert!(to_service());
        let changes = held.credential_changes();
        held.set_http(answering("new", seen.clone()));
        assert_eq!(gateway.credential_changes(), changes + 1);
        block_on(gateway.schema("g")).ok();
        block_on(held.schema("g")).ok();
        assert!(to_service());
        assert_eq!(*seen.lock().unwrap(), ["old", "old", "new", "new", "new"]);

        held.retire("the target is gone".into());
        assert_eq!(gateway.credential_changes(), changes + 2);
        let refused = block_on(gateway.schema("g")).unwrap_err();
        // A write is not sent either, and says so: nothing was written.
        assert!(matches!(
            block_on(gateway.mutate("g", &ChangeRequest::default(), None)),
            WriteOutcome::NotSent { reason } if reason == "the target is gone"
        ));
        assert!(
            format!("{refused:?}").contains("the target is gone"),
            "{refused:?}"
        );
        assert!(to_service(), "the transport still sends");
        assert_eq!(
            seen.lock().unwrap().len(),
            6,
            "the retired client sent nothing"
        );
    }

    /// A write whose connection the server refused was never sent, and says so: nothing was
    /// written. One whose connection failed otherwise (reset) may have landed after it went out,
    /// and so may one whose failure only says "connection refused" in words: the error's kind
    /// decides, not its text. Before, every transport failure was an answer lost.
    #[test]
    fn a_write_the_server_refused_to_connect_to_is_not_sent() {
        let failing = |kind: Option<std::io::ErrorKind>| {
            client(FakeHttpClient::create(move |_| async move {
                Err(match kind {
                    Some(kind) => anyhow::Error::from(std::io::Error::from(kind))
                        .context("error sending request"),
                    None => anyhow::anyhow!("connection refused, in words alone"),
                })
            }))
        };
        let write =
            |client: GraphClient| block_on(client.mutate("g", &ChangeRequest::default(), None));
        assert!(matches!(
            write(failing(Some(std::io::ErrorKind::ConnectionRefused))),
            WriteOutcome::NotSent { reason } if reason == "the connection was refused"
        ));
        assert!(matches!(
            write(failing(Some(std::io::ErrorKind::ConnectionReset))),
            WriteOutcome::Unknown { .. }
        ));
        assert!(matches!(write(failing(None)), WriteOutcome::Unknown { .. }));
    }

    /// Stored queries: the catalog decodes (a kind Studio does not know is `Other`), a read
    /// says it expects a read, and a write says it expects a write, drops any snapshot and, bound
    /// to a commit, goes to the conditional route with the commit in its header. The name is one
    /// path segment, whatever it holds.
    #[test]
    fn stored_queries_are_listed_and_invoked_by_name() {
        type Sent = (String, String, Option<String>, Value);
        let seen: Arc<std::sync::Mutex<Vec<Sent>>> = Arc::default();
        let log = seen.clone();
        let http = FakeHttpClient::create(move |request| {
            let log = log.clone();
            async move {
                let method = request.method().to_string();
                let path = request.uri().path().to_owned();
                let if_commit = request
                    .headers()
                    .get(IF_GRAPH_COMMIT_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let mut body = String::new();
                request.into_body().read_to_string(&mut body).await.unwrap();
                let body = serde_json::from_str(&body).unwrap_or(Value::Null);
                log.lock()
                    .unwrap()
                    .push((method.clone(), path.clone(), if_commit, body));
                let answer = if method == "GET" {
                    r#"{"queries":[{"name":"find","tool_name":"find","description":"People by name","mutation":false,"params":[{"name":"name","kind":"string","nullable":false},{"name":"tags","kind":"list","item_kind":"string","nullable":true},{"name":"v","kind":"vector","vector_dim":3,"nullable":false},{"name":"x","kind":"duration","nullable":true}]}]}"#
                } else if path.ends_with("/find") {
                    r#"{"query_name":"find","target":{"branch":"main"},"row_count":0,"columns":[],"rows":[],"graph_commit_id":"c1"}"#
                } else {
                    CHANGE
                };
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from(answer))
                    .unwrap())
            }
        });
        let client = client(http);

        let catalog = block_on(client.stored_queries("g")).unwrap();
        let find = &catalog.queries[0];
        assert_eq!(find.description.as_deref(), Some("People by name"));
        assert!(!find.mutation);
        let kinds: Vec<_> = find.params.iter().map(|p| p.kind).collect();
        assert_eq!(
            kinds,
            [
                ParamKind::String,
                ParamKind::List,
                ParamKind::Vector,
                ParamKind::Other
            ]
        );
        assert_eq!(find.params[1].item_kind, Some(ParamKind::String));
        assert_eq!(find.params[2].vector_dim, Some(3));

        let request = InvokeStoredQueryRequest {
            params: Some(serde_json::json!({ "name": "Ada" })),
            branch: None,
            snapshot: Some("c0".into()),
            expect_mutation: None,
        };
        let read = block_on(client.invoke_stored_read("g", "find", &request)).unwrap();
        assert_eq!(read.graph_commit_id.as_deref(), Some("c1"));

        let write = InvokeStoredQueryRequest {
            branch: Some("main".into()),
            ..request.clone()
        };
        let outcome = block_on(client.invoke_stored_write("g", "add/one", &write, Some("c1")));
        assert!(matches!(outcome, WriteOutcome::Committed(_)));

        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent[0].0, "GET");
        assert_eq!(sent[0].1, "/graphs/g/queries");
        assert_eq!(sent[1].1, "/graphs/g/queries/find");
        assert_eq!(
            sent[1].3,
            serde_json::json!({ "params": { "name": "Ada" }, "snapshot": "c0", "expect_mutation": false })
        );
        assert_eq!(sent[2].1, "/graphs/g/queries/add%2Fone/if-graph-commit");
        assert_eq!(sent[2].2.as_deref(), Some("c1"));
        assert_eq!(
            sent[2].3,
            serde_json::json!({ "params": { "name": "Ada" }, "branch": "main", "expect_mutation": true })
        );
    }

    /// The gateway's forward goes to the same root with Studio's token and contract, keeps the
    /// caller's other headers and body, streams the answer back, and never leaves the root.
    #[test]
    fn a_forward_carries_studios_credentials_and_stays_under_the_root() {
        type Sent = (String, String, Vec<(String, String)>, String);
        let seen: Arc<std::sync::Mutex<Vec<Sent>>> = Arc::default();
        let log = seen.clone();
        let http = FakeHttpClient::create(move |request| {
            let log = log.clone();
            async move {
                let uri = request.uri().to_string();
                let method = request.method().to_string();
                let headers = request
                    .headers()
                    .iter()
                    .filter(|(name, _)| {
                        matches!(
                            name.as_str(),
                            "authorization" | "omnigraph-http-api" | "content-type"
                        )
                    })
                    .map(|(name, value)| {
                        (name.to_string(), value.to_str().unwrap_or("").to_owned())
                    })
                    .collect();
                let mut body = String::new();
                request.into_body().read_to_string(&mut body).await.unwrap();
                log.lock().unwrap().push((method, uri, headers, body));
                Ok(Response::builder()
                    .status(201)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from("{\"ok\":true}"))
                    .unwrap())
            }
        });
        let client =
            GraphClient::new("http://127.0.0.1:8787/prefix", Some("t".into()), http).unwrap();
        let mut response = block_on(client.forward(ForwardRequest {
            method: Method::POST,
            path_and_query: "graphs/g/load/ndjson?branch=agent%2Fx&mode=append".into(),
            headers: vec![("content-type".into(), "application/x-ndjson".into())],
            body: AsyncBody::from_reader(futures::io::Cursor::new(b"{}\n".to_vec())),
        }))
        .unwrap();
        assert_eq!(response.status, StatusCode::CREATED);
        let mut body = String::new();
        block_on(response.body.read_to_string(&mut body)).unwrap();
        assert_eq!(body, "{\"ok\":true}");
        let (method, uri, mut headers, sent) = seen.lock().unwrap().remove(0);
        headers.sort();
        assert_eq!(method, "POST");
        assert_eq!(
            uri,
            "http://127.0.0.1:8787/prefix/graphs/g/load/ndjson?branch=agent%2Fx&mode=append"
        );
        assert_eq!(
            headers,
            vec![
                ("authorization".into(), "Bearer t".into()),
                ("content-type".into(), "application/x-ndjson".into()),
                ("omnigraph-http-api".into(), CONTRACT.into()),
            ]
        );
        assert_eq!(sent, "{}\n");

        for path in [
            "/graphs/g/query",
            "../graphs/g/query",
            "graphs/../../etc",
            "graphs/%2e%2e/query",
            "graphs/g/%2E./query",
            "graphs/g\t/query",
            "http://elsewhere/graphs",
            "graphs\\g",
        ] {
            let refused = block_on(client.forward(ForwardRequest {
                method: Method::GET,
                path_and_query: path.into(),
                headers: Vec::new(),
                body: AsyncBody::empty(),
            }));
            assert!(
                matches!(refused, Err(NoAnswer::Unsendable(_))),
                "{path} was sent"
            );
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_committed_write_needs_the_contract_header() {
        let outcome = block_on(client(respond(200, Some(CONTRACT), CHANGE)).mutate(
            "g",
            &ChangeRequest::default(),
            None,
        ));
        assert!(matches!(outcome, WriteOutcome::Committed(_)));

        let outcome = block_on(client(respond(200, None, CHANGE)).mutate(
            "g",
            &ChangeRequest::default(),
            None,
        ));
        assert!(matches!(outcome, WriteOutcome::Unknown { .. }));
    }

    /// A branch delete's receipt is read as `BranchDeleteOutput`: the branch it names and the
    /// actor. A success whose body is not one cannot say what was deleted, so it is unknown.
    #[test]
    fn a_branch_delete_reads_its_receipt() {
        let outcome = block_on(
            client(respond(
                200,
                Some(CONTRACT),
                r#"{"uri":"s3://graphs/g","name":"feature","actor_id":"act-dev"}"#,
            ))
            .delete_branch("g", "feature"),
        );
        let WriteOutcome::Committed(receipt) = outcome else {
            panic!("expected a receipt, got {outcome:?}");
        };
        assert_eq!(receipt.name, "feature");
        assert_eq!(receipt.uri, "s3://graphs/g");
        assert_eq!(receipt.actor_id.as_deref(), Some("act-dev"));

        let outcome =
            block_on(client(respond(200, Some(CONTRACT), "{}")).delete_branch("g", "feature"));
        assert!(
            matches!(outcome, WriteOutcome::Unknown { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_refusal_carries_the_servers_reason() {
        let body = r#"{"error":"graph commit moved","code":"precondition_failed","disposition":{"action":"refresh","effects":"none"},"precondition_failure":{"expected":"a","actual":"b"}}"#;
        let outcome = block_on(client(respond(412, Some(CONTRACT), body)).mutate(
            "g",
            &ChangeRequest::default(),
            Some("a"),
        ));
        match outcome {
            WriteOutcome::Refused(error) => {
                assert_eq!(error.status, 412);
                assert_eq!(error.body.precondition_failure.unwrap().expected, "a");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        // The same answer without the contract header came from something between Studio and
        // the server, which cannot say whether the server applied the write.
        let outcome = block_on(
            client(respond(503, None, r#"{"error":"upstream busy"}"#)).mutate(
                "g",
                &ChangeRequest::default(),
                None,
            ),
        );
        assert!(
            matches!(outcome, WriteOutcome::Unknown { .. }),
            "{outcome:?}"
        );
    }

    /// A commit's changes refused because it has no parent say so by the refusal's reason, as
    /// the server answers for the graph's first commit (`change_diff_refusal`, a typed field of
    /// the error); a schema boundary says its own and names its type, and another refusal none.
    #[test]
    fn a_refused_commit_comparison_names_its_reason() {
        use crate::api::{ChangeDiffRefusalOutput, ChangeDiffRefusalReason};
        let refused = |body: &'static str| {
            let client = client(respond(409, Some(CONTRACT), body));
            match block_on(client.commit_changes("g", "c0", None, 10)) {
                Err(ReadError::Refused(summary)) => (
                    summary.message,
                    summary.change_diff_refusal.map(|refusal| *refusal),
                ),
                other => panic!("expected a refusal, got {other:?}"),
            }
        };
        assert_eq!(
            refused(
                r#"{"error":"commit 'c0' has no first parent; bootstrap from an exact baseline","code":"conflict","change_diff_refusal":{"reason":"parentless_commit","graph_commit_id":"c0"}}"#
            )
            .1,
            Some(ChangeDiffRefusalOutput {
                reason: ChangeDiffRefusalReason::ParentlessCommit,
                graph_commit_id: "c0".into(),
                type_name: None,
            })
        );
        assert_eq!(
            refused(
                r#"{"error":"schema boundary","change_diff_refusal":{"reason":"schema_boundary","graph_commit_id":"c0","type_name":"Person"}}"#
            ),
            (
                "schema boundary".to_owned(),
                Some(ChangeDiffRefusalOutput {
                    reason: ChangeDiffRefusalReason::SchemaBoundary,
                    graph_commit_id: "c0".into(),
                    type_name: Some("Person".into()),
                })
            )
        );
        // A reason a later build may send still names the refusal.
        assert_eq!(
            refused(
                r#"{"error":"later","change_diff_refusal":{"reason":"retention_gap","graph_commit_id":"c0"}}"#
            )
            .1
            .map(|refusal| refusal.reason),
            Some(ChangeDiffRefusalReason::Other)
        );
        assert_eq!(refused(r#"{"error":"conflict"}"#).1, None);
    }

    /// A merge preview reads what a merge would do, or the conflicts that would stop it; any
    /// other answer is a refused read, never a merge outcome.
    #[test]
    fn a_merge_preview_reads_the_outcome_or_the_conflicts() {
        let merges = r#"{"source":"feature","target":"main","outcome":"fast_forward","commit":null,"dry_run":true}"#;
        let preview = block_on(
            client(respond(200, Some(CONTRACT), merges)).merge_preview("g", "feature", "main"),
        );
        assert!(matches!(
            preview,
            Ok(MergePreview::Merges(
                crate::api::BranchMergeOutcome::FastForward
            ))
        ));
        let conflicts = r#"{"error":"merge conflicts","code":"conflict","merge_conflicts":[{"entity_kind":"node","type_name":"Person","kind":"divergent_update","message":"changed on both","entity_id":"p1"}]}"#;
        let preview = block_on(
            client(respond(409, Some(CONTRACT), conflicts)).merge_preview("g", "feature", "main"),
        );
        match preview {
            Ok(MergePreview::Conflicts(conflicts)) => {
                assert_eq!(conflicts[0].entity_id.as_deref(), Some("p1"));
            }
            other => panic!("{other:?}"),
        }
        let refused = block_on(
            client(respond(403, Some(CONTRACT), r#"{"error":"denied"}"#))
                .merge_preview("g", "feature", "main"),
        );
        assert!(matches!(refused, Err(ReadError::Refused(summary)) if summary.status == 403));
    }

    #[test]
    fn a_server_error_on_a_write_is_unknown() {
        let outcome = block_on(
            client(respond(500, Some(CONTRACT), r#"{"error":"boom"}"#)).mutate(
                "g",
                &ChangeRequest::default(),
                None,
            ),
        );
        assert!(matches!(outcome, WriteOutcome::Unknown { .. }));
    }

    /// The access route asks about the branch, and the target only with it, and its answer
    /// decodes, an action a later build adds included.
    #[test]
    fn access_asks_about_a_branch_and_its_target() {
        let asked: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let seen = asked.clone();
        let http = FakeHttpClient::create(move |request| {
            seen.lock().unwrap().push(request.uri().to_string());
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from(
                        r#"{"graph_id":"g","actor_id":"act-ada","graph_wide":["read"],"branch":"main","target_branch":"fix","actions":["read","change","branch_create","later"]}"#,
                    ))
                    .unwrap())
            }
        });
        let client = client(http);
        let access = block_on(client.access("g", Some("main"), Some("fix"))).unwrap();
        assert_eq!(access.actor_id.as_deref(), Some("act-ada"));
        assert_eq!(
            access.actions,
            [
                AccessAction::Read,
                AccessAction::Change,
                AccessAction::BranchCreate,
                AccessAction::Other
            ]
        );
        block_on(client.access("g", None, Some("fix"))).unwrap();
        assert_eq!(
            *asked.lock().unwrap(),
            [
                "http://127.0.0.1:8787/graphs/g/access?branch=main&target_branch=fix",
                "http://127.0.0.1:8787/graphs/g/access",
            ]
        );
    }

    /// A Blob cell is null, or a reference, only by the server's word: a 404 or a redirect
    /// without the contract header comes from something in between, and the read fails as one
    /// that did not reach the server.
    #[test]
    fn a_blob_cell_is_null_only_by_the_servers_answer() {
        let cell = BlobCell {
            edge: false,
            type_name: "Doc".into(),
            id: "d1".into(),
            property: "file".into(),
        };
        let head = |contract| {
            block_on(client(respond(404, contract, "")).blob_head(
                "g",
                &cell,
                BlobAt::Branch("main"),
            ))
        };
        assert_eq!(head(Some(CONTRACT)).unwrap(), BlobMeta::Null);
        assert!(matches!(head(None), Err(ReadError::Transport(_))));
        let redirect = FakeHttpClient::create(|_| async {
            Ok(Response::builder()
                .status(302)
                .header("location", "s3://bucket/doc.pdf")
                .body(AsyncBody::empty())
                .unwrap())
        });
        assert!(matches!(
            block_on(client(redirect).blob_head("g", &cell, BlobAt::Branch("main"))),
            Err(ReadError::Transport(_))
        ));
    }

    #[test]
    fn a_read_checks_the_contract() {
        let result =
            block_on(client(respond(200, Some("0.13"), r#"{"branches":[]}"#)).branches("g"));
        assert!(matches!(
            result,
            Err(ReadError::ContractMismatch { served: Some(_) })
        ));
        let result = block_on(
            client(respond(200, Some(CONTRACT), r#"{"branches":["main"]}"#)).branches("g"),
        );
        assert_eq!(result.unwrap().branches, vec!["main"]);
    }

    /// How a read ended, its answer dropped.
    fn ended<T>(result: Result<T, ReadError>) -> Result<(), ReadError> {
        result.map(|_| ())
    }

    /// Every read the client makes, sent once to `http`: its name and how it ended.
    fn every_read(http: Arc<dyn HttpClient>) -> Vec<(&'static str, Result<(), ReadError>)> {
        let at = client(http);
        let graph = "g";
        let cell = cell("d1");
        let query = QueryRequest {
            query: "query q() {}".into(),
            ..QueryRequest::default()
        };
        let stored = InvokeStoredQueryRequest::default();
        let (main, commit) = (BlobAt::Branch("main"), BlobAt::Snapshot("c1"));
        block_on(async {
            vec![
                ("health", ended(at.health().await)),
                ("readiness", ended(at.readiness().await)),
                ("graphs", ended(at.graphs().await)),
                ("discover_graphs", ended(at.discover_graphs().await)),
                ("schema", ended(at.schema(graph).await)),
                ("snapshot", ended(at.snapshot(graph, "main").await)),
                ("query", ended(at.query(graph, &query).await)),
                ("stored_queries", ended(at.stored_queries(graph).await)),
                (
                    "invoke_stored_read",
                    ended(at.invoke_stored_read(graph, "find", &stored).await),
                ),
                ("access", ended(at.access(graph, Some("main"), None).await)),
                ("branches", ended(at.branches(graph).await)),
                (
                    "commit_page",
                    ended(at.commit_page(graph, "main", 10, None).await),
                ),
                ("commit", ended(at.commit(graph, "c1").await)),
                (
                    "commit_changes",
                    ended(at.commit_changes(graph, "c1", None, 10).await),
                ),
                ("diff", ended(at.diff(graph, "c1", "c2", None, 10).await)),
                (
                    "merge_base",
                    ended(at.merge_base(graph, "feature", "main").await),
                ),
                (
                    "merge_preview",
                    ended(at.merge_preview(graph, "feature", "main").await),
                ),
                (
                    "changes",
                    ended(
                        at.changes(graph, "main", FeedAt::Start("now"), &[], 10)
                            .await,
                    ),
                ),
                ("blob_head", ended(at.blob_head(graph, &cell, main).await)),
                (
                    "blob_bytes",
                    ended(at.blob_bytes(graph, &cell, commit, 0..10).await),
                ),
            ]
        })
    }

    /// An error answer is the server's refusal only when it carries the contract Studio speaks,
    /// on every read. One without the header, a gateway's 404 or 502, came from something
    /// between Studio and the server, and the read fails as one that did not reach the server:
    /// a gateway's 404 on a commit is not the server saying it does not serve the commit, and
    /// its 502 is not an answer that clears a graph's unavailable mark. One naming another
    /// contract is a server Studio cannot read.
    #[test]
    fn an_error_is_the_servers_refusal_only_with_its_contract_header() {
        for status in [404, 502] {
            for (read, ended) in every_read(respond(status, None, r#"{"error":"not here"}"#)) {
                match ended {
                    Err(ReadError::Transport(reason)) => {
                        assert!(reason.contains(&status.to_string()), "{read}: {reason}")
                    }
                    other => panic!("{read}, answered {status} without the header: {other:?}"),
                }
            }
        }
        let denied = r#"{"error":"denied","code":"forbidden"}"#;
        for (read, ended) in every_read(respond(403, Some(CONTRACT), denied)) {
            assert!(
                matches!(&ended, Err(ReadError::Refused(refusal))
                    if refusal.status == 403 && refusal.message == "denied"),
                "{read}: {ended:?}"
            );
        }
        for (read, ended) in every_read(respond(404, Some("0.13"), r#"{"error":"not found"}"#)) {
            assert!(
                matches!(&ended, Err(ReadError::ContractMismatch { served: Some(served) })
                    if served == "0.13"),
                "{read}: {ended:?}"
            );
        }

        // The server's own 404 on a commit is its refusal.
        let not_found = r#"{"error":"commit not found","code":"not_found"}"#;
        let commit = block_on(client(respond(404, Some(CONTRACT), not_found)).commit("g", "c9"));
        assert!(
            matches!(&commit, Err(ReadError::Refused(refusal))
                if refusal.status == 404 && refusal.code == Some(crate::api::ErrorCode::NotFound)),
            "{commit:?}"
        );
        let gateway =
            block_on(client(respond(502, None, "<html>bad gateway</html>")).commit("g", "c9"));
        assert_eq!(
            gateway.unwrap_err().to_string(),
            "Could not reach the server: something between Studio and the server answered 502 \
             Bad Gateway"
        );
    }

    /// Answers 429 (`Retry-After: 0`) `limited` times, then 200; counts the requests.
    fn rate_limited(
        limited: usize,
        body: &'static str,
    ) -> (Arc<dyn HttpClient>, Arc<std::sync::atomic::AtomicUsize>) {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        let http = FakeHttpClient::create(move |_request| {
            let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                let builder = Response::builder().header(CONTRACT_HEADER, CONTRACT);
                let response = if n < limited {
                    builder
                        .status(429)
                        .header("retry-after", "0")
                        .body(AsyncBody::from(r#"{"error":"slow down"}"#))
                } else {
                    builder.status(200).body(AsyncBody::from(body))
                };
                Ok(response.unwrap())
            }
        });
        (http, count)
    }

    #[test]
    fn a_rate_limited_read_waits_and_goes_again_a_bounded_number_of_times() {
        let (http, count) = rate_limited(2, r#"{"branches":["main"]}"#);
        let result = block_on(client(http).branches("g"));
        assert_eq!(result.unwrap().branches, vec!["main"]);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);

        let (http, count) = rate_limited(10, r#"{"branches":["main"]}"#);
        let result = block_on(client(http).branches("g"));
        assert!(matches!(result, Err(ReadError::Refused(e)) if e.status == 429));
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1 + READ_RETRIES
        );
    }

    #[test]
    fn a_connection_keeps_a_bounded_number_of_reads_on_the_wire() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (current, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (now, peak) = (current.clone(), most.clone());
        let http = FakeHttpClient::create(move |_request| {
            let (now, peak) = (now.clone(), peak.clone());
            async move {
                let running = now.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(running, Ordering::SeqCst);
                smol::Timer::after(Duration::from_millis(5)).await;
                now.fetch_sub(1, Ordering::SeqCst);
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from(r#"{"branches":["main"]}"#))
                    .unwrap())
            }
        });
        let client = client(http);
        // A clone shares the bound: views hold clones of their connection's client.
        let other = client.clone();
        let reads = (0..40).map(|i| {
            let client = if i % 2 == 0 {
                client.clone()
            } else {
                other.clone()
            };
            async move { client.branches("g").await }
        });
        let results = block_on(futures::future::join_all(reads));
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(most.load(Ordering::SeqCst), READ_CONCURRENCY);
    }

    #[test]
    fn a_rate_limited_write_is_never_sent_again() {
        let (http, count) = rate_limited(1, CHANGE);
        let outcome = block_on(client(http).mutate("g", &ChangeRequest::default(), None));
        assert!(!matches!(outcome, WriteOutcome::Committed(_)));
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn paths_and_queries_are_encoded() {
        let client = client(respond(200, Some(CONTRACT), "{}"));
        let url = client.url(
            &["graphs", "g", "branches", "feature/x y"],
            &[("branch", "a&b")],
        );
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:8787/graphs/g/branches/feature%2Fx%20y?branch=a%26b"
        );
    }

    #[test]
    fn a_refused_historical_read_names_its_type_or_property() {
        let gap = |message: &str| {
            ApiErrorSummary {
                status: 400,
                message: message.into(),
                code: None,
                diagnostic: None,
                change_diff_refusal: None,
                challenge: None,
            }
            .historical_gap()
        };
        assert_eq!(
            gap("property 'actual_end' of node type 'Event' does not exist at this snapshot"),
            Some(HistoricalGap::Property {
                owner: "Event".into(),
                property: "actual_end".into(),
                then: PropertyThen::Absent,
            })
        );
        assert_eq!(
            gap(
                "property 'remark' of node type 'Person' is named 'memo' at this snapshot; a historical query cannot read a property across a rename"
            ),
            Some(HistoricalGap::Property {
                owner: "Person".into(),
                property: "remark".into(),
                then: PropertyThen::Named("memo".into()),
            })
        );
        assert_eq!(
            gap("property 'since' of edge type 'Knows' does not exist at this snapshot"),
            Some(HistoricalGap::Property {
                owner: "Knows".into(),
                property: "since".into(),
                then: PropertyThen::Absent,
            })
        );
        assert_eq!(
            gap(
                "property 'note' of node type 'Doc' has no persisted property-lifetime witness at this snapshot"
            ),
            Some(HistoricalGap::Property {
                owner: "Doc".into(),
                property: "note".into(),
                then: PropertyThen::Unwitnessed,
            })
        );
        assert_eq!(
            gap("edge type 'Knows' does not exist at this snapshot"),
            Some(HistoricalGap::Type {
                name: "Knows".into()
            })
        );
        assert_eq!(
            gap("node type 'Reservation' does not exist at this snapshot"),
            Some(HistoricalGap::Type {
                name: "Reservation".into()
            })
        );
        assert_eq!(gap("parse error: expected not_expr"), None);
    }

    /// A server for Blob reads: answers by `id`, and records each request's `Range` and
    /// redirect policy.
    fn blob_server() -> (Arc<dyn HttpClient>, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let http = FakeHttpClient::create(move |request| {
            let log = log.clone();
            async move {
                let url = request.uri().to_string();
                let range = request
                    .headers()
                    .get("range")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                let follows = request.extensions().get::<RedirectPolicy>().cloned();
                log.lock()
                    .unwrap()
                    .push(format!("{} {range} {:?}", request.method(), follows));
                let ok = || Response::builder().header(CONTRACT_HEADER, CONTRACT);
                let response = if url.contains("id=bytes") {
                    if request.method() == Method::HEAD {
                        ok().status(200)
                            .header("content-length", "10")
                            .header("etag", "\"e1\"")
                            .body(AsyncBody::empty())
                    } else {
                        ok().status(206)
                            .header("content-range", "bytes 2-4/10")
                            .body(AsyncBody::from(vec![2u8, 3, 4]))
                    }
                } else if url.contains("id=external") {
                    ok().status(302)
                        .header("location", "s3://bucket/report.pdf")
                        .body(AsyncBody::empty())
                } else if url.contains("id=gone") {
                    ok().status(404).body(AsyncBody::from(
                        r#"{"error":"no Note with id gone at this commit"}"#,
                    ))
                } else {
                    ok().status(404).body(AsyncBody::empty())
                };
                Ok(response.unwrap())
            }
        });
        (http, seen)
    }

    fn cell(id: &str) -> BlobCell {
        BlobCell {
            edge: false,
            type_name: "Note".into(),
            id: id.into(),
            property: "attachment".into(),
        }
    }

    #[test]
    fn a_blob_cell_is_managed_external_or_null() {
        let (http, seen) = blob_server();
        let client = client(http);
        let at = BlobAt::Branch("main");
        assert_eq!(
            block_on(client.blob_head("g", &cell("bytes"), at)).unwrap(),
            BlobMeta::Managed {
                size: 10,
                etag: Some("\"e1\"".into())
            }
        );
        assert_eq!(
            block_on(client.blob_head("g", &cell("external"), at)).unwrap(),
            BlobMeta::External {
                uri: "s3://bucket/report.pdf".into()
            }
        );
        assert_eq!(
            block_on(client.blob_head("g", &cell("empty"), at)).unwrap(),
            BlobMeta::Null
        );
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .all(|request| request.ends_with("Some(NoFollow)")),
            "no request follows a redirect: {:?}",
            seen.lock().unwrap()
        );
    }

    /// Bytes without end, counting how many were read.
    struct Endless(Arc<std::sync::atomic::AtomicUsize>);

    impl futures::AsyncRead for Endless {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buffer: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            buffer.fill(7);
            self.0
                .fetch_add(buffer.len(), std::sync::atomic::Ordering::SeqCst);
            std::task::Poll::Ready(Ok(buffer.len()))
        }
    }

    /// A body whose length is said is read into one allocation of that length: a whole range, or
    /// what is kept of a value read from its start, is not grown to twice its size at its end.
    #[test]
    fn a_body_of_known_length_is_read_without_growing() {
        // Not a power of two, which growth by doubling could land on.
        let len = (1 << 20) + 3;
        let whole = block_on(read_body(
            AsyncBody::from(vec![7_u8; len]),
            0,
            len,
            Some(len as u64),
        ))
        .unwrap();
        assert_eq!(whole.len(), len);
        assert!(whole.capacity() <= len + 1, "{}", whole.capacity());
        let cut = block_on(read_body(
            AsyncBody::from(vec![7_u8; len]),
            100,
            len / 2,
            Some(len as u64),
        ))
        .unwrap();
        assert_eq!(cut.len(), len / 2);
        assert!(cut.capacity() <= len / 2 + 1, "{}", cut.capacity());

        // No body follows a HEAD, a 204 or a 304, whatever length the header names.
        let mut headers = HeaderMap::new();
        headers.insert("content-length", HeaderValue::from_static("10485760"));
        assert_eq!(
            body_length(false, StatusCode::OK, &headers),
            Some(10_485_760)
        );
        assert_eq!(body_length(true, StatusCode::OK, &headers), None);
        assert_eq!(body_length(false, StatusCode::NO_CONTENT, &headers), None);
        assert_eq!(body_length(false, StatusCode::NOT_MODIFIED, &headers), None);
    }

    /// A server that ignores Range sends the whole value: the range is cut from it, and what
    /// follows the range is never read.
    #[test]
    fn a_range_answered_with_the_whole_value_reads_no_further_than_the_range() {
        let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = read.clone();
        let http = FakeHttpClient::create(move |_| {
            let counted = counted.clone();
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from_reader(Endless(counted)))
                    .unwrap())
            }
        });
        let client = client(http);
        let bytes =
            block_on(client.blob_bytes("g", &cell("whole"), BlobAt::Branch("main"), 10..14));
        assert_eq!(bytes.unwrap(), [7, 7, 7, 7]);
        assert_eq!(read.load(std::sync::atomic::Ordering::SeqCst), 14);
    }

    /// A query's answer past its cap is refused whole, read one byte past the cap and no
    /// further; one of exactly the cap decodes. Every query and stored read takes the cap, and
    /// every other read its own (a change-feed page here).
    #[test]
    fn a_query_answer_over_its_cap_is_refused_and_read_no_further() {
        let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = read.clone();
        let http = FakeHttpClient::create(move |request| {
            let counted = counted.clone();
            let body = if request.uri().path().ends_with("/small") {
                AsyncBody::from(r#"{"a":1}"#)
            } else {
                AsyncBody::from_reader(Endless(counted))
            };
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(body)
                    .unwrap())
            }
        });
        let client = client(http);
        let read_count = || read.swap(0, std::sync::atomic::Ordering::SeqCst);
        let capped = block_on(client.read_capped::<Value>(
            Method::POST,
            &["graphs", "g", "query"],
            &[],
            None,
            64,
        ));
        assert!(matches!(capped, Err(ReadError::TooLarge { limit: 64 })));
        assert_eq!(read_count(), 65);
        let whole = block_on(client.read_capped::<Value>(
            Method::GET,
            &["graphs", "g", "small"],
            &[],
            None,
            r#"{"a":1}"#.len(),
        ));
        assert_eq!(whole.unwrap(), serde_json::json!({"a": 1}));

        let query = block_on(client.query("g", &QueryRequest::default()));
        assert!(matches!(
            query,
            Err(ReadError::TooLarge {
                limit: QUERY_ANSWER_MAX
            })
        ));
        assert_eq!(read_count(), QUERY_ANSWER_MAX + 1);
        let stored = block_on(client.invoke_stored_read(
            "g",
            "people",
            &InvokeStoredQueryRequest::default(),
        ));
        assert!(matches!(
            stored,
            Err(ReadError::TooLarge {
                limit: QUERY_ANSWER_MAX
            })
        ));
        assert_eq!(
            ReadError::TooLarge {
                limit: QUERY_ANSWER_MAX
            }
            .to_string(),
            "The answer is over 32 MB, the most Studio reads at once"
        );
        read_count();
        let page = block_on(client.changes("g", "main", FeedAt::Start("after:c1"), &[], 1_000));
        assert!(matches!(
            page,
            Err(ReadError::TooLarge {
                limit: READ_ANSWER_MAX
            })
        ));
        assert_eq!(read_count(), READ_ANSWER_MAX + 1);
    }

    #[test]
    fn blob_bytes_are_read_by_range_and_external_values_are_not_fetched() {
        let (http, seen) = blob_server();
        let client = client(http);
        let at = BlobAt::Snapshot("c1");
        assert_eq!(
            block_on(client.blob_bytes("g", &cell("bytes"), at, 2..5)).unwrap(),
            [2, 3, 4]
        );
        assert!(seen.lock().unwrap()[0].starts_with("GET bytes=2-4"));
        // The server answers bytes 2-4 whatever is asked: from 3 on, those are not the bytes
        // asked for.
        assert!(matches!(
            block_on(client.blob_bytes("g", &cell("bytes"), at, 3..6)),
            Err(ReadError::Decode(message)) if message.contains("bytes 2-4/10")
        ));
        assert!(content_range_starts_at(" bytes 3-5/10", 3));
        assert!(!content_range_starts_at("bytes */10", 3));
        assert!(matches!(
            block_on(client.blob_bytes("g", &cell("external"), at, 0..5)),
            Err(ReadError::Refused(e)) if e.status == 302
        ));
        assert_eq!(
            block_on(client.blob_bytes("g", &cell("bytes"), at, 4..4)).unwrap(),
            Vec::<u8>::new(),
            "an empty range sends nothing"
        );
        assert_eq!(seen.lock().unwrap().len(), 3);
        // A refusal's message reads whole, however few bytes of the value were asked for.
        assert!(matches!(
            block_on(client.blob_bytes("g", &cell("gone"), at, 40..44)),
            Err(ReadError::Refused(e)) if e.message == "no Note with id gone at this commit"
        ));
    }

    /// One Blob write as the server saw it: method, query, every `content-type`, the
    /// `If-Match`, and the body.
    type BlobWriteSeen = (String, String, Vec<String>, Option<String>, Vec<u8>);

    /// A server for Blob writes: answers each with `status` and `body`, contract header and
    /// all, and records what it was sent.
    fn blob_write_server(
        status: u16,
        body: &'static str,
    ) -> (
        Arc<dyn HttpClient>,
        Arc<std::sync::Mutex<Vec<BlobWriteSeen>>>,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let http = FakeHttpClient::create(move |request| {
            let log = log.clone();
            async move {
                let method = request.method().to_string();
                let query = request.uri().query().unwrap_or_default().to_owned();
                let types = request
                    .headers()
                    .get_all("content-type")
                    .iter()
                    .filter_map(|value| value.to_str().ok().map(str::to_owned))
                    .collect();
                let if_match = request
                    .headers()
                    .get("if-match")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.unwrap();
                log.lock()
                    .unwrap()
                    .push((method, query, types, if_match, bytes));
                Ok(Response::builder()
                    .status(status)
                    .header(CONTRACT_HEADER, CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        (http, seen)
    }

    const BLOB_PUT: &str = r#"{"selector":{"entity":"node","type":"Note","id":"n1","property":"attachment"},"branch":"main","kind":"managed","size":3,"etag":"\"e2\"","commit":{"graph_commit_id":"c2","graph_manifest_version":2,"created_at":0,"parent_commit_id":"c1","kind":"published"}}"#;

    /// A put sends the bytes once, as raw bytes on the branch, bound to the ETag it was given;
    /// its receipt names the commit and the new value.
    #[test]
    fn a_blob_put_sends_raw_bytes_bound_to_the_etag_read() {
        let (http, seen) = blob_write_server(200, BLOB_PUT);
        let client = client(http);
        let outcome =
            block_on(client.blob_put("g", "feature/x", &cell("n1"), Some("\"e1\""), vec![1, 2, 3]));
        let WriteOutcome::Committed(receipt) = outcome else {
            panic!("committed: {outcome:?}");
        };
        assert_eq!(receipt.commit.unwrap().graph_commit_id, "c2");
        assert_eq!(
            (receipt.kind, receipt.size, receipt.etag.as_deref()),
            (crate::api::BlobWriteState::Managed, Some(3), Some("\"e2\""))
        );
        let (method, query, types, if_match, body) = seen.lock().unwrap().remove(0);
        assert_eq!(method, "PUT");
        assert_eq!(
            query,
            "entity=node&type=Note&id=n1&property=attachment&branch=feature%2Fx"
        );
        assert_eq!(types, ["application/octet-stream"], "one type, not JSON");
        assert_eq!(if_match.as_deref(), Some("\"e1\""));
        assert_eq!(body, [1, 2, 3]);
    }

    /// A clear goes unbound when given no ETag, and a cell already null publishes nothing.
    #[test]
    fn a_blob_clear_of_a_null_cell_commits_nothing() {
        let receipt = r#"{"selector":{"entity":"edge","type":"Cites","id":"e9","property":"scan"},"branch":"main","kind":"null","commit":null}"#;
        let (http, seen) = blob_write_server(200, receipt);
        let client = client(http);
        let mut edge = cell("e9");
        edge.edge = true;
        let outcome = block_on(client.blob_clear("g", "main", &edge, None));
        assert!(
            matches!(&outcome, WriteOutcome::Committed(receipt) if receipt.commit.is_none()),
            "{outcome:?}"
        );
        let (method, query, types, if_match, body) = seen.lock().unwrap().remove(0);
        assert_eq!(method, "DELETE");
        assert!(query.starts_with("entity=edge&"), "{query}");
        assert!(types.is_empty() && if_match.is_none() && body.is_empty());
    }

    /// A value changed since it was read refuses the write with 412, and the refusal names
    /// the cell's current ETag; nothing is sent again.
    #[test]
    fn a_changed_blob_refuses_the_write_with_its_current_etag() {
        let changed = r#"{"error":"If-Match did not hold for Note.attachment","code":"precondition_failed","disposition":{"action":"refresh","effects":"none"},"blob_precondition_failure":{"current_etag":"\"e3\""}}"#;
        let (http, seen) = blob_write_server(412, changed);
        let outcome = block_on(client(http).blob_put(
            "g",
            "main",
            &cell("n1"),
            Some("\"e1\""),
            b"new".to_vec(),
        ));
        match outcome {
            WriteOutcome::Refused(error) => {
                assert_eq!(error.status, 412);
                let failure = error.body.blob_precondition_failure.unwrap();
                assert_eq!(failure.current_etag.as_deref(), Some("\"e3\""));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(seen.lock().unwrap().len(), 1, "sent once");
    }

    /// A put whose answer is lost, or comes back without the server's contract header or as a
    /// gateway's error, is unknown: it may have been stored.
    #[test]
    fn a_blob_put_without_the_servers_answer_is_unknown() {
        let lost: Arc<dyn HttpClient> =
            FakeHttpClient::create(|_| async { Err(anyhow::anyhow!("connection reset")) });
        let put = |http: Arc<dyn HttpClient>| {
            block_on(client(http).blob_put("g", "main", &cell("n1"), None, vec![0; 4]))
        };
        assert!(matches!(put(lost), WriteOutcome::Unknown { .. }));
        assert!(matches!(
            put(respond(200, None, BLOB_PUT)),
            WriteOutcome::Unknown { .. }
        ));
        assert!(matches!(
            put(respond(502, None, "bad gateway")),
            WriteOutcome::Unknown { .. }
        ));
        // Too large for the server is its refusal: nothing was stored.
        assert!(matches!(
            put(respond(
                413,
                Some(CONTRACT),
                r#"{"error":"Blob put exceeds 1048576 bytes","code":"payload_too_large","disposition":{"action":"fix","effects":"none"}}"#
            )),
            WriteOutcome::Refused(error) if error.status == 413
        ));
    }

    #[test]
    fn a_server_address_gets_a_scheme_and_a_bare_word_is_refused() {
        assert_eq!(
            server_root("localhost:8080").as_deref(),
            Ok("http://localhost:8080")
        );
        assert_eq!(
            server_root(" 127.0.0.1:8787/ ").as_deref(),
            Ok("http://127.0.0.1:8787")
        );
        assert_eq!(
            server_root("[::1]:8787").as_deref(),
            Ok("http://[::1]:8787")
        );
        assert_eq!(
            server_root("graph.example.com").as_deref(),
            Ok("https://graph.example.com")
        );
        assert_eq!(
            server_root("https://graph.example.com/base/").as_deref(),
            Ok("https://graph.example.com/base")
        );
        // Only a loopback host goes without TLS, not a name that begins like one.
        for remote in [
            "127.example.com:8787",
            "[::ffff:203.0.113.7]:8787",
            "localhost.example",
        ] {
            assert!(
                server_root(remote).unwrap().starts_with("https://"),
                "{remote}"
            );
        }
        assert!(server_root("0.0.0.0:8080").unwrap().starts_with("http://"));
        assert!(server_root("").is_err());
        assert!(server_root("ftp://graph.example.com").is_err());
        assert!(server_root("mailto:someone@example.com").is_err());
    }

    #[test]
    fn a_transport_error_names_its_cause_and_never_the_url() {
        let refused = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "Connection refused (os error 61)",
        ))
        .context("tcp connect error")
        .context(
            "error sending request for url (http://127.0.0.1:8787/graphs/knowledge/blob?type=Person&id=person-000123&property=photo)",
        );
        let described = describe_transport(&refused);
        assert_eq!(described, "the connection was refused");
        let other = anyhow::anyhow!(
            "odd failure talking to https://graph.example.com/graphs/g/blob?id=row-9"
        )
        .context(
            "error sending request for url (https://graph.example.com/graphs/g/blob?id=row-9)",
        );
        let described = describe_transport(&other);
        assert_eq!(described, "odd failure talking to …");
        assert!(!described.contains("row-9"));
        assert_eq!(
            ReadError::Transport(describe_transport(&refused)).to_string(),
            "Could not reach the server: the connection was refused"
        );
    }

    /// A timeout says whether the server was reached: reqwest's chain for a request the server
    /// had not answered in time, and for a connection that did not open in time (hyper's
    /// `Connect` layer under it), at `zed-reqwest` `33bc764` and `hyper-util` 0.1.17.
    #[test]
    fn a_timeout_says_whether_the_server_was_reached() {
        let url = "error sending request for url (https://graph.example.com/graphs/g/diff?from=a)";
        let unanswered =
            anyhow::Error::new(std::io::Error::other("operation timed out")).context(url);
        let unopened = anyhow::Error::new(std::io::Error::other("operation timed out"))
            .context("client error (Connect)")
            .context(url);
        let unanswered = ReadError::Transport(describe_transport(&unanswered));
        let unopened = ReadError::Transport(describe_transport(&unopened));
        assert!(unanswered.timed_out());
        assert_eq!(
            unanswered.to_string(),
            "Could not reach the server: the request timed out"
        );
        assert!(!unopened.timed_out());
        assert_eq!(
            unopened.to_string(),
            "Could not reach the server: the connection timed out"
        );
        assert!(!ReadError::Transport("the connection was refused".into()).timed_out());
    }

    #[test]
    fn a_client_refuses_a_root_that_cannot_take_a_path() {
        // `localhost:8080` parses with `localhost` as its scheme; building a URL on it used to
        // panic. The client is refused instead, with an error that says why.
        let http = respond(200, None, "{}");
        let error = GraphClient::new("localhost:8080", None, http.clone()).unwrap_err();
        assert!(error.to_string().contains("http://"), "{error}");
        assert!(GraphClient::new("http://localhost:8080", None, http).is_ok());
    }

    /// A 401 carries the server's `Bearer` challenge (Omnigraph #1122): whether it refused the
    /// token sent, and where its protected-resource metadata is; another status, or a 401
    /// without the header (an older server), carries none.
    #[test]
    fn a_401_carries_its_bearer_challenge() {
        let refused = |status: u16, challenge: Option<&'static str>| {
            let http = FakeHttpClient::create(move |_request| async move {
                let mut builder = Response::builder()
                    .status(status)
                    .header(CONTRACT_HEADER, CONTRACT);
                if let Some(challenge) = challenge {
                    builder = builder.header("www-authenticate", challenge);
                }
                Ok(builder
                    .body(AsyncBody::from(
                        r#"{"error":"unauthorized","code":"unauthorized"}"#,
                    ))
                    .unwrap())
            });
            match block_on(client(http).graphs()) {
                Err(ReadError::Refused(summary)) => summary.challenge,
                other => panic!("{other:?}"),
            }
        };
        let challenge = refused(
            401,
            Some(
                r#"Bearer error="invalid_token", resource_metadata="https://g.test/.well-known/oauth-protected-resource/mcp""#,
            ),
        )
        .unwrap();
        assert!(challenge.token_refused());
        assert_eq!(
            challenge.resource_metadata(),
            Some("https://g.test/.well-known/oauth-protected-resource/mcp")
        );
        let none_sent = refused(401, Some("Bearer")).unwrap();
        assert!(!none_sent.token_refused());
        assert_eq!(none_sent.resource_metadata(), None);
        assert_eq!(refused(401, None), None);
        assert_eq!(refused(403, Some("Bearer")), None);

        // Several challenges in one value, a quoted escape, and names in any case.
        let all = Challenge::parse_all(
            r#"Basic realm="a, b", Bearer ERROR=invalid_token, error_description="say \"no\"""#,
        );
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].scheme, "Basic");
        assert_eq!(all[0].param("realm"), Some("a, b"));
        assert_eq!(all[1].scheme, "Bearer");
        assert!(all[1].token_refused());
        assert_eq!(all[1].param("error_description"), Some(r#"say "no""#));

        // A token68 payload before a Bearer challenge keeps the Bearer one, and is no parameter.
        let all = Challenge::parse_all(r#"Negotiate abc==, Bearer error="invalid_token""#);
        assert_eq!(all.len(), 2, "{all:?}");
        assert_eq!(all[0].scheme, "Negotiate");
        assert!(all[0].params.is_empty());
        assert_eq!(all[1].scheme, "Bearer");
        assert!(all[1].token_refused());
        let one = Challenge::parse_all("Basic dXNlcg=, Bearer");
        assert_eq!(one.len(), 2, "{one:?}");
        assert!(one[0].params.is_empty() && one[1].params.is_empty());
    }
}
