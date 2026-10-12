//! The only crate that makes HTTP requests: the 0.13 contract header, request and response
//! types, typed errors, write outcomes, change-feed paging. No GPUI.
//!
//! Layer 2 (`docs/architecture.md`). Plan: `specs/003-connections/plan.md`.
//!
//! Built on Zed's `http_client::HttpClient` (Apache-2.0): the app passes `reqwest_client`,
//! tests pass Zed's fake client. The contract is `openapi/omnigraph-0.13.0.json`, copied from
//! OmniGraph `v0.13.0` (MIT).

pub mod api;
mod client;

pub use api::ErrorOutput;
pub use client::{
    ApiErrorSummary, BLOB_READ_MAX, BlobAt, BlobCell, BlobMeta, CommitChanges, GraphClient,
    ReadError, ServerHealth,
};

/// Header every protected request sends and every response echoes
/// (`omnigraph-api-types` `HTTP_API_CONTRACT_HEADER` at OmniGraph v0.13.0).
pub const CONTRACT_HEADER: &str = "omnigraph-http-api";

/// The contracts this client speaks (`HTTP_API_CONTRACT` at OmniGraph v0.13.0).
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
