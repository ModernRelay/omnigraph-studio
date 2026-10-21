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
  Built: ⌘S saves a query tab to its file; a new query goes to the graph's queries folder
  (`<data dir>/queries/<graph key>/`, the graph's one key `<server url>/<graph id>` made safe,
  `http_~~127.0.0.1_8787~knowledge`; since 2026-10-20, before which the folder was the server's
  name, `<server>~<graph>`, and opening the graph moves it once: renamed when the key has no
  folder, else each file the key's folder lacks, never over one), named after its first declaration
  (`list_company.gq`, then `-2`, …). ⌘⇧S picks any path in the system dialog; ⌘O opens a file
  (or `studio_graph::OpenQueryFile { path }`). A file tab is titled by the file, keyed by its
  path so it opens once, and shows the changed dot while its text differs from the file; its
  draft and path come back after a restart. The left dock's Queries panel lists the folder,
  newest first; New query, Open a file… and Show the folder are icon buttons in its title
  line. The palette's Queries scope lists saved files no tab has open, and the stored queries
  with their aliases, which ↵ opens (spec 002).
- **Stored queries:** the queries a server runs by name for the graph (declared in
  `cluster.yaml`, applied with `omnigraph cluster apply`; `GET /graphs/{id}/queries` lists them
  with typed parameters and never their text). The session reads the catalog with the graph (a
  404 is an empty catalog) and again from the panel's refresh. The Queries panel's Stored
  section lists them, "writes" marking a mutation, each with the CLI config's aliases for it
  indented under it; aliases whose query the graph does not serve fold under Not served. A
  stored query opens in a query tab without the editor: the toolbar names it and what it does,
  the catalog's `@description` and `@instruction` sit under it, and its parameters are fields
  typed by the catalog's kinds (a 64-bit integer is sent as typed text, so values past 2^53
  stay exact; an unknown kind is sent as text). Run invokes it by name at the branch head or the
  picked commit, saying it expects a read, so the server refuses one that writes; a stored
  mutation is confirmed against the branch first, written once and logged like any write, and
  its commit is the branch head. Explain is not offered: a plan needs the text. An alias fills
  the parameters with its values; the tab's values come back after a restart, and a tab
  opened before the catalog arrives takes its parameters when it does.
- **Page layout:** as the table's (spec 004). The toolbar names what the page runs on the
  leading side and holds the commit picker, Explain and Run on the trailing side; parameters
  are a field grid under the editor; the result's facts (rows, time, commit, and the branch
  when it is not the window's) and its actions (Canvas, copy hints) are the tab's part of the
  window footer, not a strip of their own; outcomes and failures are an icon, a title, what
  to do, and their facts.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Compiler in-process | A language server | No process to manage; the server is a deferred engine change |
| Zed's editor behind a spike | Write an editor first | Zed's editor is the best available if its build cost is acceptable |

## What landed (2026-10-12)

- `NewQuery` (⌘N) opens "Query 1", "Query 2", …, starting with a read over the schema's first
  node type (`gq_language::example_read`).
- Highlighting follows the syntax tree: the editor's buffer has the GQ language
  (`query_editor::syntax`), the tree-sitter grammar in `grammars/tree-sitter-gq` with its
  highlight, bracket, indent and outline queries, so brackets match and auto-close, new lines
  indent inside a block and folds end where the block does. Read-only views of a query keep the
  lexical colouring (`gq_language::tokenize`); both use the same theme names. If the grammar's
  queries failed to compile the editor would fall back to the lexical colouring (a test keeps
  them compiling).
- The schema's source has a read-only tab, "Schema" (`studio_graph::OpenSchema`, the Graph
  menu, and a row under Ontology and Explorer in the sidebar; `query_editor::schema_view`). It
  is the `.pg` text the server holds for the graph (`GET /schema`), parsed with
  `grammars/tree-sitter-pg`: coloured with the GQ grammar's theme names, folded by declaration,
  selectable and copyable. It follows the session, so a branch switch or a refresh that brings
  another schema shows it; the same text keeps the scroll position. `OpenSchema` with a type
  selects that type's name in its `node`, `edge` or `interface` line. The footer counts the
  types and relationships and says the view is read-only: a schema changes by deployment
  (`omnigraph cluster apply`), never from Studio (spec 012, E14). The tab comes back after a
  restart.
- After typing pauses (150 ms) the source is checked off the main thread (`analysis.rs`): the
  whole file for problems, each declaration on its own for mutation/read, parameters and result
  columns, so one broken declaration does not hide its siblings. Problems are underlined and
  listed under the editor (code, message, fix, line:column; a click moves the cursor there).
  A type error the compiler reports without a position is placed on the last name its message
  quotes. F8 and ⇧F8 go to the next and previous problem (in the order of the text, round
  again), lighting its row; ⌘. applies the suggested fix of the problem gone to or under the
  cursor, and that problem's Fix button shows the key. A check redraws the problems, the run
  lines and the parameters, never the result grid; an input whose parameter no declaration
  declares any more goes once a check reads every declaration.
- A run line above each declaration (a Zed editor block anchored to the declaration, rebuilt
  when the declarations change): ▶ Run, then that declaration's last outcome ("25 rows · 96
  ms", "Committed · 1 nodes, 0 edges", "Refused", "Outcome unknown · check History"). A line,
  not a gutter glyph: Zed's gutter run indicators belong to its task system, which needs a
  project.
- The toolbar's snapshot picker reads "Head of <branch>" or "Read only · <commit>": the head,
  or one of the branch's 15 latest commits (short id, age, actor), read when the picker opens
  and kept until a commit lands on the branch or the branch changes; then read again only
  while the picker is open, which shows them as they come. A tab that never opens it reads no
  commits. A commit pins reads (`QueryRequest.snapshot`) and refuses writes; the head unpins.
- Hover goes through Zed's `SemanticsProvider` (`query_editor::hover`), the rest of which
  answers that it has nothing, with `gq_language::hover`: `$p: Person · node`, `$name: String
  · parameter`, `Person.born: Date?`, `node Person · 6 properties`, `worksAt: Person →
  Company · edge WorksAt`. Zed's popover needs no project.
- Completion goes through Zed's `CompletionProvider` with `gq_language::completions`. After `$`
  it offers the parameters and `match` bindings of the declaration under the cursor with their
  types; after `$var.` the properties of that declaration's binding of `$var`; after `$param:`
  in a signature the parameter types; elsewhere types, relationships and keywords.
- ⌘↵ runs the declaration under the cursor (else the last one before it, else the first) as the
  settings prefix plus that declaration, so a broken sibling does not stop it. A read goes to
  `POST /query` on the session branch; the result header shows rows, elapsed time, the branch
  and the `graph_commit_id` read at. Before the first run, the expected columns and their
  compiler types are shown, named as the answer will name them (`p.name`, from the compiler's
  `executed_column_name`), so a run leaves the headers as they were. A query's answer is read
  up to 32 MB (`graph_client`'s cap on every query and stored read): a longer one shows
  nothing of itself and says "The answer is over 32 MB, the most Studio reads at once. Add a
  `limit` to the query, or return fewer properties". F6 moves the keyboard to the result grid
  and back; ↑↓←→ move its cell, ⌘C copies the cell and ⌘⇧C the row.
- A mutation never runs on ⌘↵: a bar names the branch ("Run on main", ↵) and Esc cancels. A
  write Studio cannot put back says what goes and waits for its name typed (constitution IV):
  `branch delete x` the branch and its commits no other branch holds (type `x`), `commit drop`
  the proposal (its short id), `load … mode overwrite` every row of the type (the type), and a
  `delete` that publishes what its `where` matches (the type; the query's name for several).
  The write is sent once, never retried, and its outcome is shown as what the server says it
  did, from the commit's `kind` and a statement's `outcome`: committed to the branch (nodes,
  edges, commit) only for a `published` commit; "Held as commit <id>, not on any branch yet"
  for `as commit`, with the `commit publish`/`commit drop` commands; "Saved as snapshot <id>,
  on no branch" for `as unattached branch`; "Created branch x at commit <id>", "Deleted
  branch x", "Merged a into b" (or already up to date), "Published commit <id> to b" and
  "Dropped commit <id>" for statements, which report no counts; "Nothing changed" for no
  commit. Refused ("Nothing was written", with the server's diagnostic) or unknown ("Check
  History before trying again") as before. The text is the draft and is never cleared.
  Closing the tab does not cancel a write in flight; the session logs its outcome.
- Parameters of the query under the cursor get an input each; values are typed by
  `gq_language::param_value` and sent as JSON (`null` for an empty nullable).
- ⌘⇧↵ sends `explain query …` and shows the logical, physical and DataFusion trees indented by
  depth, then the planner's notes. Each operator's details read as named facts (muted name,
  plain value): a filter is its text, an edge selection `Knows out`, a list its items, and
  bookkeeping (ids, table versions, statistics, tie-breaks) is left out; the tooltip has every
  fact in full.
- Hovering an operator lights its part of the query in the editor
  (`gq_language::operator_parts`): a scan or join its variable's binding (or the traversal that
  reaches it) and the filters it applies, an expand the traversal between its two variables, an
  anti-join the `not` block naming its outer variable, a filter its clause, a sort the `order`
  clause, a limit or page the `limit`, a projection or aggregate the `return`. The match is
  lexical (`gq_language::query_parts` reads a declaration's clauses with byte ranges), since
  the compiler's AST has no spans; the declaration is found again by name, so edits since the
  plan move the light with the text. An operator the planner adds on its own lights nothing.
- Statements (`branch …`, `show …`) run without a request target; `branch create/delete/merge`
  asks first like a mutation. A statement read names no branch of the tab's ("Running branch
  diff a into b…"); a `branch diff` shows the two commits its page compared and, while the
  server's `page.next_page_token` says more follow, "Page N · more follow" and Next page
  (⌘⌥N), which reads the same statement with `after page "<token>"`. The plan of a load
  heads its first tree "Initial logical plan" (`logical_initial`).
- To adopt with the next minimum-server bump (spec 012, "On OmniGraph main, 2026-10-20"):
  - The compiler's new checks show as you type: `T27` for a full-text call (`search`, `fuzzy`,
    `match_text`, `bm25`) on a property without `@index`, `T59` for an `rrf()` over two bindings
    one traversal connects, exact `@range` bounds, and `from commit "<id>"` in `branch create`.
    The engine catalogue's `@` types (`@Operator` and the rest) complete and highlight as
    built-in types.
  - A 409 with `full_text_index_required` reads "`Type.property` has no full-text index at this
    snapshot" and names the fix: `omnigraph optimize` on `main`, or
    `omnigraph rebuild-full-text-indexes --branch <branch>` elsewhere.
  - Explain shows a document's `pipelines` (`explain_version` 7) as their own section, each
    with its source, operators and sink, the pipelines it depends on, and its operators lit in
    the physical tree on hover.
  - An overwrite `load csv`'s `executed.operators` shows under the write's outcome: each node's
    rows, compute time and wall time.

## Deferred

- Editing a schema: the schema view is read-only. Schema changes stay with
  `omnigraph cluster apply`; E14 would add a read-only migration plan, not an apply.
- A row cap: the cap is on the answer's bytes (32 MB), not its rows, so the query's own
  `limit` is the way past it (constitution IX). Paging a query's rows needs a cursor the
  server does not offer for queries; only `branch diff` pages. The example query carries
  `limit 50`.
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
| 2026-10-12 | The schema grammar parses OmniGraph's schemas and the Schema tab uses it | `tree-sitter parse --stat` over the 269 schemas at v0.13.0 (the `.pg` files and the `.gqt` corpus's `--- schema` sections): all parse with no error node; `tree-sitter-pg` tests (every construct, recovery after a broken declaration, queries compile, outline names every declaration); `query_editor` `both_languages_build_with_every_query` and `a_type_is_found_at_its_declaration_and_not_in_a_comment_or_a_longer_name`; offscreen snapshot of `OpenSchema {"type_name":"Person"}` on the development graph | The tab reads "Schema", the sidebar row is selected, `Person` is selected in `node Person`; keywords, types, properties, annotations, enum values and numbers are coloured from the tree, declarations fold, and the footer reads "3 types · 4 relationships" and "Read-only" |
| 2026-10-12 | The GQ grammar parses OmniGraph's queries and the editor uses it | `tree-sitter parse --stat` over the 1,390 `--- query`/`--- mutate` steps of the v0.13.0 `.gqt` corpus that expect no error, and the two `.gq` files: all parse with no error node; `tree-sitter-gq` tests (every clause kind, recovery around a broken clause, queries compile, outline names both declarations); `query_editor` `the_language_builds_with_every_query`; offscreen snapshot of a new query tab | Traversals (edge binding, `<a \| b>`, `(a \| b)`, hop bounds), filters, subquery predicates and negations come out as their own nodes; the editor colours keywords, names, types, properties, strings, numbers and edges from the tree and highlights the matching bracket |
| 2026-10-11 | Compiler APIs above exist | `git show v0.13.0:crates/omnigraph-compiler/src/lib.rs` | All exported |
| 2026-10-12 | Zed's editor works with no project: completion provider, text highlights, placeholder | `editor/src/completions.rs` (`CompletionProvider`, `set_completion_provider`, no `project` needed outside snippets), `keymap_editor/src/action_completion_provider.rs` (a project-less provider), `editor/src/display_map.rs` (`HighlightKey`) at `v1.23.2`; offscreen snapshots of completion and underlines | Works. `HighlightKey` is a closed enum; `ConsoleAnsiHighlight(n)` is the one numbered key, used for syntax classes and diagnostics |
| 2026-10-12 | Diagnostics within S4's 100 ms after typing stops on a 500-line file | `analysis_time_on_a_500_line_source` (ignored instrument), debug build, Apple M5 Pro, 48 GB, 504 lines / 63 queries | 12.6 ms per analysis. With the 150 ms debounce the underline lands about 163 ms after the last key, over S4's 100 ms; the debounce, not the check, is the cost (see Corrections) |
| 2026-10-12 | `explain` rows | `POST /query` with `explain query …` against the 0.13 dev server | Columns `tree, depth, node, detail`; trees `logical`, `physical`, `datafusion`, then `plan` rows with a null depth (`pass`, `route`, `logical_hash`, `statistics`, `assumptions`) |
| 2026-10-12 | Statements and mutations on `POST /query` | dev server | A statement with a `branch` target is refused ("a branch statement names its branches itself; drop the request target"); a mutation is refused ("use POST /mutate for write queries"), so a mis-classified write cannot run as a read |
| 2026-10-12 | Compiler result names | `describe_query_operation` in `gq_language::check` | Names a projected property alone (`slug`) where the server says `c.slug`; types are matched to server columns by position |
| 2026-10-12 | `$` completion | `gq_language` completion tests; an offscreen snapshot of `$p.name = $` in a two-parameter query | Offers `name String`, `p Person`, `since I32?`; nothing while a parameter is being named; `$x.` follows the binding of the declaration under the cursor |
| 2026-10-12 | Saved queries | offscreen snapshots: a new query, `key:cmd-s`, the Queries panel; a second run with restore, `OpenQueryFile` on the same path, the palette's Queries scope | `list_company.gq` written to the graph folder; the tab took its name and showed the dot after an edit; restored with its draft; the second open reused the tab |
| 2026-10-12 | A refused read shows the server's diagnostic | `ApiErrorSummary.diagnostic` from `ErrorOutput.diagnostic`; an offscreen snapshot running `return { $p.nope }` | "T6 · type `Person` has no property `nope`" under the refusal, as writes show it; it jumps to the text when the diagnostic has a position |
| 2026-10-12 | Quick fixes | `gq_language` `a_missing_parameter_list_comes_with_its_edit`; offscreen snapshot of `query people {` with the problems list's Fix clicked | Q002 carries its `()` edit; Fix applied it, the problem cleared and the columns showed. At 0.13 this is the one suggestion the compiler makes; any other `Suggestion` gets the same button, applied only while the text is the one checked |
| 2026-10-12 | Hover types | `gq_language` `hovers_name_types`; an offscreen snapshot with the new `hover:` step resting on `$p` | "$p: Person · node" in Zed's hover popover, the token highlighted |
| 2026-10-12 | Snapshot picker | offscreen snapshots: the picker on a new query, the older of main's two commits chosen, Run | The tab and picker named the commit; the read ran "on commit EPD69MJ0" and returned no rows, that commit being before the seed load |
| 2026-10-12 | Run lines | offscreen snapshot of two declarations, Run clicked on the first's line | The first ran and its line read "25 rows · 96 ms". A first version ran the declaration under the old cursor: the editor reports a moved cursor after the click, so the line now sets it before running |
| 2026-10-12 | Plan operators light their clauses | `gq_language::parts` tests over logical, physical and DataFusion details from the 0.13 dev server (the fixture checked by the compiler); offscreen snapshots hovering Expand, AntiJoin and Filter | `$p knows $f`, `not { $f worksAt $c }` and `$p.name != $f.name` lit in turn |
| 2026-10-12 | Run loop end to end | offscreen snapshots against the dev server: example query, run, explain, parameters, confirm bar, committed and refused writes, a parse error underline, completion | As described under What landed |
| 2026-10-14 | Stored queries are listed, opened and run by name | `graph_client` test `stored_queries_are_listed_and_invoked_by_name` (catalog kinds, `expect_mutation` either way, the conditional route, an encoded name); `graph_store` tests `stored_queries_are_read_with_the_graph_and_run_by_name` (a 404 catalog is empty, a write is logged and is the head) and `cli_servers_follow_the_cli_files` (aliases by server and graph); `query_editor` tests of the parameter kinds; offscreen snapshots against a scratch 0.13 cluster serving `people_titled` and `set_title` | The panel listed both with the `founders` alias indented and one alias under Not served; `founders` opened `people_titled` with Founder filled and ⌘↵ read 20 rows; `set_title` on person-000002 asked first, committed, toasted, and a read of the server showed the new title |
| 2026-10-14 | The query page follows the design system | offscreen snapshots of a query tab and a stored query's tab after a run | Toolbar: name and kind leading, picker, Explain and Run trailing; parameters as fields; rows, time and commit in the window footer beside the graph and branch, no second strip |
| 2026-10-14 | Result columns are resized from their header's edge | `query_editor` headless test `a_dragged_result_column_keeps_its_width_by_name`; `studio_ui` test `a_drag_follows_the_pointer_within_bounds`; offscreen snapshots of `people` (slug, name, title) on the dev graph, then `drag:604,446,704,446` on name's edge and `hover:704,446` | Name went 100 px wider and title and its cells moved with it; the edge's line showed on hover. The test: the width held for `name` when the next result put it second, the tab saved `"widths": {"name": 250.0}`, and resetting fitted it to its values again |
| 2026-10-14 | Typing in the query does not draw the results | Main-thread CPU per key from the snapshot tool, `release-fast`, `people` with no result, 3 and 6 columns of 200 rows, before and after in one session; headless `typing_in_the_query_leaves_the_results_as_they_were`, which fails with the per-key `notify` restored | Before: 1.7 ms with no result, 2.3 to 2.7 ms with 3 columns, 3.2 to 3.5 ms with 6, each column about 0.25 ms more. After: 1.5, 1.55 and 1.85 to 2.0 ms. A run, a column drag and a cell click drew as before |
| 2026-10-20 | A write's outcome says what the server says it did | `run_branch_statement` and `run_commit_statement` (`crates/omnigraph-server/src/handlers/dispatch.rs`), `BranchOutcomeOutput` and `CommitOutput.kind` (`crates/omnigraph-api-types/src/lib.rs`) at `edge-20261020-7fd9cc9`: a statement answers `commit: null` (create, delete, drop, an up-to-date merge) with its effect in `outcome`; a write `as commit`/`as unattached branch` answers `kind` `unpublished`/`unattached`. `query_editor` tests `write_effects_say_what_moved` and `a_write_says_what_it_did_from_the_commits_kind_and_the_outcome` (a served `as commit` write) | "Committed to main" only for a `published` commit, as the session's `published()` reads it; a proposal reads "Held as commit …, not on any branch yet", a `branch create` "Created branch feature at commit …" with no counts |
| 2026-10-20 | A query's answer is capped | `graph_client` test `a_query_answer_over_its_cap_is_refused_and_read_no_further` (an endless body: 64-byte cap reads 65 bytes; `query` and `invoke_stored_read` read 32 MiB + 1 and refuse); `query_editor` `an_answer_over_the_cap_says_so_and_shows_nothing`. The size: a scratch decode of 100,000 node rows (13.8 MB of JSON) into `serde_json::Value` with `preserve_order`, counting allocations, Apple M5 Pro, 48 GB, release build | 164 MB live after decoding, 11.9 times the body: a 32 MB answer can hold about 400 MB as values before the table is built, the most one answer should take; a type table's longest page (1,000 rows) reaches 32 MB only with 33 KB rows |
| 2026-10-20 | `branch diff` pages | `branch_diff_read_output` at `edge-20261020-7fd9cc9` (a statement read's `target` is null on both sides; `page` carries `from_commit_id`, `to_commit_id`, `base_commit_id`, `next_page_token`); `graph_client` `a_branch_diff_answer_carries_its_page` and the contract table (`ReadPageOutput`); `query_editor` `a_diff_continues_at_its_next_page` and `a_branch_diff_names_its_sides_and_reads_its_next_page` (the session on `dev`) | Run says "Running branch diff feature into main limit 1…", not "on dev"; the rows are read at the statement, not dev; Next page reads `… after page "t2" limit 1` as page 2 |
| 2026-10-20 | Expected columns are named as the answer names them | `executed_column_name` (`crates/omnigraph-compiler/src/query/typecheck.rs`, the name T25 guards the executed batch by) at `edge-20261020-7fd9cc9`; `gq_language` `a_valid_read_reports_its_columns`; `query_editor` `tables_fill_missing_cells_and_take_types_by_name` | `ResultColumn.column` is `p.name`, `p.born`, `who` for an alias; `name` stays the inferred schema's `name`. Types are matched to the answer's columns by name, so the positional match is gone |
| 2026-10-20 | The snapshot picker reads commits when it opens | `query_editor` `commits_are_read_when_the_picker_opens` (requests to `/commits` counted) | None at open; one when the picker opens, none when it opens again; one more for a commit landing while it is open; none for one landing while it is closed until it opens |
| 2026-10-20 | A check does not draw the results | `typing_in_the_query_leaves_the_results_as_they_were`, now on a session serving a schema so the check after typing runs (its analysis holds the typed text and the `namesake` problem); it fails with a results redraw put back in `apply_analysis` (5 draws against 4) | Typing and its check leave the grid; a notify of the tab leaves it; a moved cell draws it once |
| 2026-10-20 | Keyboard reach | `query_editor` `the_result_grid_is_reached_and_crossed_by_keys` (F6 in and out, ← → by column, ⌘C copies `title` from the middle column) and `problems_are_gone_to_and_fixed_by_keys` (F8 to the only problem, ⌘. adds `()`); the three keymaps checked against Zed's and Studio's bindings for F6, F8, ⇧F8, ⌘./Ctrl-., ⌘⌥N/Ctrl-Alt-N, ← → | No clash where they apply: Zed's F8/⇧F8/⌘. are its project diagnostics and code actions in `Editor`, which `QueryEditor > Editor` overrides; Zed's F6 is `Workspace`; ⌘⌥N is Studio's Reject in `AgentPanel` only |
| 2026-10-20 | A destructive write waits for its name | `gq_language` `writes_say_what_they_destroy`; `query_editor` `a_destructive_write_waits_for_its_name` | `branch delete feature` says "Deletes branch feature, with its commits no other branch holds. Studio cannot put it back. Type feature to confirm."; ↵ with the field empty sends nothing; with `feature` typed it sends once; `branch create` confirms with ↵ |
| 2026-10-20 | Plan names | `engine/explain.rs` (`logical_initial`, `logical`, `physical`, `datafusion`, `plan`), `omnigraph-planner/src/physical.rs` and `engine/operators/hydrate.rs` at `edge-20261020-7fd9cc9`; `query_editor` `plans_become_an_indented_list_under_tree_headers`; `gq_language` `logical_and_physical_operators_find_their_clauses`, `datafusion_operators_find_their_clauses` | `logical_initial` heads "Initial logical plan". `Hydrate` was right: it is DataFusion's `HydrateExec` (the `Exec` is dropped before matching); `HydrateColumns` and `HydrateExec` light the `return`. `HydrateByAddress` is planned only for diffs and merges (`optimizer.rs`, `LogicalNode::Join` and `MergeClassify`), never for a query or a load, so nothing in the editor's plans needs it |
| 2026-10-20 | F32 parameters | `json_value_to_literal_typed` (`omnigraph-compiler/src/query_input.rs`) takes any finite number for F32, but the v2 binding of every declared parameter (`engine/typed_value.rs`, `validate_params` → `typed_scalar`) refuses one infinite as a Float32, and its test `direct_parameter_values_are_checked_even_without_a_consumer` holds an F32 of `f64::MAX` refused | Studio's refusal of `1e39` for F32 is the engine's; the test comment now says which step refuses it |
| 2026-10-20 | Saved queries follow the graph's key, not its server's name | `graph_store` `named_keys` tests `saved_queries_move_to_the_graphs_key_without_writing_over_one` (neither folder, the name's only, both), `a_graph_opening_takes_its_saved_queries_along` (a `GraphSession` made for `dev` at `http://127.0.0.1:8787`), `a_name_resolves_only_to_a_known_server`; `studio_settings` `query_folders_are_safe_names_per_graph`; every caller of the removed `GraphSession::settings_key` (Queries panel, ⌘S, ⌘⇧S, the palette's Queries scope) now on `overrides_key` | With neither folder nothing is made; the name's alone is renamed whole; with both, `a.gq` moved, `b.gq` kept the key's text ("new b") and stayed in the old folder with `notes.txt`. Opening the graph moved `people.gq` to `queries/http_~~127.0.0.1_8787~moved-queries`. Not yet seen in the running app on a renamed CLI target |
| 2026-10-20 | Run reuses the check | `query_editor` `a_run_reuses_the_check_of_the_same_text` | A held check of the same text and schema answers Run and Confirm; text changed since is checked again |


## Corrections

- 2026-10-20 · A write `as commit` or `as unattached branch` showed "Committed to main" with its
  commit, though nothing moved main; `branch create`, `branch delete` and `commit drop`
  showed "Nothing changed · 0 nodes, 0 edges". The outcome now reads the commit's `kind` and
  the statement's `outcome` (`results::WriteEffect`).
- 2026-10-20 · A query's answer was read and decoded whole, however long; it is capped at
  32 MB with an honest exit, and a `branch diff` showed its first page as if it were all of
  it. Statement reads said "Running … on dev" and filed their rows under the tab's branch.
- 2026-10-20 · Every check drew the result grid again: the grid followed every notify of the
  tab. It is drawn now only when what it shows changes (`QueryEditor::redraw_results`), and
  the expected columns when the declaration under the cursor, its check or the branch
  changes. Each tab read 15 commits when it opened and after every commit, picker or not.
- 2026-10-20 · The expected columns took the compiler's projection names (`name`), so the
  header became `p.name` after a run and types were matched by position.
- 2026-10-14 · Every key in the query drew the result grid again, so wide results made typing
  slower. The result area is now a view of its own (`QueryResults`), made with the tab and
  drawn cached; the tab answers `caches_its_parts`; and a moved cursor notifies only when it
  enters another declaration, which is all the result area shows of it (`docs/drawing.md`).

- 2026-10-14 · Result columns can be resized as the table's can: drag a header's right edge
  (48 to 1,200 px), click it twice to fit the column to its values. A dragged width belongs to
  the column's name, so it holds when the query is run again or changed, and it is saved with
  the tab. The handle and the drag are `studio_ui`'s, shared with type tables.

- 2026-10-14 · The query page drew its own status strip over the window's footer, with the
  result's facts as a sentence ("25 rows 67 ms list_company on main at …") that repeated the
  query and the branch; its parameters, confirm bar and messages used raw paddings and colours.
  The page now takes its layout and parts from `studio_ui::ds`, and the facts are the footer's.

- 2026-10-13 · The result's node graph for "Canvas" was built on every render, cloning every
  node and checking each link against every other (`links.contains`). It is built once when
  the result arrives, with a set for the links.

- 2026-10-12 · Design said a mutation's run button "asks once per tab". The editor asks every
  time: each ⌘↵ on a write shows the confirm bar, and only ↵ on the bar or its button sends it.
  Asking once per tab would let the second ⌘↵ write on its own.
- 2026-10-12 · Spike S4's 100 ms budget and the 150 ms debounce disagree. The analysis takes
  12.6 ms on 500 lines (debug build), so a debounce of about 80 ms meets the budget; it stays
  at 150 ms until someone decides which number wins.
- 2026-10-12 · `gq_language::Diagnostic.code` was the code's `Debug` form
  (`QueryCode(QueryCodeSpec { code: "T12", … })`); it is now the code (`T12`), with a test.
