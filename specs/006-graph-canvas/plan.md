# 006: Graph canvas

Status: first slice implemented · 2026-10-12 (`crates/graph_canvas`)

## Problem

Some questions are visual: how a cluster of nodes connects, what sits between two people. The
canvas draws the schema and a growing neighbourhood of real nodes, fast, without becoming a
hairball.

## Principles

V (layout of the schema view never depends on counts), VII (camera motion only on request), IX
(caps with an exit), XII (performance measured).

## What exists today (checked)

- GPUI `canvas(prepaint, paint)` with `PathBuilder` for edges and quads for nodes; free-painted
  edges need `window.paint_layer` to stay in one draw batch; each path is capped near 65k
  vertices (research note, GPUI source at Zed `93d7456`).
- Measured on an Apple Silicon Mac (research benchmark, fjadra force layout and edge
  tessellation): 1k nodes 2.2 ms per layout step and 0.14 ms per frame of edges; 10k 34 ms and
  1.3 ms; 50k 254 ms and 8 ms.
- An earlier web client's canvas: schema landing; hubs per edge × direction × other type with
  counts; 8 neighbours then "+K more"; each node drawn once; cap at 600 chips with "Too many
  nodes to draw" and a way out; zoom ×1.25 steps clamped to [min(0.2, 0.8 × fit), 2]; arrow keys
  move to the nearest node in a 45° cone, scored `along + 2 × across`.

## Design

- **Landing:** the ontology (types as nodes, edge types as lines), laid out from the schema
  alone, with counts from `/snapshot` added after layout. Hovering a type highlights its edges
  and, on an ontology too large to label every line, names them over the chips.
- **Exploring:** expanding a node adds one hub per relationship group with its count; opening a
  hub adds 8 neighbours, then "+K more". State lives in `Exploration`, shared with the tree.
- **Rendering:** one custom element. Nodes as quads with labels shown above a zoom threshold;
  edges batched by color in one paint layer with cached geometry; hit-testing through an
  R-tree. Layout runs on a background thread and streams positions; the main thread only paints.
- **Caps:** a legibility cap (start at 2,000 visible nodes, tune with spike S2) with "Too many
  nodes to draw · Open as table · Show in tree". The cap is `studio.canvas_max_nodes`, which
  a graph can override (spec 002).
- **Keys (context `Canvas`):** arrows move to the nearest node; ↵ expand; Space peek; ⌫ collapse;
  `0` fit; `+`/`-` zoom; F focus; `[`/`]` walk the expansion path.
- **Query results:** a query whose result has node or edge columns can open as a canvas.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Custom GPUI canvas | WebView with a JS renderer | WebView embedding needs a GPUI fork; not on Zed's lineage |
| Grow by relationship group | Show the whole neighbourhood | Counts first keep large hubs from flooding the view |

## Ontology, explorer and placed nodes

- The canvas has two faces, named for what they show: the **Ontology** (the schema's types
  and relationships, `studio_graph::OpenOntology`, ⌘⇧O) and the **Explorer** (the nodes walked
  from a root, `studio_graph::OpenCanvas`, ⌘⇧E). Both are rows at the top of the sidebar's
  Graph panel, above Types, and items of the Graph menu; the tab is titled by the face shown.
- Dragging a type, a node or a relationship pill places it: it stays where it was dropped
  through every later layout, and the canvas tab saves the placed chips with the graph's tabs
  (`{"pinned": {key: [x, y]}}`, keys `type:Name`, `node:Type/id` and
  `hub:Type/id/out:Edge`). A dragged node carries its pills that were not placed by hand, and
  their "+K more", so they keep their places beside it; "+K more" is never placed alone.
  Reset layout (↺, R) forgets them all. A drag that starts on empty space pans.
- **Drawing cost.** Dotted lines are cut into 2-on 3-off dashes by walking each line once, in
  paths of at most 6,000 dashes, each dash placed at a whole number of periods along the line
  (in f64, stepped by an integer, so every walk ends; a straight piece of more than 100,000
  dashes is drawn solid); GPUI's own `dash_array` measured the batch's whole path every
  frame and overflowed a path's 65k-vertex cap, which dropped the batch. A path that still
  overflows is painted in halves. Labels scale in half-pixel text steps while zooming, since
  GPUI keeps a shaped line for one frame only and a smooth zoom reshaped every label every
  frame. Chip widths come from character widths kept per canvas (GPUI's app-level
  `layout_width` shapes each character anew). Reads that finish together (a node's counts)
  arrive as one `ExplorationEvent::Loaded` within 8 ms and are laid out once; answers for
  closed relationships (read ahead on hover) lay out nothing.
- **Reading ahead:** when the pointer rests on a closed node or relationship pill for 120 ms,
  what opening it will show is read without opening it (`Exploration::prefetch`): a node's
  relationship counts and single-target fields, a relationship's first neighbours, or its
  values when grouped. Reads stay on the exploration's commit, and nothing read or under way is
  read again, so passing over chips costs nothing and a rested-on chip opens at once. The
  Explore panel's closed rows do the same.
- **Clicks:** a click expands a closed node, opens or closes a relationship pill, and shows
  more on "+K more"; a click on an open node only selects it (a double click closes it), so
  a click never folds away what was walked. ⇧-click shows the chip in the inspector
  (`PeekNode`; a pill shows its node) and opens nothing. On a query's result canvas a node is
  selected by a click and opened by a double click.
- **Marking:** ⇧-drag on empty space draws an area and marks every chip it touches (added to
  those marked); ⌘-click marks or unmarks one chip, as it picks rows in a table; ⌘A marks
  all, Esc unmarks. Marked chips
  carry the selection's warm ring and never fade with the pointed-at neighbourhood. Dragging a
  marked chip moves every marked chip, and each stays where it was dropped. A plain click
  unmarks them.
- **Right-click menus.** A node: Open, Expand or Collapse, Explore from here, Open in table,
  Copy key. A relationship pill: Open or Close, Group by ▸ (the tree's grouping, spec 005),
  Open related table. "+K more": Show more. A type on the ontology: Browse rows, Open table.
  A chip placed by hand adds "Let the layout place it". A right click on a marked chip, with
  several marked, acts on all of them: Expand all, Collapse all, Keep where they are, Let the
  layout place them, Clear selection. On empty space: Fit to window, Select all, Reset layout.

## Deferred

- Engine change E16 (neighbourhood counts per node), [spec 012](../012-omnigraph-engine-changes/plan.md), for hub counts; shared with the
  tree's interim count queries until then.
- GPU compute layout above 50k nodes (spike S3, later).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Layout and edge costs at 1k/10k/50k | research benchmark on this Mac | 2.2/34/254 ms per layout step; 0.14/1.3/8 ms edges per frame |
| pending | Pan and zoom p95 < 16 ms at 10k nodes, 30k edges | spike S2 | |
| 2026-10-12 | Schema landing: types on a circle by sorted name, sized by `/snapshot` counts, one arrowed line per edge type, self-loops | offscreen snapshot, `OpenCanvas`, dev graph | Company 25, Person 200, Project 40; Knows drawn as a loop on Person (superseded by the stress layout below) |
| 2026-10-12 | The ontology reads as a map | `layout` tests (determinism, no overlaps, related types nearer than unrelated by 1.5×, hubs central, landscape, fewer crossings and shorter edges than a ring, unrelated types beside, placed types fixed); a CRM ontology (25 types, 78 relationships) measured locally; the app on that graph | Ring 668 crossings / 78.7k px edges; map 117 / 26.5k; fit shows every type named |
| 2026-10-12 | The explorer is a compact mind map | `layout` tests (hubs split across the root's sides, neighbours in one column beyond their hub within a chip and a gap, rectangle overlap, drawn columns grow at the bottom with "+K more" last); offscreen snapshots of Goldwasser Labs → WorksAt → Ivan Hopper expanded | One aligned column of nine people and "+1 more"; Ivan's four hubs in a column beside him; nothing drawn moved |
| 2026-10-13 | A column opened later finds room | `layout` `exploration_chips_do_not_overlap` (a root's four hubs drawn, four neighbours under one, then one under the next hub on the same side; it overlapped before the fix); offscreen snapshots of Radia Knuth opening → Knows 4, ← Knows 1, → Contributes 1 and → WorksAt 1 in turn | Jean Shannon under Leslie Dahl, the nearest free row to "← Knows"; Ole Lamarr visible; nothing drawn moved |
| 2026-10-12 | Exploring shares the tree's `Exploration`: hubs per group with counts, 8 neighbours then "+K more", each node drawn once, selection mirrored | snapshot Company `company-00000` with 103 incoming `WorksAt` | "← WorksAt · 103", eight people, "+95 more"; the tree's selected row is the ringed node |
| 2026-10-12 | Canvas actions change the shared state | snapshot: tree `ShowOnCanvas` (G) on Frances Lovelace, then `canvas::ExpandNode` | Frances gets four hubs on the canvas and is expanded in the tree |
| 2026-10-12 | Layout off the main thread is deterministic, keeps drawn nodes in place, and bounds cost at the cap | `cargo test -p graph_canvas` (`layout::tests`) | 1,999 nodes in 464 ms in an unoptimized debug build, Apple M5 Pro; same input gives the same positions |
| 2026-10-12 | Errors and loading show in the canvas, not a blank view | snapshot `OpenNode` Person `nobody` then `OpenCanvas` | banner "No Person with key nobody at this commit." · Retry |
| 2026-10-12 | Keys walk and focus | offscreen snapshot: Allen Labs expanded, → → `[` F Space | `[` returned to Allen Labs, F centred it, Space showed it in the inspector |
| 2026-10-12 | Dotted lines, labels and scene building cost less | `graph_canvas` `a_dotted_line_is_cut_into_dashes_across_its_bends`; `graph_store` `only_answers_for_what_is_open_change_what_is_shown`; snapshot `measure:60` (debug) on Sammet Systems → WorksAt → Ada, whose closed relationships draw dotted | A frame after a workspace notify 3.2 → 1.6 ms; a full redraw 11.9 → 7.5 ms; the dotted lines look as before |
| 2026-10-13 | Every dotted line ends | `graph_canvas` `every_dotted_line_ends_with_dashes_set_by_its_length` (a three-point line on which the old walk never ends, a curve from the frozen frame, a long cubic, a 10⁷ px piece, an infinite one); the snapshot example on Liskov Systems: mark all, Expand all, mark all, Expand all | Before: no frame after the second expansion, 13.8 GB resident and a 159 GB footprint, then killed (HEAD without this session's other changes behaved the same). After: exit 0, 166 MB resident, 220 MB footprint, eight people and two projects drawn with their relationships |
| 2026-10-12 | A click expands a node; ⇧-click inspects | offscreen snapshots on Sammet Systems → WorksAt: one click on Ada Lovelace; ⇧-click on Hedy Turing | Ada's relationship pills appeared beside her; the inspector showed Hedy Turing, with nothing opened |
| 2026-10-12 | Resting on a chip reads ahead what it opens | `graph_store` `prefetching_reads_what_opening_would_show_and_nothing_twice`; two offscreen runs each, differing only in where the pointer rests, counting the dev server's read log lines | Resting on the closed "← WorksAt 7" pill: 21 reads against 20 (its first page); resting on a person under it: 25 against 21 (one count per Person relationship) |
| 2026-10-12 | Pills drag, areas mark, and chips have menus | `graph_canvas` tests (a pill's pin key round trips; an area marks the chips it touches, not unpainted ones); offscreen snapshots on Sammet Systems with the snapshot tool's `drag:…,shift` and `click:…,right`: WorksAt dragged up then the company down; an area over three people; right clicks on a marked person, on WorksAt and on a person | WorksAt stayed where dropped while Owns moved with its company; Edsger, Ada and Grace marked with rings; "3 selected · Expand all · Collapse all · Keep where they are · Clear selection"; "Close · Group by › · Open related table"; "Open · Expand ↵ · Explore from here · Open in table · Copy key" |
| 2026-10-12 | Placed nodes persist per graph | `graph_canvas` `placed_nodes_round_trip_and_bad_data_reads_as_none`; offscreen snapshot: drag Company on the ontology, then reopen the data directory with restore | The saved canvas holds `{"pinned": {"type:Company": [-174, -275]}}`; after restore Company is where it was dropped, the rest laid out around it, and ↺ (Reset layout, R) shows |
| 2026-10-12 | The ontology and the explorer are found without the menu | offscreen snapshot of `OpenOntology` (⌘⇧O) | The sidebar's Graph panel opens with Ontology (selected) and Explorer above Types; the tab and the mode switch read Ontology / Explorer |
| 2026-10-12 | Query results as a canvas | `gq_language` `whole_node_columns_carry_their_type`; `query_editor` `node_columns_become_a_graph_of_distinct_nodes_linked_by_row`; snapshot of `return { $p, $c }` | The result offers Canvas; a results canvas draws each node once and links the nodes of a row; it is a tab of its own and is not restored |

## Corrections

- **Dotted lines froze the canvas (2026-10-13).** The dash walk carried its place in the
  pattern from piece to piece in f32. When the step left in a dash fell below the resolution
  of the distance walked (phase 1.9999995 of 2 at 8.14 px into a piece), the walk stopped
  advancing and pushed empty dashes until memory ran out: expanding 20 marked nodes reached a
  159 GB footprint before the process was killed. Curves, which bend every few pixels, hit it
  often (3 of the first 20 random three-point lines in an f32 emulation). Dashes are now
  placed at whole periods along the line in f64; the same expansion peaks at 220 MB.

- **Ontology layout (2026-10-12).** Types on a circle by name gave position no meaning: on a
  25-type, 78-relationship CRM ontology it drew 668 crossings. The ontology is now laid out by
  stress majorization (Gansner, Koren and North 2004; Graphviz neato's model): every pair of
  types as far apart as their graph distance, so related types sit together and hubs in the
  middle, from a classical-MDS start (deterministic). Each relationship is as long as its two
  chips and, when every label is shown (16 or fewer), its label. Several relationships between
  two types pull them closer. Chips are pushed apart until none overlap, a squarish map is
  widened towards 16:10 (at most 1.4×), and types with no relationship stand in columns beside
  it. Placed types anchor the rest. On the CRM ontology: 117 crossings, a third of the edge
  length, about 9 ms. Widths are measured with the UI font from the chip's own text
  (`paint::chip_text`), with room for a six-digit count, so a count never moves a type.
  Several relationships between the same two types fan out as bowed lines with their own
  labels; several loops on one type nest.
- **Explorer layout (2026-10-12).** Radial rings put neighbours ~450 px from their node: chips
  are wide and short, and a ring spaces them by width. The explorer is now a mind map: a node's
  hubs stand in a column beside it (a root's split across both sides), a hub's neighbours in a
  column beyond it, each further step outward on the same side, columns aligned on their near
  edge and as tall as what hangs off them; branches are curves leaving and arriving level.
  About a third of the old distances. Drawn nodes never move; new members join the bottom of a
  drawn column, and "+K more" ends its column. Overlap relaxation treats chips as rectangles.
- **Columns opened later landed on drawn chips (2026-10-13).** A new column stood centred on its
  hub whatever was already drawn there. On Radia Knuth, opening "→ Knows 4" and then "← Knows 1"
  put Jean Shannon on Ole Lamarr: the first column reached past the second hub, and relaxation
  could not part them, since drawn chips never move and the column had no room. A new column now
  starts at the nearest height where no chip already placed crosses its span (a column it joins
  only grows downward); below everything always fits.
- **Motion (2026-10-12).** `studio_ui::motion` (after `gpui-animation`'s design: easing curves,
  `Lerp`, a retargetable `Tween`; no registry or tick thread, a view asks for frames while a
  tween runs). New nodes grow out of the node they hang off and a reset glides nodes to their
  new places (220 ms, ease-out); Fit, F, zoom buttons and centring glide the camera (ease
  in-out, zoom on a log scale). Wheel, pinch and drags are immediate and stop a glide where it
  is. Off when the system asks for reduced motion (and in offscreen snapshots).
- **From the ontology to the explorer (2026-10-12).** The Explorer face is always available;
  before a node is chosen it says how to start: Search nodes (⌘P, which roots the exploration
  at the node picked) or G on a row in any table (`studio_table::ExploreRow`, also in the Nodes
  panel a type click opens from the ontology, and in its footer hints). The inspector's node
  header has an Explore button. The face shown is saved with the tab (`"ontology": bool`); a
  new canvas shows the ontology. The face switch is `ds::segmented`, and the search and zoom
  panels share its height and radius.
- **Relationships first.** A relationship with at most one target is a hub on the canvas like
  any other ("→ WorksAt · 1"), so expanding a node shows its relationships before the nodes
  they reach; the tree still shows it inline, as a field.
- **Fit** waits for the canvas's own bounds and fits again when they change (a dock opening),
  until the person moves the view by hand; Fit hands the camera back to fitting.
- **Hit-testing** is a linear scan over at most the node cap (2,000 by default), not an
  R-tree.
- **Hub labels** sit between the hub and its node, on a backdrop, so they never cover the fan
  of neighbours.
- **Mode.** "Schema" and "Exploration" toggle in the canvas; the schema view never clears the
  shared exploration.
- **Look (2026-10-12):** every type, node, hub and "+K more" is a chip, a white pill with a
  hairline and a soft contact shadow, with a letter mark (the label's initial in white on one of
  eight muted tints, chosen by a hash of the type's name) and a muted
  count; lines are 1-pixel greys (schema 15%, a pointed type's 40% and the rest 5%; hub lines
  20% and dotted while the hub is closed); pointing at a chip fades what it does not reach to
  30%; the selection carries a warm ring; a faint dot grid lies behind; a schema with up
  to 16 relationships labels every line. Chips grow with the view to their size at zoom 1 and
  become marks alone from afar. Overlay: the mode switch, a "Search nodes…" pill (⌘P) and the
  zoom controls on top, the caption bottom left. Colour is kept to node types: relationships
  show a neutral arrow wherever a type is listed. A click on a type chip browses its rows in
  the Nodes panel of the bottom dock (`studio_graph::BrowseType`), under the canvas;
  a double click opens its table as a tab.
- **Keys:** ←↑→↓ (`canvas::MoveLeft` …, nearest node in a 45° cone, `along + 2 × across`),
  `0`, `=`/`+`, `-`, ↵ expand, ⌫ collapse, Space peek, F focus on the selected node's
  neighbourhood, `[`/`]` step back and forward along the nodes walked. Bound in the `Canvas`
  context of `assets/keymaps/`. Scroll and pinch zoom, drag pans.
- **The cap exit** offers "Open as table" for the type with most hidden nodes and "Show in
  tree" (the Explore panel). The cap is the `canvas_max_nodes` setting, per graph if set.
- **Query results** open as a canvas from the result header's Canvas button
  (`studio_graph::ShowResultsOnCanvas`): each node once, a line between the nodes of a row.
