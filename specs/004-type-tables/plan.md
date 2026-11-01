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

- Omnigraph `v0.13.0` HTTP: `POST /graphs/{id}/query` takes `{query, params, branch | snapshot}`
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
  relationship tree rooted at the row (spec 005), and Blob previews: an image, an SVG or a
  PDF's first page drawn when the whole value is at most 8 MiB, else the start of its text or
  its first bytes in hex, the value's kind judged by its first 4 KiB as the Blob tab and a saved
  file's name judge it (`blob::value_kind`; since 2026-10-21, before which an SVG showed as
  text and saved as `.txt`, and a PDF showed as text or hex). A stored Blob value has
  "Save…": the person picks a file (named `<id>-<property>` with the extension of its kind),
  the value is read in ranges where the value shown is held (the commit it was read at, or the
  commit of the write that stored it) into `<file>.part` beside it, off
  the main thread, and renamed into place only when whole; the line shows how far it got, then
  "Show in Finder". A value no commit the peek knows holds (its table's own Blob write past
  another writer's commit) has Save disabled, saying so (since 2026-10-21; before, Save read at
  the peek's commit, which after a write from behind held the value replaced, so it saved that
  value's first bytes or failed). The peek reads at one commit; once the session sees its
  branch's head elsewhere (spec 003, Studio's commit or another writer's), a node's peek shows
  "Newer data · Refresh" beside that commit, and Refresh reads the node, its relationships and
  its Blobs again at the head. A peek opened from a table behind its branch compares the head
  the session has seen as it opens, as it compares a head move (since 2026-10-21; before, it
  showed the table's older values as current until the head next moved). A field's save that
  lands on the commit the peek shows after the node was read again meanwhile (a Refresh,
  another field's conflict) shows the value written at the save's commit (since 2026-10-21;
  before, the read's older value). A peek from a table opened at a commit holds that commit and
  offers no Refresh.
  A peek opened while one of the row's cell saves is out shows what the row holds at the
  table's commit, that property with the save's dot and not editable there, and follows the
  save as it follows its own writes: landed on its commit, the peek moves to the save's commit
  with the value; refused, it keeps the value it shows (since 2026-10-21; before, it showed the
  value sent as stored, kept it after a refusal, and bound an edit to it). Every value write of
  the table's own tells its peek, which follows it so (since 2026-10-21; before, only a cell's
  save and a drafts' save did, and the rest left the peek offering Refresh): a bulk set, a row
  inserted, a Blob stored or cleared, whose value the peek's slot then shows. One that changed
  another row the peek shows among a group's neighbours leaves the peek behind, since it would
  show that neighbour by its old name. A delete is not told, nor is its Undo, which the
  workspace sends through the session: both change relationships as well as rows (a deleted
  node takes its edges with it, Undo puts them back), which a told write cannot carry, so the
  peek compares that commit as it does another writer's, moving to the head when nothing it
  shows changed and offering Refresh when something did; the peek of a deleted row closes. A peek opened from the table shows its Blob values as the
  cells show them, where they hold (`TableBlobs`: past the pin after the table's own Blob write
  over another writer's commit), binds its Blob writes there, and a tab it opens for a value
  reads there; where they hold at no known commit, the tab reads which commit the branch head
  is when it opens and holds to it, every range and a Save read there (since 2026-10-21;
  before, each range and a Save read the session's head as it was by then, another branch's
  once the window moved). A peek opened from the table reads no
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
  `1` its actual size, `0` fit again, a drag to pan; an SVG (text whose first element is
  `<svg`, shown as text when it does not parse) fitted; a PDF's pages drawn by the
  system (CoreGraphics on macOS, twice the width shown, the first 20 pages: a page's pixels are
  about 13 MB, held while the tab is open) and stacked to scroll; UTF-8 text, its first 4 MiB or
  up to its first byte that is no text, in a read-only editor (⌘F searches it); anything else as
  a hex dump of its
  first 64 KB, sixteen bytes a line. The footer gives the kind, the pixel size or page count,
  the size and the commit; zoom (−, the percentage, +, Fit) for an image or a PDF; Save…; and
  "Open in default app", which writes the value to an owner-only folder in Studio's data
  directory, emptied of anything a day old, and hands it to the system. The tab comes back after
  a restart reading where it read: past its own write, at that write's commit (since
  2026-10-21; before, at the commit it opened at, showing the value its write replaced and
  refusing its next write as changed elsewhere). A tab saved before it said whether it holds its
  commit does not come back. A tab that writes comes back writing to the branch it was saved on,
  whichever the window shows, and one tab writes to one branch; a branch gone since disables
  Choose File and Clear, saying so, and nothing is sent; one saved before it named its branch
  does not come back (since 2026-10-21; before, a restored tab wrote to the window's branch,
  bound to a value read on the branch it was saved on). A list read before the branch was made
  lacks it as well, so a tab that finds its branch missing has the list read again before it
  says the branch is gone, the server deciding meanwhile (since 2026-10-22; before, a tab on a
  branch made after the list was read, by Studio a moment before or elsewhere, said it no
  longer existed until the list was next read). Elsewhere than macOS, a PDF opens in the
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
  at the view's commit, taken as the write is sent, after the file is read, from what the table
  cell, the peek's slot or the Blob tab shows then (`blob_write::upload_bound`, the one upload;
  since 2026-10-21, before which a write of the view's own, or of the table a peek came from,
  landing while the file was read had it refused as changed): a managed value by its
  ETag, which names the type's table version, so it is read again at the view's commit when
  the view moved since (its own writes move it). A view behind its branch reads it at its own
  last commit when that commit's write was itself bound to an ETag read where the values shown
  hold, since the server applied it only while the type's table was as the view showed it, and
  else is bound to the ETag the cell shows (`model::BlobsAt`; since 2026-10-21, before which a
  second file stored behind was refused as changed elsewhere); a null cell and an external
  reference have none, so the cell is read at the head first and the write goes only while it
  still holds what is shown (`*` for a reference, no `If-Match` for null). While out the value
  shows the amber dot and "saving to <branch>…"; committed, the view moves to its commit as a
  cell edit does and shows the new value; refused, the server's reason;
  changed elsewhere (a 412, or found before sending), what it holds now and "Replace anyway",
  the change again bound to that (a table cell: drop it again); unknown, check History. Nothing
  is retried. A view held at a commit, an edge's value and a property that is not a Blob are
  disabled with the reason, and Clear also for a property that must hold a value. The
  outcome log names the file and its size, never its bytes. An edge's value is changed with
  `omnigraph blob put edge …`, as the disabled control says: Studio edits no edge's values.

### Reading

- Page query, generated from the schema (sketch for `Person` ordered by key, a page after the
  first; the first page has no cursor, so neither the `$after` parameter nor its clause):

  ```
  query studio_page($after: String) {
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
  Every page, section and group count read is tagged, as it is sent, with how many of the
  table's own writes had moved its pin by then (`own_writes`); one that lands after another of
  them is read again at the new pin, never shown under it (since 2026-10-21; before, a page read
  before a save landed showed the old value, a deleted row came back and an inserted one never
  showed). A pin
  carried forward to a head that changed nothing the table reads (spec 015) is no such write,
  so a read out then lands as it was (since 2026-10-21; before, each read was tagged with the
  pin it was sent at, and sent again at every move of it).
- Sorted, a read that shows no row and is not the last (the rows with a value ran out) reads
  the rows without one at once (since 2026-10-21; before, a table whose rows all lacked the
  sort's value said it had none).
- Once the table's own write moved its pin, a later page leaves out the rows the table shows
  already (grouped by a relationship or a neighbour, those its section shows): read at the new
  commit, a row it edited can sort again further on, or into another section, and stays where
  it was read (constitution V). A section's count, read before the write, is spent only on the
  rows it shows, so none is left unread (since 2026-10-21; before, such a row showed twice and
  could take the place of a row of its new section).
- The next page is read a page ahead of the view: as soon as a page lands, while no more than a
  page of rows is left past the lines in view (`Paging::wants_more`), so the rows past the view
  stay within two pages (since 2026-10-22; before, once the view passed 80% of the rows read). A
  read still out when the order, filters or branch change is dropped with the reload. No row
  cap: the grid is virtualized.
- Relationship columns: per page and relationship, with the page's ids as an `in` list
  parameter; at most two neighbours as chips, those that fit the column whole, then "+N" (a
  chip is never cut mid-name; its width is estimated from its label, as a cell is laid out
  before its text is measured). Built as two reads at the page's commit, sent together: a
  grouped count (`return { $n.@id as id, count($m) as n }`) and every neighbour of the page's
  rows, by row then neighbour, up to 2,000 rows. The counts show as soon as they land, the
  chips once the neighbours do (since 2026-10-22; before, the neighbours were asked for only
  once the counts were in, and neither showed until both were). A row with more than 200
  neighbours shows its count alone. The rows of up to 200 that the first answer cut short (a
  hub before them spent the budget) are read again together, sized by their counts, up to
  2,000 again; a row past that shows its count alone, so a hub never leaves the rows after it
  bare. A click opens the other type filtered to this row's neighbours through the same
  relationship.
- Filters read as segmented pills (`ds::filter_pill`): the field with its kind's icon (text,
  number, time, choice, file, or the relationship's direction), the test, the value and a
  remove button, split by hairlines. A filter being written is the same pill outlined in the
  accent, its field and test as menus (the field menu shows each property's icon) and its value
  as a field at the label size.
- Counts: the count of every row is the session's snapshot count (`/snapshot`'s
  `entity_count`) when the snapshot counts the table's pin: the session found the branch's
  newest commit at the snapshot's manifest version (`GraphSession::counts_commit`), and it is
  the pin, at the head or held at that commit. Then no count of every row is read, and
  filtered, only the count through the filters is (since 2026-10-22). Otherwise the counts are
  read at the pin; while the count of every row is out, a table at the head the session holds
  shows the snapshot's count in its place, which the read replaces as it lands (since
  2026-10-22). A table held at another commit, or one whose own writes past its pin change its
  counts, shows no snapshot count.
- Filtered counts: `return { count($n) as n }` with the same filters, pinned to the snapshot.
- Reading on a branch (since 2026-11-01): a table on any branch but `main` reads what the
  branch changed in its type, as the net diff from the merge base (`branches/merge-base`, then
  `/diff` a page at a time, folded per entity as review folds, up to review's cap of 20,000
  changes), and draws it as a diff draws its lines: a row the branch added or changed on a
  faint wash of its kind's colour (`ds::surface::wash`) with a bar of it in the gutter, a
  changed row's changed cells on the stronger mark (`ds::surface::mark`), and the rows the
  branch removed first, before the rows read, their last values struck through in the removed
  colour. A removed row is on the branch no more: it is not picked, edited or opened, and a
  grouped table shows none (it is in no section). The toolbar counts them after the row count,
  "+3 ~2 −1", with the kinds in words as its tooltip. The diff is read when the table is next
  drawn, as the rows are, and again when the rows are read at another commit (Refresh, a branch
  or schema move) or the table's own write moves its pin; a read that fails leaves the rows
  unmarked. On `main`, and at a commit, nothing is read.

### Filters and views

- Filter chips per column: equals, not equals, greater or less, contains, starts with, in
  list, is empty, is not empty; dates as Today, This week, Last 30 days, or a range, through
  `now()`. A node's free text with a full-text index also offers "matches words", the
  engine's `search($n.p, $q)`: the words in any order and form. The compiler refuses it on a
  property without the index (T27) and on every edge property, so it is offered only where
  the compiler's own reading of the schema finds one (`NodeType::has_full_text_index`: a
  one-column `@index` on the property or the type, or a key of one property; since
  2026-10-31). A preset
  holds no value of its own: a row edited, inserted or deleted is judged here against the days
  it stands for now, as the engine is asked (since 2026-10-21; before, every row was dimmed
  after an edit and counted as not matching).
- Relationship filters: "has any", "has none", "related to …" through `exists { }`, `not { }`,
  and a bound endpoint. Measure "has none" on a large type before shipping it (it was too
  slow on an older engine).
- A condition, anything the pills cannot say: the filter menu's last entry, "Condition (GQ)",
  takes a GQ condition on the row's variable (`$n`; `$e`, `$a` and `$b` in an edge table) and
  splices it into the match block in parentheses (`FilterOp::Expr`, key `@where`,
  `builder::expression_clause`): `or`, `not`, a further hop, two tests on one neighbour,
  anything the grammar has. Before it is applied, the page query with it is type-checked by
  the compiler against the schema (`TypeTable::check_condition`, `gq_language::check`), and
  a diagnostic is the draft's error, so nothing wrong is sent; the pill reads "Condition ·
  where · {text}" with the code icon, and a view saves it as any filter. The table does not
  re-judge rows against it after an edit (as with relationships): the engine judges it on the
  next read (since 2026-10-31).
- Every applied filter can be edited: a click on its pill takes it out of the bar into the
  draft with its key, test and value as they were (the list as typed, a related record by its
  id); ↵ applies the change, Esc or the draft's × puts it back as it was (since 2026-10-31).
- A neighbour's property, a nested search: each relationship in the menu opens a submenu, as
  the Group menu's do, with the relationship itself first and then the other type's filterable
  properties (chosen, "attended Person · name", key `out:Attended.name`; since 2026-11-01: listed
  after each relationship in the one menu, the personal graph's Person offered 1,122 entries,
  every one drawn at each frame the menu was open), with that
  property's own operators and offers, through `exists { $r: Person  $n attended $r  $r.name
  contains $f }` (`builder::neighbour_property_clause`, which grouping already used). The pill
  reads "attended name contains Aaron" with the direction's arrow. This is how "events Aaron
  attended" filters when several records are called Aaron: all of them, where "related to"
  binds one record (since 2026-10-31).
- The value field offers what it can take: an enum's values (or `true` and `false`), narrowed
  as text is typed with matches at the start first, and for "related to" the other type's
  nodes, searched on the server by id or label (`builder::node_search`, 8 at a time, at the
  table's commit, once typing pauses for 120 ms). ↑/↓ move through the offers (context
  `FilterValue > Editor`), ↵ or a click picks one; a single value adds the filter at once, an
  `in` list adds the value and waits for the next. With nothing typed ↵ adds the filter as it
  stands. A "related to" pill reads the picked node's label. The offers hide while the
  property or operator menu is open.
- A view is a name for a filter, sort, grouping and column state; saved per graph in Studio's
  settings: `graphs."<target>/<graph>".views` in `settings.json`, each with its type, filters
  (`FilterOp::key` names), sort, grouping, hidden columns and column order. The toolbar's
  Views menu lists the type's views with the shown one checked, saves the table's state under
  a name (the same name replaces), and deletes the shown one; its chip reads "Views" and the
  view's name while the table matches it, and its × leaves the view: no filter, order or
  grouping, every column in its first place (the view stays saved). Its "Name rows by ▸" submenu picks the property that names
  the type's nodes across the graph (spec 002, "Node names"), "Automatic" first. Filters, a sort or a grouping the schema no longer has are
  left out; a hidden or ordered column the schema no longer has is ignored.

### Columns

Built (`model::ColumnLayout`): the first column, the key or an edge's source, is pinned and
always shows first. Every other column can be hidden and moved. A header's right-click menu
sorts ascending or descending, groups by the column, starts a filter on it, moves it left or
right among the shown columns, hides it, and shows every column again once any is hidden or
moved. Dragging a header onto another moves its column into that place. The toolbar's Columns
menu lists every column but the pinned one, checked while it shows, and stays open while
columns are picked; its chip reads "Columns N hidden" while any is, and its × shows every
column in its first order. A column the schema adds later
appears after the ordered ones. Hiding only stops showing: a filter, a sort or a grouping on a
hidden column still applies, and a hidden relationship column reads nothing until it shows,
when its neighbours are read for the rows already loaded, 100 at a time, at the table's commit.
The layout is part of the tab and of saved views. Widths stay fitted to the column's type.

### The toolbar

Built: the toolbar's controls are spec 002's control chips, after the reference filter chips:
Views, Columns, Sort, Group, Filters and Row, each a raised pill with its icon and name. A set
control says what it is set to in the accent ("Sort name ↑", "Group status", "Filters 2") and
clears with its × after a hairline; an unset one is its button alone. The Sort chip's menu
lists the properties the rows can be ordered by (a header's menu orders by its column too),
then Ascending and Descending for the order set; its × returns the rows to their ids' order.
Group's × stops grouping; Filters' clears every filter (the bar under the toolbar keeps each
filter's own ×). Row is an action, with no state to clear.

### Grouping

Built: the toolbar's Group menu groups the rows by a property (text, enum, Bool, integer or
date; not the key, a list or a vector). One read counts the rows per value under the filters
(`builder::value_counts`: `return { $n.title as value, count($n) as n }`, by value, at most
200 values, rows without a value last); each value is a section with a header (value, count,
disclosure). An open section reads its first 10 rows when its line comes into view, then "Show
N more of M" (or "Show N more" when the page holds the rest) reads a page, all keyset pages narrowed with `title = $value` (or `is null`) and
pinned to the table's commit, in the table's sort. A closed section reads nothing. A section's
read whose line is more than a screen from the lines drawn is dropped, freeing its slot (a
connection has 8), and reads as many rows again when its line returns, a "Show more" page as a
first read (before 2026-10-21 a dropped "Show more" never read again); rows a read puts in
above the lines in view move the scroll with them, so those lines stay where they are (since
2026-10-21). Every read is engine-side, so the counts are right at any size. Edge tables group
by their own properties the same way.

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
  Built (bulk set): a row's checkbox picks it, as ⌘-click does, and ⇧ on either picks the
  rows between (since 2026-10-29). The box sits on the page's gutter at the start of the
  pinned key cell, the key's text as far after it as the gutter is wide (16 px either side); it
  shows under the pointer, and on every row while any is picked, so a table nobody picks from
  stays plain. A click on it moves no
  cursor and opens nothing. The key column's header holds a select-all: it picks every loaded
  row (a grouped row once), shows a dash while some are picked and, with all picked, takes
  them out. With one row picked or more (two or more, before checkboxes), a bar says how many,
  offers Delete… (the dialog below) and sets one property on all of them as one mutation of
  one `update` per row
  (`builder::update_cells`), each bound to the value its row shows, so one commit. Rows that
  already hold the value are left out. A short `affected_nodes` means some rows changed
  elsewhere: they were left alone, the bar says how many and the table reads the head. Esc or
  Clear drops the selection; a set already sent stays out, Set stays disabled until it is back,
  and its outcome shows on the bar, or clears the pick, only while its rows are the ones picked
  (since 2026-10-21; before, Esc enabled Set again and the outcome landed on a later pick).
  Built (drafts): ⌥↵ keeps an edit as a draft instead of saving it, as clicking away already
  did; a draft cell shows the typed text with a dot, and its tooltip says what the cell holds.
  While drafts exist a pill floats over the bottom of the table ("3 drafts not saved ·
  Discard · Save as one commit"; it floats so no row moves when it appears). Save (or ⌘S)
  sends one mutation of one `update` per row (`builder::update_rows`) setting every drafted
  property, bound to every value the row shows, so one commit; a draft that does not parse
  opens instead and nothing is sent. A short `affected_nodes` means some rows changed
  elsewhere and were left alone whole: those rows are read at the head, the drafts that landed
  clear, and the others stay with "Changed elsewhere to “X”". A refused or lost write puts
  every draft back with the reason. A list draft on a row deleted elsewhere fails only itself
  ("This row was deleted elsewhere"), and the others are saved (since 2026-10-21; before, it
  marked the whole batch changed, and every save sent nothing until Refresh). Discard asks for
  a second click, and drops the drafts the pill counts: one whose save is out stays until the
  save settles it (since 2026-10-21; before, a refused save then lost its text). Each edit has
  an editor of its own, so ⌘Z never brings back the text of the cell edited before.
- Insert: a form generated from the node shape (required properties, key, enums, ranges).
- Delete (⌘⌫, or Delete… on the bulk bar): the picked rows (checkboxes, ⌘-click, ⇧-click), or
  else the cursor's, in one commit bound
  to the table's pin (`/mutate/if-graph-commit`). Before it is offered, the dialog reads what
  else goes (`graph_store::deletes`): a node's relationships go with it, and a node that needs a
  relationship to a deleted node (`@card(n..)`) and would be left with fewer than `n` is a
  dependent, deleted in the same commit, because the server refuses a delete that leaves it so.
  Dependents of dependents follow, round by round (`builder::required_dependents`, one read per
  required relationship into a type that grew, held at the pin), up to 1,000 and 32 rounds;
  more refuses, saying which (a chain whose every round finds more goes "at least 32 steps
  deep"; before 2026-10-21 such a chain was refused as more than 1,000 nodes), and points to
  the query editor. The dialog lists them by type and name with the relationship each
  needs ("Also deletes 1 Person, 1 Badge that cannot exist without it"), and ↵ deletes. The
  outcome's toast offers Undo (`studio_graph::UndoDelete`): it reads the delete commit's changes,
  inserts every node and then every relationship again in one commit bound to the delete's
  commit, so it applies only while the branch is still there; unkeyed relationships come back
  under new ids. When Undo could not restore it (a node type without `@key`, or Blob or Vector
  values on the nodes or their relationships, judged from the schema; or more than the 20,000
  nodes and relationships Undo puts back in one commit, counted at the pin with one
  `builder::edge_count` per relationship type and deleted end, less those both ends lose, since
  2026-10-21: before, such a delete offered an Undo that restored nothing), the dialog says why
  and asks for the key, or `delete N` for several rows, to be typed (constitution IV). An edge
  delete takes no dependents; deleting a relationship its source needs is refused by the
  server, which the dialog shows. The dialog deletes the kind its table shows, as a node type
  and an edge type may share a name (since 2026-10-21; before, it took the edge). More than
  20,000 picked relationships, past what Undo puts back in one commit, are confirmed by typing
  `delete N`, with no Undo (since 2026-10-21).
- An edit open when the table moves to another branch (a link, a toast's follow-up), when its
  tab closes, or when the Nodes panel lets its table go, is kept as the cell's draft on the
  branch it was typed on, as leaving the cell keeps it; a tab holding such an edit asks before
  it closes (since 2026-10-21; before, the text was lost).
- Every editing surface shows `write · <branch>`. A 403 shows "Your token cannot change
  <branch>" and keeps the draft.

### Outcomes

`ChangeOutput.commit` present: committed, when its kind is `published`. A commit on no branch,
a proposal or a snapshot, is not a save: "main did not change" with the commit, the draft kept;
a kind Studio does not know is an unknown outcome (both judged by `OutcomeKind::written`, for
cell saves, drafts, bulk sets and Blob writes, since 2026-10-21). A 4xx with `ErrorOutput`:
refused, "Nothing was written." plus the reason (a 401 says what became of the token, as
every write site does: refused, never received, or none sent). A write never sent (its
connection retired, no connection to the server opened, or the request not made): "Not sent:"
and why, "Nothing was written." (since 2026-10-30). A lost response, a timeout or a response
without the contract header: unknown, "Check History before trying again; your changes are
still here", with a button that opens the branch's recent commits.

### Drawing cost

The table is a cached view (spec 002): it is drawn again only when it notifies. When it is, the
row callback works out how each drawn line looks (picked, dimmed, the cursor's, a group
header) for the lines it draws, not for every loaded row; a date's or time's relative text is
made once a minute with the cell's other text; relationship and Blob cells name their elements
by their place and make their tooltips only when shown; and the grouped line list is built
once a frame. Blob cells are read once scrolling has paused for 120 ms, for the rows the lines
then drawn show and the Blob columns drawn (grouped, a folded section's rows are on no line;
since 2026-10-21, before which the span from the first row drawn to the last was read, and
every Blob column). A section folded or opened puts other rows on the lines drawn, as rows put
in or taken out do, and they are read (since 2026-10-21; before, they showed a grey "Blob"
and Choose File refused until a scroll); reads still out for cells scrolled away, up, down or
sideways, are dropped and asked again when they return. Relationship cells read the rows of the
lines near those drawn alike. Moving through a filter's offered values notifies its field, which
draws the table around it and leaves the grid as it was (since 2026-10-21). A
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
| One offer list (`offers.rs`) for the filter's value field, the edge dialog's endpoints and, since 2026-10-21, the peek's add-relationship search | Zed's editor completion menu (`CompletionProvider`), as the query editor uses | The menu draws code completions in the editor's monospace font and opens on typing; these fields offer names with their ids muted beside them, in the interface's type, and an enum's values as soon as the field has focus. The two hand-built copies became one: the list, ↑/↓ around the ends, the rows and the pick on press |
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
| 2026-11-01 | On a branch the rows the branch removed show first, struck and not for picking or editing, and the rows it added or changed are marked, a changed row by its changed cells | `type_table` `a_branch_shows_what_it_changed_in_the_rows` (a table on main, then on fix with p1 added, p2's name changed and p3 removed, then back on main), `branch_diff` `marks_the_types_rows_by_what_the_branch_did`, `a_removed_edge_keeps_its_ends`, `a_row_that_ends_where_it_began_is_not_marked` | Pass. On main nothing is read for it; on fix the rows read p3, p1, p2, select-all picked p1 and p2, Enter on p3 opened nothing; back on main p3 went. Not drawn by a test: the wash, the gutter bar and the strikethrough |
| 2026-10-31 | A condition filter is spliced in as written and judged by the compiler before a read | `gq_language` `a_condition_is_spliced_in_and_judged_by_the_compiler` (an `or … not exists { }` condition type-checks beside a pill's clause; `$n.nope = 1` is a diagnostic; an empty one is refused by the builder) | Pass. The pill's edit-on-click and the draft's compile error are not drawn by a test |
| 2026-10-31 | "Matches words" is offered where the compiler takes `search`, and only there | `gq_language` `every_offered_filter_op_compiles` (extended: edge properties too, and a schema with a composite key, `@index(title)` on the type, `summary: String @index`, a lone key and `note: String @index` on an edge), `text_filters_start_with_search_and_reach_neighbours` (extended: offered on `title`, `summary` and the lone key, not on a composite key's member or an unindexed property). Each fails under the earlier rule (checked) | Fixed. The first rule marked every key member and every property `@index` as searchable, so a composite key's members and an edge's indexed property offered an operator whose query failed to compile, and a one-column index declared on the type was not offered. Each property now asks the compiler's catalog, which Studio already builds |
| 2026-11-01 | The filter menu nests each relationship's neighbour properties, its choices are built once with the columns, and a schema change keeps every filter it can still test | `type_table` `the_filter_menu_nests_each_relationships_neighbour_properties` (new), `a_schema_changed_elsewhere_waits_for_refresh` (extended: a column, relationship, neighbour-property and condition filter stay under a schema that adds a type; under one that drops the name, the two on it go and the menus no longer offer it). Each fails with its part of the fix undone (checked) | Fixed. The flat menu listed Person's 1,122 keys on the personal graph, and the toolbar rebuilt them at every draw while a filter was written; they now come from the cache the Group menu's choices use. A schema change dropped neighbour-property and condition filters, whose keys are neither a column nor a relationship |
| 2026-10-31 | A filter tests a neighbour's property, text starts-with, and full-text search where the property is indexed | `gq_language` `text_filters_start_with_search_and_reach_neighbours` (a schema with `name: String @index` and `bio` without: Search offered for one, not the other; `starts_with`, `search()` and `search($r0.name …)` through `out:Knows.name` type-check), `every_offered_filter_op_compiles`; live on the personal graph with the CLI: `starts_with`, `search($p.name, …)` and an `or` group answer, `fuzzy` answers loosely; `cargo test -p type_table` | Pass. `fuzzy()` and `or` groups are not offered yet |
| 2026-10-29 | A table's rows do not paint as one scene layer | `type_table` `instrument_a_frame_of_a_full_table` (ignored, `instrument:`; 200 rows of 20 text columns, a frame with the grid replayed from its cache, the mean of 600), three runs each with the grid wrapped in `Window::paint_layer` and without; GPUI's `Scene` batching read at Zed v1.23.2 | Without, 9.72, 9.64 and 9.61 ms a frame; with, 8.86, 8.85 and 8.78 (two columns: 3.25 against 3.08). Inside a layer every quad takes one draw order and skips the bounds-tree search, but at one order GPUI draws every shadow before every quad, so a ring drawn as a shadow (the design system's, an unchecked row's checkbox) goes under the cursor row's fill, and every shadowed piece put in a cell later would too. Not adopted: the rows keep the bounds tree, and the instrument stays for the next frame-cost change |
| 2026-10-29 | Rows are picked by checkbox; the bar comes with the first and deletes | `type_table` `checkboxes_pick_rows_and_the_header_selects_all` (drawn and clicked: the select-all picks both rows, a row's box takes the first out and leaves a dash and the cursor where it was, one row has the bar, the select-all picks the rest and then takes all out); the workspace suite; the relaunched app's Claim table | Pass. Before, rows were picked only by ⌘-click and ⇧-click on a cell, which nothing on screen told, and the bar waited for a second row |
| 2026-10-29 | A table not drawn reads no rows | `type_table` `a_table_not_drawn_reads_no_rows` (a table in a window that draws a blank root, then the branch moved: no page read; another drawn: rows read); every other table test, each drawn in its window | Pass. Before, every table restored behind another tab read its first page at start, and again at every branch or schema move; it now lets go of what it showed at once and reads when next drawn. A sort, a filter or Refresh, all asked in view, still read at once |
| 2026-10-24 | A wheel over the rows keeps its gesture's axis, works after a key, and leaves the open cell editor its wheel | `type_table` `a_sideways_wheel_scrolls_the_columns_not_the_rows`, extended: a key before the wheel; gestures sent as a trackpad sends them (−30 then −10/−15 px; 0/−40 then −30/−20; 0/−10 then −4/0); −60 px over the open editor of a 360-character name; each assertion run with its fix undone | Columns −160 px with the rows still, then rows −60 with the columns still; a −4 px sideways jitter of a vertical gesture moved nothing. Undone: after a key the wheel moved the rows −120 (`is_hovered`); without the axis held, rows −15 in a sideways gesture; without the jitter rule, rows −74; without the editor check, the columns moved −60 and the editor not at all. Notifying the grid's view rather than refreshing the window passes either way here, the table being the window's only cached view; by reading GPUI (`view.rs` skips a cache while `refreshing`, `window.rs` `mark_view_dirty`), a refresh draws every view in the window again, a notify the grid and the views holding it |
| 2026-10-22 | A sideways swipe over the rows moves the columns, and they stay | `type_table` `a_sideways_wheel_scrolls_the_columns_not_the_rows` (60 people and 20 relationship columns: a wheel of −120 px sideways over a row, then a draw); offscreen snapshots of Person in the dev graph with 20 frames of a −40 px sideways wheel, and 10 of a vertical one, before and after | Two causes. GPUI gives a vertical-only scroll area a purely sideways delta as a vertical one, and Zed's row list has no axis lock, so the rows moved down; a capture-phase listener over the rows now takes a swipe more sideways than down, moves the shared horizontal handle within its range and stops it. And the header's strip held its columns in a block child stretched to the strip, so it measured nothing to scroll and clamped the shared offset, every row's, back to 0 on each draw; it is a flex container now, as Zed's row strips are. After: the header and rows moved together to `contributes`, `knows`, `worksAt` with person-000000 still first; a vertical swipe still moved only the rows. The test fails with either change undone |
| 2026-10-22 | A table pinned at the commit the session's snapshot counts sends no count read of every row | `type_table` `a_table_at_the_commit_the_snapshot_counts_reads_no_count` (`/snapshot` counting 70,702 Person at manifest version 1, the newest commit c1 at 1: a table at the head, one held at c1; another writer's c2 at 2, with 70,703, seen by a head check; Refresh; a filter by name), failing with the table's count read sent whatever the counts' commit; `a_tables_snapshot_count_stands_in_until_the_read_at_its_pin`, extended (the session's read of the newest commit held with its first head read, so it answers c2 at 2 against a snapshot at 1), failing with the versions not compared | No `studio_count` read at c1, nor for the table held there, nor after the Refresh at c2, which counted 70,703 from the second snapshot; filtered, the count through the filter alone went out, at c2, with 70,703 its total. One `limit=1` read of the newest commit for each snapshot, and a branch list's `head_commit` took the first rather than asking again. With c2 at 2 against a snapshot at 1 the counts name no commit (`GraphSession::counts_commit`), and the table stands the snapshot's 70,702 in and reads at its pin as before. Undone, the table at c1 sent its count read; with the versions not compared, the counts named c2. Before, every table sent the count read at its pin (106 ms on mr-cluster's 70,702-row type, measured before); the commit read that replaces it once per snapshot is not measured here |
| 2026-10-22 | An unfiltered table at the head shows the session's snapshot count with its first page, until the count read at its pin replaces it | `type_table` `a_tables_snapshot_count_stands_in_until_the_read_at_its_pin` (`/snapshot` counting 70,702 Person at c1; another writer's Person at c2 landing while the session's first head read is held, so the session holds c2 and a snapshot of c1; the table's count read held, then let go; Refresh; a filter by name; a table held at c1 with its count read held), failing with the snapshot's count taken instead of read | 70,702 showed with the first page at c2 while the read at c2 was out, 70,703 once it landed, and 70,703 again after Refresh; filtered, 1 of 70,703; the held table showed none until its read at c1 gave 70,702. Taken instead of read, no count read went out and 70,702 stayed at c2: the session reads no snapshot again for its first head, nor on a table's Refresh. The snapshot cannot be matched to the pin: the session drops `SnapshotOutput.graph_manifest_version`, and a read's answer names no manifest version (only `CommitOutput` does), so the read at the pin still goes out (106 ms on mr-cluster's 70,702-row type, measured before); what it saves is the "counting…" after the first page |
| 2026-10-22 | A page's relationship counts and neighbours go out together, the counts showing first | `type_table` `relationship_counts_show_before_the_neighbours_asked_for_with_them` (both reads held, then the counts let go, then the neighbours), `a_row_a_hub_left_bare_is_read_again_sized_by_its_count` (p1 with 5,000, whose 2,000 fill the first read, then p2 with 3), each failing against the committed reads and the second also with the read for the rows cut short undone | The neighbour read of both rows went out while the counts were held; the counts showed alone once in, the chips once the neighbours were, and no second read. With the hub, p2 was read again alone with `limit 3`, showed three chips, and p1 its count alone. Before, the neighbours were asked for only once the counts were in, and neither showed until both were: on mr-cluster 769 ms then 852 ms after each page, measured before. The first read asks for up to 2,000 neighbours of every row of the page, so a page with hubs costs one read more than before |
| 2026-10-22 | The next page is read a page ahead of the view | `type_table` `the_next_page_is_read_a_page_ahead_and_dropped_when_the_order_changes` (1,000 people, 100 a page: opened, scrolled to line 110 with the third page held, then ordered by name), `model` `the_next_page_is_read_a_page_ahead_of_the_view`, both failing with the 80% rule back | The second page went out as the first landed, and no third until the view reached line 110; ordered by name while the third was out, it was dropped (no read left running), and the rows were read again from the start, the second sorted page at once. Before, the next page went out only once the view passed 80% of the rows read, which on mr-cluster (3.3–4.1 s a page, measured before) left a scroll waiting at the end. The rows past the view stay within two pages |
| 2026-10-22 | A page read from the start names no row to come after | `gq_language` `node_pages_compile_with_and_without_filters`, `sorted_pages_read_values_then_nulls_and_compile`, `a_page_without_a_property_sorts_and_filters_as_if_no_row_held_it`, `edge_pages_and_counts_compile` and `relationship_queries_compile_in_both_directions`, extended: the first page of a node type (filtered, sorted, and as fields at an older commit), of an edge type and of a node's neighbours, and the rows without a value from their start; each failing with `@id > ""` sent | No `@id >` clause and no `after` parameter, the order and limit unchanged, and each type-checks; a later page keeps `@id > $after` (with a sort, inside the keyset pair; for the peek, with the edge). A section reads its first page through the same builder. Measured through the CLI on a type of 70,702 rows: 4.09 s with `$n.@id > ""`, 0.69 s without, 3.0 s for the ids alone with it, 0.61 s after cursor `"m"`. Before, every first page, section and neighbour page sent `@id > ""` |
| 2026-10-22 | A Blob tab on a branch made after the session's list was read is not refused as gone | `type_table` `a_restored_tab_writes_only_to_the_branch_it_was_saved_on`, extended: the session's list set to main alone, as read before fix was made, then a tab saved on fix restored; failing with the list's answer taken as it is held | Choose File and Clear were not refused while the list was read again, nor after it came back holding fix, and the file went to fix. `gone` is still refused once the list read again lacks it, and nothing is sent. Undone, fix read "fix no longer exists" and nothing was read to correct it |
| 2026-10-22 | A dropped "Show more" is the read Show more sends | `type_table` `a_show_more_dropped_away_from_the_view_reads_again_when_its_line_returns`, rewritten: `load_section` sends the page, as the button does, and the view scrolls to the end in the same turn, so the frame drawn as it ends drops the read before it is answered (30 seeds) | Dropped with its 100 rows noted, nothing read while away, one `limit 100` read for its value back at the top. Before, the test built the read and the section's state by hand, so a change to what `load_section` notes would not have failed it |
| 2026-10-22 | A table's upload is not refused, on either branch | `type_table` `a_tables_upload_is_bound_where_its_values_hold_as_it_goes`: the outcome its toast tells is now checked, a commit on main, beside the cell's notice | Before, the check was the cell's notice alone, which a table moved to fix clears whatever the outcome, so it could not fail there |
| 2026-10-21 | Folding a section reads the cells of the rows it moves into the lines drawn | `type_table` `folding_a_section_reads_the_cells_of_the_rows_it_brings_into_view` (two open sections of 300 rows, a Blob column and two relationship columns; the first folded), failing with the fold's reset undone | The lines drawn were the same before and after; the second section's first row had its Blob and relationships read. Undone, neither was |
| 2026-10-21 | A read out when the pin moves to a head that changed nothing it reads lands | `type_table` `a_read_out_when_a_commit_to_another_type_moves_the_pin_lands` (another writer's Note at c2 while a Person page is read at c1), failing with every pin move counted as the table's own; `a_read_sent_before_the_tables_own_write_landed_is_read_again_at_its_commit`, extended (one read sent again) | The page landed once, the pin at c2, one request. Undone, the table sent it again, which the session answered without a request from c1's kept answer (`GraphSession::read`, carry-forward), so the test counts the table's sends: the request is sent again only where the session no longer keeps that answer or cannot tell every commit since |
| 2026-10-21 | A "Show more" dropped away from the view reads again when its line returns | `type_table` `a_show_more_dropped_away_from_the_view_reads_again_when_its_line_returns` (section 0 with 2 of 12 rows and a page out, 150 folded sections after it, scrolled to the end and back), failing with the dropped read's size undone | Dropped at the end with its 100 rows noted; back at the top, one read of `limit 100` for its value. Undone, nothing was read: only a section with no rows read again |
| 2026-10-21 | A restored Blob tab writes to the branch it was saved on | `type_table` `a_restored_tab_writes_only_to_the_branch_it_was_saved_on` (saved on main, restored with the window on fix; saved on `gone`; saved without its branch), `a_restored_tab_reads_and_binds_where_its_own_write_left_it` (now through `saved_blob` and `blob_view`, as the workspace restores), `a_tab_from_a_view_at_the_head_stores_a_file_bound_to_what_it_read`, extended (one key per branch, a held tab's alike) | The file went to main bound to the ETag read there; `gone` read "gone no longer exists" on Choose File and Clear and sent nothing; a writing tab saved without its branch did not come back, a held one did |
| 2026-10-21 | A table's upload is bound as it goes, through the table | `type_table` `a_tables_upload_is_bound_where_its_values_hold_as_it_goes`, rewritten to upload: the table's insert of p3 with a Blob value lands on its pin while p1's file is read (every ETag of the type moved), then, in a second table, the window moves to fix instead; failing with the binding taken when the file was chosen | The first carried p1's ETag at the insert's commit and stored the file; the second went to main bound to what the cell showed when chosen. Bound when chosen, the first carried `"e1"`, read at c1, while p1 held `"e3"`: a 412. The Blob tab's test still sets its state by hand: no call of the tab's reaches it (its note says why) |
| 2026-10-21 | The peek and the Blob tab bind an upload as it is sent | `type_table` `a_slot_upload_is_bound_to_what_the_view_shows_as_it_is_sent` (the peek's table saves p1's `scan` at c2 while a file for `doc` is read), `a_tabs_upload_is_bound_to_what_the_tab_shows_as_it_is_sent` (the tab comes to show a save of its cell at c2 while the file is read), each failing with the binding taken when the file was chosen | Both writes carried the ETag read at c2 and stored the file at c3. Bound when chosen, each sent the ETag read at c1 (the peek `"e2"`, the tab `"e1"`), which the server refuses with a 412, though the value each would replace was the one it showed then. `blob_write::upload`, which bound when chosen, is gone. The tab sends one write at a time and follows no other view's, so its test moves it to c2 itself |
| 2026-10-21 | A delete chain deeper than the rounds followed says so | `graph_store` `a_chain_deeper_than_the_rounds_followed_is_refused_for_its_depth` (`Link` needing the one before, 40 and 31 long), failing with the round limit's old `TooMany` | 40 long: 32 rounds read, `PlanError::TooDeep`, "at least 32 steps deep", no "1,000"; 31 long: planned with 31 dependents, the 32nd round finding none. The dialog shows the plan error's own sentence, the read failure's "Studio could not read what depends on it" included. The edge delete's Undo cap is `deletes::UNDO_CAP`, the constant Undo reads to |
| 2026-10-21 | The toolbar's controls are chips that say what they are set to and clear | `type_table` tests (179) unchanged; `studio_ui` `property_and_control_parts_draw_at_their_sizes` (a chip 26 px, its button centred); offscreen snapshots of Person (1440×900 at 2×, a scratch HOME so no config toast covers the toolbar): at rest, the Sort menu open, "name" chosen, then its × | At rest six pills, Views to Row, and Refresh. The Sort menu listed name, title, email and born, with Ascending and Descending disabled until an order was set; choosing name read "Sort name ↑ ×", put the header's arrow on name and ordered the rows from Ada Allen; the × read "Sort" again and the rows returned to id order. A ↓ icon beside "name ↑" contradicted it, so Sort's icon is ⇕ |
| 2026-10-21 | A read sent before the table's own write landed is read again at the write's commit | `type_table` `a_read_sent_before_the_tables_own_write_landed_is_read_again_at_its_commit` (a header click reads the first page at c1 while the table's delete of p1 lands at c2; grouped, a section's page read at c2 while p2's delete lands at c3), failing with the pin check undone | Both read again at the new pin: p1 does not come back, nor p2. Undone, the c1 page showed p1 again under c2 |
| 2026-10-21 | A sorted table whose rows lack the sort's value reads them | `type_table` `a_sorted_table_whose_rows_hold_no_value_reads_them_at_once` ("email is empty", sorted by email), failing with the read-on undone | p1 and p2 read, the list exhausted. Undone, no row showed while the count said 2 |
| 2026-10-21 | ⌘Z in a cell takes nothing from the cell edited before | `type_table` `undo_in_a_cell_never_brings_back_another_cells_text` ("Ada L" typed in p1's name and saved, then p2's name opened, ⌘Z, ↵), failing with one editor kept across edits | p2's name stayed "Grace" and one write went, p1's. With the shared editor, ⌘Z brought back "Ada L" |
| 2026-10-21 | The delete dialog deletes the kind the table shows | `type_table` `a_delete_takes_the_kind_its_table_shows` (`node Review` and `edge Review` in one schema) | Opened from the node table it reads the dependents; from the edge table, a relationship's delete. Before, it took the edge either way. Not done here: `table_key` and `OpenType` name a type without its kind, so an edge sharing a node's name opens the node's table (`type_table.rs`, `studio_actions`) |
| 2026-10-21 | Discard leaves a draft whose save is out | `type_table` `discard_leaves_a_draft_whose_save_is_out` (two drafts, one confirmed and refused with a 403 while Discard is clicked twice) | The other draft went; the confirmed one stayed in the session while out and kept "Ada King" once refused |
| 2026-10-21 | Esc leaves a bulk set out | `type_table` `esc_while_a_bulk_set_is_out_leaves_it_out` (Esc, two rows picked again, Set, one row picked, then the commit lands) | Set sent nothing more; the commit left the pick of p2 and no notice, and the rows showed the value set |
| 2026-10-21 | A list draft on a row deleted elsewhere fails only its own part | `type_table` `a_node_gone_elsewhere_fails_only_its_own_part_of_a_list_write` (a bulk set of two rows' tags, one gone at the head; then the gone one alone), failing with the old `holds` | The write went bound to the head (c5); alone, nothing was sent. Before, the whole batch was refused as changed |
| 2026-10-21 | A frame reads the cells of the rows and columns it draws | `type_table` `a_grouped_frame_reads_only_the_rows_its_lines_show` (sections of 3, 50 folded and 3 rows, a Blob column, two relationship columns), `blob_cells_are_read_in_the_columns_drawn` (a Blob column after 20 text columns), each failing with its fix undone | Six Blob HEADs and six rows per relationship read; before, all 56. The Blob column read nothing until scrolled into view |
| 2026-10-21 | Section reads away from the view are dropped, and rows above it move nothing | `type_table` `section_reads_away_from_the_view_are_dropped_and_rows_above_it_move_nothing` (40 sections, scrolled to section 20's header) | Section 0's read dropped, section 21's kept; section 0's two rows landing kept section 20's header first in view. Without the scroll follow, section 19's read line was |
| 2026-10-21 | Moving through the filter's offers leaves the grid as it was | `type_table` `typing_in_a_field_leaves_the_grid_as_it_was`, extended (a Bool filter's two offers, ↓), failing with the table notified | The table drew again and the grid did not |
| 2026-10-21 | An upload is bound as it is sent | `type_table` `an_upload_is_bound_to_what_the_view_shows_as_it_is_sent` (the view's own value of the same size stored while the file is read), failing with the binding taken before the read; `a_tables_upload_is_bound_where_its_values_hold_as_it_goes` | The write carried the new ETag and stored the file; the table's binding followed its pin to its own commit, and gave none for another branch. Bound before the read, the write was refused as changed. The peek and the Blob tab still bind when the file is chosen (`blob_write::upload`) |
| 2026-10-21 | A commit on no branch is not a save | `type_table` `a_commit_on_no_branch_is_not_a_save` (`unpublished`, `unattached`, an unknown kind, `published`), `a_blob_write_held_off_its_branch_is_not_saved`, each failing with every commit taken as published | A proposal or a snapshot reads "main did not change" and keeps the draft; an unknown kind is an unknown outcome; a published one saves |
| 2026-10-21 | More relationships than Undo puts back are typed for | `type_table` `an_edge_delete_past_what_undo_puts_back_is_confirmed_by_typing` (20,000 and 20,001 picked `Knows`), failing with the count undone | 20,000 confirm with ↵; 20,001 say why and ask for `delete 20001` |
| 2026-10-21 | The peek names a saved value by the bytes that judge its kind | `type_table` `a_slot_names_a_saved_value_by_the_bytes_that_judge_its_kind` (an SVG whose root follows an XML declaration and a comment) | The slot kept its first `blob::KIND_PREFIX` bytes, here the whole value, and Save offered `p1-doc.svg`; it kept 16 before, which named it `.txt` |
| 2026-10-21 | A peek's Save reads the value it shows where it is held | `type_table` `a_value_stored_from_a_view_behind_is_saved_from_where_it_is_held` (a page at c2 holding "first" stores "second" past another writer's c3, then saves; a peek of a table whose cell holds at `BlobsAt::Unknown`), failing with Save read at the view's commit | Saved 6 bytes "second", read at the write's commit c4; the table's value disabled Save, "Studio knows no commit that holds this value", and read nothing. Undone: "The value ended before its size; nothing was saved.", c2 holding the 5 bytes replaced |
| 2026-10-21 | A peek from a table behind its branch compares the head as it opens | `type_table` `a_peek_of_a_table_behind_its_branch_compares_the_head_as_it_opens` (another writer changes p1 at c2; the table stays at c1; p1 and p2 peeked), failing with the comparison undone | p1's peek read at c1 offering Refresh; p2's moved to c2. Undone, p1's showed c1's values as current |
| 2026-10-21 | A field saved after its node was read again shows the value written | `type_table` `a_field_saved_after_its_node_was_read_again_shows_the_value_written` (a Refresh read "Ada" at c1, then the save landed at c2 on c1; again, landing past another commit), failing with the value not put back | "Ada Lovelace" at c2; behind, "Ada" at c1. Undone, "Ada" at c2 |
| 2026-10-21 | A draft opened in the text editor stays bound to what it was typed over | `type_table` `a_draft_opened_here_is_bound_to_the_value_it_was_typed_over` (the draft typed over "Ada Byron", the read at c1 finding "Ada"), failing with the read value bound | The editor bound "Ada Byron" and its write sent it as `old`, so a change made since is a conflict. The text editor finds the value in the session's draft, which the peek writes before ⌘⇧↵ opens it, so `OpenTextEditor` carries no copy of it |
| 2026-10-21 | A relationship replace is two commits, each told as it ended | Omnigraph `crates/omnigraph/src/exec/mutation.rs` `enforce_no_mixed_destructive_constructive` at `7fd9cc9` (the edge build Studio needs) refuses a mutation holding a delete and an insert (spec 014, E25); `type_table` `a_replaced_relationship_follows_both_commits_and_reports_a_half` (the addition committed, refused, its answer lost; the removal's answer lost), `a_relationship_that_needs_its_one_neighbour_is_not_replaced_and_says_why` (`@card(1..1)`), `a_relationship_written_to_no_branch_is_told_as_it_is` (`unpublished`, `unattached` and an unknown kind) | A lost addition says the old one was removed and whether the new one was added is not known, with Open history; a lost removal sends no addition. A `@card(1..1)` group's "+" is disabled with why, and a choice sends nothing. A held commit or one of an unknown kind leaves the peek at c1 and is said as the session logs it (`OutcomeKind::written`). One mutation of both statements was not built: the server refuses it |
| 2026-10-21 | The peek's add search is the shared offer list | `type_table` `enter_waits_for_the_results_of_what_is_typed` (↑ from the first offer takes the last, ↓ the first), `a_refused_relationship_says_why_on_its_group`, `a_replace_waits_for_the_edge_it_replaces`; `groups_at_a_commit_before_their_edge_or_its_property_existed`, extended (a held table's peek opens a counted group whose edge property is newer than the commit) | The search keeps `offers::Offers` and draws its rows; ↑/↓ now go around the ends as in the filter field. A group opened with a table's count is read as a whole group is, so at an older commit it reads again without the newer property instead of failing |
| 2026-10-21 | A delete larger than Undo puts back is confirmed by typing | `graph_store` `a_delete_past_what_undo_puts_back_is_planned_without_undo` (⌘⌫ on a Tag on 25,000 documents; the same on ten; an unkeyed Note), `relationships_between_deleted_nodes_count_once` (the counts per relationship type and end, each type-checked); the first run again with the count undone; `dialogs.rs` read: a plan's `undo_unavailable` already asks for the key and leaves Undo off the toast | The Tag's plan named 25,005 changes (1 node, 25,000 Tagged, Broader 3 + 2 − 1) past 20,000, four count reads all held at c1; ten documents offered Undo; the Note read no count. Undone, the plan offered Undo, which then restored nothing. `DeletePlan.requested`, which nothing read, is gone |
| 2026-10-21 | A Blob tab opened at the head reads and saves one commit | `type_table` `a_tab_opened_at_the_head_reads_and_saves_at_the_head_it_found` (a 16 MiB + 10 value at c1, main moving to c2 with other bytes once the first range is served, then the window moved to `fix` and the value saved), failing with every read at the branch again | The tab read the head probe's c1, its HEAD, both ranges and both saved ranges at `snapshot=c1` and none at a branch; the file held c1's bytes whole and the footer read "at c1". Before, the second range was c2's and the save read fix's ten bytes |
| 2026-10-21 | A restored Blob tab reads where its own write left it | `type_table` `a_restored_tab_reads_and_binds_where_its_own_write_left_it` (a tab at c2 stores a file at c3; its saved form read back through `saved_blob` and opened), failing with the commit it opened at saved | Saved `at_commit` c3, `held` false; the restored tab showed the 3 bytes stored with their ETag, and its next file landed at c4 bound to it. Before, it came back at c2 with the value replaced and its write was refused (412). A saved form without `held` opens nothing |
| 2026-10-21 | The peek, the Blob tab and a saved name tell a value alike | `type_table` `the_peek_the_tab_and_a_saved_name_tell_a_value_alike` (a PNG, an SVG past its declaration, a comment and a doctype, a page holding an icon's markup, a one-page PDF, text, bytes), `blob` `an_svg_is_told_by_its_first_element` | One `blob::value_kind`: the SVG drew in both and named `.svg`, the page was text in both; the PDF drew its page in the tab and its first page in the peek and named `.pdf`; PNG, text and bytes agreed. The peek names a saved file from the first 16 bytes it keeps (`peek.rs`), too few to see an SVG's root past its declaration |
| 2026-10-21 | The grid sits flush in the document and ends at its last column | Offscreen snapshots (1440×900 at 2×) against a scratch dev server, light and dark, before and after: Person, Project, Company, and Person at 2400×700 | Before, the grid was a card inset 16 px in the page, and the thin strip after `tags` was the next column's (`contributes`) first 12 px, its cell inset, cut at the card's edge. After, the header and rows start at the document's edge under the toolbar, the key's text at the toolbar title's 16 px, every column but the last ruled on its right; Project, narrower than the page, ends at its last column with no fill or hairline past it |
| 2026-10-21 | Which of the table's writes tell its peek | `table.rs` `tell_peek_wrote`'s callers: a saved cell, drafts saved together, a bulk set, `row_inserted`, a Blob stored or cleared; not `rows_deleted`, nor `undo_delete`, which writes through `GraphSession::write_on` | The doc said the peek hears every write; it now names these and says why a delete and its Undo are not told (they change relationships, which `TableWrote` cannot carry) and what the peek does instead |
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
| 2026-10-17 | A delete takes its dependents, says so, and can be undone | `gq_language` `deletes_take_many_ids_and_find_the_sources_that_need_them`; `graph_store` `dependents_follow_the_chain_of_required_relationships` (company → person → badge, every read held at the pin, an optional relationship never read), `undo_is_known_before_the_delete`, `a_plan_deletes_every_type_in_one_mutation`; `studio_workspace` `a_delete_that_can_be_put_back_offers_undo`; on a scratch Omnigraph 0.13 graph (`WorksAt @card(1..)`, `Holds @card(1..1)`): `delete Company` alone, then the planned delete, then the undo as one insert mutation; offscreen snapshots: Company, ⌘⌫ on Acme, ↵, then Undo, with CLI reads after each | Alone: refused, "@card violation on edge WorksAt: source 'ada' has 0 edges (min 1)". The dialog listed Ada (needs WorksAt) and b-ada (needs Holds); ↵ deleted 3 nodes and 4 relationships in one commit, Bob kept his Beta job, and the one toast read "Delete Company Acme and 2 that needed them · committed to main" with Undo. Undo committed once more and the CLI read every node and all four `WorksAt` back; the table showed "Newer data · Refresh" |
| 2026-10-14 | A wider grid draws the columns its new width shows in the frame that widens it | `type_table` `rows_draw_only_the_columns_in_view` extended: the window widened from about 1,730 px of scrolled columns to 5,810, then the columns the rows drew read in that frame, with the fix that asked for a second frame removed | Every column drawn in the frame of the resize: Zed's header row tracks the same scroll handle and is laid out before the list picks its rows' columns. A test with the extra frame disabled passed the same, so the extra frame was not kept |
| 2026-10-14 | Grouping by a neighbour's property and by an end's property | `gq_language` `rows_group_by_a_neighbours_or_an_ends_property` (for every relationship of every dev node type and every groupable property of the other type: the counts, and pages and counts narrowed to a value and to "no value", compile; for both ends of every dev edge the same), `group_keys_name_one_kind_each`; `type_table` `a_neighbours_property_narrows_by_its_value_and_an_end_has_no_value_in_the_row`, `rows_group_by_a_neighbours_or_an_ends_property` (the menu's keys and labels on Person and WorksAt); offscreen snapshots on the dev server: Person, Group, the `worksAt Company` submenu, city; scrolled to the end; WorksAt, Group, `to Company`, city; and the same counts read with curl | Submenus for `contributes Project`, `knows Person`, `worksAt Company` and `← knows Person`, each with its arrow; Berlin 9, Lisbon 9, London 46, San Francisco 46, Tokyo 58, Toronto 35, "No city" 38 of 200 people, as curl counted; WorksAt by `to Company.city`: Berlin 9, Lisbon 9, London 49, and 7 edges to companies with no city |
| 2026-10-20 | The Blob write contract | `openapi/omnigraph-0.14.0-edge.json` (`cluster_putBlob`, `cluster_clearBlob`, `BlobWriteOutput`, `ErrorOutput.blob_precondition_failure`); Omnigraph `crates/omnigraph-server/src/handlers.rs` (`server_blob_put`, `server_blob_delete`) and `crates/omnigraph/src/blob.rs` (`managed_blob_etag`) at `cf2b20e9` | `If-Match` is `*` (a value is there) or strong tags; a null cell satisfies neither, and a put takes no `If-None-Match`, so a write into a null cell cannot be bound. The ETag hashes the table version with the row: any write to the type moves every ETag of it. A put needs `Content-Type: application/octet-stream`, at most 32 MiB; a receipt's `commit` is null only for a clear of a cell already null |
| 2026-10-20 | A Blob write is bound to the value read and has three outcomes | `graph_client` `a_blob_put_sends_raw_bytes_bound_to_the_etag_read`, `a_blob_clear_of_a_null_cell_commits_nothing`, `a_changed_blob_refuses_the_write_with_its_current_etag`, `a_blob_put_without_the_servers_answer_is_unknown`; `type_table` `blob_write` tests, the peek's `a_file_stored_in_a_slot_is_bound_to_the_value_read`, `a_value_changed_elsewhere_is_offered_again_over_the_new_one`, `uploads_the_view_cannot_send_say_why_and_send_nothing`, `a_page_held_at_a_commit_stores_no_file`, the table's `a_file_dropped_on_a_blob_cell_is_stored_bound_to_the_value_read`, the tab's `a_file_stored_from_the_tab_shows_at_its_own_commit` and `a_tab_at_a_commit_writes_nothing`, against the fake server's Blob cells | One `PUT` with one content type, the bytes and the ETag read; the view, the table and the tab move to the write's commit and show its value; a value changed elsewhere sends one write, keeps the other writer's bytes and offers "Replace anyway" bound to the new ETag, which lands; a 32 MiB + 1 file and a held view send nothing; a lost answer is unknown and not sent again. The native picker and a drop from Finder were not driven: no test reaches the system's open panel or its file drags, so the paths after them (`upload_blob`, `upload`) are what the tests call |

## Corrections

- 2026-10-22 · "Reading" still gave what `dab901f` and the snapshot's commit changed. Its sketch
  put the `$after` cursor on every page; the first page has none. It said the next page was read
  once the view passed 80% of the rows read; it is read a page ahead. It said a page's
  relationship counts and neighbours were read in turn; they go out together, the counts
  showing first. It did not say how a table counts its rows: from the session's snapshot with no
  read when that snapshot counts the pin, and otherwise by a read at the pin, the snapshot's
  count standing in meanwhile at the head. Views were filed under `<server url>/<graph>`; the
  key is `<target>/<graph>`.

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
