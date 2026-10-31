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
    ForwardRequest, ForwardResponse, GraphClient, HistoricalGap, MergePreview, NoAnswer,
    PropertyThen, ReadError, ServerHealth, Unauthorized, judge_write, server_root,
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
#[expect(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum WriteOutcome<T> {
    /// The server published the change.
    Committed(T),
    /// The server refused the change; nothing was written.
    Refused(ApiError),
    /// Studio never sent the write: its client was retired ([`GraphClient::retire`]), the
    /// server refused the connection, or the request could not be made. A refusal in the
    /// constitution's sense (III): nothing was written, and there is nothing to settle, as with
    /// [`Self::Refused`], though no server answered.
    NotSent { reason: String },
    /// The response was lost, timed out, or lacked the contract header. Settle it by reading
    /// history, never by sending the write again.
    Unknown { reason: String },
}

/// What a server's answer to a write says of it ([`judge_write`]): an answer means the write was
/// sent, so it was made, refused, or may have been made. [`WriteOutcome`] adds the write never
/// sent.
#[expect(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Answered<T> {
    /// The server published the change.
    Committed(T),
    /// The server refused the change; nothing was written.
    Refused(ApiError),
    /// The answer cannot say whether the change was made.
    Unknown { reason: String },
}

/// A refusal from the server: the HTTP status, the parsed `ErrorOutput` body, and what a 401
/// says of the token the write carried.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub body: ErrorOutput,
    /// What a 401 says of the token the write carried; `None` for any other status, and from
    /// [`judge_write`], which sees no request.
    pub unauthorized: Option<Unauthorized>,
    /// The token the write carried was replaced before its answer came: a 401 says nothing of
    /// the one held now.
    pub token_replaced: bool,
}

impl ApiError {
    /// Why the server refused the write, as a sentence: a 401's in Studio's words, which say
    /// what became of the token (every write site says it alike), any other's as the server
    /// put it.
    pub fn reason(&self) -> &str {
        match self.unauthorized {
            Some(unauthorized) => unauthorized.reason(),
            None => &self.body.error,
        }
    }
}
