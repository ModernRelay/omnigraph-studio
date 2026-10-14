//! Request and response types for the OmniGraph 0.13 HTTP contract.
//!
//! Field names and optionality follow `openapi/omnigraph-0.13.0.json`; the test at the bottom
//! of this file fails if a type here and the contract disagree. Add fields only as surfaces
//! need them, and add each new type to the test.

use serde::{Deserialize, Serialize};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphAvailabilityAction {
    None,
    WaitForStartup,
    WaitForTransition,
    ApplyCorrectionOrRestart,
    WaitForRestart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphStartupFailure {
    InvalidConfiguration,
    InvalidPolicy,
    InvalidExternalBlobPolicy,
    OpenFailed,
    InvalidStoredQueries,
}

// ---- schema and snapshot ----

/// `GET /graphs/{id}/schema`.
#[derive(Debug, Clone, Deserialize)]
pub struct SchemaOutput {
    pub schema_source: String,
    #[serde(default)]
    pub system_columns: Option<SystemColumnsOutput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemColumnsOutput {
    pub id: String,
    pub src: String,
    pub dst: String,
}

/// `GET /graphs/{id}/snapshot?branch=`.
#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotOutput {
    pub graph_branch: String,
    pub graph_manifest_version: i64,
    pub internal_schema_version: i32,
    pub datasets: Vec<SnapshotDatasetOutput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotDatasetOutput {
    pub entity_kind: EntityKind,
    pub type_name: String,
    pub dataset_path: String,
    pub published_dataset_version: i64,
    pub entity_count: i64,
    #[serde(default)]
    pub native_dataset_branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

/// A stored-query parameter's kind, as the catalog names it (0.13). One Studio does not know
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
    pub rows: Value,
    #[serde(default)]
    pub graph_commit_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadTargetOutput {
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub snapshot: Option<String>,
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

/// `POST /graphs/{id}/branches`.
#[derive(Debug, Clone, Serialize)]
pub struct BranchCreateRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BranchCreateOutput {
    pub uri: String,
    pub from: String,
    pub name: String,
    #[serde(default)]
    pub actor_id: Option<String>,
}

/// `POST /graphs/{id}/branches/merge`.
#[derive(Debug, Clone, Serialize)]
pub struct BranchMergeRequest {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub delete_branch: bool,
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
}

/// `GET /graphs/{id}/commits?branch=`.
#[derive(Debug, Clone, Deserialize)]
pub struct CommitListOutput {
    pub commits: Vec<CommitOutput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitOutput {
    pub graph_commit_id: String,
    pub graph_manifest_version: i64,
    /// Unix microseconds.
    pub created_at: i64,
    #[serde(default)]
    pub graph_branch: Option<String>,
    #[serde(default)]
    pub parent_commit_id: Option<String>,
    #[serde(default)]
    pub merged_parent_commit_id: Option<String>,
    #[serde(default)]
    pub actor_id: Option<String>,
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
}

/// An entity's properties at one side of a change. A null keeps its key; an absent key means
/// the property was outside that commit's schema. `endpoints` is present for edges only.
#[derive(Debug, Clone, Deserialize)]
pub struct ChangeImageOutput {
    pub properties: serde_json::Map<String, Value>,
    #[serde(default)]
    pub endpoints: Option<Value>,
}

// ---- errors ----

/// Every refusal's body. Typed details are added as surfaces need them; the rest stay in
/// `other` so nothing the server sent is lost.
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorOutput {
    pub error: String,
    #[serde(default)]
    pub code: Option<ErrorCode>,
    #[serde(default)]
    pub merge_conflicts: Option<Vec<MergeConflictOutput>>,
    #[serde(default)]
    pub precondition_failure: Option<PreconditionFailureOutput>,
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

    const CONTRACT: &str = include_str!("../openapi/omnigraph-0.13.0.json");

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
        ("GraphListResponse", &["graphs"]),
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
        ("SchemaOutput", &["schema_source", "system_columns"]),
        ("SystemColumnsOutput", &["id", "src", "dst"]),
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
        ("BranchCreateRequest", &["name", "from"]),
        ("BranchCreateOutput", &["uri", "from", "name", "actor_id"]),
        ("BranchMergeRequest", &["source", "target", "delete_branch"]),
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
        ("CommitListOutput", &["commits"]),
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
            "ErrorOutput",
            &[
                "error",
                "code",
                "merge_conflicts",
                "precondition_failure",
                "diagnostic",
            ],
        ),
        (
            "MergeConflictOutput",
            &["entity_kind", "type_name", "kind", "message", "entity_id"],
        ),
        ("PreconditionFailureOutput", &["expected", "actual"]),
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
        ("EntityKindOutput", &["node", "edge"]),
        (
            "BranchMergeOutcome",
            &["already_up_to_date", "fast_forward", "merged"],
        ),
        ("ChangeOpOutput", &["insert", "update", "delete"]),
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
            ],
        ),
        (
            "MergeConflictKindOutput",
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
}
