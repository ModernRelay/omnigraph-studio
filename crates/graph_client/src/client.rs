//! The HTTP client for one OmniGraph server (spec 003).
//!
//! Reads return `Result<T, ReadError>`. Writes return a [`WriteOutcome`] and are never retried
//! here (constitution III).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use futures::AsyncReadExt;
use http_client::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use http_client::{AsyncBody, HttpClient, HttpRequestExt, RedirectPolicy};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use crate::api::{
    BranchCreateOutput, BranchCreateRequest, BranchListOutput, BranchMergeOutput,
    BranchMergeRequest, ChangeFeedOutput, ChangeOutput, ChangeRequest, CommitListOutput,
    ErrorOutput, GraphListResponse, HealthOutput, QueryRequest, ReadOutput, ReadinessOutput,
    SchemaOutput, SnapshotOutput,
};
use crate::{ApiError, CONTRACT_HEADER, IF_GRAPH_COMMIT_HEADER, SUPPORTED_CONTRACTS, WriteOutcome};

const READ_TIMEOUT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(300);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// How many times a read refused with 429 is tried again (spec 003). Writes never are.
const READ_RETRIES: usize = 2;
/// The longest `Retry-After` a read waits; a longer one is shown as the refusal.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// The wait when a 429 names none.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);

/// Why a read failed.
#[derive(Debug, Clone)]
pub enum ReadError {
    /// The request did not complete: connection refused, timeout, TLS, DNS.
    Transport(String),
    /// The server answered without the contract header, or with a contract Studio does not
    /// speak.
    ContractMismatch { served: Option<String> },
    /// The server refused the read.
    Refused(ApiErrorSummary),
    /// The body did not match the 0.13 contract.
    Decode(String),
}

/// A cloneable copy of a refusal, for showing in the UI.
#[derive(Debug, Clone)]
pub struct ApiErrorSummary {
    pub status: u16,
    pub message: String,
    pub code: Option<crate::api::ErrorCode>,
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
                SUPPORTED_CONTRACTS.join(", ")
            ),
            ReadError::ContractMismatch { served: None } => {
                write!(f, "The server did not say which contract it speaks")
            }
            ReadError::Refused(summary) => write!(f, "{} ({})", summary.message, summary.status),
            ReadError::Decode(message) => write!(f, "Unexpected response: {message}"),
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
        self.contract
            .as_deref()
            .is_some_and(|served| SUPPORTED_CONTRACTS.contains(&served))
    }
}

struct RawResponse {
    status: StatusCode,
    contract: Option<String>,
    /// `Retry-After` in seconds; the HTTP-date form is not used by OmniGraph.
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
pub const BLOB_READ_MAX: u64 = 16 * 1024 * 1024;

/// A client for one server. Cheap to clone.
#[derive(Clone)]
pub struct GraphClient {
    base: Url,
    token: Option<Arc<str>>,
    http: Arc<dyn HttpClient>,
}

impl std::fmt::Debug for GraphClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the token.
        f.debug_struct("GraphClient")
            .field("base", &self.base.as_str())
            .finish()
    }
}

impl GraphClient {
    /// `base` is the server root, for example `http://127.0.0.1:8787`.
    pub fn new(
        base: &str,
        token: Option<String>,
        http: Arc<dyn HttpClient>,
    ) -> anyhow::Result<Self> {
        let mut base = Url::parse(base)?;
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Ok(Self {
            base,
            token: token.map(Into::into),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = self.base.clone();
        {
            let mut path = url.path_segments_mut().expect("http URL has a path");
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
    ) -> Result<RawResponse, String> {
        // No route answers with a redirect to follow; a Blob's external reference answers 302
        // with a URI Studio shows and never fetches.
        let mut builder = Request::builder()
            .method(method)
            .uri(url.as_str())
            .header(CONTRACT_HEADER, SUPPORTED_CONTRACTS[0])
            .follow_redirects(RedirectPolicy::NoFollow)
            .timeout(timeout);
        if let Some(token) = &self.token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(body.map(AsyncBody::from).unwrap_or_else(AsyncBody::empty))
            .map_err(|error| error.to_string())?;
        let response = self
            .http
            .send(request)
            .await
            .map_err(|error| error.to_string())?;
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
        let mut bytes = Vec::new();
        response
            .into_body()
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;
        Ok(RawResponse {
            status,
            contract,
            retry_after,
            headers,
            body: bytes,
        })
    }

    /// Sends a read; one refused with 429 waits as asked and goes again.
    async fn fetch(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        headers: &[(&str, &str)],
    ) -> Result<RawResponse, ReadError> {
        let mut retries = 0;
        loop {
            let raw = self
                .send(
                    method.clone(),
                    url.clone(),
                    body.clone(),
                    headers,
                    READ_TIMEOUT,
                )
                .await
                .map_err(ReadError::Transport)?;
            // A read changes nothing, so a rate-limited one waits as asked and goes again.
            if raw.status == StatusCode::TOO_MANY_REQUESTS && retries < READ_RETRIES {
                let wait = raw.retry_after.unwrap_or(DEFAULT_RETRY_AFTER);
                if wait <= MAX_RETRY_AFTER {
                    retries += 1;
                    smol::Timer::after(wait).await;
                    continue;
                }
            }
            return Ok(raw);
        }
    }

    async fn read<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<T, ReadError> {
        let url = self.url(segments, query);
        let raw = self.fetch(method, url, body, &[]).await?;
        if raw.status.is_success() {
            check_contract(&raw)?;
            serde_json::from_slice(&raw.body).map_err(|error| ReadError::Decode(error.to_string()))
        } else {
            Err(ReadError::Refused(summary(raw.status, &raw.body)))
        }
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
    /// is null.
    pub async fn blob_head(
        &self,
        graph: &str,
        cell: &BlobCell,
        at: BlobAt<'_>,
    ) -> Result<BlobMeta, ReadError> {
        let url = self.blob_url(graph, cell, at);
        let raw = self.fetch(Method::HEAD, url, None, &[]).await?;
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
            301 | 302 | 303 | 307 | 308 => match header("location") {
                Some(uri) => Ok(BlobMeta::External { uri }),
                None => Err(ReadError::Decode("a redirect without a location".into())),
            },
            404 => Ok(BlobMeta::Null),
            _ => Err(ReadError::Refused(summary(raw.status, &raw.body))),
        }
    }

    /// `GET /graphs/{id}/blob` with a `Range`: bytes `range` of a managed value, at most
    /// [`BLOB_READ_MAX`]. An external reference is refused; Studio never fetches it.
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
        let raw = self
            .fetch(Method::GET, url, None, &[("range", &bytes)])
            .await?;
        match raw.status.as_u16() {
            200 | 206 => {
                check_contract(&raw)?;
                let mut body = raw.body;
                // A server that ignores Range sends the whole value from the start.
                if raw.status == StatusCode::OK {
                    body = body
                        .get(range.start as usize..)
                        .unwrap_or_default()
                        .to_vec();
                }
                body.truncate((end - range.start) as usize);
                Ok(body)
            }
            301 | 302 | 303 | 307 | 308 => Err(ReadError::Refused(ApiErrorSummary {
                status: raw.status.as_u16(),
                message: "The value is a reference to an object outside the graph; Studio does \
                          not fetch it."
                    .into(),
                code: None,
            })),
            _ => Err(ReadError::Refused(summary(raw.status, &raw.body))),
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
            Err(reason) => return WriteOutcome::Unknown { reason },
        };
        classify_write(raw)
    }

    // ---- public reads ----

    /// `GET /healthz`. Public; used to discover the server's contract.
    pub async fn health(&self) -> Result<ServerHealth, ReadError> {
        let url = self.url(&["healthz"], &[]);
        let raw = self
            .send(Method::GET, url, None, &[], HEALTH_TIMEOUT)
            .await
            .map_err(ReadError::Transport)?;
        if !raw.status.is_success() {
            return Err(ReadError::Refused(summary(raw.status, &raw.body)));
        }
        let health =
            serde_json::from_slice(&raw.body).map_err(|e| ReadError::Decode(e.to_string()))?;
        Ok(ServerHealth {
            contract: raw.contract,
            health,
        })
    }

    /// `GET /readyz`. Public. A 503 still carries a readiness body.
    pub async fn readiness(&self) -> Result<ReadinessOutput, ReadError> {
        let url = self.url(&["readyz"], &[]);
        let raw = self
            .send(Method::GET, url, None, &[], HEALTH_TIMEOUT)
            .await
            .map_err(ReadError::Transport)?;
        serde_json::from_slice(&raw.body).map_err(|e| ReadError::Decode(e.to_string()))
    }

    /// The operation ids the server's own OpenAPI document advertises (constitution X).
    pub async fn operations(&self) -> Result<BTreeSet<String>, ReadError> {
        let url = self.url(&["openapi.json"], &[]);
        let raw = self
            .send(Method::GET, url, None, &[], READ_TIMEOUT)
            .await
            .map_err(ReadError::Transport)?;
        if !raw.status.is_success() {
            return Err(ReadError::Refused(summary(raw.status, &raw.body)));
        }
        let document: Value =
            serde_json::from_slice(&raw.body).map_err(|e| ReadError::Decode(e.to_string()))?;
        Ok(operation_ids(&document))
    }

    // ---- protected reads ----

    pub async fn graphs(&self) -> Result<GraphListResponse, ReadError> {
        self.read(Method::GET, &["graphs"], &[], None).await
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

    pub async fn query(
        &self,
        graph: &str,
        request: &QueryRequest,
    ) -> Result<ReadOutput, ReadError> {
        self.read(
            Method::POST,
            &["graphs", graph, "query"],
            &[],
            Some(json(request)),
        )
        .await
    }

    pub async fn branches(&self, graph: &str) -> Result<BranchListOutput, ReadError> {
        self.read(Method::GET, &["graphs", graph, "branches"], &[], None)
            .await
    }

    pub async fn commits(&self, graph: &str, branch: &str) -> Result<CommitListOutput, ReadError> {
        self.read(
            Method::GET,
            &["graphs", graph, "commits"],
            &[("branch", branch)],
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
        self.read(
            Method::GET,
            &["graphs", graph, "commits", commit_id, "changes"],
            &query,
            None,
        )
        .await
    }

    /// One page of a branch's change feed. `start` is `now`, `beginning` or
    /// `after:<commit_id>`; pass `page_token` from the previous page to continue.
    pub async fn changes(
        &self,
        graph: &str,
        branch: &str,
        start: Option<&str>,
        page_token: Option<&str>,
        limit: u32,
    ) -> Result<ChangeFeedOutput, ReadError> {
        let limit = limit.to_string();
        let mut query = vec![("branch", branch), ("limit", limit.as_str())];
        if let Some(token) = page_token {
            query.push(("page_token", token));
        } else if let Some(start) = start {
            query.push(("start", start));
        }
        self.read(Method::GET, &["graphs", graph, "changes"], &query, None)
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
        match if_commit {
            Some(commit) => {
                self.write(
                    Method::POST,
                    &["graphs", graph, "mutate", "if-graph-commit"],
                    &[],
                    Some(json(request)),
                    &[(IF_GRAPH_COMMIT_HEADER, commit)],
                )
                .await
            }
            None => {
                self.write(
                    Method::POST,
                    &["graphs", graph, "mutate"],
                    &[],
                    Some(json(request)),
                    &[],
                )
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

    pub async fn delete_branch(&self, graph: &str, branch: &str) -> WriteOutcome<Value> {
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

/// `GET /commits/{id}/changes`: one page of a commit's entity changes.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CommitChanges {
    pub cause: crate::api::ChangeCauseOutput,
    pub changes: Vec<crate::api::EntityChangeOutput>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

fn json<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("request types serialize")
}

/// A successful read must carry a contract Studio speaks.
fn check_contract(raw: &RawResponse) -> Result<(), ReadError> {
    match raw.contract.as_deref() {
        Some(served) if SUPPORTED_CONTRACTS.contains(&served) => Ok(()),
        served => Err(ReadError::ContractMismatch {
            served: served.map(str::to_owned),
        }),
    }
}

fn summary(status: StatusCode, body: &[u8]) -> ApiErrorSummary {
    match serde_json::from_slice::<ErrorOutput>(body) {
        Ok(error) => ApiErrorSummary {
            status: status.as_u16(),
            message: error.error,
            code: error.code,
        },
        Err(_) => ApiErrorSummary {
            status: status.as_u16(),
            message: status
                .canonical_reason()
                .unwrap_or("Request failed")
                .to_owned(),
            code: None,
        },
    }
}

/// Classifies a write's response (constitution III, spec 003).
///
/// - 2xx with the contract header and a readable body: committed.
/// - 4xx, 503 and 429: refused; the server did not admit or did not apply the write.
/// - Anything else (a lost contract header, an unreadable success body, other 5xx): unknown.
fn classify_write<T: DeserializeOwned>(raw: RawResponse) -> WriteOutcome<T> {
    let contract_ok = raw
        .contract
        .as_deref()
        .is_some_and(|served| SUPPORTED_CONTRACTS.contains(&served));
    if raw.status.is_success() {
        if !contract_ok {
            return WriteOutcome::Unknown {
                reason: "the response did not carry the contract header".into(),
            };
        }
        return match serde_json::from_slice(&raw.body) {
            Ok(value) => WriteOutcome::Committed(value),
            Err(error) => WriteOutcome::Unknown {
                reason: format!("the success response could not be read: {error}"),
            },
        };
    }
    let refused = raw.status.is_client_error()
        || raw.status == StatusCode::SERVICE_UNAVAILABLE
        || raw.status == StatusCode::TOO_MANY_REQUESTS;
    if refused && let Ok(body) = serde_json::from_slice::<ErrorOutput>(&raw.body) {
        return WriteOutcome::Refused(ApiError {
            status: raw.status.as_u16(),
            body,
        });
    }
    WriteOutcome::Unknown {
        reason: format!("the server answered {}", raw.status),
    }
}

fn operation_ids(document: &Value) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    if let Some(paths) = document.get("paths").and_then(Value::as_object) {
        for operations in paths.values() {
            if let Some(operations) = operations.as_object() {
                for operation in operations.values() {
                    if let Some(id) = operation.get("operationId").and_then(Value::as_str) {
                        ids.insert(id.to_owned());
                    }
                }
            }
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::executor::block_on;
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};

    use super::*;

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

    const CHANGE: &str =
        r#"{"branch":"main","query_name":"u","affected_nodes":1,"affected_edges":0,"commit":null}"#;

    #[test]
    fn a_committed_write_needs_the_contract_header() {
        let outcome = block_on(client(respond(200, Some("0.13"), CHANGE)).mutate(
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

    #[test]
    fn a_refusal_carries_the_servers_reason() {
        let body = r#"{"error":"graph commit moved","code":"conflict","precondition_failure":{"expected":"a","actual":"b"}}"#;
        let outcome = block_on(client(respond(412, Some("0.13"), body)).mutate(
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
    }

    #[test]
    fn a_server_error_on_a_write_is_unknown() {
        let outcome = block_on(
            client(respond(500, Some("0.13"), r#"{"error":"boom"}"#)).mutate(
                "g",
                &ChangeRequest::default(),
                None,
            ),
        );
        assert!(matches!(outcome, WriteOutcome::Unknown { .. }));
    }

    #[test]
    fn a_read_checks_the_contract() {
        let result =
            block_on(client(respond(200, Some("0.14"), r#"{"branches":[]}"#)).branches("g"));
        assert!(matches!(
            result,
            Err(ReadError::ContractMismatch { served: Some(_) })
        ));
        let result =
            block_on(client(respond(200, Some("0.13"), r#"{"branches":["main"]}"#)).branches("g"));
        assert_eq!(result.unwrap().branches, vec!["main"]);
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
                let builder = Response::builder().header(CONTRACT_HEADER, "0.13");
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
    fn a_rate_limited_write_is_never_sent_again() {
        let (http, count) = rate_limited(1, CHANGE);
        let outcome = block_on(client(http).mutate("g", &ChangeRequest::default(), None));
        assert!(!matches!(outcome, WriteOutcome::Committed(_)));
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn paths_and_queries_are_encoded() {
        let client = client(respond(200, Some("0.13"), "{}"));
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
    fn operation_ids_come_from_the_served_document() {
        let document: Value =
            serde_json::from_str(include_str!("../openapi/omnigraph-0.13.0.json")).unwrap();
        let ids = operation_ids(&document);
        assert!(ids.contains("cluster_query"));
        assert!(
            !ids.contains("cluster_publishCommit"),
            "0.13.0 has no commit publish"
        );
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
                let ok = || Response::builder().header(CONTRACT_HEADER, "0.13");
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
                    Response::builder()
                        .status(302)
                        .header("location", "s3://bucket/report.pdf")
                        .body(AsyncBody::empty())
                } else {
                    Response::builder().status(404).body(AsyncBody::empty())
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
        assert!(matches!(
            block_on(client.blob_bytes("g", &cell("external"), at, 0..5)),
            Err(ReadError::Refused(e)) if e.status == 302
        ));
        assert_eq!(
            block_on(client.blob_bytes("g", &cell("bytes"), at, 4..4)).unwrap(),
            Vec::<u8>::new(),
            "an empty range sends nothing"
        );
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}
