//! One virtualized table per node or edge type: engine-side filter, sort and count, keyset pages pinned to one commit, peek, value-bound cell edits.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/004-*/plan.md`.

/// Registers this surface with every new workspace.
pub fn init(_cx: &mut gpui::App) {}
