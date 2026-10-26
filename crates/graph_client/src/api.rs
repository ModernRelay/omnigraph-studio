//! Request and response types for the Omnigraph 0.14 HTTP API.
//!
//! Field names and optionality follow `openapi/omnigraph-0.14.0.json`; the test at the bottom
//! of this file fails if a type here and the contract disagree. Add fields only as surfaces
//! need them, and add each new type to the test.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

// ---- health and graphs ----

/// `GET /healthz`.
#[derive(Debug, Clone, Deserialize)]
pub struct HealthOutput {
    pub status: String,
    pub version: String,
    pub internal_schema_version: i32,
    #[serde(default)]
    pub source_version: Option<String>,
}

/// `GET /readyz`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReadinessOutput {
    pub ready: bool,
    pub status: String,
    pub state_revision: i64,
    pub served_graph_count: i64,
    pub ready_graph_count: i64,
    pub loading_graph_count: i64,
    pub blocked_graph_count: i64,
    pub shutdown_grace_seconds: i64,
    #[serde(default)]
    pub state_cas: Option<String>,
    #[serde(default)]
    pub booted_serving_digest: Option<String>,
}

/// `GET /graphs`.
#[derive(Debug, Clone, Deserialize)]
pub struct GraphListResponse {
    pub graphs: Vec<GraphInfo>,
}

/// `GET /graphs/discovery`: the graphs an identity credential (managed or OIDC) may know of,
/// with no storage or serving metadata. A static token is refused it.
#[derive(Debug, Clone, Deserialize)]
pub struct GraphDiscoveryResponse {
    pub graphs: Vec<GraphDiscoveryEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphDiscoveryEntry {
    pub graph_id: String,
    /// The graph's id for now: the server configures no separate display name.
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GraphInfo {
    pub graph_id: String,
    pub uri: String,
    pub state: GraphAvailability,
    pub read_available: bool,
    pub write_available: bool,
    pub action: GraphAvailabilityAction,
    #[serde(default)]
    pub failure: Option<GraphStartupFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphAvailability {
    Loading,
    Ready,
    Transitioning,
    Blocked,
    Stopping,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphAvailabilityAction {
    None,
    WaitForStartup,
    WaitForTransition,
    ApplyCorrectionOrRestart,
    WaitForRestart,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphStartupFailure {
    InvalidConfiguration,
    InvalidPolicy,
    InvalidExternalBlobPolicy,
    OpenFailed,
    InvalidStoredQueries,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

// ---- schema and snapshot ----

/// `GET /graphs/{id}/schema`.
#[derive(Debug, Clone, Deserialize)]
pub struct SchemaOutput {
    pub schema_source: String,
}

/// `GET /graphs/{id}/snapshot?branch=`.
#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotOutput {
    pub graph_branch: String,
    /// A Lance version, `u64` on the server: the contract's `int64` is narrower than what it
    /// sends (`docs/deferred-engine-changes.md`).
    pub graph_manifest_version: u64,
    pub internal_schema_version: i32,
    pub datasets: Vec<SnapshotDatasetOutput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotDatasetOutput {
    pub entity_kind: EntityKind,
    pub type_name: String,
    pub dataset_path: String,
    /// A Lance version, `u64` on the server, as the manifest's is.
    pub published_dataset_version: u64,
    pub entity_count: i64,
    #[serde(default)]
    pub native_dataset_branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Node,
    Edge,
}

// ---- query and mutate ----

/// `POST /graphs/{id}/query`. Set `snapshot` to pin a read to a commit (constitution II).
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueryRequest {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

/// `POST /graphs/{id}/queries/{name}`: runs the stored query the path names. Only its inputs
/// travel; the query's text stays on the server. `branch` and `snapshot` are exclusive.
#[derive(Debug, Clone, Default, Serialize)]
pub struct InvokeStoredQueryRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// The kind the caller expects; the server refuses (400) a stored query of the other kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect_mutation: Option<bool>,
}

/// `GET /graphs/{id}/queries`: the graph's stored queries, each with typed parameters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QueriesCatalogOutput {
    #[serde(default)]
    pub queries: Vec<QueryCatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct QueryCatalogEntry {
    /// The registry name, and the last segment of the invoke route.
    pub name: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    /// From the query's `@description(…)`.
    #[serde(default)]
    pub description: Option<String>,
    /// From the query's `@instruction(…)`.
    #[serde(default)]
    pub instruction: Option<String>,
    /// The stored query changes data.
    #[serde(default)]
    pub mutation: bool,
    #[serde(default)]
    pub params: Vec<ParamDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ParamDescriptor {
    pub name: String,
    pub kind: ParamKind,
    /// The element kind of a `list`.
    #[serde(default)]
    pub item_kind: Option<ParamKind>,
    /// The dimension of a `vector`.
    #[serde(default)]
    pub vector_dim: Option<u32>,
    /// The parameter may be left out.
    #[serde(default)]
    pub nullable: bool,
}

/// A stored-query parameter's kind, as the catalog names it. One Studio does not know
/// is `Other`: its value is sent as text, for the server to read or refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    String,
    Bool,
    Int,
    #[serde(rename = "bigint")]
    BigInt,
    Float,
    Date,
    #[serde(rename = "datetime")]
    DateTime,
    Blob,
    Vector,
    List,
    #[serde(other)]
    Other,
}

/// The response of `POST /query`. Rows are JSON objects; a null cell's key is omitted.
#[derive(Debug, Clone, Deserialize)]
pub struct ReadOutput {
    pub query_name: String,
    pub target: ReadTargetOutput,
    pub row_count: i64,
    #[serde(default)]
    pub columns: Vec<String>,
    /// Shared: an answer kept for its commit is handed to every reader that asks again, and a
    /// copy is made only by one that takes the rows for its own (`Arc::unwrap_or_clone`).
    #[serde(deserialize_with = "shared_value")]
    pub rows: Arc<Value>,
    #[serde(default)]
    pub graph_commit_id: Option<String>,
    /// Where a paged statement read (`branch diff`) stands; `None` for every other answer.
    #[serde(default)]
    pub page: Option<ReadPageOutput>,
}

/// A `branch diff` answer's page: the commits it compares, and the token `after page "<token>"`
/// continues it with, which its last page has none of.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ReadPageOutput {
    #[serde(default)]
    pub from_commit_id: Option<String>,
    #[serde(default)]
    pub to_commit_id: Option<String>,
    /// The merge base, for a branch's diff since it.
    #[serde(default)]
    pub base_commit_id: Option<String>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

/// A JSON value read once and shared.
fn shared_value<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Arc<Value>, D::Error> {
    Value::deserialize(deserializer).map(Arc::new)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadTargetOutput {
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub snapshot: Option<String>,
}

impl ReadOutput {
    /// The answer of no rows to `query_name` at `commit`, for a read its reader knows holds
    /// none without asking.
    pub fn empty(query_name: &str, commit: String) -> Self {
        Self {
            query_name: query_name.to_owned(),
            target: ReadTargetOutput {
                branch: None,
                snapshot: Some(commit.clone()),
            },
            row_count: 0,
            columns: Vec::new(),
            rows: Arc::new(Value::Array(Vec::new())),
            graph_commit_id: Some(commit),
            page: None,
        }
    }
}

/// `POST /graphs/{id}/mutate` and `/mutate/if-graph-commit`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChangeRequest {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// A write's result. `commit` is `None` when the mutation changed nothing.
#[derive(Debug, Clone, Deserialize)]
pub struct ChangeOutput {
    pub branch: String,
    pub query_name: String,
    pub affected_nodes: i64,
    pub affected_edges: i64,
    pub commit: Option<CommitOutput>,
    #[serde(default)]
    pub actor_id: Option<String>,
    /// Present for branch statements; kept raw until a surface needs it.
    #[serde(default)]
    pub outcome: Option<Value>,
}

// ---- branches and commits ----

/// `GET /graphs/{id}/branches`.
#[derive(Debug, Clone, Deserialize)]
pub struct BranchListOutput {
    pub branches: Vec<String>,
}

/// `POST /graphs/{id}/branches`: the new branch starts at `from`'s head, or at the published
/// commit `from_commit` names; never both.
#[derive(Debug, Clone, Serialize)]
pub struct BranchCreateRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_commit: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BranchCreateOutput {
    pub uri: String,
    /// The branch the new one started from; for a commit start, the branch that wrote it.
    pub from: String,
    pub name: String,
    /// The commit the new branch starts at.
    pub from_commit_id: String,
    #[serde(default)]
    pub actor_id: Option<String>,
}

/// `DELETE /graphs/{id}/branches/{branch}`: the receipt of a branch deletion.
#[derive(Debug, Clone, Deserialize)]
pub struct BranchDeleteOutput {
    /// The graph's URI, as the server names it.
    pub uri: String,
    /// The branch deleted, as the request's path named it.
    pub name: String,
    #[serde(default)]
    pub actor_id: Option<String>,
}

/// A branch create refused at the commit `from_commit` named (409, or 410 for
/// `not_retained`); nothing was created.
#[derive(Debug, Clone, Deserialize)]
pub struct BranchStartRefusalOutput {
    pub reason: BranchStartRefusalReason,
    pub graph_commit_id: String,
    /// For `branch_deleted`, the branch that wrote the commit.
    #[serde(default)]
    pub branch: Option<String>,
    /// For `not_retained`, the type whose stored version cleanup reclaimed.
    #[serde(default)]
    pub type_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchStartRefusalReason {
    /// Cleanup reclaimed a version the commit names.
    NotRetained,
    /// The schema changed since the commit.
    SchemaChanged,
    /// The commit predates the graph's storage format.
    EarlierStorageFormat,
    /// The branch that wrote the commit was deleted.
    BranchDeleted,
    UnpublishedCommit,
    UnattachedSnapshot,
    Unknown,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

/// A commit's changes the server cannot compare with its parent's (409); nothing was read. It
/// is an error's [`ErrorOutput::change_diff_refusal`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChangeDiffRefusalOutput {
    pub reason: ChangeDiffRefusalReason,
    pub graph_commit_id: String,
    /// For `schema_boundary`, the type at the boundary.
    #[serde(default)]
    pub type_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeDiffRefusalReason {
    /// The commit has no first parent: the graph's first commit.
    ParentlessCommit,
    /// The commit and its parent are on either side of a schema change the server cannot
    /// compare across.
    SchemaBoundary,
    Unknown,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

// ---- access ----

/// `GET /graphs/{id}/access`: what the caller's own credential may do on one graph, by its
/// installed policy.
#[derive(Debug, Clone, Deserialize)]
pub struct AccessOutput {
    pub graph_id: String,
    /// The actor the credential resolves to; none on a server without authentication.
    #[serde(default)]
    pub actor_id: Option<String>,
    /// Actions allowed with no branch named: the graph-scoped ones, and those a single rule
    /// allows on every branch. An action allowed on every branch only by two rules together
    /// is missing here; `actions` for a branch has it.
    pub graph_wide: Vec<AccessAction>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub target_branch: Option<String>,
    /// With `branch`, the actions allowed on it (with `target_branch`, also `branch_create`
    /// and `branch_merge` from it into that one). Empty without `branch`.
    pub actions: Vec<AccessAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessAction {
    Read,
    Export,
    Change,
    SchemaApply,
    BranchCreate,
    BranchDelete,
    BranchMerge,
    CommitPublish,
    CommitDrop,
    InvokeQuery,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

/// `POST /graphs/{id}/branches/merge`.
#[derive(Debug, Clone, Serialize)]
pub struct BranchMergeRequest {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub delete_branch: bool,
    /// Classify and validate only: the outcome the merge would have now, or its conflicts, with
    /// nothing written. The server reads both branches without the merge's write gate.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
}

/// `GET /graphs/{id}/branches/merge-base`: the commit a merge of `source` into `target` would
/// classify against, and both branches' heads.
#[derive(Debug, Clone, Deserialize)]
pub struct MergeBaseOutput {
    pub merge_base: CommitOutput,
    pub source: String,
    pub source_commit_id: String,
    pub target: String,
    pub target_commit_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BranchMergeOutput {
    pub source: String,
    pub target: String,
    pub outcome: BranchMergeOutcome,
    pub commit: Option<CommitOutput>,
    #[serde(default)]
    pub actor_id: Option<String>,
    #[serde(default)]
    pub branch_deleted: Option<bool>,
    #[serde(default)]
    pub branch_delete_error_details: Option<ErrorOutput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchMergeOutcome {
    AlreadyUpToDate,
    FastForward,
    Merged,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

/// `GET /graphs/{id}/commits?branch=`: the whole history, or with `limit` or `page_token` a
/// page of it.
#[derive(Debug, Clone, Deserialize)]
pub struct CommitListOutput {
    pub commits: Vec<CommitOutput>,
    /// The head a paged listing resolved: later pages answer for its history.
    #[serde(default)]
    pub head_commit_id: Option<String>,
    /// Where a paged listing goes on, sent back as `page_token`; none on its last page and on
    /// a whole history.
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitOutput {
    pub graph_commit_id: String,
    /// The `__manifest` version holding the commit, `u64` on the server. An unpublished commit
    /// or an unattached snapshot is held in a detached version, which Lance numbers with the
    /// high bit set: past `i64::MAX`.
    pub graph_manifest_version: u64,
    /// Unix microseconds.
    pub created_at: i64,
    /// The branch the commit was published on, `None` for main, as the server records it: a
    /// commit keeps the branch that wrote it, so a branch's first-parent chain lists its own
    /// commits, then those of the branch it forked from. A held commit names the branch it was
    /// written on, which it is not on.
    #[serde(default)]
    pub graph_branch: Option<String>,
    #[serde(default)]
    pub parent_commit_id: Option<String>,
    #[serde(default)]
    pub merged_parent_commit_id: Option<String>,
    #[serde(default)]
    pub actor_id: Option<String>,
    pub kind: CommitKind,
}

/// Which kind of commit a commit is (`CommitKindOutput`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitKind {
    /// A commit of a branch's history.
    Published,
    /// A proposal written `as commit`, awaiting `commit publish`.
    Unpublished,
    /// An unattached snapshot, written `as unattached branch`, which never publishes.
    Unattached,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

// ---- changes ----

/// `GET /graphs/{id}/changes` (one page).
#[derive(Debug, Clone, Deserialize)]
pub struct ChangeFeedOutput {
    pub blocks: Vec<ChangeBlockOutput>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub caught_up: Option<bool>,
}

/// `GET /graphs/{id}/diff` (one page): the net entity changes between two commits, with no
/// cause, since a range has no single author.
#[derive(Debug, Clone, Deserialize)]
pub struct DiffOutput {
    pub from_commit_id: String,
    pub to_commit_id: String,
    pub changes: Vec<EntityChangeOutput>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

/// `GET /graphs/{id}/commits/{commit_id}/changes` (one page, `CommitChangesOutput`): a commit's
/// entity changes against its first parent, with the commit as their cause.
#[derive(Debug, Clone, Deserialize)]
pub struct CommitChanges {
    pub cause: ChangeCauseOutput,
    /// Shared, as `ReadOutput.rows` is: a page kept for its commit is handed to every reader
    /// that asks again, and a copy is made only by one that takes the changes for its own
    /// (`Arc::unwrap_or_clone`).
    #[serde(deserialize_with = "shared_changes")]
    pub changes: Arc<Vec<EntityChangeOutput>>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

/// A page's changes read once and shared.
fn shared_changes<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Arc<Vec<EntityChangeOutput>>, D::Error> {
    Vec::deserialize(deserializer).map(Arc::new)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChangeBlockOutput {
    pub cause: ChangeCauseOutput,
    pub changes: Vec<EntityChangeOutput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChangeCauseOutput {
    pub graph_commit_id: String,
    pub authored_branch: String,
    /// Unix microseconds.
    pub authored_at: i64,
    #[serde(default)]
    pub parent_commit_id: Option<String>,
    #[serde(default)]
    pub merged_parent_commit_id: Option<String>,
    #[serde(default)]
    pub actor_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EntityChangeOutput {
    pub kind: EntityKind,
    #[serde(rename = "type")]
    pub entity_type: ChangeTypeOutput,
    pub id: String,
    pub op: ChangeOp,
    #[serde(default)]
    pub before: Option<ChangeImageOutput>,
    #[serde(default)]
    pub after: Option<ChangeImageOutput>,
}

/// A type in a change: `id` is opaque and stable across renames; `name` is the current name.
#[derive(Debug, Clone, Deserialize)]
pub struct ChangeTypeOutput {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
    /// An operation a newer server names: the change still decodes, read by its images.
    #[serde(other)]
    Other,
}

/// An entity's properties at one side of a change. A null keeps its key; an absent key means
/// the property was outside that commit's schema. `endpoints` is present for edges only.
#[derive(Debug, Clone, Deserialize)]
pub struct ChangeImageOutput {
    pub properties: serde_json::Map<String, Value>,
    #[serde(default)]
    pub endpoints: Option<Value>,
}

// ---- Blob writes ----

/// The largest body `PUT /graphs/{id}/blob` takes, inclusive: 32 MiB, the most any server
/// stores in one value. A server configured for less refuses a larger body with 413.
pub const BLOB_PUT_MAX: u64 = 32 * 1024 * 1024;

/// `PUT`/`DELETE /graphs/{id}/blob`: the receipt of a Blob put or clear. A put leaves managed
/// bytes, with their `size` and `etag`; a clear leaves null, with neither. The `etag` is the
/// one a read at `commit` reports.
#[derive(Debug, Clone, Deserialize)]
pub struct BlobWriteOutput {
    pub selector: BlobSelectorOutput,
    /// The branch the write published to.
    pub branch: String,
    pub kind: BlobWriteState,
    /// The write's own publication; `None` only for a clear of a cell that was already null,
    /// which publishes nothing.
    pub commit: Option<CommitOutput>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub actor_id: Option<String>,
}

/// The cell a Blob write named.
#[derive(Debug, Clone, Deserialize)]
pub struct BlobSelectorOutput {
    pub entity: EntityKind,
    #[serde(rename = "type")]
    pub type_name: String,
    pub id: String,
    pub property: String,
}

/// What a Blob write left its cell holding (`BlobWriteStateOutput`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobWriteState {
    Managed,
    Null,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

/// A Blob write's `If-Match` did not hold for its cell (HTTP 412), and the write changed
/// nothing. `current_etag` is the cell's validator when it holds a managed value; a null or
/// external value has none.
#[derive(Debug, Clone, Deserialize)]
pub struct BlobPreconditionFailureOutput {
    #[serde(default)]
    pub current_etag: Option<String>,
}

// ---- errors ----

/// Every refusal's body. Typed details are added as surfaces need them; the rest stay in
/// `other` so nothing the server sent is lost. `Default` is an empty body, for building one
/// field by field.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorOutput {
    pub error: String,
    #[serde(default)]
    pub code: Option<ErrorCode>,
    /// What the request did and what to do next; Omnigraph 0.14.0 and later send it on every
    /// error. Without it a write's outcome is unknown ([`crate::judge_write`]).
    #[serde(default)]
    pub disposition: Option<DispositionOutput>,
    #[serde(default)]
    pub merge_conflicts: Option<Vec<MergeConflictOutput>>,
    #[serde(default)]
    pub precondition_failure: Option<PreconditionFailureOutput>,
    /// Set with a 412 when a Blob write's `If-Match` did not hold: the value changed since it
    /// was read.
    #[serde(default)]
    pub blob_precondition_failure: Option<BlobPreconditionFailureOutput>,
    /// Set when a branch could not start at the commit `from_commit` named.
    #[serde(default)]
    pub branch_start_refusal: Option<BranchStartRefusalOutput>,
    /// Set with a 409 when a commit's changes cannot be compared with its parent's: the graph's
    /// first commit, or a schema change between them.
    #[serde(default)]
    pub change_diff_refusal: Option<ChangeDiffRefusalOutput>,
    #[serde(default)]
    pub diagnostic: Option<Value>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    Forbidden,
    BadRequest,
    ApiContractMismatch,
    NotFound,
    MethodNotAllowed,
    Conflict,
    TooManyRequests,
    ServiceUnavailable,
    GraphUnavailable,
    Internal,
    /// `commit publish` of an unattached snapshot, which never publishes.
    UnattachedCannotPublish,
    /// The credential resolves to another actor than `Omnigraph-If-Actor` named (412).
    ActorMismatch,
    /// The server cannot say whether the request's effects happened; its disposition says to
    /// reconcile (settle it by reading history, never by sending it again).
    OutcomeUnknown,
    /// A condition the request set did not hold (412): nothing was done.
    PreconditionFailed,
    /// The graph needs recovery before it takes this request.
    RecoveryRequired,
    /// An external Blob source refused or failed.
    ExternalBlobSource,
    /// The change feed no longer holds the position asked for; read a new baseline.
    ChangeFeedGap,
    /// The request did not arrive whole in time (408).
    RequestTimeout,
    /// A branch could not start at the commit named; see `branch_start_refusal`.
    BranchStartRefusal,
    /// The request body is larger than the server takes (413).
    PayloadTooLarge,
    /// The request body's media type is not one the route takes (415).
    UnsupportedMediaType,
    /// A Blob byte range that cannot be satisfied (416); see `blob_range`.
    RangeNotSatisfiable,
    /// The request was cancelled before it finished.
    Cancelled,
    /// The request ran out of its server-side time.
    DeadlineExceeded,
    /// The result is larger than the server returns.
    ResultTooLarge,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

/// What a refused or failed request did and what its caller should do next, stated by the
/// server on every error since 0.14.0 (RFC #1017, "Dispositions and refusals").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct DispositionOutput {
    pub action: DispositionAction,
    pub effects: DispositionEffects,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispositionAction {
    Fix,
    Refresh,
    Retry,
    Recover,
    Reconcile,
    #[serde(other)]
    Other,
}

/// Whether the request's effects happened: none, some, or the server cannot say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispositionEffects {
    None,
    Partial,
    Unknown,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MergeConflictOutput {
    pub entity_kind: EntityKind,
    pub type_name: String,
    pub kind: MergeConflictKind,
    pub message: String,
    #[serde(default)]
    pub entity_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeConflictKind {
    DivergentInsert,
    DivergentUpdate,
    DeleteVsUpdate,
    OrphanEdge,
    UniqueViolation,
    CardinalityViolation,
    ValueConstraintViolation,
    /// A value a later build speaking contract 0.14 may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreconditionFailureOutput {
    pub expected: String,
    #[serde(default)]
    pub actual: Option<String>,
}

#[cfg(test)]
mod contract_tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    const CONTRACT: &str = include_str!("../openapi/omnigraph-0.14.0.json");

    /// (schema name, the fields our type declares). Fields we declare must exist in the
    /// contract, and every field the contract requires must be declared.
    const OBJECTS: &[(&str, &[&str])] = &[
        (
            "HealthOutput",
            &[
                "status",
                "version",
                "internal_schema_version",
                "source_version",
            ],
        ),
        (
            "ReadinessOutput",
            &[
                "ready",
                "status",
                "state_revision",
                "served_graph_count",
                "ready_graph_count",
                "loading_graph_count",
                "blocked_graph_count",
                "shutdown_grace_seconds",
                "state_cas",
                "booted_serving_digest",
            ],
        ),
        ("GraphListOutput", &["graphs"]),
        (
            "MergeBaseOutput",
            &[
                "merge_base",
                "source",
                "source_commit_id",
                "target",
                "target_commit_id",
            ],
        ),
        (
            "GraphInfo",
            &[
                "graph_id",
                "uri",
                "state",
                "read_available",
                "write_available",
                "action",
                "failure",
            ],
        ),
        ("SchemaOutput", &["schema_source"]),
        (
            "SnapshotOutput",
            &[
                "graph_branch",
                "graph_manifest_version",
                "internal_schema_version",
                "datasets",
            ],
        ),
        (
            "SnapshotDatasetOutput",
            &[
                "entity_kind",
                "type_name",
                "dataset_path",
                "published_dataset_version",
                "entity_count",
                "native_dataset_branch",
            ],
        ),
        (
            "QueryRequest",
            &["query", "name", "params", "branch", "snapshot"],
        ),
        (
            "ReadOutput",
            &[
                "query_name",
                "target",
                "row_count",
                "columns",
                "rows",
                "graph_commit_id",
                "page",
            ],
        ),
        (
            "ReadPageOutput",
            &[
                "from_commit_id",
                "to_commit_id",
                "base_commit_id",
                "next_page_token",
            ],
        ),
        ("ReadTargetOutput", &["branch", "snapshot"]),
        ("ChangeRequest", &["query", "name", "params", "branch"]),
        (
            "ChangeOutput",
            &[
                "branch",
                "query_name",
                "affected_nodes",
                "affected_edges",
                "commit",
                "actor_id",
                "outcome",
            ],
        ),
        ("BranchListOutput", &["branches"]),
        ("BranchCreateRequest", &["name", "from", "from_commit"]),
        (
            "BranchCreateOutput",
            &["uri", "from", "name", "from_commit_id", "actor_id"],
        ),
        ("BranchDeleteOutput", &["uri", "name", "actor_id"]),
        (
            "BranchStartRefusalOutput",
            &["reason", "graph_commit_id", "branch", "type_name"],
        ),
        (
            "ChangeDiffRefusalOutput",
            &["reason", "graph_commit_id", "type_name"],
        ),
        (
            "AccessOutput",
            &[
                "graph_id",
                "actor_id",
                "graph_wide",
                "branch",
                "target_branch",
                "actions",
            ],
        ),
        (
            "BranchMergeRequest",
            &["source", "target", "delete_branch", "dry_run"],
        ),
        (
            "BranchMergeOutput",
            &[
                "source",
                "target",
                "outcome",
                "commit",
                "actor_id",
                "branch_deleted",
                "branch_delete_error_details",
            ],
        ),
        ("GraphDiscoveryOutput", &["graphs"]),
        ("GraphDiscoveryEntry", &["graph_id", "display_name"]),
        (
            "CommitListOutput",
            &["commits", "head_commit_id", "next_page_token"],
        ),
        (
            "CommitOutput",
            &[
                "graph_commit_id",
                "graph_manifest_version",
                "created_at",
                "graph_branch",
                "parent_commit_id",
                "merged_parent_commit_id",
                "actor_id",
                "kind",
            ],
        ),
        (
            "ChangeFeedOutput",
            &["blocks", "next_page_token", "cursor", "caught_up"],
        ),
        ("ChangeBlockOutput", &["cause", "changes"]),
        (
            "ChangeCauseOutput",
            &[
                "graph_commit_id",
                "authored_branch",
                "authored_at",
                "parent_commit_id",
                "merged_parent_commit_id",
                "actor_id",
            ],
        ),
        (
            "EntityChangeOutput",
            &["kind", "type", "id", "op", "before", "after"],
        ),
        ("ChangeTypeOutput", &["id", "name"]),
        ("ChangeImageOutput", &["properties", "endpoints"]),
        (
            "BlobWriteOutput",
            &[
                "selector", "branch", "kind", "commit", "size", "etag", "actor_id",
            ],
        ),
        ("BlobSelectorOutput", &["entity", "type", "id", "property"]),
        ("BlobPreconditionFailureOutput", &["current_etag"]),
        (
            "ErrorOutput",
            &[
                "error",
                "code",
                "disposition",
                "merge_conflicts",
                "precondition_failure",
                "blob_precondition_failure",
                "branch_start_refusal",
                "change_diff_refusal",
                "diagnostic",
            ],
        ),
        (
            "MergeConflictOutput",
            &["entity_kind", "type_name", "kind", "message", "entity_id"],
        ),
        ("PreconditionFailureOutput", &["expected", "actual"]),
        ("DispositionOutput", &["action", "effects"]),
        (
            "InvokeStoredQueryRequest",
            &["params", "branch", "snapshot", "expect_mutation"],
        ),
        ("QueriesCatalogOutput", &["queries"]),
        (
            "QueryCatalogEntry",
            &[
                "name",
                "tool_name",
                "description",
                "instruction",
                "mutation",
                "params",
            ],
        ),
        (
            "ParamDescriptor",
            &["name", "kind", "item_kind", "vector_dim", "nullable"],
        ),
        (
            "DiffOutput",
            &[
                "from_commit_id",
                "to_commit_id",
                "changes",
                "next_page_token",
            ],
        ),
        // Read as `CommitChanges`.
        (
            "CommitChangesOutput",
            &["cause", "changes", "next_page_token"],
        ),
    ];

    /// (schema name, the variants our enum declares, in serde's spelling).
    const ENUMS: &[(&str, &[&str])] = &[
        (
            "GraphAvailability",
            &["loading", "ready", "transitioning", "blocked", "stopping"],
        ),
        (
            "GraphAvailabilityAction",
            &[
                "none",
                "wait_for_startup",
                "wait_for_transition",
                "apply_correction_or_restart",
                "wait_for_restart",
            ],
        ),
        (
            "GraphStartupFailure",
            &[
                "invalid_configuration",
                "invalid_policy",
                "invalid_external_blob_policy",
                "open_failed",
                "invalid_stored_queries",
            ],
        ),
        ("EntityKind", &["node", "edge"]),
        ("BlobWriteState", &["managed", "null"]),
        (
            "BranchMergeOutcome",
            &["already_up_to_date", "fast_forward", "merged"],
        ),
        ("ChangeOp", &["insert", "update", "delete"]),
        (
            "ErrorCode",
            &[
                "unauthorized",
                "forbidden",
                "bad_request",
                "api_contract_mismatch",
                "not_found",
                "method_not_allowed",
                "conflict",
                "too_many_requests",
                "service_unavailable",
                "graph_unavailable",
                "internal",
                "unattached_cannot_publish",
                "actor_mismatch",
                "outcome_unknown",
                "precondition_failed",
                "recovery_required",
                "external_blob_source",
                "change_feed_gap",
                "request_timeout",
                "branch_start_refusal",
                "payload_too_large",
                "unsupported_media_type",
                "range_not_satisfiable",
                "cancelled",
                "deadline_exceeded",
                "result_too_large",
            ],
        ),
        (
            "DispositionAction",
            &["fix", "refresh", "retry", "recover", "reconcile"],
        ),
        ("DispositionEffects", &["none", "partial", "unknown"]),
        (
            "ChangeDiffRefusalReason",
            &["parentless_commit", "schema_boundary", "unknown"],
        ),
        (
            "BranchStartRefusalReason",
            &[
                "not_retained",
                "schema_changed",
                "earlier_storage_format",
                "branch_deleted",
                "unpublished_commit",
                "unattached_snapshot",
                "unknown",
            ],
        ),
        (
            "AccessAction",
            &[
                "read",
                "export",
                "change",
                "schema_apply",
                "branch_create",
                "branch_delete",
                "branch_merge",
                "commit_publish",
                "commit_drop",
                "invoke_query",
            ],
        ),
        ("CommitKind", &["published", "unpublished", "unattached"]),
        (
            "ParamKind",
            &[
                "string", "bool", "int", "bigint", "float", "date", "datetime", "blob", "vector",
                "list",
            ],
        ),
        (
            "MergeConflictKind",
            &[
                "divergent_insert",
                "divergent_update",
                "delete_vs_update",
                "orphan_edge",
                "unique_violation",
                "cardinality_violation",
                "value_constraint_violation",
            ],
        ),
    ];

    fn schemas() -> serde_json::Map<String, Value> {
        let contract: Value = serde_json::from_str(CONTRACT).expect("contract parses");
        contract["components"]["schemas"]
            .as_object()
            .expect("schemas object")
            .clone()
    }

    #[test]
    fn object_types_match_the_contract() {
        let schemas = schemas();
        for (name, ours) in OBJECTS {
            let schema = schemas
                .get(*name)
                .unwrap_or_else(|| panic!("{name} not in contract"));
            let theirs: BTreeSet<&str> = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name} has no properties"))
                .keys()
                .map(String::as_str)
                .collect();
            let required: BTreeSet<&str> = schema["required"]
                .as_array()
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let ours: BTreeSet<&str> = ours.iter().copied().collect();
            let unknown: Vec<_> = ours.difference(&theirs).collect();
            assert!(
                unknown.is_empty(),
                "{name}: fields not in the contract: {unknown:?}"
            );
            let missing: Vec<_> = required.difference(&ours).collect();
            assert!(
                missing.is_empty(),
                "{name}: required fields not declared: {missing:?}"
            );
        }
    }

    #[test]
    fn enum_types_match_the_contract() {
        let schemas = schemas();
        for (name, ours) in ENUMS {
            let schema = schemas
                .get(*name)
                .unwrap_or_else(|| panic!("{name} not in contract"));
            let theirs: BTreeSet<&str> = schema["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} is not an enum"))
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let ours: BTreeSet<&str> = ours.iter().copied().collect();
            assert_eq!(ours, theirs, "{name}: variants differ from the contract");
        }
    }

    /// A value a newer server sends decodes as `Other`, so its answer is still read: an error
    /// with a new code is still a refusal, with its message.
    #[test]
    fn a_value_this_build_does_not_know_still_decodes() {
        use super::{ErrorCode, ErrorOutput, GraphAvailability};
        let error: ErrorOutput =
            serde_json::from_str(r#"{"error":"slow down","code":"brand_new_code"}"#).unwrap();
        assert_eq!(error.code, Some(ErrorCode::Other));
        assert_eq!(error.error, "slow down");
        let state: GraphAvailability = serde_json::from_str(r#""draining""#).unwrap();
        assert_eq!(state, GraphAvailability::Other);
        let op: super::ChangeOp = serde_json::from_str(r#""upsert""#).unwrap();
        assert_eq!(op, super::ChangeOp::Other);
    }

    /// A commit held in a detached `__manifest` version decodes: Lance sets the high bit of a
    /// detached version, so it is past `i64::MAX`, as the server answers a write `as commit`.
    /// A snapshot's versions are as wide.
    #[test]
    fn a_detached_manifest_version_decodes() {
        use super::{CommitKind, CommitOutput, SnapshotOutput};
        let detached: u64 = 10_415_222_161_995_537_841;
        assert!(detached > i64::MAX as u64);
        let commit: CommitOutput = serde_json::from_value(serde_json::json!({
            "graph_commit_id": "01JPROPOSAL", "graph_branch": null,
            "graph_manifest_version": detached, "parent_commit_id": "c1",
            "merged_parent_commit_id": null, "actor_id": "act-dev", "created_at": 0,
            "published": false, "kind": "unpublished", "root_commit_id": null
        }))
        .unwrap();
        assert_eq!(commit.graph_manifest_version, detached);
        assert_eq!(commit.kind, CommitKind::Unpublished);
        let snapshot: SnapshotOutput = serde_json::from_value(serde_json::json!({
            "graph_branch": "main", "graph_manifest_version": u64::MAX,
            "internal_schema_version": 16,
            "datasets": [{
                "entity_kind": "node", "type_name": "Person", "dataset_path": "nodes/1",
                "published_dataset_version": detached, "native_dataset_branch": null,
                "entity_count": 3
            }]
        }))
        .unwrap();
        assert_eq!(snapshot.graph_manifest_version, u64::MAX);
        assert_eq!(snapshot.datasets[0].published_dataset_version, detached);
    }

    /// A `branch diff` answer says where its page stands, as `branch_diff_read_output` writes
    /// it; any other answer has no page.
    #[test]
    fn a_branch_diff_answer_carries_its_page() {
        use super::{ReadOutput, ReadPageOutput};
        let diff: ReadOutput = serde_json::from_str(
            r#"{"query_name":"branch diff","target":{"branch":null,"snapshot":null},"row_count":0,"columns":["kind","type","id","op","before","after"],"rows":[],"page":{"from_commit_id":"c1","to_commit_id":"c2","next_page_token":"t2"}}"#,
        )
        .unwrap();
        assert_eq!(
            diff.page,
            Some(ReadPageOutput {
                from_commit_id: Some("c1".into()),
                to_commit_id: Some("c2".into()),
                base_commit_id: None,
                next_page_token: Some("t2".into()),
            })
        );
        let read: ReadOutput = serde_json::from_str(
            r#"{"query_name":"q","target":{"branch":"main"},"row_count":0,"rows":[]}"#,
        )
        .unwrap();
        assert_eq!(read.page, None);
    }
}
