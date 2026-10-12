//! Schema and query intelligence on `omnigraph-compiler`: the schema model, every query Studio
//! generates, diagnostics with suggested fixes, result-column types, completion, the lexical
//! outline used for highlighting and running one declaration, the parts of a declaration a plan
//! operator stands for, and typed parameter values. No GPUI.
//!
//! Layer 2 (`docs/architecture.md`). Plans: `specs/004-*`, `specs/005-*`, `specs/007-*`.

pub mod builder;
pub mod check;
pub mod complete;
pub mod input;
pub mod lex;
pub mod params;
pub mod parts;
pub mod schema;

pub use builder::{BuildError, BuiltQuery, Filter, FilterOp, PageStart, Sort, example_read};
pub use check::{Diagnostic, QueryCheck, QueryInfo, ResultColumn, check};
pub use complete::{Completion, CompletionKind, completions, hover};
pub use lex::{
    Declaration, Outline, Statement, Token, TokenKind, looks_like_mutation, offset_of, outline,
    problem_range, tokenize,
};
pub use params::param_value;
pub use parts::{PartKind, QueryPart, operator_parts, query_parts};
pub use schema::{
    Direction, EdgeType, NodeType, Property, RelationshipGroup, ScalarKind, SchemaModel,
    traversal_name,
};
