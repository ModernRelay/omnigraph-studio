# 024: Views

Status: in progress · step 1 built 2026-11-07

## Problem

A saved view today is a name for one type table's filters, sort, grouping and columns. It lives
in a menu on that table's toolbar, so it is found only by opening the table first; it cannot be
seen as a tree or a canvas, renamed, or built from a query. A question the person asks again
("active customers in Berlin", "people with more than 40 hours this month") has nowhere to live
but a tab they keep open or a `.gq` file in the Queries panel, whose results are a read-only
grid.

This plan makes a view a thing of its own: named, listed in the sidebar, built from a type or a
query, and opened as a table, a tree or a canvas.

## Principles

- I: the server answers every value a view shows; a view keeps only how to ask.
- II: a view's tab reads one commit, like any table or tree; switching its shape keeps the read
  context.
- III: edits in a view are the same value-bound writes as in a table, with all three outcomes.
- V: a view never refreshes under the person; "Newer data · Refresh" as everywhere.
- VI: every view action is in the sidebar's context menu, the palette and the keymap.
- IX: a view reads in pages, and a canvas of a view draws at most `canvas_max_nodes` nodes.
- X: shared views and the provenance of a stored query's columns wait on the engine (E42, E43).

## What exists today (checked)

Studio at `77ca3964`, Omnigraph `v0.14.0`.

- **Saved views:** `SavedView { name, type, filters, sort, group_by, hidden, order }`, kept per
  graph in `settings.json` under `graphs."<target>/<graph>".views`, upserted by (type, name)
  (`crates/studio_settings/src/studio_settings.rs:243`, `:690-746`). The only UI is the table
  toolbar's Views menu: apply, "Save view…", delete the one shown; no rename
  (`crates/type_table/src/table.rs:1740-1840`, `:3074-3175`).
- **The sidebar's Data panel** lists Schema, then node types and edge types under flat section
  labels; one click previews, a double click keeps (`crates/type_table/src/types_list.rs:20`,
  `:95`, `:226-245`). Spec 020 rules that the left lists things, never views, and the test
  `every_row_is_a_thing` holds it (`specs/020-ui-unification/plan.md:160-166`,
  `types_list.rs:256`).
- **Queries:** the Queries panel lists local `.gq` files and the server's stored queries
  (`crates/query_editor/src/queries_panel.rs`). A stored query is known by its catalog entry:
  name, description and typed parameters, never its text (`GET /graphs/{id}/queries`,
  `crates/graph_client/src/api.rs:171-200`). Results show in the editor's own read-only grid
  (`crates/query_editor/src/results.rs:18`); the status bar offers Tree (`ShowSetInTree` with
  listed nodes) and Graph (`ShowResultsOnCanvas`) (`crates/query_editor/src/view.rs:242`,
  `:3464-3493`).
- **Sets:** `ShowSetInTree` carries a type with filters, or listed nodes, and a commit
  (`crates/studio_actions/src/studio_actions.rs:346`). A table hands a tree its selected rows, or
  its type and filters, but not its sort (`table.rs:7212-7257`). A canvas walks from one node,
  or draws a query's nodes and links (`GraphCanvas::with_results`, which is not restored with
  the workspace, `crates/graph_canvas/src/graph_canvas.rs:552`, `:727`).
- **The view strip** switches a tab's thing between Table, Tree and Canvas (`ViewKind`,
  `ViewLink`, `Item::views`); `Thing` is Schema, Type, Node or Branch
  (`studio_actions.rs:13-95`).
- **Result columns:** `gq_language::check` gives each result column's name, kind and
  nullability, and the node type of a whole-node column (`ResultColumn`,
  `crates/gq_language/src/check.rs:63`). The compiler's AST gives each projection's expression
  (`Projection { expr, alias }`, `Expr::PropAccess { variable, property }`) and a return's
  `distinct` (`omnigraph-compiler` `src/query/ast.rs:467`, `:664`, `:1130`), and each variable's
  binding gives its type.
- **Sharing:** nothing per graph is shared between people today; the only server registry Studio
  can read is stored queries, declared in `cluster.yaml` and read-only to Studio.

## Design

### What a view is

```
View {
  id: String,                 // a UUID; renaming keeps it
  name: String,               // unique per graph
  source: Type {
            type_name, filters, sort, group_by,
            columns: { hidden, order, added },   // `added`: lookups and rollups (spec 023)
          }
        | Query {
            query: File { path } | Stored { name },
            params: Map<String, Value>,
          },
  shape: Table | Tree | Canvas, // the one it opens in: the last one shown
}
```

- Kept per graph in `settings.json`, `graphs."<target>/<graph>".views`, as today: personal, on
  this Mac. Existing `SavedView` entries are read as Type views: shape Table, and an id made
  from their type and name (`Person/Founders`, unique per graph) until they are next changed,
  when it is written. Reading the file never writes it.
- A view names a graph, not a branch: it opens on the window's branch. A filter, sort or column
  the branch's schema lacks is left out with the toolbar notice tables give today.

### The sidebar

```
Data
  Schema
VIEWS                          +
  ▦ Active customers
  ⑂ People by project
  ◇ Big projects        (query)
NODE TYPES
  Person     230k
  …
```

- **Views** is the Data panel's first section, before node types, listed by name. The icon is
  the shape it opens in; a query view says "query" in its tooltip. The section is hidden with no
  views.
- One click previews, a double click keeps, as types do. A view's row is lit while the active tab
  shows it, in any shape.
- **+** offers "New view from this table" (when a table or tree of a type is active) and "New
  query view…" (a `.gq` file or a stored query that only reads).
- Context menu: Open as Table, Open as Tree, Open as Canvas; Rename (inline); Duplicate; Delete,
  with Undo in its toast (a view is a setting, so Undo puts it back).
- ⌘K lists views under **Views**, in the All and Types scopes, each with its type ("view ·
  Person"); running one opens it.
- Spec 020's rule is amended: a sidebar row answers "what?", and a view does ("active
  customers"); its shapes (Table, Tree, Canvas) still have no row.

### A view's tab

- Key `view:<id>@<commit or head>`, title the view's name, the view strip the shapes it can take.
  Switching shape keeps the tab's read context and sets `shape`.
- **Changed since saved.** Changing a view's filters, sort, grouping, columns or parameters in its
  tab marks it: a dot on the tab and "Save · Save as new view… · Revert" in the toolbar (⌘⌥S,
  ⌘⇧S, ⌘⌥Z), and closing the tab asks first. Nothing is saved by itself. Save as new view makes
  the tab the new view; the one it was stays as saved. A view renamed elsewhere renames its tab;
  one deleted leaves the tab as its type's table.
- The table toolbar's Views menu stays, listing the views of that type, and "Save view…" makes a
  view that appears in the sidebar.

### Type views

- **Table:** a type table with the view's state, editable as any table.
- **Tree:** a set tree of the view's rows (type and filters), paged in the view's sort.
  `ShowSetInTree` gains `sort`.
- **Canvas:** the view's first `canvas_max_nodes` rows, in its sort, and the relationships among
  them, drawn as a results canvas does; a view with more rows says "Showing 200 of 1,284 ·
  Narrow the view". Positions are kept per view (`canvas_positions` under `view:<id>`). The
  canvas is restored with the workspace, from the view.

### Query views

- **Reading.** The view's query runs at the tab's commit with the view's parameters; the
  parameter bar shows above the shape, as in the editor. A query view has no Newer data until it
  is run again: Refresh runs it at the head.
- **Table:** a results table: the editor's grid in a tab of its own, with the type table's cell
  cursor, copy, and editing as below.
- **Tree:** the set of nodes its rows return, as the editor's Tree does today.
- **Canvas:** its nodes and the links among them, as the editor's Graph does today; offered only
  when its rows link nodes, and otherwise disabled with the reason.

### Editing a query view

A result cell is editable when it shows one property of one node or edge of its row:

- **Where a column comes from.** For a query whose text Studio holds (a `.gq` file), the
  compiler's AST says what each projection is. A column whose expression is
  `Expr::PropAccess { variable, property }` comes from `property` of the type `variable` is
  bound to. An alias, a whole-node column (`return { $p }`, each of its properties) and an
  edge variable's property are traced the same way.
- **Which node a row is.** The row needs that variable's `@id`. When the query does not return
  it, Studio adds `$<variable>.@id as __studio_id_<variable>` to the `return` of the query it
  sends, and hides the column. The person's file is not changed. A query with `distinct` is not
  rewritten (an added id would change which rows it keeps), so its cells stay read-only.
- **Never editable:** aggregates, search scores, `now()`, literals, a `distinct` query's cells,
  and a cell whose id is null (an absent node). Their header says "Read-only: an aggregate" (or
  the reason) in its tooltip.
- **The write** is the table's: `write_properties` on that type and id, bound to the value the
  cell shows, with the same drafts, hints ("↵ save to Acme · esc cancel · write · main") and
  outcomes. A node shown in several rows (a join) shows the saved value in every one, through
  `SessionEvent::PropertiesWritten` (spec 014); the tab follows the write's commit when every
  node written is one it shows, and otherwise offers Newer data.
- **Stored queries** stay read-only: their text never reaches Studio, so their columns' sources
  are unknown (E43). Their header tooltip says so. Studio does not match a stored query to a
  local `.gq` file of the same name to learn them: the two can differ, and a cell would write
  the wrong property.

### Views on stored queries

A stored query is the graph's shared definition of a question; a view is one person's way of
looking at its answer. A stored query is therefore a view's source, never a second kind of row
in the Views section.

- **What the view keeps:** the stored query's name, the parameter values, the shape, and how
  its columns show (hidden, order). The parameters are its filters: Studio adds no filter or
  sort of its own, since the server could not answer it and a page sorted on this Mac breaks
  principle I.
- **Where each lives.** Stored queries stay in the Queries panel under Stored, and are not
  listed as views by themselves: many need parameters before they run, and some are written
  for agents, not people. A stored query's row has "Save as view…" in its context menu, and
  ⌘⇧S in its results tab does the same; both make a view with the parameters as set.
- **Only reads.** A stored query that changes data (its catalog entry says so) cannot be a
  view's source; its "Save as view…" is disabled with that reason.
- **Kept by name.** When the catalog no longer has the name, the view's row is dimmed with
  "Stored query <name> is gone" in its tooltip, as a view of a deleted type is, and opening it
  says so in a toast. When the parameters change, values of parameters that are gone are left
  out, and a new parameter the query needs opens the parameter bar on that field rather than
  running and failing.
- **Sharing goes through stored queries.** Until E42, a type view offers "Copy as stored
  query": the `.gq` text that asks what the view asks (its type, filters and sort) and the
  `cluster.yaml` entry for it, on the clipboard. Someone commits them; every Studio then lists
  the stored query, and anyone can make a view of it. Studio never writes `cluster.yaml`.

### Build order

1. Views in the sidebar: the `View` record and the migration, the Views section, open and
   preview, rename, duplicate, delete, ⌘K, a view's tab with Save, Save as and Revert. Type
   views as tables.
2. Type views as trees (with sort) and canvases (bounded, positions kept).
3. Query views: tables, trees and canvases from a `.gq` file or a stored query, with
   parameters; "Save as view…" on stored queries, and "Copy as stored query" on type views.
4. Editing a query view's cells.
5. Lookup and rollup columns in type views (spec 023).

## Decisions

- **"View" for the saved thing; Table, Tree and Canvas are its shapes.** The person named it a
  view, and the toolbar's Views menu already uses the word. Rejected: "collection" or "lens",
  new words for something people already call a view. Code keeps `ViewKind` for shapes; the
  record is `View`.
- **Personal, in settings, for now** (Ragnor, 2026-11-07). The server has no place for them, and
  storing views as nodes would add a type to every schema. A view built on a stored query is
  already shared through it. Shared views wait on E42.
- **An id, not (type, name).** Renaming keeps open tabs and kept positions, and a query view has
  no type.
- **The last shape shown is the one it opens in.** Rejected: always Table, which makes a view
  meant as a canvas two clicks away.
- **Studio adds hidden ids rather than asking for them.** A query written to read is editable
  without being rewritten by hand. Rejected: requiring `@id` in every return, which most queries
  written to read do not have.
- **No edits through `distinct` or aggregates.** A cell there is not one node's value.
- **A stored query is a view's source, not a view** (Ragnor, 2026-11-08). Stored queries are
  shared and declared by whoever runs the graph; views are personal and cheap. Listing every
  stored query in Views would mix the two and show rows that cannot run without parameters.
  Rejected: stored queries as read-only views in the Views section. Rejected: filters and sort
  over a stored query's rows, which the server cannot answer.
- **Shared views go through stored queries until E42**, and E42 is asked as views that name a
  stored query or a type, never their own query text: one registry of questions per graph, not
  two.

## Deferred

- **E42** (spec 012): a writable view registry on the server, so a view can be shared with
  everyone who opens the graph. A shared view names a stored query or a type, never its own query
  text. *Today:* views are personal; a stored query is the shared form ("Copy as stored
  query").
- **E43** (spec 012): a stored query's result columns with where each comes from (the variable,
  its type and the property), in its catalog entry. *Today:* stored-query views are read-only.
- Following views (a canvas that tracks a table's filter, spec 020), folders of views, and a
  count beside each view in the sidebar.
- Exporting and importing views as a file.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-07 | Saved views, the Data panel, Queries, sets, the view strip and result columns are as described above | A read of `studio_settings`, `type_table`, `query_editor`, `relationship_tree`, `graph_canvas`, `studio_actions` and `gq_language` at `77ca3964`, citing file and line | As listed under "What exists today" |
| 2026-11-07 | Step 1: a view has an id and a shape, views list first in the Data panel, a view opens in its own tab keyed by id, and its tab saves, saves as new and reverts | `studio_settings` `a_view_keeps_its_id_through_rename_duplicate_and_undo`; `type_table` `a_views_tab_saves_and_reverts_into_its_view`, `saved_views_list_before_the_types`; `command_palette` `saved_views_are_listed_with_the_types`; `studio` keymap tests | Pass. The context menu offers Open as Table until step 2 adds Tree and Canvas. Rename, Duplicate and Delete name a view by id, so they are in its context menu, not the keymap. Not yet checked in the running app |
| 2026-11-08 | A stored query's catalog entry says whether it changes data | `graph_client` `api.rs:164-200`: `StoredQuery.mutation` and `params` (`:183-187`) | Present; a view's source is limited to reads by it |
| 2026-11-07 | The compiler's AST names each projection's variable and property | `omnigraph-compiler` at `v0.14.0` (`60bfd19`), `src/query/ast.rs`: `Projection { expr, alias }`, `Expr::PropAccess { variable, property }`, the return's `distinct` | Present; a query's result descriptor (`descriptor.rs:21`) gives names and kinds but no sources |

## Corrections

None yet.
