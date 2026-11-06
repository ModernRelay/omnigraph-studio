//! Studio's requests to a managed Omnigraph control plane (spec 021): the clusters of the
//! signed-in organization, a cluster's detail and status, and a cluster's data token. No GPUI
//! here; `graph_store` holds the entities that drive it.
//!
//! This crate alone calls a control plane, with a control-plane access token and never a graph
//! token. A data token it is given goes only to the endpoint it names (constitution I). Every
//! request has a time limit and a size cap, follows no redirect, and is never retried here.

mod api;
mod client;

pub use api::{
    BackupObservation, Binding, ClusterCell, ClusterDetail, ClusterList, ClusterStatus,
    ClusterView, DataCredential, DeploymentView, Envelope, Freshness, IdentityRequest, Meta,
    OperationKind, OrganizationList, OrganizationSummary, Problem, Provenance, RequestedSource,
    ServiceObservation, ServingHealth,
};
pub use client::{ControlClient, ControlError, DATA_TOKEN_SECONDS, Limits, ORGANIZATION_HEADER};
