# 004: Type tables

Status: draft · 2026-10-11

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
  `is null` split.
- The first page records `graph_commit_id`; later pages pass it as `snapshot` (constitution II).
- The next page is fetched when the viewport passes 80% of loaded rows. No row cap: the grid is
  virtualized.
- Relationship columns: one query per page and relationship, with the page's ids as an `in`
  list parameter; the first three neighbours as chips, then "+N".
- Filtered counts: `return { count($n) as n }` with the same filters, pinned to the snapshot.

### Filters and views

- Filter chips per column: equals, not equals, greater or less, contains, in list, is empty, is
  not empty; dates as Today, This week, Last 30 days, or a range, through `now()`.
- Relationship filters: "has any", "has none", "related to …" through `exists { }`, `not { }`,
  and a bound endpoint. Measure "has none" on a large type before shipping it (Tower could not,
  on an older engine).
- A view is a name for a filter, sort and column state; saved per graph in Studio's settings.

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

- Typed result columns from the server (deferred engine change); the compiler describes them.
- Group-by (Tower 084) until filters and editing ship.
- Per-row history beyond the commits that the change feed can cheaply return.
- Blob writes, available only on servers that advertise `cluster_putBlob`.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Grammar features used above exist in 0.13.0 | `git show v0.13.0:crates/omnigraph-compiler/src/query/query.pest` | `or`, `in`, null tests, `exists`, `@id` metadata, `update … set { } where` present |
| 2026-10-11 | Compiler describes result columns | `git show v0.13.0:crates/omnigraph-compiler/src/lib.rs` | `describe_query_operation`, `QueryResultFieldDescriptor` exported |
| 2026-10-11 | Zed table sources exist | `git ls-tree -r v1.23.2 crates/ui/src/components crates/tabular_data_preview/src` | `data_table.rs`, `table_view.rs`, `table_data_engine.rs`, `types/coordinates.rs` |
| pending | `affected_nodes = 0` when the bound old value no longer matches | run against a 0.13 server | |
| pending | Keyset pages stay consistent when pinned to `snapshot` | run against a 0.13 server | |
| pending | Merge-mode NDJSON leaves unspecified properties untouched | run against a 0.13 server | |

## Corrections

None yet.
