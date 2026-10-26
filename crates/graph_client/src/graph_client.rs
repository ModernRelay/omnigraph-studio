//! The only crate that makes HTTP requests: the contract header, request and response types,
//! typed errors, write outcomes, change-feed paging. No GPUI.
//!
//! Layer 2 (`docs/architecture.md`). Plan: `specs/003-connections/plan.md`.
//!
//! Built on Zed's `http_client::HttpClient` (Apache-2.0): the app passes `reqwest_client`,
//! tests pass Zed's fake client. The contract is `openapi/omnigraph-0.14.0.json`, copied
//! from Omnigraph `v0.14.0` (MIT), the release that fixed contract `0.14` and the oldest server
//! Studio supports.

pub mod api;
mod client;

pub use api::{CommitChanges, ErrorOutput};
pub use client::{
    ApiErrorSummary, BlobAt, BlobCell, BlobMeta, CHANGE_READ_TIMEOUT, Challenge, FeedAt,
    ForwardRequest, ForwardResponse, GraphClient, HistoricalGap, MergePreview, PropertyThen,
    ReadError, ServerHealth, judge_write, server_root,
};
pub use http_client::{
    AsyncBody,
    http::{HeaderMap, Method, StatusCode},
};

/// Header every protected request sends and every response echoes
/// (`omnigraph-api-types` `HTTP_API_CONTRACT_HEADER`).
pub const CONTRACT_HEADER: &str = "omnigraph-http-api";

/// The contract this client speaks: `HTTP_API_CONTRACT`. A server refuses a request that does
/// not send exactly this value, and an earlier 0.14 build, which still speaks `0.13`, is told
/// apart by its header alone.
pub const CONTRACT: &str = "0.14";

/// Whether a response's contract header names the contract Studio speaks.
pub fn speaks_contract(served: Option<&str>) -> bool {
    served == Some(CONTRACT)
}

/// Header that binds a write to the commit it was read at.
pub const IF_GRAPH_COMMIT_HEADER: &str = "Omnigraph-If-Graph-Commit";

/// How a write ended (constitution III). A write is never retried by this crate.
// A value of this type is returned once per write and matched at once; boxing the refusal
// would only add an allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum WriteOutcome<T> {
    /// The server published the change.
    Committed(T),
    /// The server refused the change; nothing was written.
    Refused(ApiError),
    /// The response was lost, timed out, or lacked the contract header. Settle it by reading
    /// history, never by sending the write again.
    Unknown { reason: String },
}

/// A refusal from the server: the HTTP status and the parsed `ErrorOutput` body.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub body: ErrorOutput,
}
