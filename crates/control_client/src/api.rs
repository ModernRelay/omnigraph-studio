//! The control plane's answers Studio reads, and the one request body it sends.
//!
//! Field names and optionality follow `openapi/control-plane-0.1.0.json`; the tests at the
//! bottom of this file fail when a type here and that document disagree. Add fields only as a
//! surface needs them: an unknown field in an answer is ignored, so a later control plane
//! still decodes.

use serde::{Deserialize, Serialize};

/// Every 2xx answer: what was asked for, and where it came from.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Envelope<T> {
    pub data: T,
    pub meta: Meta,
}

/// Where an answer came from and how current it is. Studio shows `stale` and `observed_at`
/// with what it read, never its own idea of freshness (constitution I).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Meta {
    pub provenance: Provenance,
    pub assurance: String,
    pub stale: bool,
    pub observed_at: String,
    #[serde(default)]
    pub cluster_id: Option<String>,
    #[serde(default)]
    pub incarnation: Option<String>,
    #[serde(default)]
    pub state_revision: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Storage,
    ServiceDb,
    Telemetry,
    /// A value a later control plane may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

/// `GET /v1/organizations`: the organizations the signed-in person is an active member of.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OrganizationList {
    pub organizations: Vec<OrganizationSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OrganizationSummary {
    pub organization_id: String,
    /// Its name, as the identity provider holds it.
    pub name: Option<String>,
}

/// `GET /v1/clusters`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClusterList {
    pub clusters: Vec<ClusterView>,
}

/// One cluster of the organization, as the list and its detail describe it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClusterView {
    pub cluster_id: String,
    pub name: String,
    pub incarnation: String,
    pub canonical_root: String,
    pub binding: Binding,
    /// `provisioning`, `ready`, …: shown as answered.
    pub state: String,
    pub cell: ClusterCell,
    pub created_by: String,
    pub created_at: String,
    /// Present only while data access is available.
    #[serde(default)]
    pub data_endpoint: Option<String>,
    /// `{data_endpoint}/mcp`, the cluster's OAuth resource; present with `data_endpoint`.
    #[serde(default)]
    pub mcp_url: Option<String>,
}

/// Where a cluster's configuration comes from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Binding {
    pub mode: String,
    pub repository: String,
    #[serde(default)]
    pub r#ref: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub entry: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClusterCell {
    pub namespace: String,
    pub cluster: String,
}

/// `GET /v1/clusters/{id}`: the cluster, and what the service last observed of it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClusterDetail {
    #[serde(flatten)]
    pub cluster: ClusterView,
    pub service_observation: ServiceObservation,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ServiceObservation {
    pub cluster_id: String,
    pub incarnation: String,
    pub lifecycle_provenance: String,
    pub lifecycle_state: String,
    pub latest_operation_id: Option<String>,
    pub latest_operation_kind: Option<OperationKind>,
    pub latest_operation_state: Option<String>,
    pub latest_operation_created_at: Option<String>,
    pub deletion_id: Option<String>,
    pub undo_deadline: Option<String>,
    pub cleanup_requires_zero_retention: Option<bool>,
    pub health_provenance: String,
    pub observed_at: Option<String>,
    pub freshness: Freshness,
    pub serving_health: ServingHealth,
    pub backup: Option<BackupObservation>,
    pub observability_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Create,
    Delete,
    Undo,
    /// A value a later control plane may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Stale,
    Unknown,
    /// A value a later control plane may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServingHealth {
    Ready,
    Unavailable,
    Unknown,
    /// A value a later control plane may send, so an answer still decodes.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BackupObservation {
    pub observed_at: Option<String>,
    pub freshness: Freshness,
    pub completed_recovery_points: Option<i64>,
    pub latest_completed_at: Option<String>,
    pub restore_qualified: Option<bool>,
}

/// `GET /v1/clusters/{id}/status`. `source` and `observed` are untyped in the document, so
/// they are kept as JSON and not shown (spec 021, Deferred).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClusterStatus {
    pub cluster_id: String,
    pub incarnation: String,
    pub source: serde_json::Value,
    #[serde(default)]
    pub requested: Option<RequestedSource>,
    /// The latest deployment.
    #[serde(default)]
    pub delivery: Option<DeploymentView>,
    pub observed: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RequestedSource {
    pub revision: String,
    pub bundle_digest: String,
    pub input_digest: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DeploymentView {
    pub deployment_id: String,
    pub native_deployment_id: String,
    pub preview_id: String,
    /// How the deployment's delivery stands, as answered.
    pub delivery: String,
    pub archive: String,
    pub first_attempt_deadline: String,
    #[serde(default)]
    pub attempted_at: Option<String>,
    #[serde(default)]
    pub native_result_status: Option<String>,
    pub observation: serde_json::Value,
}

/// The body of `POST /v1/clusters/{id}/tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct IdentityRequest {
    pub version: i32,
    pub ttl_seconds: Option<i64>,
}

/// A cluster's data token. Its `Debug` withholds the token (no token is ever logged).
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct DataCredential {
    pub version: i32,
    pub token: String,
    pub endpoint: String,
    pub expires_at: String,
    pub kid: String,
    pub actor: String,
}

impl std::fmt::Debug for DataCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataCredential")
            .field("version", &self.version)
            .field("token", &"<withheld>")
            .field("endpoint", &self.endpoint)
            .field("expires_at", &self.expires_at)
            .field("kid", &self.kid)
            .field("actor", &self.actor)
            .finish()
    }
}

/// An RFC 9457 refusal (`ProblemDetails`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Problem {
    /// The refusal's kind: `unauthenticated`, `scope_missing`, … as the control plane names it.
    pub r#type: String,
    pub title: String,
    pub status: u16,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub details_withheld: Option<bool>,
}

#[cfg(test)]
mod contract_tests {
    use std::collections::BTreeSet;

    use serde_json::{Value, json};

    use super::*;

    const CONTRACT: &str = include_str!("../openapi/control-plane-0.1.0.json");

    /// (schema name, the fields our type declares). Fields we declare must exist in the
    /// document, and every field it requires must be declared.
    const OBJECTS: &[(&str, &[&str])] = &[
        (
            "Meta",
            &[
                "provenance",
                "assurance",
                "stale",
                "observed_at",
                "cluster_id",
                "incarnation",
                "state_revision",
            ],
        ),
        ("ClusterList", &["clusters"]),
        ("OrganizationList", &["organizations"]),
        ("OrganizationSummary", &["organization_id", "name"]),
        (
            "ClusterView",
            &[
                "cluster_id",
                "name",
                "incarnation",
                "canonical_root",
                "binding",
                "state",
                "cell",
                "created_by",
                "created_at",
                "data_endpoint",
                "mcp_url",
            ],
        ),
        ("Binding", &["mode", "repository", "ref", "path", "entry"]),
        ("ClusterCell", &["namespace", "cluster"]),
        (
            "ServiceObservation",
            &[
                "cluster_id",
                "incarnation",
                "lifecycle_provenance",
                "lifecycle_state",
                "latest_operation_id",
                "latest_operation_kind",
                "latest_operation_state",
                "latest_operation_created_at",
                "deletion_id",
                "undo_deadline",
                "cleanup_requires_zero_retention",
                "health_provenance",
                "observed_at",
                "freshness",
                "serving_health",
                "backup",
                "observability_url",
            ],
        ),
        (
            "BackupObservation",
            &[
                "observed_at",
                "freshness",
                "completed_recovery_points",
                "latest_completed_at",
                "restore_qualified",
            ],
        ),
        (
            "NativeClusterStatus",
            &[
                "cluster_id",
                "incarnation",
                "source",
                "requested",
                "delivery",
                "observed",
            ],
        ),
        (
            "NativeRequestedSource",
            &["revision", "bundle_digest", "input_digest"],
        ),
        (
            "NativeDeploymentView",
            &[
                "deployment_id",
                "native_deployment_id",
                "preview_id",
                "delivery",
                "archive",
                "first_attempt_deadline",
                "attempted_at",
                "native_result_status",
                "observation",
            ],
        ),
        ("IdentityRequest", &["version", "ttl_seconds"]),
        (
            "DataCredential",
            &["version", "token", "endpoint", "expires_at", "kid", "actor"],
        ),
        (
            "ProblemDetails",
            &["type", "title", "status", "detail", "details_withheld"],
        ),
    ];

    /// (enum schema, the values our type names besides `Other`).
    const ENUMS: &[(&str, &[&str])] = &[
        ("Provenance", &["storage", "service_db", "telemetry"]),
        ("Freshness", &["fresh", "stale", "unknown"]),
        ("ServingHealth", &["ready", "unavailable", "unknown"]),
        ("OperationKind", &["create", "delete", "undo"]),
    ];

    /// (route, method, request schema or none, the 2xx envelope's schema). The routes this
    /// crate sends, as the document has them.
    const ROUTES: &[(&str, &str, Option<&str>, &str)] = &[
        (
            "/v1/organizations",
            "get",
            None,
            "Envelope_OrganizationList",
        ),
        ("/v1/clusters", "get", None, "Envelope_ClusterList"),
        (
            "/v1/clusters/{cluster_id}",
            "get",
            None,
            "Envelope_ClusterDetail",
        ),
        (
            "/v1/clusters/{cluster_id}/status",
            "get",
            None,
            "Envelope_NativeClusterStatus",
        ),
        (
            "/v1/clusters/{cluster_id}/tokens",
            "post",
            Some("IdentityRequest"),
            "Envelope_DataCredential",
        ),
    ];

    fn contract() -> Value {
        serde_json::from_str(CONTRACT).expect("the document parses")
    }

    fn reference(value: &Value) -> Option<&str> {
        value["$ref"]
            .as_str()?
            .strip_prefix("#/components/schemas/")
    }

    #[test]
    fn object_types_match_the_document() {
        let contract = contract();
        let schemas = &contract["components"]["schemas"];
        for (name, ours) in OBJECTS {
            let schema = &schemas[*name];
            assert!(schema.is_object(), "{name} not in the document");
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
                "{name}: not in the document: {unknown:?}"
            );
            let missing: Vec<_> = required.difference(&ours).collect();
            assert!(
                missing.is_empty(),
                "{name}: required, not declared: {missing:?}"
            );
        }
    }

    #[test]
    fn enum_types_match_the_document() {
        let contract = contract();
        for (name, ours) in ENUMS {
            let theirs: BTreeSet<&str> = contract["components"]["schemas"][*name]["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} is not an enum in the document"))
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let ours: BTreeSet<&str> = ours.iter().copied().collect();
            assert_eq!(ours, theirs, "{name}");
        }
    }

    /// The detail is the list's cluster with one more field, as `ClusterDetail` flattens it.
    #[test]
    fn detail_is_the_view_and_its_observation() {
        let contract = contract();
        let parts = contract["components"]["schemas"]["ClusterDetail"]["allOf"]
            .as_array()
            .expect("ClusterDetail is allOf");
        assert_eq!(reference(&parts[0]), Some("ClusterView"));
        assert_eq!(
            reference(&parts[1]["properties"]["service_observation"]),
            Some("ServiceObservation")
        );
        assert_eq!(parts[1]["required"], json!(["service_observation"]));
    }

    #[test]
    fn routes_and_their_envelopes_match_the_document() {
        let contract = contract();
        let schemas = &contract["components"]["schemas"];
        for (path, method, request, envelope) in ROUTES {
            let operation = &contract["paths"][*path][*method];
            assert!(operation.is_object(), "{method} {path} not in the document");
            assert_eq!(
                reference(&operation["responses"]["200"]["content"]["application/json"]["schema"]),
                Some(*envelope),
                "{method} {path}"
            );
            let body = &operation["requestBody"]["content"]["application/json"]["schema"];
            assert_eq!(reference(body), *request, "{method} {path} request");
            assert_eq!(
                schemas[*envelope]["required"],
                json!(["data", "meta"]),
                "{envelope}"
            );
            assert_eq!(
                reference(&schemas[*envelope]["properties"]["meta"]),
                Some("Meta"),
                "{envelope}"
            );
        }
    }

    /// The token request is version 2 with a TTL Studio's 900 s is within.
    #[test]
    fn the_token_request_is_within_the_document() {
        let contract = contract();
        let schema = &contract["components"]["schemas"]["IdentityRequest"]["properties"];
        assert_eq!(schema["version"]["minimum"], json!(2));
        assert_eq!(schema["version"]["maximum"], json!(2));
        let ttl = crate::DATA_TOKEN_SECONDS as i64;
        assert!(schema["ttl_seconds"]["minimum"].as_i64().unwrap() <= ttl);
        assert!(schema["ttl_seconds"]["maximum"].as_i64().unwrap() >= ttl);
    }

    /// Answers shaped as the live service sends them decode, unknown fields and values
    /// included.
    #[test]
    fn answers_decode() {
        let cluster = json!({
            "cluster_id": "01CLUSTER",
            "name": "acme",
            "incarnation": "01INCARNATION",
            "canonical_root": "s3://bucket/clusters/x",
            "binding": {"mode": "github", "repository": "acme/graph", "ref": "main"},
            "state": "ready",
            "cell": {"namespace": "ns", "cluster": "cell-1"},
            "created_by": "user_1",
            "created_at": "2026-11-01T00:00:00Z",
            "data_endpoint": "https://acme.graphs.example",
            "mcp_url": "https://acme.graphs.example/mcp",
            "a_later_field": true,
        });
        let meta = json!({
            "provenance": "a_later_source",
            "assurance": "observed",
            "stale": false,
            "observed_at": "2026-11-05T00:00:00Z",
        });
        let list: Envelope<ClusterList> =
            serde_json::from_value(json!({"data": {"clusters": [cluster.clone()]}, "meta": meta}))
                .unwrap();
        assert_eq!(list.meta.provenance, Provenance::Other);
        let view = &list.data.clusters[0];
        assert_eq!(
            view.data_endpoint.as_deref(),
            Some("https://acme.graphs.example")
        );
        assert_eq!(view.binding.r#ref.as_deref(), Some("main"));

        let mut provisioning = cluster.clone();
        let object = provisioning.as_object_mut().unwrap();
        object.remove("data_endpoint");
        object.remove("mcp_url");
        let provisioning: ClusterView = serde_json::from_value(provisioning).unwrap();
        assert_eq!(provisioning.data_endpoint, None);

        let mut detail = cluster;
        detail["service_observation"] = json!({
            "cluster_id": "01CLUSTER",
            "incarnation": "01INCARNATION",
            "lifecycle_provenance": "service_db",
            "lifecycle_state": "ready",
            "latest_operation_id": null,
            "latest_operation_kind": "create",
            "latest_operation_state": "succeeded",
            "latest_operation_created_at": null,
            "deletion_id": null,
            "undo_deadline": null,
            "cleanup_requires_zero_retention": null,
            "health_provenance": "telemetry",
            "observed_at": null,
            "freshness": "fresh",
            "serving_health": "ready",
            "backup": null,
            "observability_url": null,
        });
        let detail: ClusterDetail = serde_json::from_value(detail).unwrap();
        assert_eq!(detail.cluster.name, "acme");
        assert_eq!(
            detail.service_observation.serving_health,
            ServingHealth::Ready
        );

        let status: ClusterStatus = serde_json::from_value(json!({
            "cluster_id": "01CLUSTER",
            "incarnation": "01INCARNATION",
            "source": {"kind": "git"},
            "requested": null,
            "delivery": {
                "deployment_id": "d1",
                "native_deployment_id": "n1",
                "preview_id": "p1",
                "delivery": "converged",
                "archive": "a",
                "first_attempt_deadline": "2026-11-05T00:00:00Z",
                "observation": {},
            },
            "observed": null,
        }))
        .unwrap();
        assert_eq!(status.delivery.unwrap().delivery, "converged");
    }

    #[test]
    fn a_data_credential_never_prints_its_token() {
        let credential: DataCredential = serde_json::from_value(json!({
            "version": 2,
            "token": "secret-token-value",
            "endpoint": "https://acme.graphs.example",
            "expires_at": "2026-11-05T00:15:00Z",
            "kid": "k1",
            "actor": "user_1",
        }))
        .unwrap();
        let printed = format!("{credential:?}");
        assert!(!printed.contains("secret-token-value"), "{printed}");
        assert!(printed.contains("withheld"));
    }
}
