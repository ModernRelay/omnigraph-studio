//! GPUI entities over `graph_client`: the server registry, connections, graph sessions with
//! schema and counts, and the outcome log of every write.
//!
//! Layer 3 (`docs/architecture.md`). Plan: `specs/003-connections/plan.md`.

pub mod changes;
pub mod cli_config;
mod connection;
pub mod degrees;
mod exploration;
pub mod fixes;
mod memo;
mod registry;
mod session;

use std::sync::Arc;

use gpui::App;

pub use changes::{
    ChangeFold, FoldedChange, FoldedChanges, NetChange, PropertyDiff, REVIEW_CHANGE_CAP,
    TypeCounts, commits_since, contains_merge_of, fork_point, grouped, summary_sentence,
};
pub use connection::{
    Connection, ConnectionEvent, ConnectionState, GraphListing, ServerEntry, ServerSource,
};
pub use exploration::*;
pub use fixes::{RevertUnavailable, revert_query};
pub use registry::{ConnectionRegistry, RegistryEvent, data_dir};
pub use session::{
    CommitList, GraphSession, Loadable, OutcomeEntry, OutcomeKind, SessionEvent, open_session,
};

/// Sets up the HTTP client and loads every known server. Call once at startup.
pub fn init(cx: &mut App) {
    let http: Arc<dyn http_client::HttpClient> = Arc::new(
        reqwest_client::ReqwestClient::user_agent(concat!(
            "OmniGraph-Studio/",
            env!("CARGO_PKG_VERSION")
        ))
        .expect("the HTTP client starts"),
    );
    cx.set_http_client(http.clone());
    ConnectionRegistry::init(http, cx);
}
