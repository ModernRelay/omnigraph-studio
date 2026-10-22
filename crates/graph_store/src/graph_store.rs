//! GPUI entities over `graph_client`: the server registry, connections, graph sessions with
//! schema and counts, and the outcome log of every write.
//!
//! Layer 3 (`docs/architecture.md`). Plan: `specs/003-connections/plan.md`.

mod access;
pub mod carry;
mod changes;
pub mod cli_config;
mod cli_tokens;
mod connection;
mod degrees;
pub mod deletes;
mod entity_groups;
mod exploration;
pub mod fixes;
mod memo;
mod refusal;
mod registry;
pub mod scope;
mod session;
#[cfg(any(test, feature = "test-support"))]
pub mod test_server;

use std::sync::Arc;

use gpui::App;

pub use access::{AccessScope, Allowed};
pub use changes::{
    ChangeFold, FoldedChange, FoldedChanges, ForkSearch, NetChange, PropertyDiff,
    REVIEW_CHANGE_CAP, TypeCounts, contains_merge_of, plural, summary_sentence,
};
pub use connection::{
    Connection, ConnectionEvent, ConnectionState, ConnectionWatch, GraphListing, ServerEntry,
    ServerSource,
};
pub use degrees::page_counts;
pub use entity_groups::{
    EntityGroup, EntityGroups, GroupRelationship, GroupRun, GroupSection, NodeNames,
    RelationshipEntry, RelationshipLines, entity_name, relationship_words,
};
pub use exploration::*;
pub use fixes::{RevertUnavailable, revert_query};
pub use graph_client::api::{AccessAction, EntityKind};
pub use registry::ConnectionRegistry;
pub use session::{
    BranchStart, DraftEntry, DraftKey, ExternalWrite, FollowUp, GraphSession, HeadWatch, HeldAs,
    Loadable, MergeEntry, MergeSent, MergeSettled, OutcomeEntry, OutcomeKind, ServerState,
    SessionEvent, open_session, open_session_behind,
};

/// The user agent of Studio's requests.
pub(crate) const USER_AGENT: &str = concat!("OmniGraph-Studio/", env!("CARGO_PKG_VERSION"));

/// Sets up the HTTP client and loads every known server. Call once at startup.
pub fn init(cx: &mut App) {
    let http: Arc<dyn http_client::HttpClient> = Arc::new(
        reqwest_client::ReqwestClient::user_agent(USER_AGENT).expect("the HTTP client starts"),
    );
    cx.set_http_client(http.clone());
    ConnectionRegistry::init(http, cx);
}
