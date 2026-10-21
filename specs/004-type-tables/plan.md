# 004: Type tables

Status: in progress · 2026-10-12. `crates/type_table` ships the Types sidebar, node and edge
tables (keyset pages pinned to one commit, filtered counts, filters, a cell cursor, value-bound
cell edits with all three outcomes, peek, insert, and delete with Undo), sorting by a column,
Blob cells and previews, relative dates, relationship columns, saved views, bulk set,
preview tabs (spec 002), and edge insert and delete. See Deferred for what is left.

## Problem

The everyday surface: one table per node and edge type, fast at any size, filterable and
sortable, with a peek, and editable without fear. An earlier web client proved the model on
real graphs; Studio rebuilds it natively, without a browser's limits.

## Principles

II (one snapshot per table), III (write outcomes), IV (branch named; a delete lists what it removes and offers Undo), V (rows
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
  relationship tree rooted at the row (spec 005), and Blob previews. A stored Blob value has
  "Save…": the person picks a file (named `<id>-<property>` with the extension its first bytes
  announce), the value is read in ranges at the peek's commit into `<file>.part` beside it, off
  the main thread, and renamed into place only when whole; the line shows how far it got, then
  "Show in Finder". The peek reads at one commit; once the session sees its branch's head
  elsewhere (spec 003, Studio's commit or another writer's), a node's peek shows "Newer data ·
  Refresh" beside that commit, and Refresh reads the node, its relationships and its Blobs again
  at the head. A peek from a table opened at a commit holds that commit and offers no Refresh.
  A peek opened while one of the row's cell saves is out shows what the row holds at the
  table's commit, that property with the save's dot and not editable there, and follows the
  save as it follows its own writes: landed on its commit, the peek moves to the save's commit
  with the value; refused, it keeps the value it shows (since 2026-10-21; before, it showed the
  value sent as stored, kept it after a refusal, and bound an edit to it). Every write of the
  table's own tells its peek, which follows it so (since 2026-10-21; before, only a cell's save
  and a drafts' save did, and the rest left the peek offering Refresh): a bulk set, a row
  inserted, a Blob stored or cleared, whose value the peek's slot then shows. One that changed
  another row the peek shows among a group's neighbours leaves the peek behind, since it would
  show that neighbour by its old name. A peek opened from the table shows its Blob values as the
  cells show them, where they hold (`TableBlobs`: past the pin after the table's own Blob write
  over another writer's commit), binds its Blob writes there, and a tab it opens for a value
  reads there; where they hold at no known commit, the tab reads the branch head. A peek opened from the table reads no
  relationship the table already read at the same commit: the table's relationship columns
  read every row's count and, for rows of up to 200 neighbours, the whole list (keeping the
  first eight, the cell showing two), so the peek takes the count and its first neighbours from
  there and reads only what a hidden column left unread.
- "Open" shows a stored value in a tab of its own (`studio_graph::OpenBlob`), read whole at the
  peek's commit (up to 64 MB; a larger value says so and offers Save and the default app). A
  peek held at a commit opens it held there, read-only (`OpenBlob.held`); a peek at the branch
  head opens one that writes on its branch, bound to the value read at that commit (since
  2026-10-21; before, every tab opened from a peek was read-only). The value is shown for what
  its bytes are, as Zed shows an image: an image fitted to the tab (never past its
  own size) on a checkerboard with a hairline, ⌘-scroll or `=`/`-` zooming about the pointer,
  `1` its actual size, `0` fit again, a drag to pan; an SVG fitted; a PDF's pages drawn by the
  system (CoreGraphics on macOS, twice the width shown, the first 20 pages: a page's pixels are
  about 13 MB, held while the tab is open) and stacked to scroll; UTF-8 text, its first 4 MiB, in
  a read-only editor (⌘F searches it); anything else as a hex dump of its
  first 64 KB, sixteen bytes a line. The footer gives the kind, the pixel size or page count,
  the size and the commit; zoom (−, the percentage, +, Fit) for an image or a PDF; Save…; and
  "Open in default app", which writes the value to an owner-only folder in Studio's data
  directory, emptied of anything a day old, and hands it to the system. The tab comes back after
  a restart with the same value at the same commit. Elsewhere than macOS, a PDF opens in the
  system's viewer.
- A Blob value takes a file (R1, `PUT`/`DELETE /graphs/{id}/blob`): "Choose File…" opens the
  system's picker, and a file dropped from the file manager does the same, on the Blob tab
  (its status bar, the whole tab as the drop target, and "Choose File…" in an empty value's
  state), on a peek's slot (an open-folder icon and a ×, the slot the drop target) and on a
  table's Blob cell (a drop, and the keys). "Clear" sets a nullable value to null. Both are
  actions, `studio_blob::ChooseFile` (⌘⌥U; Ctrl-Alt-U elsewhere) and `studio_blob::ClearValue`
  (⌃⌘⌫; Ctrl-Alt-Shift-Backspace elsewhere), on the Blob tab while its value can take them and
  on the table's Blob cell under the cursor, whose status bar then lists both; the buttons'
  tooltips name the keys. The file is read off the main thread and refused over 32 MiB before anything
  is sent. The write goes to the branch the view shows, once, bound to the value shown as read
  at the view's commit: a managed value by its ETag, which names the type's table version, so
  it is read again at the view's commit when the view moved since (its own writes move it). A
  view behind its branch reads it at its own last commit when that commit's write was itself
  bound to an ETag read where the values shown hold, since the server applied it only while
  the type's table was as the view showed it, and else is bound to the ETag the cell shows
  (`model::BlobsAt`; since 2026-10-21, before which a second file stored behind was refused as
  changed elsewhere); a
  null cell and an external reference have none, so the cell is read at the head first and the
  write goes only while it still holds what is shown (`*` for a reference, no `If-Match` for
  null). While out the value shows the amber dot and "saving to <branch>…"; committed, the view
  moves to its commit as a cell edit does and shows the new value; refused, the server's reason;
  changed elsewhere (a 412, or found before sending), what it holds now and "Replace anyway",
  the change again bound to that (a table cell: drop it again); unknown, check History. Nothing
  is retried. A view held at a commit, an edge's value and a property that is not a Blob are
  disabled with the reason, and Clear also for a property that must hold a value. The
  outcome log names the file and its size, never its bytes. An edge's value is changed with
  `omnigraph blob put edge …`, as the disabled control says: Studio edits no edge's values.

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
- Once the table's own write moved its pin, a later page leaves out the rows the table shows
  already (grouped by a relationship or a neighbour, those its section shows): read at the new
  commit, a row it edited can sort again further on, or into another section, and stays where
  it was read (constitution V). A section's count, read before the write, is spent only on the
  rows it shows, so none is left unread (since 2026-10-21; before, such a row showed twice and
  could take the place of a row of its new section).
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
  not empty; dates as Today, This week, Last 30 days, or a range, through `now()`. A preset
  holds no value of its own: a row edited, inserted or deleted is judged here against the days
  it stands for now, as the engine is asked (since 2026-10-21; before, every row was dimmed
  after an edit and counted as not matching).
- Relationship filters: "has any", "has none", "related to …" through `exists { }`, `not { }`,
  and a bound endpoint. Measure "has none" on a large type before shipping it (it was too
  slow on an older engine).
- The value field offers what it can take: an enum's values (or `true` and `false`), narrowed
  as text is typed with matches at the start first, and for "related to" the other type's
  nodes, searched on the server by id or label (`builder::node_search`, 8 at a time, at the
  table's commit, once typing pauses for 120 ms). ↑/↓ move through the offers (context
  `FilterValue > Editor`), ↵ or a click picks one; a single value adds the filter at once, an
  `in` list adds the value and waits for the next. With nothing typed ↵ adds the filter as it
  stands. A "related to" pill reads the picked node's label. The offers hide while the
  property or operator menu is open.
- A view is a name for a filter, sort, grouping and column state; saved per graph in Studio's
  settings: `graphs."<server url>/<graph>".views` in `settings.json`, each with its type, filters
  (`FilterOp::key` names), sort, grouping, hidden columns and column order. The toolbar's
  Views menu lists the type's views with the shown one checked, saves the table's state under
  a name (the same name replaces), and deletes the shown one; the button reads the view's
  name while the table matches it. Its "Name rows by ▸" submenu picks the property that names
  the type's nodes across the graph (spec 002, "Node names"), "Automatic" first. Filters, a sort or a grouping the schema no longer has are
  left out; a hidden or ordered column the schema no longer has is ignored.

### Columns

Built (`model::ColumnLayout`): the first column, the key or an edge's source, is pinned and
always shows first. Every other column can be hidden and moved. A header's right-click menu
sorts ascending or descending, groups by the column, starts a filter on it, moves it left or
right among the shown columns, hides it, and shows every column again once any is hidden or
moved. Dragging a header onto another moves its column into that place. The toolbar's Columns
menu lists every column but the pinned one, checked while it shows, and stays open while
columns are picked; its button reads "N hidden" while any is. A column the schema adds later
appears after the ordered ones. Hiding only stops showing: a filter, a sort or a grouping on a
hidden column still applies, and a hidden relationship column reads nothing until it shows,
when its neighbours are read for the rows already loaded, 100 at a time, at the table's commit.
The layout is part of the tab and of saved views. Widths stay fitted to the column's type.

### Grouping

Built: the toolbar's Group menu groups the rows by a property (text, enum, Bool, integer or
date; not the key, a list or a vector). One read counts the rows per value under the filters
(`builder::value_counts`: `return { $n.title as value, count($n) as n }`, by value, at most
200 values, rows without a value last); each value is a section with a header (value, count,
disclosure). An open section reads its first 10 rows when its line comes into view, then "Show
N more of M" (or "Show N more" when the page holds the rest) reads a page, all keyset pages narrowed with `title = $value` (or `is null`) and
pinned to the table's commit, in the table's sort. A closed section reads nothing. Every read
is engine-side, so the counts are right at any size. Edge tables group by their own properties
the same way.

A node table also groups by a relationship ("people by company"): the Group menu lists the
table's relationships after its properties, as does a relationship column's header menu. One
read counts the filtered rows per neighbour (`builder::neighbour_counts`: `return { $g.@id as
value, $g.name as label, count($n) as n }`, by label then id, at most 200 neighbours), and a
second, at the same commit, counts the rows with none (`node_count` with "has none"). Each
neighbour is a section named by its label, with its id beside the label when two neighbours
share it and in the tooltip; the rows with none come last as "No Company". A section's pages
are narrowed with "related to" its neighbour, or "has none". A row related to two neighbours
shows in both sections, and an edit or a delete reaches every copy; a cell edit never dims a
row out of its section, since it cannot change the row's neighbours.

A node table also groups by a property of its neighbours ("people by their company's city",
key `out:WorksAt.city`), and an edge table by a property of its source or target (`from.title`,
`to.city`). The Group menu lists the table's own properties, then a submenu per relationship
(by each neighbour, or by a property of the neighbour's type) or per end of an edge (by a
property of the node there), with the relationship's arrow in the place of the entries' check,
in the accent colour while the submenu holds the grouping. The Group button names the choice
in full: "Group: worksAt → Company.city", "Group: to Company.city".

- Neighbours: one read counts the filtered rows per value the neighbours hold
  (`builder::neighbour_value_counts`), each row once per value: a `not {}` block keeps only
  the first neighbour (by `@id`) holding a value, so a person at two companies in Berlin counts
  once there. A second read, at the same commit, counts the rows no neighbour gives a value
  (`node_count` with "is empty" on the key: `not { $r: Company  $n worksAt $r  $r.city is not
  null }`), which come last as "No city". A section's pages are narrowed with `exists { $r:
  Company  $n worksAt $r  $r.city = $value }`. A row whose neighbours hold two values shows in
  both sections, as with a relationship.
- Ends: one read counts the filtered edges per value of the end's property
  (`builder::endpoint_value_counts`: `return { $b.city as value, count($e.@id) as n }`); every
  edge has one source and one target, so the sections divide the rows, edges whose end has no
  value last. Pages are narrowed with `$b.city = $value` (or `is null`).

A cell edit never dims a row out of such a section, since it changes neither its neighbours'
values nor an edge's ends. A new node goes into the "No city" section when it is open, since it
has no neighbours yet; a new edge's ends are not in its row, so it shows after the next read.

The grouping is part of the tab and of saved views. The cursor moves over
headers and past closed sections; ⇧-click picks the rows shown between two rows. A row edited
out of its section's value stays in place, dimmed, until the next read; a new row goes first in
its open section.

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
  Built (drafts): ⌥↵ keeps an edit as a draft instead of saving it, as clicking away already
  did; a draft cell shows the typed text with a dot, and its tooltip says what the cell holds.
  While drafts exist a pill floats over the bottom of the table ("3 drafts not saved ·
  Discard · Save as one commit"; it floats so no row moves when it appears). Save (or ⌘S)
  sends one mutation of one `update` per row (`builder::update_rows`) setting every drafted
  property, bound to every value the row shows, so one commit; a draft that does not parse
  opens instead and nothing is sent. A short `affected_nodes` means some rows changed
  elsewhere and were left alone whole: those rows are read at the head, the drafts that landed
  clear, and the others stay with "Changed elsewhere to “X”". A refused or lost write puts
  every draft back with the reason. Discard asks for a second click.
- Insert: a form generated from the node shape (required properties, key, enums, ranges).
- Delete (⌘⌫): the picked rows (⌘-click, ⇧-click), or else the cursor's, in one commit bound
  to the table's pin (`/mutate/if-graph-commit`). Before it is offered, the dialog reads what
  else goes (`graph_store::deletes`): a node's relationships go with it, and a node that needs a
  relationship to a deleted node (`@card(n..)`) and would be left with fewer than `n` is a
  dependent, deleted in the same commit, because the server refuses a delete that leaves it so.
  Dependents of dependents follow, round by round (`builder::required_dependents`, one read per
  required relationship into a type that grew, held at the pin), up to 1,000; more refuses and
  points to the query editor. The dialog lists them by type and name with the relationship each
  needs ("Also deletes 1 Person, 1 Badge that cannot exist without it"), and ↵ deletes. The
  outcome's toast offers Undo (`studio_graph::UndoDelete`): it reads the delete commit's changes,
  inserts every node and then every relationship again in one commit bound to the delete's
  commit, so it applies only while the branch is still there; unkeyed relationships come back
  under new ids. When Undo could not restore it (a node type without `@key`, or Blob or Vector
  values on the nodes or their relationships, judged from the schema), the dialog says why and
  asks for the key, or `delete N` for several rows, to be typed (constitution IV). An edge
  delete takes no dependents; deleting a relationship its source needs is refused by the
  server, which the dialog shows.
- An edit open when the table moves to another branch (a link, a toast's follow-up), when its
  tab closes, or when the Nodes panel lets its table go, is kept as the cell's draft on the
  branch it was typed on, as leaving the cell keeps it; a tab holding such an edit asks before
  it closes (since 2026-10-21; before, the text was lost).
- Every editing surface shows `write · <branch>`. A 403 shows "Your token cannot change
  <branch>" and keeps the draft.

### Outcomes

`ChangeOutput.commit` present: committed. A 4xx with `ErrorOutput`: refused, "Nothing was
written." plus the reason. A lost response, a timeout or a response without the contract header:
unknown, "Check History before trying again; your changes are still here", with a button that
opens the branch's recent commits.

### Drawing cost

The table is a cached view (spec 002): it is drawn again only when it notifies. When it is, the
row callback works out how each drawn line looks (picked, dimmed, the cursor's, a group
header) for the lines it draws, not for every loaded row; a date's or time's relative text is
made once a minute with the cell's other text; relationship and Blob cells name their elements
by their place and make their tooltips only when shown; and the grouped line list is built
once a frame. Blob cells are read once scrolling has paused for 120 ms, for the rows then
drawn; reads still out for rows scrolled away are dropped and asked again when they return. A
new order or filter at the same commit keeps the relationship cells and Blob cells already
read and reads only rows not seen yet; the counts it asks for again come from the session's
kept answers.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| ↵ and Undo for a delete Studio can put back; typing only when it cannot | Typing the key every time (constitution 1.0.0); no dialog, Undo only | Ragnor, 2026-10-17: typing was annoying. The dialog still lists everything a delete removes, and Undo puts it back from the change feed in one commit |
| Dependents found by Studio and deleted in the same commit, listed | A checkbox per delete; only a schema-declared cascade (an engine change) | Ragnor, 2026-10-17. The server already refuses a delete that leaves a node below its `@card` minimum, so without them the delete cannot happen; the list says exactly what goes, and the delete is bound to the commit the list was read at |
| Value-bound cell saves | `if-graph-commit` on every cell | Any unrelated commit would refuse the save |
| Engine-side filter, sort and count | Filtering loaded rows | Correct at any size; grouping and filtering loaded rows is wrong once a type outgrows a page |
| No row cap | A 1,000-row cap, as a web table needs | GPUI virtualizes; such a cap exists only because a browser renders every row |
| One offer list (`offers.rs`) for the filter's value field and the edge dialog's endpoints | Zed's editor completion menu (`CompletionProvider`), as the query editor uses | The menu draws code completions in the editor's monospace font and opens on typing; these fields offer names with their ids muted beside them, in the interface's type, and an enum's values as soon as the field has focus. The two hand-built copies became one: the list, ↑/↓ around the ends, the rows and the pick on press |
| Fork Zed's CSV preview renderer and selection model | GPUI Kit's table | One GPUI lineage (constitution XI) |
| Count a row once per value its neighbours hold (`not {}` over earlier neighbours with the value) | `count($n)` over the match | `count($n)` counts matches, not rows: a person at two Berlin companies would count twice, and the section would read fewer rows than its count says |

## Deferred

- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E11 typed result columns: column types come from `describe_query_operation` until then.
  - R2 unpublished commits: staged edits as a reviewable proposal. The minimum server (0.14)
    serves them; Studio does not use them yet. Blob writes (R1) are served there too, and used
    since 2026-10-20 (Layout).
  - A precondition that a Blob cell is null (`If-None-Match: *` on `PUT /blob`): no `If-Match`
    describes an empty cell, so a file stored in one is checked at the head just before it is
    sent and goes unbound, and a value set elsewhere in between is overwritten.
- Per-row history beyond the commits that the change feed can cheaply return.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-21 | A Blob tab or a peek shows Blob values where they hold, not at the pin | `type_table` `a_views_own_blob_write_leaves_its_next_one_bound_where_it_holds`, extended (after the page's bound write over another writer's commit, `BlobsAt::Own("c2")` and Open's `at_commit` `c2`), and `a_peek_opened_after_its_tables_blob_write_shows_it_where_it_holds` (a table behind its branch stores a Blob: a peek opened then reads at the table's pin `c1`, shows the 6-byte value written, and holds its Blobs at `c3`) | Before, Open read at the view's commit (`c0`), the value replaced, and that tab's first upload was refused as changed; a peek opened from a table got the pin alone and read the value replaced there |
| 2026-10-21 | The table tells its peek of every write it makes | `type_table` `a_peek_follows_every_write_of_its_table` (a bulk set of two rows, then a row inserted through `GraphSession::write_on`), `a_peek_shows_the_blobs_its_table_stores` (a 5-byte file stored in the peek's row), and the peek test `a_group_that_fits_its_page_is_read_once`, extended (`table_wrote` naming a neighbour shown, then one not shown) | The peek moves to `c2` with "Ida" and is not newer, then to `c3` for the insert; its slot shows the 5 bytes stored at `c2`; a write to a neighbour it shows leaves it behind at its commit, one to another row moves it on. Before, a bulk set, an insert and a Blob write reached the peek only as a head move, which offered Refresh or kept the old value |
| 2026-10-21 | An edit open when the table changes branch or goes is kept as a draft | `type_table` `a_branch_change_keeps_an_open_edit_on_the_branch_it_was_typed_on`, `a_table_let_go_keeps_its_open_edit_as_a_draft`; each run again with its fix undone | Typed "Ada King" in p1's name, then the session moved to fix: main held the draft bound to "Ada", fix none, the keys back on the table; a table with the edit open was dirty (typed back to "Ada", it was not) and, its window removed, left the draft. With the fixes undone both failed: the branch change set the edit to none before the reload that keeps it, and the table had no release hook |
| 2026-10-21 | A Blob tab opened from a peek at the head writes | `type_table` `a_slot_opens_its_tab_held_only_where_the_view_holds_its_commit`, `a_tab_from_a_view_at_the_head_stores_a_file_bound_to_what_it_read` (failing with the tab's held commit taken from its snapshot alone, as before) | `OpenBlob` from a page at the head: `at_commit` c1, `held` false; held page: `held` true. The tab at c1 not held offered Choose File and Clear and stored a file on main with `If-Match` the ETag read at c1; its key is apart from the held tab's and the head's |
| 2026-10-21 | A date preset judges rows by its days | `type_table` `a_date_preset_judges_edited_and_inserted_rows_by_its_days` (born is today; a name edited, a row inserted behind the branch), failing with `row_fits` on the stored filters | The edited row stayed plain and the inserted one counted (2 → 3). Before, the preset's stored null matched no row: every edit dimmed its row and an insert counted none |
| 2026-10-21 | A row read again after the table's own write shows once | `type_table` `a_row_the_tables_write_moved_further_on_shows_once`, `a_row_moved_into_another_section_leaves_its_rows_to_read`, both failing with the check undone | After p1's name saved at c2, a page at c2 holding Cy and p1 added Cy alone. Grouped by name, p1 moved from Ada to Grace: Grace's page holding only p1 added nothing and Grace still had a row to read, which the next page added (rows p1, p2, p3). Undone, p1 showed twice and Grace's count of 2 was spent on it |
| 2026-10-21 | A table behind its branch stores a second file | `type_table` `a_tables_own_blob_write_leaves_its_next_one_bound_where_it_holds`; `model` `own_commits_move_the_pin_only_from_the_pinned_parent`; the fake server's Blob cells made to tag by table version (`Blobs::versioned`: a write gives every cell of its type a new ETag, and a read at a commit it published answers there) | Pinned at c0 behind c1: p1's file landed at c2 and p2's, bound to p2's ETag at c2, landed too; then another writer's value of the same size in p2 refused the third (412, a conflict) and kept theirs. Undone (bound to the shown ETag while behind), the second was refused as changed elsewhere |
| 2026-10-21 | A peek opened while a cell saves shows what is stored | `type_table` `a_peek_of_a_row_being_saved_shows_what_is_stored` (refused with a 403, then saved at c2), failing with the row's values given as they were | While out: "Ada", saving. Refused: "Ada" at c1. Saved: "Ada Lovelace" at c2, not newer. Before, the peek showed "Ada Lovelace" as stored and kept it after the refusal |
| 2026-10-21 | Dates are not parsed at every draw | `type_table` `a_dates_relative_text_is_kept_for_its_minute`; a read of `render_cell`, `render_relation_cell`, `render_blob_cell` and `TypesList::render_row` | A DateTime cell's text and exact value are kept with its minute, given back within it and made again in the next. Relationship and Blob cells take their element ids from their place (`ElementId::NamedInteger`) and look their tooltip's text up when it shows; their state is looked up without copying keys (`CellMap`); the Types list reads the active tab's key once a frame. No measurement was taken |
| 2026-10-21 | A table follows a new token without a write | `graph_store` `a_new_token_reads_access_and_every_answer_again` (the session's branch access read again on the token change; the table draws on `SessionEvent::AccessChanged`, as `type_table` `a_branch_the_credential_may_not_change_offers_no_writes` checks) | A read-only token replaced by a write-capable one allowed changing main as soon as its answer landed. Before, the table said "Your credential may not change main" until the branch moved or a write was sent, and none could be |
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
| 2026-10-12 | Grouping by a relationship | `gq_language` `neighbour_counts_group_filtered_nodes_by_each_relationship` (every relationship of the dev schema, with property and relationship filters, type-checks); `type_table` `a_relationship_groups_by_neighbour_with_the_unrelated_rows_last`; the counts and the "has none" count on the dev server (32 people without a company); offscreen snapshots of Person grouped by `worksAt →` from the Group menu, then two seeded views applied from the Views menu ("Founders by company", "Knuths by company", each hiding email and tags) | Sections by company label with counts; the two "Allen Works" companies read "…pany-00009" and "…pany-00023"; Shafi Thompson showed under both of his companies; "Knuths by company" read 12 of 200 in nine sections summing to 12, "No Company 3" last |
| 2026-10-12 | A peek says when it is older than Studio's own write | offscreen snapshot on the scratch branch `drafts-test`: `OpenNode` person-000000, then its title edited in the Person table, then Refresh in the peek | After the commit the peek still read its commit with "Newer data · Refresh" beside it; Refresh read a new commit and the new title |
| 2026-10-12 | Hidden and moved columns | `type_table` `a_layout_hides_and_orders_columns_and_keeps_the_pinned_one_first` and `a_layout_survives_schema_changes`; `studio_settings` `views_are_saved_per_graph_and_type_and_replaced_by_name` with hidden columns; offscreen snapshots of Person: right-click on `email` then Hide column, the Columns menu, a click on `email` in it, and a drag of the `born` header onto `name` | The menu read Sort ascending, Sort descending, Group by this column, Filter…, Move left, Move right, Hide column, aligned; `email` left the grid and the button read "1 hidden"; the Columns menu checked every shown column and stayed open while `email` came back ("Columns", Show all columns disabled); the drag put `born` before `name` and did not sort |
| 2026-10-12 | Saved views | `studio_settings` `views_are_saved_per_graph_and_type_and_replaced_by_name`; `gq_language` `filter_operators_round_trip_through_their_keys`; offscreen snapshots with `click:` and `type:` steps: a seeded "Founders" applied from the Views menu, then sorted by email and saved as "Founders by email" | "title is Founder", name ascending, "25 of 200", and the button read "Founders"; `settings.json` held both views |
| 2026-10-12 | Spike S1: several rows in one commit | on a scratch branch of the dev server: `update Person set { title: $v } where @id in $ids` (3 rows), and one mutation of two `update` statements | Both publish one commit; multi-statement mutations are the mechanism, so merge-mode NDJSON was not needed |
| 2026-10-12 | Bulk set | `gq_language` `a_bulk_set_is_one_mutation_bound_row_by_row`; offscreen snapshots with `click:…,cmd` and `click:…,shift` on Project at branch `bulk-test`: three rows picked, status set to paused | One commit; `project-00001`, already paused, was left out ("Set status on 2 Project rows"); the rows updated in place and the selection cleared |
| 2026-10-12 | Filters offer enum values and related nodes | `type_table` `suggest` tests (narrowing, prefixes first, lists skip given values, completion); offscreen snapshots on Project: ⌘F on status, `a` → "active, paused", ↓ ↵ added "status is paused" (12 of 40); `← contributes Person` · is related to · `ada` listed eight Adas with their ids, ↵ added "← contributes is Ada Sammet" (2 of 40) | As described; the property menu now hides the offers |
| 2026-10-12 | Group-by reads counts per value and pages per section | `gq_language` `value_counts_group_filtered_rows_of_nodes_and_edges` (every groupable property of every dev node type, with a filter, and an edge property, compile); `type_table` `grouping` tests (value order with no value last, section slices shifting as rows are read in and removed, steps over headers and past closed sections, the cut-off line); offscreen snapshots: Person, Group → title, Analyst collapsed, Show more on Designer | Analyst 27, Designer 29, Engineer 31, … each with 10 rows and "Show 17 more of 17"; Analyst closed to its header; Designer read 19 more in place (71 → 90 rows loaded) |
| 2026-10-12 | Drafts of different values on several rows save as one commit, each row bound to what it showed | `gq_language` `edits_of_different_values_are_one_mutation_bound_row_by_row` (one statement per row, lists unbound, compiles); offscreen snapshots on branch `drafts-test`: ⌥↵ on Person 0 title "CTO", Person 2 title "Lead Designer" and born 1990-01-02, then ⌘S; then again with Person 0's title changed by another client (`wait:` step) between staging and saving | First run: the branch went from 5 to 6 commits and both rows read back with the new values; the drafts cleared. Second run: "Design Lead" landed, Person 0 kept its draft with "Changed elsewhere to “Acting CTO”", and the pill read "1 draft not saved" |
| 2026-10-12 | A Blob value saves whole or not at all | `type_table` tests `a_save_lands_whole_or_not_at_all`, `a_saved_value_is_named_for_its_row_property_and_kind`; an offscreen snapshot of a note with a 12 KB text attachment | "Save…" at the end of the attachment's line; a save writes `<name>.part`, appends each range, and renames it into place only at the value's full size (a short value or a stop removes the part); the name is `<id>-<property>` with the kind's extension. The native save dialog cannot be driven offscreen |
| 2026-10-12 | A Blob value opens in a tab of its own, shown for its kind | `type_table` tests `values_are_told_apart_by_their_bytes`, `a_pdf_is_drawn_page_by_page`, `a_page_is_drawn_to_scale_on_white_with_its_marks`, `the_open_folder_keeps_a_day`; offscreen snapshots of four notes on the dev `notes` graph with attachments set through a `Blob` parameter | A 960 × 600 transparent PNG fitted at 100% on the checkerboard ("PNG · 960 × 600 · 8.8 KB"); a two-page PDF drawn sharp ("PDF · 2 pages · 1.2 KB"); a 12 KB text in the read-only editor with line numbers; 3 KB of a binary as sixteen-byte hex lines. The first PDF test failed until the context was scaled before drawing: CoreGraphics' page transform never enlarges a page |
| 2026-10-13 | Columns are resized from their header's edge | `model` test `a_dragged_width_holds_within_bounds_until_reset`; offscreen snapshots of `Person`: the handles drawn red to find them, then `drag:846,108,946,108` on title's edge, then a second run restoring the tab | Title went from 200 to 300 px and the columns after it moved with it; the tab saved `"widths": {"title": 300}` and the next start drew it at 300. At rest no line separates columns; the edge shows one on hover and while dragging |
| 2026-10-13 | A table of 10,000 rows scrolls faster | Snapshot tool on `Person` of the dev graph's `perf-10k` branch (10,000 people), `measure:300,scroll=-60` and `measure:300,refresh`, before and after; CPU time per frame added to the tool because wall time on the shared machine varied fourfold between identical runs | Debug build, scrolling p95 48.7 → 27.8 ms wall; with the dev profile now optimizing taffy, resvg, serde_json and proc-macros as Zed does, CPU median 17.4 ms and p95 20.2 ms (that profile change alone was not measured apart). Optimized (`release-fast`), scrolling p95 was already 7.4 ms. Per cell, the table no longer reads the local time zone (once a frame instead), clones two keys to look up cell state when there is none, or formats an element id; the selection is an `IndexSet`, so drawing a row no longer scans every picked id |
| 2026-10-13 | An edge is added from its table with its endpoints found by search | Offscreen snapshots of `WorksAt` on the dev server: Row, "radia" typed into From; then ↓ ↵, "gold" ↵ in To, "Advisor" ↵ in role; then a CLI read | Six people listed by name with their ids, the first highlighted; ↓ moved the highlight; ↵ put the id in the field and moved on. The edge committed to main ("Insert WorksAt person-000000 → company-00000 · committed to main"), the table showed "Newer data · Refresh", and the CLI read role Advisor |
| 2026-10-13 | An edge to a missing node is refused and the form stays | Snapshot: From "nobody", To picked, ↵ | "Nothing was written. __src 'nobody' not found in Person" under the form, values kept; the server answered 400 |
| 2026-10-13 | An edge is deleted from its table, bound to the table's commit | Snapshot of `WorksAt`: ↓, Delete, the dialog, then ↵; CLI counts before and after | "Delete WorksAt person-000175 → company-00012 · This removes this WorksAt from main. Both nodes stay."; ↵ deleted it: the row left the table, 217 → 216, and the CLI found no such edge |
| 2026-10-13 | ↵ on an offered value types nothing into the field | Snapshot of ↵ on the first offer, before and after the fix | Before: the field read `person-000000\n`; Studio's `move_to_end` after `set_text` only propagates in a single-line editor, so the ↵ reached the field as text too. After: `person-000000`. The table's filter field had the same call. Guarded by the headless `type_table` test `enter_on_an_offered_node_picks_it_and_types_nothing`, which fails with the old call (`"person-000004\n"`) |
| 2026-10-13 | A peek reads only what the table did not | offscreen snapshots through a request-counting proxy: the Person table (four relationship columns), then the peek and ↓ twice | Before: the peek read 4 counts and up to 4 neighbour lists for each row it showed, 22 reads in all; after: none. The peek of Grace Allen read the same before and after (contributes 2 with both projects, knows 0, worksAt 0, known by 2 with both people) |
| 2026-10-13 | Another writer's commit shows "Newer data" within a poll | offscreen snapshots on the dev server: the Person table, then a title set from curl 25 s in, then 40 s more; and a control with the write before the table's first read | The table kept its commit's title and showed "Newer data · Refresh" with the tab's dot; in the control it read the new title and showed nothing |
| 2026-10-14 | Typing in the filter field or a cell does not draw the grid | Main-thread CPU per frame from the snapshot tool, `release-fast`, Person (200 rows), 200 frames, before and after in the same sessions; `sample` of the filter loop; headless `typing_in_a_field_leaves_the_grid_as_it_was` and `studio_workspace` `a_tab_that_caches_its_parts_keeps_them_while_a_field_changes`, each failing with its fix undone | Before, a key in either field cost a full grid draw (10.5 ms filter, 10 ms scroll, under load); the profile put it in the grid's layout and paint, the editor's own work under 1 ms. After: filter 2.2 ms, cell 2.3 ms, query editor 3.5 ms, scroll 7.6 ms (load average 20). Undoing the per-key `notify` fails the filter assertion; drawing the editor inside the grid fails the cell assertion |
| 2026-10-14 | A scroll frame's cost, and whether lighter cell text lowers it | `sample` of 1,500 scroll frames on Person, `release-fast`; then cell text drawn as one styled element instead of a `Label`, measured against the old build interleaved, three rounds each | A scroll costs 6 ms of main-thread CPU, inside a 120 Hz frame. The table's own code is 7% of it; GPUI's layout (40%), element layout requests (24%) and paint (25%) of the visible rows are the rest. Lighter cell text changed nothing measurable (5.9 to 6.6 ms before, 5.8 to 6.4 after), so it was not kept; a lower cost needs fewer elements per row, such as drawing a row's text without an element per cell |
| 2026-10-14 | A row draws only the columns in view | Snapshot tool, `release-fast`, Person (10 columns, about 6.5 in view at 1440 px), 300 scroll frames at 1440×900 and 1440×1400, against the build before, interleaved, three rounds; renders after → nine times and six times; headless `rows_draw_only_the_columns_in_view` | Main-thread CPU a frame: 6.1 → 5.0 ms at 1440×900, 9.4–10.3 → 7.6 ms at 1440×1400. Every column that shows was drawn, including one half under the pinned column. The query result grid, which draws plain text cells without `ui::Table`, costs about the same per cell; what the table paid for was cells out of view |
| 2026-10-17 | A delete takes its dependents, says so, and can be undone | `gq_language` `deletes_take_many_ids_and_find_the_sources_that_need_them`; `graph_store` `dependents_follow_the_chain_of_required_relationships` (company → person → badge, every read held at the pin, an optional relationship never read), `undo_is_known_before_the_delete`, `a_plan_deletes_every_type_in_one_mutation`; `studio_workspace` `a_delete_that_can_be_put_back_offers_undo`; on a scratch OmniGraph 0.13 graph (`WorksAt @card(1..)`, `Holds @card(1..1)`): `delete Company` alone, then the planned delete, then the undo as one insert mutation; offscreen snapshots: Company, ⌘⌫ on Acme, ↵, then Undo, with CLI reads after each | Alone: refused, "@card violation on edge WorksAt: source 'ada' has 0 edges (min 1)". The dialog listed Ada (needs WorksAt) and b-ada (needs Holds); ↵ deleted 3 nodes and 4 relationships in one commit, Bob kept his Beta job, and the one toast read "Delete Company Acme and 2 that needed them · committed to main" with Undo. Undo committed once more and the CLI read every node and all four `WorksAt` back; the table showed "Newer data · Refresh" |
| 2026-10-14 | A wider grid draws the columns its new width shows in the frame that widens it | `type_table` `rows_draw_only_the_columns_in_view` extended: the window widened from about 1,730 px of scrolled columns to 5,810, then the columns the rows drew read in that frame, with the fix that asked for a second frame removed | Every column drawn in the frame of the resize: Zed's header row tracks the same scroll handle and is laid out before the list picks its rows' columns. A test with the extra frame disabled passed the same, so the extra frame was not kept |
| 2026-10-14 | Grouping by a neighbour's property and by an end's property | `gq_language` `rows_group_by_a_neighbours_or_an_ends_property` (for every relationship of every dev node type and every groupable property of the other type: the counts, and pages and counts narrowed to a value and to "no value", compile; for both ends of every dev edge the same), `group_keys_name_one_kind_each`; `type_table` `a_neighbours_property_narrows_by_its_value_and_an_end_has_no_value_in_the_row`, `rows_group_by_a_neighbours_or_an_ends_property` (the menu's keys and labels on Person and WorksAt); offscreen snapshots on the dev server: Person, Group, the `worksAt Company` submenu, city; scrolled to the end; WorksAt, Group, `to Company`, city; and the same counts read with curl | Submenus for `contributes Project`, `knows Person`, `worksAt Company` and `← knows Person`, each with its arrow; Berlin 9, Lisbon 9, London 46, San Francisco 46, Tokyo 58, Toronto 35, "No city" 38 of 200 people, as curl counted; WorksAt by `to Company.city`: Berlin 9, Lisbon 9, London 49, and 7 edges to companies with no city |
| 2026-10-20 | The Blob write contract | `openapi/omnigraph-0.14.0-edge.json` (`cluster_putBlob`, `cluster_clearBlob`, `BlobWriteOutput`, `ErrorOutput.blob_precondition_failure`); OmniGraph `crates/omnigraph-server/src/handlers.rs` (`server_blob_put`, `server_blob_delete`) and `crates/omnigraph/src/blob.rs` (`managed_blob_etag`) at `cf2b20e9` | `If-Match` is `*` (a value is there) or strong tags; a null cell satisfies neither, and a put takes no `If-None-Match`, so a write into a null cell cannot be bound. The ETag hashes the table version with the row: any write to the type moves every ETag of it. A put needs `Content-Type: application/octet-stream`, at most 32 MiB; a receipt's `commit` is null only for a clear of a cell already null |
| 2026-10-20 | A Blob write is bound to the value read and has three outcomes | `graph_client` `a_blob_put_sends_raw_bytes_bound_to_the_etag_read`, `a_blob_clear_of_a_null_cell_commits_nothing`, `a_changed_blob_refuses_the_write_with_its_current_etag`, `a_blob_put_without_the_servers_answer_is_unknown`; `type_table` `blob_write` tests, the peek's `a_file_stored_in_a_slot_is_bound_to_the_value_read`, `a_value_changed_elsewhere_is_offered_again_over_the_new_one`, `uploads_the_view_cannot_send_say_why_and_send_nothing`, `a_page_held_at_a_commit_stores_no_file`, the table's `a_file_dropped_on_a_blob_cell_is_stored_bound_to_the_value_read`, the tab's `a_file_stored_from_the_tab_shows_at_its_own_commit` and `a_tab_at_a_commit_writes_nothing`, against the fake server's Blob cells | One `PUT` with one content type, the bytes and the ETag read; the view, the table and the tab move to the write's commit and show its value; a value changed elsewhere sends one write, keeps the other writer's bytes and offers "Replace anyway" bound to the new ETag, which lands; a 32 MiB + 1 file and a held view send nothing; a lost answer is unknown and not sent again. The native picker and a drop from Finder were not driven: no test reaches the system's open panel or its file drags, so the paths after them (`upload_blob`, `upload`) are what the tests call |

## Corrections

- 2026-10-14 · Grouping by a property of a neighbour or of an edge's end, which was deferred, is
  built (see Grouping). "Show N more of M" read "Show 25 more of 25" when the page held the
  rest of a section; it now reads "Show 25 more" then. An empty table at the branch head says
  how to add a row for edges too (edge insert, 2026-10-13), with Row's shortcut read from the
  keymap (`studio_ui::shortcut_text`); a table at a past commit no longer suggests it.

- 2026-10-14 · Every key in the filter field or a cell drew the whole grid again: the fields
  were inside the table's view, GPUI draws every view around a changed field, and the pane
  drew the table cached, so the grid inside it was drawn again as well (`prepaint_view`). The
  grid is now a view of its own (`TableGrid`), made with the table, drawn cached, and drawn
  again when the table notifies; the table answers `caches_its_parts` so its pane draws it
  uncached; the cell editor is drawn over its cell from outside the grid
  (`studio_ui::OverlaySlot`); and the filter's suggestions no longer notify on every key.
  The rules and their guards are in `docs/drawing.md`.

- 2026-10-13 · Columns can be resized, which was deferred. Each header's right edge is a 6 px
  handle: drag it to set the width (48 to 1,200 px), click it twice to fit the column to its
  type again. Dragged widths are kept per column key in the table's layout and saved with the
  tab. Zed's resizable columns were not used: they draw a full-height rule between every
  column, which the visual language leaves out; a line shows at the edge only on hover and
  while dragging. The handle and its drag now live in `studio_ui` (`column_resize_handle`,
  `ColumnResize`), which query results use too (spec 007).

- 2026-10-13 · Blob images were handed to GPUI as encoded images, which GPUI decodes into its
  asset cache and keeps until asked, with a texture in every window that drew them; nothing
  asked, so a peek moving through rows of photos kept every one (an 8 MB JPEG decodes to about
  100 MB). The peek and the Blob tab now decode raster images off the main thread into images
  they hold, draw those, and free them when they move on, reload or close; an SVG stays
  GPUI's to rasterize. A value that sniffs as an image but does not decode shows its bytes
  (offscreen snapshots of the notes graph's PNG in the peek and its tab: both drawn).

- 2026-10-13 · A peek of a node missing at its commit read "This type takes part in no
  relationships" under "This node does not exist at this commit", and a peek still loading
  showed the same line for a moment: the relationship list is filled only after the node is
  read, and an empty list read as a type without relationships. The section now shows once the
  node is read (offscreen snapshots: `company-000001`, which the dev graph does not hold, shows
  the error alone; `company-00003` lists owns 2 and worksAt 18).

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
- 2026-10-13 · Edge insert and delete landed. Row opens "New <Edge>": From and To search their
  node types by name or id at the table's commit (↑/↓ and ↵ pick, a click picks), then the
  edge's properties. The engine assigns an unkeyed edge's id, so the table shows "Newer data"
  rather than placing the row. Delete shows the edge as "from → to" and is confirmed with the
  button or ↵, not by typing a key: an edge's id is generated, and deleting it leaves both
  nodes. Like a node delete it is bound to the table's commit. The toolbar reads "Cells read
  only" for an edge table at the head.
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
- 2026-10-12 · The footer's key hints read the bindings of the frame drawn before, so after an
  edit closed they showed no keys until something else redrew; ending an edit now redraws once
  more on the next frame.
- 2026-10-12 · A drafts save that lands only in part still toasts its description ("Save 2
  changes on 2 Person rows · committed"); the pill and the kept drafts say which did not land.
- 2026-10-12 · A mutation that matched nothing used to toast "committed"; the workspace now
  shows no toast for it and the status line says nothing was written (constitution VII).
- 2026-10-12 · The branch-head check is a one-row read at the head every 30 s while the table is
  the active tab, and after any Studio write to the same branch. Offscreen snapshots do not run
  GPUI timers, so the poll and the peek debounce are not visible there.
- 2026-10-13 · The table no longer asks after its head itself: the session's head watch (spec
  003) reads it every 30 s while a window is active, for every view at once, and the table
  compares its pin with the head it saw. The insert and delete dialogs write on the table's
  behalf, so they tell it a write is out (`save_started`, `save_finished`); a head seen while a
  write is out is compared once its outcome moved the pin. Offscreen snapshots do run timers
  (`wait:` advances the simulated clock), and the snapshot window is now active, so the poll
  shows there.
