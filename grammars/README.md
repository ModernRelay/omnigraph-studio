# Grammars

Tree-sitter grammars for OmniGraph's languages, used by the query editor for highlighting,
folding and outline (spec 007):

- `tree-sitter-gq`: queries and mutations (`.gq`), GQ language version 2.1 at OmniGraph v0.13.0.
- `tree-sitter-pg`: schemas (`.pg`).

The source of truth for both languages is OmniGraph's pest grammars
(`crates/omnigraph-compiler/src/query/query.pest` and `schema/schema.pest` at the pinned tag).
Type checking, diagnostics and completion come from `omnigraph-compiler` itself, not from these
grammars.

These grammars live here until OmniGraph takes ownership of them together with an
`omnigraph lsp` server (`docs/deferred-engine-changes.md`).
