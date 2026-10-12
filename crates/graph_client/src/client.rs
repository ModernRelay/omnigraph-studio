//! The HTTP client for one OmniGraph server (spec 003).
//!
//! Reads return `Result<T, ReadError>`. Writes return a [`WriteOutcome`] and are never retried
//! here (constitution III).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use futures::AsyncReadExt;
use http_client::http::{HeaderValue, Method, Request, StatusCode};
use http_client::{AsyncBody, HttpClient, HttpRequestExt};
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
    body: Vec<u8>,
}

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
        let mut builder = Request::builder()
            .method(method)
            .uri(url.as_str())
            .header(CONTRACT_HEADER, SUPPORTED_CONTRACTS[0])
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
        let mut bytes = Vec::new();
        response
            .into_body()
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;
        Ok(RawResponse {
            status,
            contract,
            body: bytes,
        })
    }

    async fn read<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<T, ReadError> {
        let url = self.url(segments, query);
        let raw = self
            .send(method, url, body, &[], READ_TIMEOUT)
            .await
            .map_err(ReadError::Transport)?;
        if raw.status.is_success() {
            match raw.contract.as_deref() {
                Some(served) if SUPPORTED_CONTRACTS.contains(&served) => {}
                served => {
                    return Err(ReadError::ContractMismatch {
                        served: served.map(str::to_owned),
                    });
                }
            }
            serde_json::from_slice(&raw.body).map_err(|error| ReadError::Decode(error.to_string()))
        } else {
            Err(ReadError::Refused(summary(raw.status, &raw.body)))
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
}
