# 007: Query editor

Status: first slice landed · 2026-10-12 (`crates/query_editor`). Working: Zed's editor with
lexical highlighting, checks as you type, completion and expected result columns; ⌘↵ reads,
confirmed writes, parameters, a virtualized result grid with copy, and `explain` as a tree.
Open items are under Deferred.

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

## What landed (2026-10-12)

- `NewQuery` (⌘N) opens "Query 1", "Query 2", …, starting with a read over the schema's first
  node type (`gq_language::example_read`).
- Highlighting is lexical (`gq_language::tokenize`), applied as editor text highlights with the
  theme's syntax colors; no tree-sitter grammar yet.
- After typing pauses (150 ms) the source is checked off the main thread (`analysis.rs`): the
  whole file for problems, each declaration on its own for mutation/read, parameters and result
  columns, so one broken declaration does not hide its siblings. Problems are underlined and
  listed under the editor (code, message, fix, line:column; a click moves the cursor there).
  A type error the compiler reports without a position is placed on the last name its message
  quotes.
- Completion goes through Zed's `CompletionProvider` with `gq_language::completions`.
- ⌘↵ runs the declaration under the cursor (else the last one before it, else the first) as the
  settings prefix plus that declaration, so a broken sibling does not stop it. A read goes to
  `POST /query` on the session branch; the result header shows rows, elapsed time, the branch
  and the `graph_commit_id` read at. Before the first run, the expected columns and their
  compiler types are shown.
- A mutation never runs on ⌘↵: a bar names the branch ("Run on main", ↵) and Esc cancels. The
  write is sent once, never retried, and its outcome is shown as committed (nodes, edges,
  commit), refused ("Nothing was written", with the server's diagnostic) or unknown ("Check
  History before trying again"). The text is the draft and is never cleared. Closing the tab
  does not cancel a write in flight; the session logs its outcome.
- Parameters of the query under the cursor get an input each; values are typed by
  `gq_language::param_value` and sent as JSON (`null` for an empty nullable).
- ⌘⇧↵ sends `explain query …` and shows the logical, physical and DataFusion trees indented by
  depth, then the planner's notes.
- Statements (`branch …`, `show …`) run without a request target; `branch create/delete/merge`
  asks first like a mutation.

## Deferred

- Run buttons in the gutter and inline short results; hover types; hovering a plan operator to
  highlight its part of the query.
- The snapshot picker (run a read at a past commit). `QueryRequest.snapshot` is there; the editor
  sends the branch today.
- Tree-sitter highlighting, folding and outline (`grammars/tree-sitter-gq`); quick fixes from
  `Suggestion`/`TextEdit`.
- Opening and saving `.gq` files, and saved queries per graph.
- A row cap: the result shows every row the server returns, so the query's own `limit` is the
  cap today (constitution IX). The example query carries `limit 50`.
- A read's server diagnostic: `graph_client::ReadError::Refused` keeps only the message, status
  and code, not `ErrorOutput.diagnostic`, so a refused read shows the server's message and the
  local problems list, but no server position. Writes show it (`ApiError.body.diagnostic`).
  Needs a small `graph_client` change.
- A write from the editor sends no `Omnigraph-If-Graph-Commit`: it is not based on a read, and
  the person confirms it against the branch head.
- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E11 typed result columns: types from the compiler until then.
  - E12 query profile over HTTP: plans only, through `explain query …`, until then.
  - E17 `omnigraph lsp` and E18 grammars in the OmniGraph repository: the compiler runs
    in-process and the grammars live in `grammars/` until then.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Compiler APIs above exist | `git show v0.13.0:crates/omnigraph-compiler/src/lib.rs` | All exported |
| 2026-10-12 | Zed's editor works with no project: completion provider, text highlights, placeholder | `editor/src/completions.rs` (`CompletionProvider`, `set_completion_provider`, no `project` needed outside snippets), `keymap_editor/src/action_completion_provider.rs` (a project-less provider), `editor/src/display_map.rs` (`HighlightKey`) at `v1.23.2`; offscreen snapshots of completion and underlines | Works. `HighlightKey` is a closed enum; `ConsoleAnsiHighlight(n)` is the one numbered key, used for syntax classes and diagnostics |
| 2026-10-12 | Diagnostics within S4's 100 ms after typing stops on a 500-line file | `analysis_time_on_a_500_line_source` (ignored instrument), debug build, Apple M5 Pro, 48 GB, 504 lines / 63 queries | 12.6 ms per analysis. With the 150 ms debounce the underline lands about 163 ms after the last key, over S4's 100 ms; the debounce, not the check, is the cost (see Corrections) |
| 2026-10-12 | `explain` rows | `POST /query` with `explain query …` against the 0.13 dev server | Columns `tree, depth, node, detail`; trees `logical`, `physical`, `datafusion`, then `plan` rows with a null depth (`pass`, `route`, `logical_hash`, `statistics`, `assumptions`) |
| 2026-10-12 | Statements and mutations on `POST /query` | dev server | A statement with a `branch` target is refused ("a branch statement names its branches itself; drop the request target"); a mutation is refused ("use POST /mutate for write queries"), so a mis-classified write cannot run as a read |
| 2026-10-12 | Compiler result names | `describe_query_operation` in `gq_language::check` | Names a projected property alone (`slug`) where the server says `c.slug`; types are matched to server columns by position |
| 2026-10-12 | Run loop end to end | offscreen snapshots against the dev server: example query, run, explain, parameters, confirm bar, committed and refused writes, a parse error underline, completion | As described under What landed |

## Corrections

- 2026-10-12 · Design said a mutation's run button "asks once per tab". The editor asks every
  time: each ⌘↵ on a write shows the confirm bar, and only ↵ on the bar or its button sends it.
  Asking once per tab would let the second ⌘↵ write on its own.
- 2026-10-12 · Spike S4's 100 ms budget and the 150 ms debounce disagree. The analysis takes
  12.6 ms on 500 lines (debug build), so a debounce of about 80 ms meets the budget; it stays
  at 150 ms until someone decides which number wins.
- 2026-10-12 · `gq_language::Diagnostic.code` was the code's `Debug` form
  (`QueryCode(QueryCodeSpec { code: "T12", … })`); it is now the code (`T12`), with a test.
