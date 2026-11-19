# 044: Pages and trees shaped by the type

Status: built 2026-11-19 · the page and the inspector, the tree, the canvas and the type table
read each node type as the person shaped it; a graph shipping its own layout waits on E27

## Problem

A node's page lists every property in schema order, then every relationship in schema order,
named as the schema names it. The cookbook study (`docs/ux/2026-11-18-cookbook-study.md`, PR #143,
proposal "Pages and trees shaped by the type") found what that costs on six of the eight
cookbooks:

- **brain N1** ([f13](../../docs/ux/2026-11-18-cookbook-study/f13-brain-person-page-order.png)):
  on Theo's page, `knows →` and `knows ←` list the same three people twice, and his preference
  note and the task Alex owes him are collapsed at the bottom.
- **vcos V1**: the wiki artifact's 18 sync fields come before its body.
- **beads B1, B3**: "blockedBy ← Issue" means "Blocks"; a related issue's badge takes the first
  letter of its title, not of its type.
- **program P1** ([f09](../../docs/ux/2026-11-18-cookbook-study/f09-program-tree-names-not-values.png)):
  the tree shows "Imager optics mass", not 24.5 kg (estimated), which is the whole reason for the
  drill-down.
- **company C1**: the claim table cuts quotes at about 25 characters, and `status` is off-screen.
- **dev D2** ([f16](../../docs/ux/2026-11-18-cookbook-study/f16-dev-canvas-fit-drops-labels.png)):
  Fit draws an incident's causal chain as unlabelled marks.
- **brain N2** (the study's polish list): a new node's page shows an empty "Relationships"
  heading, its relationships hidden behind the eye.

People want a type to read the way they use it: the text first, the relationships that matter
first and in their own words, the values that matter where they walk, and text-heavy types read
as text.

## Principles

- II: a layout changes no read's commit. A page or tree shows the line it read at the commit it
  shows; a layout chosen while a page is held at a commit reads the lines there.
- V: a layout changes only at the person's own choice; a line arriving fills a row in place, and
  a card is one height, so nothing below it moves.
- VI, VIII: every new action has a key on every platform and a place in the palette; a group's
  header says it is shaped by right-click in its tooltip.
- IX: a line is read for the nodes shown, one read per type a turn, at most `NAME_BATCH` (500)
  ids a read; cards are a uniform list read in the table's own pages.
- X: designed against contract 0.14; the graph-wide layout waits on E27 (Deferred).
- XIII: the layout is the type's, so every view of the type follows it; reading as cards is a
  toggle of the table, since it draws the same rows the grid holds.

## What exists today (checked)

Studio `98c7cea` (origin/main, 2026-11-19); Omnigraph `v0.15.0` (contract 0.14).

- **The page and the inspector are one view**, `type_table::peek::NodePeek` (spec 014): the
  properties as `listed_properties` returns them, schema order, rows then long texts
  (`render_properties`); the groups as `SchemaModel::relationships_of` lists them, outgoing
  first, each in schema order (`gq_language/src/schema.rs`, `groups_of`), placed by
  `list_items` (listed, as drawn, folded). A group's header is `"{edge} {arrow} {other}"`
  (`render_group`), a neighbour's mark is `studio_ui::type_mark(type, label)`, whose letter is
  the label's (`studio_ui.rs`), and a neighbour's row is its label alone.
- **A per-graph, per-type choice already exists** for the property that names a type's nodes:
  `GraphOverrides::labels` in the settings (spec 002, "Node names"), applied by the session to
  the schema model it hands views (`GraphSession::take_labels`).
- **The tree and the canvas share one walk**, `graph_store::Exploration`, which flattens a node's
  groups from `relationships_of` (`push_node`), reads their pages with `builder::neighbours`
  (`id`, `edge_id`, `label`, edge properties) and keeps labels in `ExploreState::labels`. A tree
  node row is its type's dot, its label, and one short edge value
  (`relationship_tree.rs`, `render_row`); a canvas chip is its label wrapped to three lines
  (`graph_canvas/src/paint.rs`, `chip_text`).
- **Canvas Fit** zooms the whole walk to the window, down to 0.05
  (`graph_canvas.rs`, `fit`); below a 8.5 px text size a chip draws as its mark alone
  (`paint.rs`, `MARK_ONLY_BELOW`), which is what f16 shows.
- **The type table** draws its rows as a grid of fixed-width cells, `uniform_list` lines, inside
  the cached `TableGrid` view (`table.rs`, `render_body`, `render_grid`).
- **Omnigraph reads a relationship both ways in one traversal**: `$a <knows> $b` "matches the edge
  in either direction" and needs one node type at both ends; unbound, it has set semantics for
  the endpoint pairs; bound (`$a $e:<knows> $b`), one row an edge
  (`docs/user/queries/traversal.md` at `v0.14.0` and `v0.15.0`). Checked against a `v0.15.0`
  server in the Validation log.
- **Annotations**: schema migration v1 refuses adding or changing an annotation other than
  `@description` and `@instruction` on an existing property (E27, spec 014), so a graph cannot
  ship a display hint for what it already holds.

## Design

### The layout

`studio_settings::TypeLayout`, one per node type and graph, in `GraphOverrides::layouts` beside
`labels`. Every field is optional and the layout goes when nothing is chosen:

| Field | Means | Set from |
|---|---|---|
| `lead` | The property a page leads with, before the others, a long text at the full width | The page's layout menu, "Lead its page with" |
| `order` | Relationship keys (`out:Knows`, `in:BlockedBy`, `both:Knows`) a page and a tree list first, in this order | A group's menu: Move up, Move down, Move to top; ⌥↑ ⌥↓ on a page |
| `names` | The reader's name for a relationship key: "Blocks" for `in:BlockedBy` | A group's menu: Name… |
| `both_ways` | Edges from the type to itself that mean the same both ways (`Knows`) | A group's menu: Read both ways as one |
| `line` | The properties a row of this type shows after its name, in order | The page's layout menu, "Show after its rows' names" |
| `reading` | The type's table reads as cards | The page's layout menu, the table's Views menu, ⌘⌥R |

The session holds the graph's layouts as the settings have them (`GraphSession::layout`,
`layouts`), says when they change (`SessionEvent::LayoutChanged { lines }`, `lines` the types
whose rows read otherwise), and writes them (`GraphSession::change_layout`). A layout is the
person's, kept in their settings for the graph, as names and views are, until a graph can ship
its own (E27).

Moving a relationship keeps every other where it was: the order kept is the page's list up to
the moved one's new place, or as far as the person's order went (`TypeLayout::step`). Folding a
relationship both ways carries its place and its name to the one key both ways, and back
(`TypeLayout::set_both_ways`).

### Relationships both ways

`gq_language::Direction::Both` is a relationship read either way: one undirected traversal,
`$n <knows> $m`, or `$n $e:<knows> $m` with its edges, in every read a group makes (its pages,
its edges counted, its neighbours counted and the counts of a page of nodes). The schema lists
no such group; `graph_store::layout::folded_groups` makes one, where the outgoing group stood,
for each edge the layout reads both ways whose two ends are the type, and leaves out the
incoming one. A relationship between two types is never folded; its menu offers no fold.

- **On a page** a group both ways reads its edges in one read, each row an edge, and lists each
  related node once, the edges to it together (`listed_rows`); it counts related nodes, not
  edges (`page_degrees`, `count($m)`). Its × removes every edge between the two, in either
  direction and whether read or not, in one write (`delete_edges_between`: `delete Knows where
  (@src = $a and @dst = $b) or (@src = $b and @dst = $a)`). Its "+" adds an edge from this
  node.
- **In the tree and on the canvas** a page both ways is read by node (`After::Neighbour`), each
  node once (`ExploreState::apply_page` drops the repeats), and is as long as the rows it read,
  so a full page still offers more.

### The page and the inspector

- **Lead.** The lead property comes before every other property, a long text at its full width
  as any is, a short value as a row.
- **Order.** `list_items` lists the person's relationships first, by their place, then the rest
  as before (known to have neighbours, then as drawn, then the empty row). A page still hides a
  relationship counted empty, and the inspector still folds one known empty under its Empty
  row. The groups keep their indices; only the listing is ordered, so a read or write out on a
  group lands on the group it was sent for. A fold changes the groups themselves: they are made
  again, each read as it is drawn, and while a relationship write is out, once it lands.
- **Names.** A group's header is the person's name for it, "Blocks", else the schema's
  "blockedBy ← Issue" or "knows ↔ Person"; its tooltip says the schema's name of a named one,
  and "right-click to move or name it".
- **A group's menu**, on right-click of its header: the schema's name as its header; Move up
  (⌥↑), Move down (⌥↓), Move to top; Name… (a dialog: "Name knows ← Person", "What Person's
  pages and trees call it, in your words…", empty lets the schema's name back), "Call it
  `<schema name>`" for a named one; "Read both ways as one" or "Read each way apart" for an
  edge from the type to itself. The header's tooltip is not drawn over its open menu.
- **A related node's row** shows the line of its type after its name, muted and cut first
  (`SidebarRow::detail`), read once for the group's nodes at the commit they were read at
  (`GraphSession::node_lines`), noted in the page's read scope so a head move that changes the
  type's nodes is not carried past, and again for a type whose line changes (reads still out are
  dropped then, so an old line never lands after a new one). Its mark is its type's
  letter on the type's tint, as every row of a group is one type (beads B3).
- **No relationships yet.** A page whose every relationship is counted empty, and hidden,
  says "No relationships yet · Show all to add one" under its heading; the button is the eye's.
- **The layout menu**, the page toolbar's reader button (⌘⌥L, `studio_node::ShapeType`): "How
  Person reads"; "Lead its page with" (its properties in schema order, or one); "Show after its
  rows' names" (each property, checked, the menu kept open; Nothing); "Its table reads as" (a
  grid, or cards); "Reset how Person reads".

### A row's line

`gq_language::line_text` writes a node's line from the values its type's layout names: each value
as short text, `·` between them; a number and the unit after it (a text property named `unit`,
`units`, or ending in `_unit` or `Unit`) as one piece, `24.5 kg`; a truth names its property; a
time to the minute, midnight as its day; a text its first sentence, at most 60 characters; a
list its first three; nothing for none, false or an empty list. The values are read with
`builder::node_lines` (`$n.@id in $ids`, `l0`, `l1`, … by place in the line), one read per type
of at most 500 ids.

- **The tree** shows it after a node's name, muted, before the key; the walk reads the lines of
  the nodes its rows show once their pages land, at its pin, notes the types in its read scope
  so a head move that changes them reads again (spec 015), and reads again for a type whose
  line changes. Its group rows take the person's names, with the type they reach.
- **The canvas** shows it as a muted row under a chip's name, cut to the chip's width; hubs take
  the person's names. **Fit** stops where chips still say their names, `LEGIBLE_ZOOM` (9 px
  text, half a pixel over the size where a chip turns into its mark): a walk too wide for that
  shows its middle, the rest a pan away, and Fit again shows the whole walk, names as marks
  ("Fit the whole walk, names as marks" in its tooltip), then names again. Only the person's
  own Fit is asked again, and only over the same walk: the fit the canvas makes when it opens
  or is resized, or a walk an expansion grew since, keeps the names on the next Fit. Zooming
  out by hand still reaches the whole walk.

### Reading as cards

A node type read as cards draws its table's rows as cards in place of the grid
(`TypeTable::render_cards`), within the same cached part of the table (`TableGrid`):

- **A card**: the type's mark, the node's name in the heading weight and its line; the text it
  leads with (the layout's lead, else its first text with words to wrap at) as one run, its
  markdown taken off (`studio_ui::text::plain_run`), wrapped over three lines and cut with "…";
  then up to six short values, `status proposed`, in schema order, those with a value.
- **One height**, three lines of text and two of name and values with the padding, so the list
  is a `uniform_list` on the table's own scroll handle: the cursor's row scrolls into view as the
  grid's does, and the table's pages are read as the cards near the end.
- **Keys**: J/K and ↑↓ move the cursor's card; Space shows the inspector; ↵ (a card has no cell)
  and ⌘↵ open the node's page; a click selects, a double click opens.
- A grouped table stays a grid: its sections are the grid's.
- `studio_table::ToggleReadingLayout` (⌘⌥R) turns it on or off for the type, from the table, the
  table's Views menu ("Read as cards") and the page's layout menu.

### Actions

| Action | Context | macOS | Linux, Windows |
|---|---|---|---|
| `studio_node::ShapeType` | `NodePage && !Editor` | ⌘⌥L | Ctrl-Alt-L |
| `studio_node::MoveRelationshipUp` | `NodePage && !Editor` | ⌥↑ | Alt-↑ |
| `studio_node::MoveRelationshipDown` | `NodePage && !Editor` | ⌥↓ | Alt-↓ |
| `studio_node::MoveRelationshipToTop` | `NodePage && !Editor` | ⌥⇧↑ | Alt-Shift-↑ |
| `studio_node::NameRelationship` | `NodePage && !Editor` | F2 | F2 |
| `studio_node::ToggleBothWays` | `NodePage && !Editor` | ⌥⇧B | Alt-Shift-B |
| `studio_table::ToggleReadingLayout` | `TypeTable` | ⌘⌥R | Ctrl-Alt-R |

The relationship keys act on the group of the related node the person selected on the page (the
group of the row clicked, as one node can sit in two groups); the group's menu shows each key.
The lead and the line are chosen in the layout menu, which ⌘⌥L opens and the arrows walk.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| The layout is the person's, per graph and type, in their settings | A schema annotation (`@display(...)`); a file in the cluster | 0.14 and 0.15 refuse an annotation added to an existing property or type (E27); Studio keeps no state of its own on the server (constitution I, X). Names (spec 002) and views (spec 024) are kept the same way |
| One layout a type, followed by the page, the inspector, the tree, the canvas and the table | A page-only layout; a layout per view | The study's point is that a type reads one way: "Blocks" on the page and "blockedBy from Issue" in the tree would be two vocabularies for one thing |
| A fold both ways is one undirected read | Two reads merged in Studio; hiding the incoming group | One read has one keyset and one exact count, and the server's set semantics count a node once; merging two keysets doubles reads and breaks "+N more"; hiding is wrong whenever an edge is stored one way only |
| A fold is the person's choice, never guessed | Folding every self-edge whose two lists match | `BlockedBy: Issue -> Issue` has two meanings; matching lists can't be told from a first page |
| The groups keep their places; only the listing is ordered | Reordering `groups` | Reads and writes name their group by its place; reordering under one in flight would land it on another group |
| Lines are a read of their own, by id | Columns added to every neighbour read | Every view's neighbour reads stay as they are (and their memo), and a changed line reads only the lines, not the counts and pages |
| A number and its unit join by the unit's name | A template string (`{value} {unit}`); always `·` | A template needs a text field and a grammar; `24.5 · kg` reads worse than `24.5 kg`, and `unit` is how the cookbooks name it |
| Fit stops where names read; Fit again shows all | Always fit whole; never zoom out past names | The whole walk as marks is what f16 showed; never zooming out leaves no overview. Two presses keep both with one control |
| Cards are one height | Cards as tall as their text (`list`) | A uniform list scrolls, pages and follows the cursor as the grid does with the same handle, and no card grows under the person as its text lands (constitution V) |
| The type's mark letter on a group's rows | The node's initial, as before | Every row of a group is one type, so the node's initial says nothing a name does not, and it read as a type (beads B3) |

## Deferred

- **E27** (annotation-only schema changes, [docs/deferred-engine-changes.md](../../docs/deferred-engine-changes.md)):
  with it a graph could carry its layout for everyone, as display hints on its types and
  properties; until then each person shapes their own, and a layout is not shared.
- The relationship columns of a type's table keep the schema's order and names; a table orders
  its columns itself (spec 004).
- Grouped tables stay grids; cards by section would need a header line of the cards' height.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | Omnigraph `v0.15.0` reads a relationship both ways in one traversal, at the head and at a commit, and counts a node once | `omnigraph-server` `v0.15.0` (release build) serving the eight cookbooks' `brain`, `program` and others, seeds loaded with `load --mode append`; curl to `POST /graphs/brain/query` | `$n $e:<knows> $m` from `per-theo` answered six rows, two edges each for Marco, Maya and Alex; `count($m)` over `$n <knows> $m` answered 3 (and 7 for `per-self`, one read for both); `count($e.@id)` 6; the keyset `($m.@id > $after or ($m.@id = $after and $e.@id > $after_edge))` paged it; the same read with `snapshot` set answered the same rows |
| 2026-11-19 | Every read a group both ways makes compiles under the pinned compiler | `gq_language` `a_relationship_read_both_ways_is_one_undirected_traversal`, `a_types_line_is_read_for_nodes_by_id` | Pass |
| 2026-11-19 | A line reads as the study asked | `gq_language` `line::tests::a_number_reads_with_its_unit_and_the_rest_apart` | `24.5 kg · estimated`; a unit alone stands apart; a truth names its property; midnight reads as a day; a long text cut with "…" |
| 2026-11-19 | A layout is kept, moved and folded without moving the rest | `studio_settings` `a_types_layout_is_kept_and_goes_when_empty`, `moving_and_folding_relationships_keeps_the_rest_in_place`; `graph_store` `layout::tests` | Pass |
| 2026-11-19 | The page reads as the type is shaped | `type_table` `a_page_reads_as_its_type_is_shaped`, `a_relationship_read_both_ways_lists_each_node_once`, `a_related_nodes_row_shows_its_types_line`, `a_page_with_no_relationships_says_so` | Pass; the fold's × sent one `studio_delete_edges` with both edges |
| 2026-11-19 | The walk reads the layout | `graph_store` `a_walk_reads_its_types_layout` | Ordered, folded, Maya once on a full page that still offers more |
| 2026-11-19 | Fit keeps names readable | `graph_canvas` `fit_keeps_the_names_readable_and_fit_again_shows_all` | A 3,000 px walk in 600 px fits at `LEGIBLE_ZOOM` centred; Fit again shows all; again names |
| 2026-11-19 | The running app reads the cookbooks as the study asked | `examples/snapshot` (debug build, Linux, lavapipe) against the `v0.15.0` server above, with each graph's layout in a settings file (`OMNIGRAPH_STUDIO_DATA`): brain `per-theo`'s page, its group menu (right-click) and its layout menu (Ctrl-Alt-L); program's tree from `blk-payload`, Imager optics opened with Alt-→; dev's canvas from `inc-presence-leak`, every relationship opened in its tree, then Fit and Fit; company's Claim table as cards; beads `lk-queue`'s page | brain: `brief` first; "Notes about them" and "Tasks" first with "preference · For Theo: peated Islay…" and "next · 2026-05-20 · i-owe"; `knows ↔ Person` lists Marco, Maya and Alex once (3, where the study saw them twice). program: "Imager optics mass 24.5 kg · estimated" in the tree and the inspector. dev: the first Fit drew every name (eleven nodes), the second the whole walk as marks. company: each claim's quote whole, `kind` and `status` in view. beads: "Blocked by" and "Blocks", each row an `I` |
| 2026-11-19 | One write removes every edge of a relationship read both ways | `studio_delete_edges_between` sent to the `v0.15.0` server on a scratch branch of `brain` for `per-theo` and `per-maya`, then `$p <knows> $m` read there; the branch deleted after | `affected_edges: 2` in one commit; Theo then knew Alex and Marco only |
| 2026-11-19 | Cards draw the rows and cost a keystroke nothing | `type_table` `a_type_read_as_cards_draws_its_rows_as_cards` (`docs/drawing.md`, Guards) | The card's text whole, its status as a value; typing in the filter field drew no card; ↵ opened the cursor's page |

## Corrections

- 2026-11-19, before landing · The first Fit after opening a canvas drew the walk as marks
  again: the canvas's own fit on opening had stopped where names read, so the person's first
  Fit counted as "again". Only the person's Fit is asked again now, over the same walk
  (`fit_again_shows_all`); `fit_keeps_the_names_readable_and_fit_again_shows_all` covers both.
- 2026-11-19, before landing · A group's tooltip stayed over the menu its right-click opened;
  it is not drawn while the menu is open.
- 2026-11-19, review of #154 · A both-ways × deleted only the edges read, so a node with more
  edges than a page stayed related: it deletes by endpoints now. A related node's line was not
  in the page's read scope; a line read still out could land after a newer one; a new token
  kept the old token's lines; the move keys moved the first group holding the selected node,
  not the one clicked; ↵ on a read-only table's card did nothing; a lead that is not text hid a
  card's text; moving to the top, naming and folding had no keys. Each is fixed as described
  above.
