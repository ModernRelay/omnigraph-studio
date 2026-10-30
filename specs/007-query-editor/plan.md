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
  `tree-sitter-pg` for schemas). They live here until Omnigraph owns them (deferred).
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
  (`<data dir>/queries/<graph key>/`, the graph's one key `<target>/<graph id>` made safe,
  `personal~knowledge`; since 2026-10-22, before which the folder was the server's address and
  the graph's id, `http_~~127.0.0.1_8787~knowledge`, which Studio no longer reads: its files stay
  there, and ⌘O opens one; a folder from before 2026-10-20, named `<server>~<graph>` after the
  server's name, is the graph's again where that name is its target's), named after its first
  declaration
  (`list_company.gq`, then `-2`, …). ⌘⇧S picks any path in the system dialog; ⌘O opens a file
  (or `studio_graph::OpenQueryFile { path }`). A file tab is titled by the file, keyed by its
  path so it opens once, and shows the changed dot while its text differs from the file; its
  path comes back after a restart with its text and a hash of the file's text it was typed
  over, the text marked as the file's while the tab is saved (since 2026-10-21; a saved text
  over 256 KB is not kept). The file is read off the main thread, the tab saying "Reading
  people.gq…" until its text is in: a saved tab shows the file as it is then; a draft over the
  text the file still holds is kept; a draft over a file changed since is kept too, under a bar
  that says the file changed on disk, and ⌘S writes nothing over it: it moves to the bar, where
  ↵ saves over the file and "Use the file's text" takes it (⌘Z brings the draft back). A file
  that cannot be read (deleted, or gone with a `git switch`) leaves the tab open with the text
  it remembered, saved or not, under a bar that says the file is gone and offers Save As (⌘⇧S);
  ⌘S writes it back where it was. The tab keeps the hash of the text it was typed over while
  the file is gone, so a file that comes back (a `git switch` back) holding the tab's text, or
  that text, is taken as the file's with no bar, whether the editor taking the keyboard or ⌘S
  finds it, and ⌘S writes; one holding other text keeps the tab's under the changed-on-disk bar
  (since 2026-10-21; before, the tab compared its text with no text while the file was gone,
  so the bar showed over a file that came back unchanged, and ⌘S refused). One back empty holds
  other text too, unless the tab's text is empty or was typed over an empty file: ⌘S, which
  expects no file while it is gone, finds one and writes nothing (since 2026-10-22; before, ⌘S
  took the empty file for no file and wrote the tab's text over it, with no bar). ⌘S also finds a
  file that changed while its tab was open,
  and a tab with nothing unsaved takes the file's text when its editor takes the keyboard
  again. The left dock's Queries panel lists the folder,
  newest first, when it is drawn (a hidden panel lists nothing), each file's age drawn again
  each minute it is shown; after a switch to another graph, the last graph's files are not
  shown while this one's are listed (since 2026-10-28; before, they stayed, and opened, until
  the listing landed, and an age stood until something else drew the panel). New query, Open a file… and Show the folder are icon buttons in its title
  line. Its rows take the keyboard as Studio's other lists do: shown from its button or key,
  the panel has it; ↑ and ↓ move among the rows that open a query (saved files, stored
  queries), passing over headers and notes and stopping at either end; ↵ opens the one selected
  as a click does (`studio_queries::SelectNext`, `SelectPrevious`, `OpenSelected`, bound in the
  `QueriesPanel` context). The selection keeps to its query as rows come and go and shows
  while the panel has the keyboard. The palette's Queries scope lists saved files no tab has
  open, and the stored queries, which ↵ opens (spec 002).
- **Stored queries:** the queries a server runs by name for the graph (declared in
  `cluster.yaml`, applied with `omnigraph cluster apply`; `GET /graphs/{id}/queries` lists them
  with typed parameters and never their text). The session reads the catalog with the graph (a
  404 is an empty catalog) and again from the panel's refresh. The Queries panel's Stored
  section lists them, "writes" marking a mutation. A stored query opens in a query tab without
  the editor: the toolbar names it and what it does, the catalog's `@description` and
  `@instruction` sit under it, and its parameters are fields
  typed by the catalog's kinds (a 64-bit integer is sent as typed text, so values past 2^53
  stay exact; an unknown kind is sent as text). Run invokes it by name at the branch head or the
  picked commit, saying it expects a read, so the server refuses one that writes; a stored
  mutation is confirmed against the branch first, written once and logged like any write, and
  its commit is the branch head. Explain is not offered: a plan needs the text. The tab's
  values come back after a restart, and a tab opened before the catalog arrives takes its
  parameters when it does.
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
  node type (`gq_language::example_read`). A tab opened with text, even none (a remembered
  tab, an empty `.gq` file, a link), keeps it when the schema arrives.
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
  columns, so one broken declaration does not hide its siblings. Each declaration is matched to
  the whole check's answer by its place, not its name, and a declaration named like one above
  it is a problem on its name (`duplicate`, since 2026-10-21), checked on its own so its
  problems are underlined in it. Problems are underlined and listed under the editor (code,
  message, fix, line:column; a click moves the cursor there), a row each, four before the
  list scrolls; the list is a view of its own, drawn again only when its problems, the one
  gone to or the one ⌘. fixes change.
  A type error the compiler reports without a position is placed on the last name its message
  quotes. F8 and ⇧F8 go to the next and previous problem (in the order of the text, round
  again), lighting its row; ⌘. applies the suggested fix of the problem gone to or under the
  cursor, and that problem's Fix button shows the key. A check redraws the problems, the run
  lines and the parameters, never the result grid; an input whose parameter no declaration
  declares any more goes once a check reads every declaration.
- A run line above each declaration (a Zed editor block anchored to the declaration, rebuilt
  when the declarations change): ▶ Run, then that declaration's last outcome ("25 rows · 96
  ms", "Committed · 1 nodes, 0 edges", "Refused", "Not sent · nothing written", "Outcome unknown ·
  check History"). A line,
  not a gutter glyph: Zed's gutter run indicators belong to its task system, which needs a
  project. A declaration's range excludes its end, so the line above a declaration runs it
  even when the one before it lacks its `}` and runs on to it.
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
  in a signature the parameter types; elsewhere types, relationships and keywords. Nothing
  inside a comment or a string, and nothing after a `.` that follows no `$var` (since
  2026-10-21): Zed's ↵ confirms a shown completion, so a menu there put a type name in prose.
- ⌘↵ runs the declaration under the cursor (else the last one before it, else the first) as the
  settings prefix plus that declaration, so a broken sibling does not stop it. A read goes to
  `POST /query` on the session branch; the result header shows rows, elapsed time, the branch
  and the `graph_commit_id` read at. Before the first run, the expected columns and their
  compiler types are shown, named as the answer will name them (`p.name`, from the compiler's
  `executed_column_name`), so a run leaves the headers as they were. A query's answer is read
  up to 32 MB (`graph_client`'s cap on every query and stored read): a longer one shows
  nothing of itself and says "The answer is over 32 MB, the most Studio reads at once. Add a
  `limit` to the query, or return fewer properties". F6 moves the keyboard to the result grid
  and back; ↑↓←→ move its cell, ⌘C copies the cell and ⌘⇧C the row. Rows read at the
  session branch's head offer "Newer data · Refresh" in the toolbar, and the tab its stale
  dot, once the branch's head moves past the commit they were read at (constitution II, since
  2026-10-21); Refresh or ⌘R reads the same read again, and nothing is read by itself. A
  write's outcome is never run again. On another branch they offer nothing; back on theirs
  they offer it again, compared with the head the session knows for it then, and a move of
  their branch while away counts.
- A mutation never runs on ⌘↵: a bar names the branch ("Run on main", ↵) and Esc cancels. One
  that moves no branch says so (since 2026-10-21): a declaration `as commit` is held as a
  commit to publish or drop later ("Hold for main"), one `as unattached branch` writes a
  snapshot that never publishes ("Write snapshot"), and a merge `as commit` holds the merge
  ("Hold as commit"). A
  write Studio cannot put back says what goes and waits for its name typed (constitution IV):
  `branch delete x` the branch and its commits no other branch holds (type `x`), `commit drop`
  the proposal (its short id), `load … mode overwrite` every row of the type (the type), and a
  `delete` that publishes what its `where` matches (the type; the query's name for several).
  What a declaration destroys and where it publishes are read from its parse alone, so they
  hold before the schema is read too (`gq_language::check::declared_write`, since 2026-10-21).
  The write is sent once, never retried, and its outcome is shown as what the server says it
  did, from the commit's `kind` and a statement's `outcome`: committed to the branch (nodes,
  edges, commit) only for a `published` commit; "Held as commit <id>, not on any branch yet"
  for `as commit`, with the `commit publish`/`commit drop` commands; "Saved as snapshot <id>,
  on no branch" for `as unattached branch`; "Created branch x at commit <id>", "Deleted
  branch x", "Merged a into b" (or already up to date), "Published commit <id> to b" and
  "Dropped commit <id>" for statements, which report no counts; "Nothing changed" for no
  commit. Refused ("Nothing was written", with the server's diagnostic), not sent ("Not sent:"
  and why, "Nothing was written.", when the connection is retired (its target gone, or a
  transport Studio cannot use), the server refused the connection, or the request could not be
  made; the run reads "Not sent · nothing written") or unknown ("Check History before
  trying again") as before. The text is the draft and is never cleared.
  Closing the tab does not cancel a write in flight; the session logs its outcome.
- Parameters of the query under the cursor get an input each; values are typed by
  `gq_language::param_value` and sent as JSON (`null` for an empty nullable).
- ⌘⇧↵ sends `explain query …` and shows the logical, physical and DataFusion trees indented by
  depth, then the planner's notes. Only a run that explains shows its answer as a plan: a read
  whose columns are named `tree`, `depth`, `node` and `detail` shows them as rows (since
  2026-10-28; before, the column names alone decided). Each operator's details read as named facts (muted name,
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
- To adopt with the next minimum-server bump (spec 012, "On Omnigraph main, 2026-10-20"):
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
  - E17 `omnigraph lsp` and E18 grammars in the Omnigraph repository: the compiler runs
    in-process and the grammars live in `grammars/` until then.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-28 | A suggested edit, a plan and the Queries panel say what is so | `query_editor` `server_positions_map_back_into_the_editor` (extended: a range of the settings prefix that ends at its end), `only_an_explain_is_shown_as_a_plan` (a read with `explain`'s columns, then an explain), `the_rows_are_selected_and_opened_by_keys` (extended: another graph shown before its folder is listed) | All pass. Before: a prefix range ending at its end was mapped to end at the declaration, so its edit replaced the queries between them; a read with those four columns drew as a plan; the last graph's files stayed listed after a switch |
| 2026-10-22 | A gone file back empty is a change | `query_editor` `a_saved_tabs_text_outlives_its_file`, extended: the file back empty, found as the editor takes the keyboard and by ⌘S; failing with a save that expects the empty text while the file is gone | Either way the tab's text stayed under the changed-on-disk bar, ⌘S asked, and the file stayed empty. Undone, ⌘S found the empty file holding what it expected and wrote the tab's text over it, with no bar. A save now expects anything, the text last read or written, or no file (`Expected`) |
| 2026-10-21 | A file that comes back to a tab that outlived it is compared with what the tab knew of it | `query_editor` `a_saved_tabs_text_outlives_its_file`, extended: the file gone at a restart, then back holding the tab's text (found as the editor takes the keyboard, then a draft saved), back holding the text a draft was typed over (found by ⌘S), back holding other text (found either way), and the tab remembered while gone; run again with the old comparison put back, and again with the remembered hash taken from the empty text | Back as the tab knew it: no bar, and ⌘S wrote the draft, by a second write when ⌘S found the file. Back holding other text: the tab's text stayed under the changed-on-disk bar, and ⌘S wrote nothing and asked. Remembered while gone, the tab kept the hash of `query kept() {}`. With the old comparison the bar showed over the unchanged file; with the old hash the tab was remembered as typed over the empty text (`fnv1a64:cbf29ce484222325:0`) |
| 2026-10-21 | A suggestion's Apply needs the text it was made for; newer rows stay newer across a branch switch; a saved tab's text outlives its file | Regression review of `e54cdab`; `query_editor` `a_suggested_edit_is_not_applied_over_text_changed_where_it_goes` (`()` typed where it inserts `()`, and the whole text replaced by the file's, take Apply back), `rows_stay_newer_across_a_branch_switch` (flagged on main, away on dev with no Refresh and no read, flagged again back on main; a move while on dev kept), `a_saved_tabs_text_outlives_its_file` (a saved tab whose file is gone stays with its text, the bar and Save As, and ⌘S writes it back; one whose file reads shows the file; a text over 256 KB is not kept). All three fail with the old behaviour put back | Each held edit is checked against its place with 16 bytes either side, anchored in the text as run: an insertion's place alone holds no text, so text typed there, or the whole text replaced, left it "holding". `BranchChanged` compares the rows with the head instead of clearing their flag, and Refresh reads only on the rows' own branch. Before, `e54cdab` remembered a saved tab without its text and closed it with "could not be read" when its file was gone |
| 2026-10-21 | Nothing is completed in a comment or a string, or after a dot no `$var` precedes | `gq_language` `nothing_is_offered_in_a_comment_or_a_string`, `a_dot_offers_only_a_variables_fields`, `comments_and_strings_hold_what_is_typed_in_them` (`lex::in_comment_or_string`) | `// see the docs.`, `// group by company`, an open `/*`, an open string and an `@description` string offer nothing; past a closed comment or string `Pe` offers `Person` again; `limit 1.` and `Person.` offer nothing, `$p.` the properties. Before, `.` offered every type and ↵ inserted the first. Not yet seen in the running app |
| 2026-10-21 | A restored file tab meets its file as it is now | `query_editor` `a_remembered_draft_meets_its_file_as_it_is_now` (a saved tab is remembered with its text marked as the file's and shows the file; a draft over the unchanged file is kept with `based_on`; a draft over a changed file is kept, the bar shows, ⌘S leaves the file as it was and moves to the bar, Save over it writes the draft; no hash and other text shows the bar; an unreadable file keeps the draft under the file-gone bar; nothing remembered and no file opens nothing), `an_open_file_changed_elsewhere_is_not_written_over` (a clean tab takes the file's text on focus; ⌘S over a file changed since writes nothing and asks) | Before, every file tab saved its text and restore preferred it to the file, so ⌘S wrote an older draft over a newer file. Hash: FNV-1a 64 with the length (`view::content_hash`), stable across builds. Not yet seen in the running app |
| 2026-10-21 | Run above a declaration runs it after an unclosed one | `gq_language` `a_declaration_starts_where_an_unclosed_one_before_it_ends` (fails with the inclusive end put back: `declaration_at(b.start)` was `a`); `query_editor` `the_run_line_runs_the_declaration_below_it` | `Outline::declaration_at` takes the half-open range; the Run line above `b` reads "Running b on main…" |
| 2026-10-21 | Declarations sharing a name are told apart by place | `gq_language` `a_second_declaration_of_a_name_is_a_problem` (the compiler accepts the copy); `query_editor` `declarations_that_share_a_name_are_told_apart_by_place`, `a_write_named_like_a_read_asks_before_it_writes` | A read copied and changed to `delete`, its name kept, is checked as a write and waits for "Person" typed; nothing goes to `/query`; its type error is underlined in it, not its namesake; the copy's name is a `duplicate` problem at 2:7. Before, the delete took the read's check and was sent to `/query` |
| 2026-10-21 | A stored query's values waiting for the catalog are saved | `query_editor` `values_waiting_for_the_catalog_are_saved_with_the_tab` | Values filled before the catalog is read are in the saved `values`; once an input is made, what it holds replaces them. Before, they were saved as `{}` |
| 2026-10-21 | An emptied tab stays empty | `query_editor` `a_tab_given_no_text_stays_empty_when_the_schema_arrives` (a tab given `""` and an empty `.gq` file before the schema is read; a new query beside them) | The emptied tab and the empty file stay empty, the file's tab not changed; the new query gets the example |
| 2026-10-21 | A refused query's diagnostic is read in contract 0.14's shape | `DiagnosticOutput` in `crates/graph_client/openapi/omnigraph-0.14.0-edge.json` (`code` and `expected` required; `position`, `stage`, `expression`, `fix`, `suggestion` with `applicability` and `edits`); `graph_client` keeps `ErrorOutput.diagnostic` untyped, so `results::DiagnosticOutput` reads it; `query_editor` `server_positions_map_back_into_the_editor`, `a_suggested_edit_lands_in_the_editors_text`, `a_refused_reads_suggested_edit_applies_to_the_query` | The code and `expected` show as before; the stage is the button's tooltip; a suggestion reads "Suggested edit: insert `()`" with Apply, which lands where it was meant after an edit above it; an edit of the `explain ` Studio added is not offered. The shape before 0.14 (`message`, `detail`) is no diagnostic of this contract, and the refusal's message stands |
| 2026-10-21 | Typing leaves the problems list | `query_editor` `typing_leaves_the_problems_as_they_were` (a key after the problem and its check that finds the same problem draw the list 0 times; F8 draws it once) | The list is its own cached view (`QueryProblems`, a `uniform_list` of 28 px rows, four before it scrolls); it reads only what the tab holds, never the editor |
| 2026-10-21 | Rows offer Refresh when their branch moves | `query_editor` `rows_offer_refresh_when_their_branch_moves` (`/query` requests counted) | A head move to the commit read at, or on another branch, offers nothing; one past it sets the stale dot and "Newer data · Refresh" and reads nothing; Refresh reads once more; under a write's outcome a head move and Refresh send nothing. The pattern is the storage inspector's and the type table's (`SessionEvent::HeadMoved`) |
| 2026-10-21 | File I/O is off the main thread | Read of `query_editor`: opening and restoring read the file in `background_spawn`, the tab "Reading …" until then and closed with a toast when there is nothing to show; ⌘S finds an unused name and writes in one background task; ⌘⇧S and the Queries panel's Show the folder make the folder in the background first | No `std::fs` call is left on the main thread in the crate outside tests |
| 2026-10-21 | A `delete` waits for its name before the schema is read | `gq_language` `writes_say_what_they_destroy`, extended (`declared_write` of the same purge and of one `as commit`), `a_write_says_where_its_result_goes` (`declared_write(…).publication`); `query_editor` `a_destructive_write_waits_for_its_name`, extended with `query purge() { delete Person where slug = "a" }` on a session whose server serves no schema | Before, `prepare` set `destroys: None` when no schema was loaded, though `deletes` needs only the parse, so the delete was confirmed with ↵ alone. Now it waits for "Person" typed, says "Deletes what its `where` matches of Person from main…", and ↵ with the field empty sends nothing. `declared_publication` became `declared_write`, which says both. Not yet seen in the running app |
| 2026-10-21 | A write `as commit` lands as held, not unknown | Found in the running app against the edge server (`7fd9cc9`): the commit's `graph_manifest_version` was `10415222161995537841`, a detached `__manifest` version past `i64::MAX`; `omnigraph-api-types` holds it as `u64` (spec 012, E30); `graph_client` `a_detached_manifest_version_decodes`; `query_editor` `a_write_says_what_it_did_from_the_commits_kind_and_the_outcome` with that version | Before, `CommitOutput.graph_manifest_version: i64` refused the answer and the write read "The outcome is unknown" though it landed; as `u64` it decodes, and the tab says "Held as commit …". Not yet seen in the running app |
| 2026-10-21 | The confirm bar says where a write goes | `gq_language` `a_write_says_where_its_result_goes` (`QueryInfo::publication`, `statement_publication`, `declared_publication` for text checked against no schema yet); `query_editor` `the_confirm_bar_says_where_a_write_goes`, and the as-commit test above | A declaration `as commit`: "add is held as a commit to publish or drop later; main does not change.", button "Hold for main"; `as unattached branch`: a snapshot that never publishes, "Write snapshot"; a merge `as commit`: "Hold as commit"; any other write as before. Before, the bar said "add_held changes data on main." with "Run on main". A stored query's text is the server's, so its bar says it changes data. Not yet seen in the running app |
| 2026-10-21 | Saved queries under a server's name are not read (decided: no conversion code) | `graph_store::named_keys`, `SessionEvent::SavedQueriesMoved` and the Queries panel's arm for it deleted, with `saved_queries_move_to_the_graphs_key_without_writing_over_one` and `a_graph_opening_takes_its_saved_queries_along` | Supersedes the 2026-10-20 row on saved queries following the graph's key: a `<server>~<graph>` folder stays as it is; its files open with ⌘O |
| 2026-10-21 | A statement's outcome names the branch it writes | `gq_language` `a_statement_names_the_branch_it_writes`; `graph_store` `a_statement_is_recorded_on_the_branch_it_changed` (on fix: a merge into release committed, a merge into main refused, a delete of feature unknown, a commit publish refused) | Logged on release (the answer's), main (the merge's target), feature (the deleted branch) and fix (a commit statement's target is only in the server's answer). Before, the refused merge into main was logged on fix, and its toast's Open history opened fix's |
| 2026-10-12 | The schema grammar parses Omnigraph's schemas and the Schema tab uses it | `tree-sitter parse --stat` over the 269 schemas at v0.13.0 (the `.pg` files and the `.gqt` corpus's `--- schema` sections): all parse with no error node; `tree-sitter-pg` tests (every construct, recovery after a broken declaration, queries compile, outline names every declaration); `query_editor` `both_languages_build_with_every_query` and `a_type_is_found_at_its_declaration_and_not_in_a_comment_or_a_longer_name`; offscreen snapshot of `OpenSchema {"type_name":"Person"}` on the development graph | The tab reads "Schema", the sidebar row is selected, `Person` is selected in `node Person`; keywords, types, properties, annotations, enum values and numbers are coloured from the tree, declarations fold, and the footer reads "3 types · 4 relationships" and "Read-only" |
| 2026-10-12 | The GQ grammar parses Omnigraph's queries and the editor uses it | `tree-sitter parse --stat` over the 1,390 `--- query`/`--- mutate` steps of the v0.13.0 `.gqt` corpus that expect no error, and the two `.gq` files: all parse with no error node; `tree-sitter-gq` tests (every clause kind, recovery around a broken clause, queries compile, outline names both declarations); `query_editor` `the_language_builds_with_every_query`; offscreen snapshot of a new query tab | Traversals (edge binding, `<a \| b>`, `(a \| b)`, hop bounds), filters, subquery predicates and negations come out as their own nodes; the editor colours keywords, names, types, properties, strings, numbers and edges from the tree and highlights the matching bracket |
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
| 2026-10-20 | The Queries panel's rows are reached by keys | `query_editor` test `queries_panel::tests::the_rows_are_selected_and_opened_by_keys` (a saved file and two stored queries served with the contract header: ↓ to the file, ↵, ↓ past the Stored header, ↓↓ stopping at the last, ↑ ↵; a file saved above keeps the selection on its query); `studio` `keymaps` test (each new action bound on macOS, Linux and Windows) | Pass: ↵ dispatched `OpenQueryFile` with the file's path, then `OpenStoredQuery` for `people_titled`; the pattern is the relationship tree's (actions in `studio_actions`, bindings in the keymaps, the panel's focus given by `set_panel_focus`) |
| 2026-10-20 | Run reuses the check | `query_editor` `a_run_reuses_the_check_of_the_same_text` | A held check of the same text and schema answers Run and Confirm; text changed since is checked again |


## Corrections

- 2026-10-22 · "Files" gave the queries folder as the graph's key `<server url>/<graph id>` made
  safe; the key is `<target>/<graph id>` since `1b5a110`, so the folder is `<target>~<graph>`,
  which a folder from before 2026-10-20 matches again where its server's name is the target's.

- 2026-10-22 · The Queries panel no longer lists the CLI's aliases under their stored queries
  or folds the ones a graph does not serve under Not served, and a stored query's tab no longer
  fills its parameters from one. Studio no longer reads the CLI's config file, where aliases
  were declared, and the CLI has no command that lists them, so Studio knows none. A parameter
  input shows a value as the text a field edits it with (`gq_language::input::edit_text`, a
  list as its items separated by commas), which `param_value` reads back; before, it showed a
  list as JSON.
- 2026-10-21 · Fixes to the fixes of the same day: a server suggestion's Apply did not check
  the text where it went, so `()` typed there became `()()`; switching branches cleared the
  rows' "Newer data" for good; and a saved file tab was remembered without its text, so a
  deleted file closed it and lost the text.
- 2026-10-21 · Completion ran inside comments and strings, and after any `.` offered every
  type, so ↵ at the end of `// see the docs.` inserted one.
- 2026-10-21 · A file tab always saved its text, and a restored one showed it over the file,
  so ⌘S wrote an older draft over a newer file. It keeps its draft only while unsaved, with
  the hash of what it was typed over, and asks before writing over a file changed since.
- 2026-10-21 · Run on the line above a declaration ran the one before it when that one lacked
  its `}`: `declaration_at` took both ends of a range.
- 2026-10-21 · Declarations were matched to the check by name, so a copy changed to a `delete`
  was classed as its namesake read and sent to `/query`; duplicate names were no problem.
- 2026-10-21 · A stored query's tab saved `{}` for values that came before the catalog.
- 2026-10-21 · A tab restored empty before the schema loaded filled with the example, and an
  empty `.gq` file's tab showed the changed dot.
- 2026-10-21 · The server's diagnostic was read as `message`/`detail`, which contract 0.14
  does not send; its `stage` and `suggestion` were not shown.
- 2026-10-21 · Files were read, folders made and names probed on the main thread.
- 2026-10-21 · The problems list was laid out again at every frame of the tab.
- 2026-10-21 · Query results never offered Refresh when their branch moved (constitution II).

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
