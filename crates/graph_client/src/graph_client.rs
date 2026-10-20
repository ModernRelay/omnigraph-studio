//! The only crate that makes HTTP requests: the contract header, request and response types,
//! typed errors, write outcomes, change-feed paging. No GPUI.
//!
//! Layer 2 (`docs/architecture.md`). Plan: `specs/003-connections/plan.md`.
//!
//! Built on Zed's `http_client::HttpClient` (Apache-2.0): the app passes `reqwest_client`,
//! tests pass Zed's fake client. The contract is `openapi/omnigraph-0.14.0-edge.json`, copied
//! from OmniGraph at `edge-20261017-d883c49` (`0.14.0-edge+d883c49`, MIT), the oldest server
//! Studio supports. Its contract header is still `0.13`.

pub mod api;
mod client;

pub use api::ErrorOutput;
pub use client::{
    ApiErrorSummary, BlobAt, BlobCell, BlobMeta, CommitChanges, FeedAt, ForwardRequest,
    ForwardResponse, GraphClient, HistoricalGap, MIN_SERVER_VERSION, MergePreview, PropertyThen,
    ReadError, ServerHealth, server_root,
};
pub use http_client::{
    AsyncBody,
    http::{HeaderMap, Method, StatusCode},
};

/// Header every protected request sends and every response echoes
/// (`omnigraph-api-types` `HTTP_API_CONTRACT_HEADER` at OmniGraph v0.13.0).
pub const CONTRACT_HEADER: &str = "omnigraph-http-api";

/// The contracts this client speaks: `HTTP_API_CONTRACT`, unchanged from OmniGraph 0.13 to
/// 0.14.
pub const SUPPORTED_CONTRACTS: &[&str] = &["0.13"];

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
