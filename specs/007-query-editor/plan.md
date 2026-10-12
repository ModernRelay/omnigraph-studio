# 007: Query editor

Status: draft · 2026-10-11

## Problem

Writing `.gq` should feel like writing code in Zed: highlighting, errors as you type with
suggested fixes, completion from the branch's schema, hover types, and a tight run loop with
results next to the query.

## Principles

I (`omnigraph-compiler` in-process is language intelligence, not data access), II (results pin
a commit), VII (mutations never run by accident), XII (spike S4 decides the editor).

## What exists today (checked)

- `omnigraph-compiler` `v0.13.0` exports: `build_catalog`, `compile_schema_shape`,
  `lint_query_file`, `describe_query_operation` (result columns before running),
  `QueryDiagnostic` with `Position`, `Suggestion` and `TextEdit` (machine-applicable fixes),
  and `GQ_LANGUAGE_VERSION = (2, 1)`.
- `POST /query` accepts `explain query …` and returns plan rows; mutations go to `POST /mutate`.
- Zed `v1.23.2`: `editor` accepts completion and semantics providers without a language server;
  Zed's own editor benchmark runs it with no project. It compiles about 100 Zed crates.

## Design

- **Editor:** Zed's `editor` in full mode, if spike S4 passes; otherwise Studio's own editor on
  GPUI's text input primitives, as Delta did.
- **Language service (`gq_language`):** parse and type-check against the catalog built from the
  pinned schema; diagnostics with byte ranges and suggested edits shown as quick fixes;
  completion of types, properties, edges, parameters and keywords; hover shows a binding's type
  and a property's declared type.
- **Grammar:** `grammars/tree-sitter-gq` for highlighting, folding and outline (and
  `tree-sitter-pg` for schemas). They live here until OmniGraph owns them (deferred).
- **Run loop:** a run button in the gutter beside each `query` block, showing the last status.
  ⌘↵ runs the block under the cursor. Results open under the editor as a table (spec 004's grid)
  or a canvas. Short results print inline ("✓ 3 nodes, 1 edge"); editing the query removes stale
  inline results.
- **Mutations:** never run on ⌘↵ alone. The run button for a mutation names the branch ("Run on
  feature-x") and asks once per tab.
- **Snapshot:** a commit picker in the editor's toolbar runs reads at a past commit.
- **Explain:** `explain query …` rendered as a tree; hovering an operator highlights its part of
  the query.
- **Files:** `.gq` files on disk can be opened; queries can be saved per graph.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Compiler in-process | A language server | No process to manage; the server is a deferred engine change |
| Zed's editor behind a spike | Write an editor first | Zed's editor is the best available if its build cost is acceptable |

## Deferred

- `omnigraph lsp` and moving the grammars to OmniGraph (`docs/deferred-engine-changes.md`).
- Query profiles over HTTP (deferred); plans only for now.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Compiler APIs above exist | `git show v0.13.0:crates/omnigraph-compiler/src/lib.rs` | All exported |
| pending | Editor build cost | spike S4 | |

## Corrections

None yet.
