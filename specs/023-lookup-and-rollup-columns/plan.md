# 023: Lookup and rollup columns

Status: draft · 2026-11-07

## Problem

A type table shows a type's own properties and, per relationship, the related nodes as chips.
To see a related node's property beside each row ("each person's company city") or a figure
over a relationship ("each person's hours across their projects"), the person writes a query in
the query editor. Its results are a read-only grid, so they cannot be edited, saved as a view
of the table, or kept beside the rows the table already filters and groups. Filtering and
grouping can already reach through a relationship (`out:WorksAt.city`); showing the value
cannot.

This plan adds two kinds of column to node tables: a **lookup** shows a property of the nodes a
relationship reaches, and a **rollup** shows one figure over them (count, sum, average, minimum,
maximum). A computed column (`price * qty`) waits on the engine (E40). Added columns are part of
a type view's state (spec 024, build step 5).

## Principles

- I: the server computes every value shown. A rollup is the server's aggregate, read at the
  table's commit, never a sum of the rows Studio happens to hold.
- II: one commit per table. Lookup and rollup cells are read at the table's pin, like
  relationship cells.
- III: editing a lookup cell writes the related node's property, value-bound, with all three
  outcomes; a rollup is never edited.
- IV: a lookup cell's edit names the node it writes, not the row it shows on.
- V: rows stay put. A save that changes a lookup value does not move the row under a sort or
  grouping by it; the row dims if it no longer fits its filter.
- VI: every added column has a header menu entry to remove it, and a tooltip naming the
  relationship and property it reads.
- IX: reads stay bounded: only rows near the screen, the relationship columns' budgets, one
  read per added column per chunk of rows.
- X: designed against 0.14; sorting by an added column and computed columns are filed (E40,
  E41).

## What exists today (checked)

Studio at `77ca3964`; Omnigraph `v0.14.0` compiler (`Cargo.lock`), and the CLI
`0.14.1-edge+269299b` for linting.

- **Column kinds** are the key, an edge's ends, a declared property, and one relationship
  column per relationship group (`ColumnRole`, `crates/type_table/src/model.rs:31`). Every
  group gets a column: there is no picker and no cap (`node_columns`, `model.rs:112-150`). The
  Columns menu and "Hide column" only toggle `ColumnLayout.hidden` (`table.rs:2755`, `:2788`).
- **Relationship cells read only rows near the screen.** `want_relations` and
  `want_row_relations` record what the grid drew; `load_shown_relations` reads those lines and
  100 rows on each side (`RELATION_READ_CHUNK`), skipping pairs already asked
  (`table.rs:1585`, `:4456`, `:4497`). `relation_sample` gives each row its whole list while a
  budget of 2,000 neighbour rows per read and 200 per row lasts (`model.rs:1081-1087`).
- **Their queries:** `page_degrees` returns `{ $n.@id as id, count($m) as n }` over
  `$n.@id in $ids`; `page_neighbours` returns `id`, `other` and `$m.<label> as label`, ordered by
  `$n.@id, $m.@id` (`crates/gq_language/src/builder.rs:1704`, `:1744`). Both declare
  `Depends::EdgesAt`, which carries their answers across commits that change no such edge
  (`crates/graph_store/src/carry.rs:207-235`).
- **Cache and drawing:** `relations: CellMap<Relation>` by (row id, relationship key), kept
  while a reload keeps its pin (`table.rs:892`, `:3320`); `render_relation_cell` draws "…",
  "?", "—", up to two chips that fit, then "+N" (`table.rs:9830`).
- **Neighbour property keys** already exist: `out:WorksAt.city` parses as
  `GroupKey::NeighbourProperty` (`builder.rs:437`, `:455`, `:484`). A filter on one is an
  `exists { … }` block (`neighbour_property_clause`, `builder.rs:893`), and grouping by one
  counts `$g.city` per value (`neighbour_value_counts`, `builder.rs:1540`). The filter menu nests
  each relationship's neighbour properties (`table.rs:696`).
- **What a view and a tab keep:** `ColumnLayout { hidden, order, widths }` keyed by column key
  (`model.rs:195`); `arranged()` keeps only keys the schema's columns have (`model.rs:241`).
  `SavedView` keeps filters, sort, grouping, hidden and order (`studio_settings.rs:243`); a tab
  keeps those, its filters and widths (`table.rs:1055`, `type_table.rs:218`).
- **Sorting** takes only the row's own single-value properties (`can_sort_by`, `table.rs:3191`;
  `Sort::allowed`, `builder.rs:516`); `keyset` pages by (value, `@id`), with rows holding no
  value read as a second run (`builder.rs:578`).
- **Editing** is only for property columns (`ColumnSpec::editable`, `model.rs:70`). Drafts are
  the session's, keyed by (type, id, property) (`DraftKey`), so a draft of a related node's
  property is the same draft its own table and page show. Writes go through
  `write::write_properties` and announce what they set (`SessionEvent::PropertiesWritten`,
  spec 014).
- **GQ at 0.14** projects a neighbour's property, aggregates per node, and orders by an
  aggregate's alias (`omnigraph lint`, Validation log). It has no arithmetic or string
  functions: `return { $n.hours * 2 as double }` does not parse. A traversal in `match` is
  inner: a node with no neighbour is not in the answer.

## Design

### Part 1: lookup columns

- **Adding one.** The Columns menu gains "Add column ›", with a submenu per relationship
  listing the related type's properties ("worksAt › city"). A relationship column's header menu
  gains "Show a property ›" with the same list. The new column sits right after its
  relationship column, titled `worksAt → city`, its detail the property's type, and its tooltip
  "city of each company worksAt reaches". Its key is the neighbour property key,
  `out:WorksAt.city`, the one its filter and grouping already use.
- **Removing one.** Its header menu has "Remove column" in place of "Hide column": an added
  column is a choice, not part of the schema.
- **What is kept.** `ColumnLayout` gains `added: Vec<String>`, the added columns' keys in the
  order they were added. A tab saves and restores it (`serialize`, `saved_table`); `SavedView`
  gains `added` with `#[serde(default)]`, and `current_view`, `apply_view` and `active_view`
  carry and compare it. `node_columns` builds the added columns before `arranged()` runs, so
  `order` and `widths` apply to them. A key the schema no longer has (the relationship or the
  property went) is dropped when the schema is read, as filters are.
- **Reading.** A new `builder::page_lookups(schema, type, group, property, ids)` returns
  `{ $n.@id as id, $m.@id as other, $m.<property> as value }` ordered by `$n.@id, $m.@id`, with
  the relationship columns' limit. It reads the rows `want_relations` reports drawn for that
  column, in the same chunks and under the same budgets, and declares `Depends::EdgesAt` and the
  related type's rows (`listed`), as `page_neighbours` does for labels. Answers go in a
  `lookups: CellMap<Lookup>` by (row id, column key): each neighbour's id and value, and whether
  the list is whole. A reload that keeps the pin keeps them, as `relations` are kept.
- **Drawing.** One neighbour: the value as a cell of its type draws it (text, number, date,
  enum, list). Several: one chip per neighbour, in the relationship column's order, that fit,
  then "+N", so a lookup's count matches its relationship's. None: "—". A neighbour with no
  value is a "—" chip. The cell is read-only in the "…" and "?" states, like a relationship
  cell.
- **Editing.** A lookup cell whose row has exactly one neighbour edits that neighbour's
  property in place, as a property cell edits: the same cell editor, the same value-bound
  `write_properties`, but to the neighbour's type and id, bound to the value the lookup read.
  Its draft is the neighbour's (`DraftKey { type, id, property }`), so the neighbour's own table
  and page show it too. The hint names the node written: "↵ save to Acme · esc cancel · write ·
  main". With several neighbours the cell is read-only; ↵ opens the related type's table
  filtered to this row, as on the relationship column.
- **Following saves.** `properties_written` today follows a write to the table's own type. It
  extends to a write to another type when everything the table shows of that type can be
  patched: lookup cells holding those nodes take the new values, and relationship chips take a
  new label when the label property was written. A table with a filter or grouping by a
  neighbour property of that type still compares the head as before.
- **Filtering and grouping** by a lookup are the existing neighbour-property filter and
  grouping; the column's header menu offers both, prefilled with its key.
- **Not sortable** in this part (Part 3).

### Part 2: rollup columns

- **Adding one.** The same "Add column ›" submenu lists, under each relationship, "Count" and,
  per property of the related type: "Sum" and "Average" for numbers, "Minimum" and "Maximum"
  for numbers, dates and text. Titles: `worksOn → count`, `worksOn → Σ hours`,
  `worksOn → avg hours`, `worksOn → min startedAt`.
- **Keys.** `count:out:WorksOn`, `sum:out:WorksOn.hours`, `avg:…`, `min:…`, `max:…`, parsed by
  a new `GroupKey::Rollup { agg, direction, edge, property }` so filters can name it.
- **Reading.** `builder::page_rollups(schema, type, group, agg, property, ids)` returns
  `{ $n.@id as id, <agg>($m.<property>) as value }` for the drawn rows' ids, one read per
  column per chunk, with `Depends::EdgesAt` and the related type's rows. A row missing from the
  answer has no neighbour: Count draws 0, the others "—".
- **Drawing.** Right-aligned, formatted as the property's type (an average of integers as a
  decimal; Count and Sum of integers as integers), read-only.
- **Filtering.** "worksOn → Σ hours > 100" builds GQ's correlated aggregate,
  `sum($m.hours) { $n worksOn $m } > 100`; Count builds `count { $n worksOn $m } > n`. The
  filter pill reads as the column's title.
- **Following saves.** A rollup is read again for the rows a write to the related type
  touched, at the write's commit; until it answers, those cells show "…" with the old value
  under it.

### Part 3: sorting by a lookup or rollup (after validation)

- GQ orders by an aggregate's alias (`order { total desc }` lints), but the inner traversal
  drops nodes with no neighbour, and keyset paging needs a bound on the aggregate. The design to
  validate: the first run orders the nodes that have neighbours by (aggregate, `@id`), the next
  page bound by `sum($m.hours) { $n worksOn $m } < $last` or equal with a greater `@id`; the
  second run reads the nodes with none (`not { $n worksOn $m }`) by `@id`, as `keyset` reads
  nulls. A lookup with several neighbours sorts by its minimum ascending and its maximum
  descending.
- Part 3 starts only after the bound compiles and costs no more than a sorted page today (E39);
  E41 would make it one run.

## Decisions

- **One chip per neighbour, not distinct values.** A lookup's "+N" then matches its
  relationship's, and each chip is one node a click opens. Rejected: distinct values, which
  hide how many neighbours share one.
- **Opt-in columns, not hidden ones.** A table already shows a column per relationship (92 on
  the personal graph's Person); adding every neighbour property hidden would make the Columns
  menu unusable. A lookup is added by name and kept in the view.
- **The neighbour property key as the column key.** Filter, grouping and column then name the
  same thing; a filter made from the column's header is the filter that already exists.
- **Edit only with one neighbour.** With several, which node a typed value goes to is a guess.
  Rejected: writing every neighbour, which is bulk set on another type and belongs in that
  type's table.
- **No computed columns in Studio.** Computing `price * qty` on the rows Studio holds cannot be
  filtered or sorted across the table, and would put a second source of values beside the
  server (I). They wait on E40.

## Deferred

- **E40** (spec 012): arithmetic and string functions in GQ expressions, so a computed column
  is the server's projection. *Today:* none.
- **E41** (spec 012): an optional traversal in `match`, so a page can sort by a neighbour's
  value or an aggregate in one run with the nodes that have no neighbour. *Today:* Part 3's two
  runs, after validation.
- Lookups two relationships away (`worksAt → city → country`), and lookups on edge tables
  (an end's property, the `from.title` key grouping already reads).
- A cap on how many relationship columns a wide type shows by default (spec 015, part 4).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-07 | GQ at 0.14 reads a neighbour's property per node, aggregates per node, and orders by an aggregate; it has no arithmetic | `omnigraph lint --schema s.pg --query q.gq` (CLI `0.14.1-edge+269299b`) on a Person–WorksOn–Project schema: a lookup returning `$n.@id, $m.@id, $m.city`; a rollup returning `count($m)`, `sum($m.hours)`, `min($m.city)` with `$n.@id`; the same ordered by `total desc`; and `return { $n.hours * 2 as double }` | The first three: OK. The last: "expected not_expr, kw_and, kw_or, kw_is, kw_in, comp_op" at the `*` |
| 2026-11-07 | The tables' column, read, cache and persistence paths are as described above | A read of `type_table`, `gq_language::builder`, `graph_store::carry` and `studio_settings` at `77ca3964`, citing file and line | As listed under "What exists today" |

## Corrections

None yet.
