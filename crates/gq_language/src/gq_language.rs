//! Schema and query intelligence on `omnigraph-compiler`: the schema model, every query Studio
//! generates, diagnostics with suggested fixes, result-column types, completion. No GPUI.
//!
//! Layer 2 (`docs/architecture.md`). Plans: `specs/004-*`, `specs/005-*`, `specs/007-*`.

pub mod builder;
pub mod check;
pub mod complete;
pub mod schema;

pub use builder::{BuildError, BuiltQuery, Filter, FilterOp};
pub use check::{Diagnostic, QueryCheck, QueryInfo, ResultColumn, check};
pub use complete::{Completion, CompletionKind, completions};
pub use schema::{
    Direction, EdgeType, NodeType, Property, RelationshipGroup, ScalarKind, SchemaModel,
    traversal_name,
};
