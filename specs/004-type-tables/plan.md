# 004: Type tables

Status: in progress · 2026-10-12. `crates/type_table` ships the Types sidebar, node and edge
tables (keyset pages pinned to one commit, filtered counts, filters, a cell cursor, value-bound
cell edits with all three outcomes, peek, insert and typed-key delete), sorting by a column,
Blob cells and previews, relative dates, relationship columns, saved views, bulk set, and
preview tabs (spec 002). See Deferred for what is left.

## Problem

The everyday surface: one table per node and edge type, fast at any size, filterable and
sortable, with a peek, and editable without fear. Tower's data pages (its specs 066, 070, 073,
084) proved the model on real graphs; Studio rebuilds it natively without Tower's browser limits.

## Principles

II (one snapshot per table), III (write outcomes), IV (branch named, typed-key deletes), V (rows
stay put), IX (paged, virtualized).

## What exists today (checked)

- OmniGraph `v0.13.0` HTTP: `POST /graphs/{id}/query` takes `{query, params, branch | snapshot}`
  and returns `{columns, rows, row_count, graph_commit_id, target}`; rows are JSON objects with
  null keys omitted. `GET /schema` returns `.pg` source. `GET /snapshot?branch=` returns
  `datasets[]` with `entity_kind`, `type_name` and `entity_count`.
- Writes: `POST /mutate` and `POST /mutate/if-graph-commit` (header
  `Omnigraph-If-Graph-Commit`; 412 on mismatch) return `ChangeOutput {affected_nodes,
  affected_edges, commit, …}`. `POST /load/ndjson?branch=&mode=` loads a batch as one commit.
- GQ at `v0.13.0` (`crates/omnigraph-compiler/src/query/query.pest`): `or`, `in`, `is null`,
  `exists { }`, `count { } op n`, metadata `$n.@id`, `order { … }`, `limit n`, and
  `update T set { p: v } where <predicate>`, where the predicate reads the target's
  properties and `@id`.
- `omnigraph-compiler` `v0.13.0` exports `compile_schema_shape` (node, edge and property shapes
  with types and constraints) and `describe_query_operation` (result column names, kinds and
  nullability before running).
- Zed `v1.23.2`: `crates/ui/src/components/data_table.rs` (uniform and variable row heights,
  resizable and redistributable columns, pinned columns) and `crates/tabular_data_preview`
  (`table_view.rs`, `table_data_engine.rs`, `types/coordinates.rs`: selection kept in data
  coordinates so it survives sorting).

## Design

### Layout

- Left dock lists types with counts from `/snapshot` (labelled "at head"). Selecting one opens
  its table as a preview tab; editing pins the tab.
- The table: a pinned key column (`@id`, or the type's label property), then properties in
  schema order, then relationship columns. Column types come from the compiled schema: numbers
  right-aligned with tabular digits, dates relative with exact value on hover, enums as chips,
  lists truncated with "+N", Blob as "Blob · size · Preview", Vector hidden.
- Peek in the right dock (in flow, not modal; focus stays in the table): all properties, the
  relationship tree rooted at the row (spec 005), and Blob previews.

### Reading

- Page query, generated from the schema (sketch for `Person` ordered by key):

  ```
  query page($after: String) {
    match { $n: Person  $n.@id > $after }
    return { $n }
    order { $n.@id asc }
    limit 100
  }
  ```

  Sorting by a property uses keyset pairs: `order { $n.name asc, $n.@id asc }` with
  `$n.name > $v or ($n.name = $v and $n.@id > $id)`. Nulls sort last, through an
  `is null` split: the engine orders nulls first when ascending, so the rows with a value are
  read first (`is not null`, in the sort's order), then the rest (`is null`, by `@id`). A
  header click cycles ascending, descending and key order; the sort is saved with the tab.
- A page holds `studio.type_table_page_size` rows (100 by default; a graph can have its own,
  spec 002), the `limit` above.
- The first page records `graph_commit_id`; later pages pass it as `snapshot` (constitution II).
- The next page is fetched when the viewport passes 80% of loaded rows. No row cap: the grid is
  virtualized.
- Relationship columns: one query per page and relationship, with the page's ids as an `in`
  list parameter; at most two neighbours as chips, those that fit the column whole, then "+N"
  (a chip is
  never cut mid-name; its width is estimated from its label, as a cell is laid out before its
  text is measured). Built as two reads per page and relationship at the page's commit, in
  turn: a grouped count (`return { $n.@id as id, count($m) as n }`), then every neighbour of
  the rows it sizes, in page order, up to 2,000 rows; a row with more than 200 neighbours, or
  past the budget, shows its count alone, so a hub never leaves the rows after it bare. A
  click opens the other type filtered to this row's neighbours through the same relationship.
- Filters read as segmented pills (`ds::filter_pill`): the field with its kind's icon (text,
  number, time, choice, file, or the relationship's direction), the test, the value and a
  remove button, split by hairlines. A filter being written is the same pill outlined in the
  accent, its field and test as menus (the field menu shows each property's icon) and its value
  as a field at the label size.
- Filtered counts: `return { count($n) as n }` with the same filters, pinned to the snapshot.

### Filters and views

- Filter chips per column: equals, not equals, greater or less, contains, in list, is empty, is
  not empty; dates as Today, This week, Last 30 days, or a range, through `now()`.
- Relationship filters: "has any", "has none", "related to …" through `exists { }`, `not { }`,
  and a bound endpoint. Measure "has none" on a large type before shipping it (Tower could not,
  on an older engine).
- A view is a name for a filter, sort and column state; saved per graph in Studio's settings.
  Built for filters and sort (columns are fixed today): `graphs."<server>/<graph>".views` in
  `settings.json`, each with its type, filters (`FilterOp::key` names) and sort. The toolbar's
  Views menu lists the type's views with the shown one checked, saves the table's state under
  a name (the same name replaces), and deletes the shown one; the button reads the view's
  name while the table matches it. Filters or a sort the schema no longer has are left out.

### Editing

- Click a name to peek, click a value to edit; one overlay editor; a cell cursor moved with
  arrows; ↵ edits, Esc cancels, Tab moves to the next cell.
- A cell save is bound to the value the cell showed:

  ```
  update Person set { title: $new } where @id = $id and title = $old
  ```

  (`title is null` when the old value was empty). `affected_nodes = 0` means the value changed
  elsewhere: the cell reopens with "Changed elsewhere to “X” while you were editing", and the
  person's value stays in the editor.
- While a save is out, the cell shows an amber dot; success is silent; the activity log records
  the commit.
- A row that no longer matches the filters after an edit stays in place, dimmed, with "No
  longer matches the filters. It leaves when the list is read again."
- Bulk set and multi-row edits stage into a draft and publish as one commit. Mechanism to
  confirm in spike S1: one mutation with several statements, or `load/ndjson` in `merge` mode;
  check that merge mode leaves unspecified properties untouched before using it.
  Built (bulk set): ⌘-click picks rows, ⇧-click the rows between; with two or more picked, a
  bar sets one property on all of them as one mutation of one `update` per row
  (`builder::update_cells`), each bound to the value its row shows, so one commit. Rows that
  already hold the value are left out. A short `affected_nodes` means some rows changed
  elsewhere: they were left alone, the bar says how many and the table reads the head. Esc or
  Clear drops the selection.
- Insert: a form generated from the node shape (required properties, key, enums, ranges).
- Delete: type the row's key to confirm; uses `/mutate/if-graph-commit` with the pinned commit.
- Every editing surface shows `write · <branch>`. A 403 shows "Your token cannot change
  <branch>" and keeps the draft.

### Outcomes

`ChangeOutput.commit` present: committed. A 4xx with `ErrorOutput`: refused, "Nothing was
written." plus the reason. A lost response, a timeout or a response without the contract header:
unknown, "Check History before trying again; your changes are still here", with a button that
opens the branch's recent commits.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Value-bound cell saves | `if-graph-commit` on every cell | Any unrelated commit would refuse the save (Tower 073 moved away from it) |
| Engine-side filter, sort and count | Filtering loaded rows | Correct at any size; Tower rejected client-side grouping and filtering |
| No row cap | Tower's 1,000-row DOM cap | GPUI virtualizes; the cap existed only because the browser rendered every row |
| Fork Zed's CSV preview renderer and selection model | GPUI Kit's table | One GPUI lineage (constitution XI) |

## Deferred

- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E11 typed result columns: column types come from `describe_query_operation` until then.
  - R1 Blob writes: upload and clear only when the server advertises `cluster_putBlob`.
  - R2 unpublished commits: staged edits as a reviewable proposal only when the server
    advertises `cluster_publishCommit`.
- Group-by (Tower 084) until filters and editing ship.
- Multi-row edits of different values staged as one draft (bulk set covers one value across
  rows). Views do not hold column state: columns are fixed. The preview tab is built (spec 002).
- Saving a Blob value to a file: the peek previews the first bytes only.
- Per-row history beyond the commits that the change feed can cheaply return.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Grammar features used above exist in 0.13.0 | `git show v0.13.0:crates/omnigraph-compiler/src/query/query.pest` | `or`, `in`, null tests, `exists`, `@id` metadata, `update … set { } where` present |
| 2026-10-11 | Compiler describes result columns | `git show v0.13.0:crates/omnigraph-compiler/src/lib.rs` | `describe_query_operation`, `QueryResultFieldDescriptor` exported |
| 2026-10-11 | Zed table sources exist | `git ls-tree -r v1.23.2 crates/ui/src/components crates/tabular_data_preview/src` | `data_table.rs`, `table_view.rs`, `table_data_engine.rs`, `types/coordinates.rs` |
| 2026-10-12 | `affected_nodes = 0` when the bound old value no longer matches | `update Person set { title: $new } where @id = $id and title = $old` with a stale `$old` against `script/dev-server` (0.13.0, port 8801) | `affected_nodes: 0`, `commit: null`. In Studio: the title changed to "CTO" by another client while the editor was open; Enter showed "Changed elsewhere to “CTO” while you were editing" with the draft kept |
| 2026-10-12 | An update that sets the value it already has is still a commit | same update with `$new = $old` | `affected_nodes: 1` and a new commit, so Studio sends nothing when the edited text parses to the shown value |
| 2026-10-12 | Keyset pages stay consistent when pinned to `snapshot` | read the head commit C, insert `person-000150a`, read the page after `person-000149` at `snapshot = C` and at the head | At C: `000150, 000151, 000152`; at the head: `000150, 000150a, 000151` |
| 2026-10-12 | Edge rows can be keyed and paged by `$e.@id` | `match { $a: Person  $a $e:worksAt $b  $e.@id > $after } return { $e.@id … } order { $e.@id asc }` | Works; ids are ULIDs. Without the typed `$a: Person` binding the engine refuses (T5); `count($e)` is refused (T23), `count($e.@id)` works |
| 2026-10-12 | Edges cannot be updated | `update WorksAt set { role: $new } where @id = $id` | Refused at type check: T16 "update mutation for edge type is not supported". Edge tables are read-only |
| 2026-10-12 | List filters take membership, not equality | compiler tests at v0.13.0 (`test_list_equality_match_is_rejected`, `test_contains_filter_accepts_list_membership`); `$n.tags contains $f0` with `$f0: String` on the server | Lists offer `contains` with an element-typed parameter; `contains` on text is a substring test |
| 2026-10-12 | Deleting a node deletes its edges | insert a person with one `Knows` edge, `delete Person where @id = $id` | `affected_nodes: 1, affected_edges: 1`; the delete dialog says so |
| 2026-10-12 | A delete bound to an old commit is refused | `/mutate/if-graph-commit` with a pin older than the head | 412 with `precondition_failure {expected, actual}`; Studio shows "Nothing was written. main moved since this table was read; refresh it, then try again." and offers Refresh |
| 2026-10-12 | Unknown outcome | stopped the 8801 server between the table read and a cell save | The save came back unknown; the cell reopened with "Check History before trying again; your changes are still here"; the head still had the old value |
| 2026-10-12 | "is one of", date presets and relationship filters | `gq_language` `membership_presets_and_relationship_filters_compile` type-checks the page and count queries on 0.13; the same shapes on the dev server | `$n.title in $f0` (55 Founders or Engineers); `exists { $r0: Company  $n worksAt $r0 }` 168 and `not { … }` 32 of 200 Person; "related to" `project-00036` returns its contributors. Presets compute today, this Monday or 30 days back in local time each time the table reads; a `DateTime` gets that local midnight in UTC |
| 2026-10-12 | The unknown notice opens history | code: the cell overlay's unknown notice carries "Open history" (`studio_graph::OpenHistory`) | As specified |
| 2026-10-12 | `compile_schema_shape` keeps declaration order | `omnigraph-compiler` v0.13.0 `catalog/schema_shape.rs` | It sorts properties (and enum values) by name. `SchemaModel` now restores the `.pg` declaration order from the AST; enum values stay sorted |
| 2026-10-12 | Zed's `ui::Table` can carry the grid | `crates/ui/src/components/data_table.rs` and `crates/tabular_data_preview/src/renderer/render_table.rs` at v1.23.2 | Resizable columns + `pin_cols(1)` + `uniform_list` + `disable_base_style`, as the CSV preview uses it |
| 2026-10-12 | Actions reach the workspace with nothing focused | `crates/gpui/src/window.rs` `dispatch_action` / `focus_node_id_in_rendered_frame` | With no focus, an action goes to the root view's node only, which is outside the workspace element that holds its handlers. The workspace now focuses itself when a session opens and nothing else has focus |
| 2026-10-12 | Blob cells and previews | `HEAD`/`GET /graphs/notes/blob` on the 0.13 dev server (`notes.attachment`: a PNG, a query text, 1 KiB of bytes, an empty value, nulls); `graph_client` blob tests; `type_table::blob` tests; offscreen snapshots of the Note table and three peeks | A managed cell answers 200 with `content-length` and an ETag (`0` for empty); a null cell and a missing row both answer 404 `not_found`, so a 404 on a row the table read is null; `Range` answers 206. Cells read "Blob · 690 B · Preview", "Blob · 0 B" and "—", for drawn rows only, at the page's commit; the peek drew the PNG, the query text and a hex dump |
| 2026-10-12 | Null order and sorted keyset pages on 0.13 | `order { $n.title asc, $n.@id asc }` and `desc` on the dev server; `is not null`; the keyset clause with `or`; `gq_language` `sorted_pages_read_values_then_nulls_and_compile` (text, Date, I32, an edge property); `type_table` paging test; offscreen snapshots of Person by title both ways and Company by city with 25-row pages | Nulls come first ascending and last descending, so the `is null` split is needed; Company by city ran Berlin to Toronto, then the company without a city, across the switch from values to nulls ("25 of 25") |
| 2026-10-12 | Relationship columns | `gq_language` `page_relationship_reads_compile_for_every_group`; the grouped count and sample on the dev server (`person-000002` with no `knows` has no row); an offscreen snapshot of Person with its five relationship columns | Counts per id, tags in the UI font with "+N" kept in view, "—" for none |
| 2026-10-12 | Saved views | `studio_settings` `views_are_saved_per_graph_and_type_and_replaced_by_name`; `gq_language` `filter_operators_round_trip_through_their_keys`; offscreen snapshots with `click:` and `type:` steps: a seeded "Founders" applied from the Views menu, then sorted by email and saved as "Founders by email" | "title is Founder", name ascending, "25 of 200", and the button read "Founders"; `settings.json` held both views |
| 2026-10-12 | Spike S1: several rows in one commit | on a scratch branch of the dev server: `update Person set { title: $v } where @id in $ids` (3 rows), and one mutation of two `update` statements | Both publish one commit; multi-statement mutations are the mechanism, so merge-mode NDJSON was not needed |
| 2026-10-12 | Bulk set | `gq_language` `a_bulk_set_is_one_mutation_bound_row_by_row`; offscreen snapshots with `click:…,cmd` and `click:…,shift` on Project at branch `bulk-test`: three rows picked, status set to paused | One commit; `project-00001`, already paused, was left out ("Set status on 2 Project rows"); the rows updated in place and the selection cleared |

## Corrections

- 2026-10-12 · "Dates relative": a `DateTime` reads relative for two weeks either way ("3 h
  ago", "in 2 days"), then as its date; a `Date` reads Today, Yesterday or Tomorrow, else as
  itself (a birthday as "48 years ago" helps no one). The exact value is the cell's tooltip;
  the peek and copy keep exact values. 0.13 returns `DateTime` as `2024-01-01T12:34:56.789`
  in UTC.

- 2026-10-12 · Blob cells: the page query cannot project a Blob, so each drawn cell is read on
  its own with `HEAD` at the page's commit, at most once per row and property until the table
  reads again. The peek reads a managed value's first 64 KiB, or the whole of a PNG, JPEG,
  GIF or WebP up to 8 MiB, and shows an image, the start of UTF-8 text, or a hex dump of the
  first 128 bytes. An external reference shows its URI; Studio never fetches it.

- 2026-10-12 · Edges are read-only in tables: the engine has no edge update (T16). An edge table
  pins its source column, then the target and the edge's properties; a click peeks the edge,
  whose endpoints open in the inspector. Edge insert and delete wait for a later step.
- 2026-10-12 · "Properties in schema order" needed a fix in `gq_language`: the compiled shape
  lists them by name. Enum values still read in the compiler's sorted order.
- 2026-10-12 · A delete is bound to the table's pin, so any commit after the table's read refuses
  it (412). A table's own committed write moves the pin when that write's parent is the pin; a
  write on top of someone else's commit leaves the table behind and shows "Newer data · Refresh".
- 2026-10-12 · Clicking away from a changed cell keeps the text as a draft on the cell (a dot;
  Enter reopens it) instead of discarding or saving it. Tab saves and moves to the next cell.
- 2026-10-12 · The peek reads relationship counts with `builder::degree` per group and expands
  groups of up to 8 neighbours by itself. Moving the cursor with a peek open re-peeks the new
  row after a 120 ms pause.
- 2026-10-12 · Key bindings for the table are registered by `type_table::init`
  (`TypeTable && !editing`: j/k/↑/↓/←/→, ↵ edit, space peek, ⌘C copy, ⌘⌫ delete, ⌘⌥F filter by
  selection; `TypeTable && editing`: ↵ save; `TypeTable`: esc, ⌘R refresh, ⌘N row, ⌘F filter,
  ⌘⌥⌫ clear filters). They move to `assets/keymaps/` when the keymap files take feature
  bindings. New actions: `studio_table::AddFilter`, `FilterBySelection`, `ClearFilters`.
- 2026-10-12 · A mutation that matched nothing used to toast "committed"; the workspace now
  shows no toast for it and the status line says nothing was written (constitution VII).
- 2026-10-12 · The branch-head check is a one-row read at the head every 30 s while the table is
  the active tab, and after any Studio write to the same branch. Offscreen snapshots do not run
  GPUI timers, so the poll and the peek debounce are not visible there.
