//! Studio checks for, downloads, verifies and installs its own updates (spec 017).
//!
//! Layer 3 (`docs/architecture.md`): the only crate that makes requests not to an Omnigraph
//! server, and only to Studio's own releases.

pub mod install;
pub mod manifest;
