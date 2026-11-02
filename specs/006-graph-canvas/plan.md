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

- **Ontology page:** the schema's types grouped into domains, the types related to most of the
  others set apart, and any type in focus with its relationships either side; a small schema
  drawn whole as one map. Grouped and laid out from the schema alone, with counts from
  `/snapshot` shown beside the types, so a count never moves anything.
- **Exploring:** expanding a node adds one hub per relationship group that has neighbours, once
  its count is in (since 2026-11-01; before, every group's hub appeared at once and the empty
  ones went as their counts came in, which read as flicker); opening a
  hub adds 8 neighbours, then "+K more". State lives in `Exploration`, shared with the tree.
- **Rendering:** one custom element. Nodes as quads with labels shown above a zoom threshold;
  edges batched by color in one paint layer with cached geometry; hit-testing through an
  R-tree. Layout runs on a background thread and streams positions; the main thread only paints.
- **Caps:** a legibility cap (start at 2,000 visible nodes, tune with spike S2) with "Too many
  nodes to draw · Open as table · Show in tree". The cap is `studio.canvas_max_nodes`, which
  a graph can override (spec 002). An open canvas draws a changed cap at once, and the banner
  names the cap its drawn scene was built under (`Scene::cap`), never a newer setting.
- **Keys (context `Canvas`):** arrows move to the nearest node; ↵ expand; Space peek; ⌫ collapse;
  `0` fit; `+`/`-` zoom; F focus; `[`/`]` walk the expansion path.
- **Query results:** a query whose result has node or edge columns can open as a canvas.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Custom GPUI canvas | WebView with a JS renderer | WebView embedding needs a GPUI fork; not on Zed's lineage |
| Grow by relationship group | Show the whole neighbourhood | Counts first keep large hubs from flooding the view |

## Ontology page, canvas and placed nodes

- Two pages, each a tab of its own (since 2026-10-20; before, two faces of one tab with a
  switch): the **Ontology** (the schema's types and relationships,
  `studio_graph::OpenOntology`, ⌘⇧O, `ontology.rs`) and the **Canvas** (the nodes walked from
  a root, `studio_graph::OpenCanvas`, ⌘⇧E; "Explorer" until 2026-10-19). Both are rows at the
  top of the sidebar's Graph panel, above Types, and items of the Graph menu.
- **The ontology page** (2026-10-20) is built from Studio's own parts, not painted chips, and
  draws no web of every relationship: a schema of a hundred types has hundreds, and no layout
  made that readable (Corrections, "Ontology layout (2026-10-20)").
  - **Domains.** The types related to more than 30% of the others ("Linked to most types",
    pills at the top) belong to every group and so to none. The rest are grouped by
    modularity (Louvain: each type joins the group it has most relationships with beyond
    chance, then the groups are grouped once more), a type left alone joins the group it is
    most related to, and the types related only to shared ones, or to none, go last as "Other
    types" (`domains.rs`). Each domain is a card titled by its most related type ("Account and
    17 more"), listing its types with their counts, its footer naming the domains it has most
    relationships with. Cards stand in as many columns as the page holds, each card in the
    column with least in it; the page's width is read as it is laid out, and a new one draws
    the page again once that frame is done. Pointing at a type selects it and dims the types
    it is not related to, in every card. Deterministic: a function of the schema alone.
    Twelve types or fewer are one group.
  - **A type in focus** (a click on any type): its card in the middle, with its count, what
    the schema says it is (its `@description`, else its `@instruction`; `NodeType::about`;
    five lines, the rest behind More), its properties (an enumeration's values below its
    name, on as many lines as they need), and Open table and Schema; its domain
    is in the header's path. The relationships that reach it stand on the left, those it
    reaches on the right (one to itself once, on the right), each side titled with how many
    it holds, each a row of the type at the other end and the relationship's name: the type
    takes up to half the row and the name the rest, each cut past that with both whole in
    the row's tooltip. Past eight a side is grouped by domain, the shared types first. The
    sides are as wide as the page allows between 240 and 440 px; a page narrower than the
    three columns (952 px) shows the card, then each side below it, full width up to 640 px,
    without lines, on the page's edge. Each row's dashed line runs to an upright
    between the side and the card, each end in a ring, as the canvas draws its lines; the
    row pointed at draws its line stronger and shows the relationship's own description. A
    click on a row brings that type into focus; the back button and Esc
    (`studio_ontology::Back`) go back through the types shown, Domains (and the palette's
    `studio_ontology::ShowDomains`) to the domains. A type the schema no longer has (dropped,
    or renamed by another writer: the page takes another writer's schema at once, as the
    types list does) leaves the focus and the way back. The tab reads "Ontology · Type" and
    comes back with its type in focus (`{"focus": "Type"}`).
  - **Keys** (context `Ontology`): ↑ and ↓ (`SelectPrevious`, `SelectNext`) point at the
    types in the order the page lists them, or at the type in focus's lines, the side
    reaching it first, as the pointer does; ↵ (`Open`) brings the type pointed at into focus.
  - **A small schema** (12 types or fewer, 24 relationships or fewer) is drawn whole as a map:
    each type a card in columns (`layered.rs`), as wide as its name and count need between
    140 and 280 px, each relationship a dashed right-angle line between their sides, named on
    its own upright, a type's relationships to itself as brackets over its card; a line meets
    a card's side as near its other end as the side allows, so a line level with it runs
    straight. Pointing at a type draws its lines stronger, a click brings it into focus.
- **Opened chips** (2026-10-19). A node's chip shows 32 characters of its name, then "…"
  (`CHIP_LABEL_CHARS`); layout always measures that, so opening a chip never moves another.
  Pointing at a cut chip for a moment opens it: its whole name, wrapped at 320 px at zoom 1
  (`OPEN_CHIP_MAX_WIDTH`) in lines of even length, drawn over its neighbours from where the
  chip starts, never narrower than it, with its mark and the pointed shadow or the selected
  border. It is laid out before the lines, which meet its sides level with its middle (not
  its first line), and the cut chip is not drawn under it;
  selecting it keeps it open until another chip is selected, and the opened chip is hit where
  it is drawn. A node chip's menu offers "Show as ▸"
  with its type's single text properties and "Automatic": the graph's node name for the type
  (spec 002, "Node names"), so the whole graph, not only the canvas, takes it.
- Chips take the radius scale's `sm` corner, fill, border, shadow and ring alike (the fill
  was drawn round until 2026-10-19 while its shadow took `sm`). Since 2026-10-20 their shadow
  is interfaces.dev's `shadow-custom` (spec 002, "Shadows and borders"), painted as a browser
  paints a CSS `box-shadow`: the ring outside the chip and each layer's corner grown or shrunk
  with its spread (`elevation::paint_layers`). A chip has no border but where you are (the here
  ring); pointing at one steps its shadow to `-hover`. The canvas's ground is `surface::canvas`, interfaces.dev's
  `#fcfcfc` in a light theme, so its white chips read as they do there.
- Dragging a node or a relationship pill places it: it stays where it was dropped through
  every later layout, and the canvas tab saves the placed chips with the graph's tabs
  (`{"pinned": {key: [x, y]}}`, keys `node:Type/id` and `hub:Type/id/out:Edge`). A dragged node carries its pills that were not placed by hand, and
  their "+K more", so they keep their places beside it; "+K more" is never placed alone.
  Reset layout (↺, R) forgets them all. A drag that starts on empty space pans.
- **Drawing cost.** Every line is right-angle pieces, and each frame merges the pieces of one
  tone that run along the same stretch (a trunk carries many lines) into one span, takes out
  what a stronger tone already drew there, clips it to the view, and cuts it into dashes, each
  a 1 px quad on the pixel grid: no paths, so no vertex cap. A dash sits at a whole number of
  periods from the world's origin on screen (in f64, stepped by an integer, so every walk
  ends; a span of more than 100,000 dashes is drawn solid), so lines sharing a stretch share
  their dashes and the dashes travel with the lines as the view pans. Labels scale in half-pixel text steps while zooming, since
  GPUI keeps a shaped line for one frame only and a smooth zoom reshaped every label every
  frame. Chip widths come from character widths kept per canvas (GPUI's app-level
  `layout_width` shapes each character anew). Reads that finish together (a node's counts)
  arrive as one `ExplorationEvent::Loaded` within 8 ms and are laid out once; answers for
  closed relationships (read ahead on hover) lay out nothing.
- **Reading ahead:** when the pointer rests on a closed node or relationship pill for 120 ms,
  what opening it will show is read without opening it (`Exploration::prefetch`): a node's
  relationship counts and single-target fields, a relationship's first neighbours, or its
  values when grouped. Reads stay on the exploration's commit, and nothing read or under way is
  read again, so passing over chips costs nothing and a rested-on chip opens at once. Leaving
  the chip stops what was read ahead for it and is still out (`Exploration::end_prefetch`).
  The Explore panel's closed rows do the same.
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
  Open related table. "+K more": Show more.
  A chip placed by hand adds "Let the layout place it". A right click on a marked chip, with
  several marked, acts on all of them: Expand all, Collapse all, Keep where they are, Let the
  layout place them, Clear selection. On empty space: Fit to window, Select all, Reset layout.

## Deferred

- Engine change E16 (neighbourhood counts per node), [spec 012](../012-omnigraph-engine-changes/plan.md), for hub counts; shared with the
  tree's interim count queries until then.
- GPU compute layout above 50k nodes (spike S3, later).

## Validation log

| Date | Claim | Check | Result |
| 2026-11-01 | A hub appears only once counted and non-empty | `graph_canvas` `model` tests (`a_single_target_hub_reached_twice_is_open_by_either_path` with counts, `the_cap_counts_relationships_and_each_node_once` counting only drawn hubs) | Pass. Ragnor's call: hubs that appeared and then went as their counts came in empty were noise |
|---|---|---|---|
| 2026-11-01 | A canvas walks from one node; a set goes to a tree | `graph_canvas` tests; an offscreen snapshot of a walk opened from a selected tree row | Spec 020 removes drawing a table's rows, which was a grid of unconnected chips. A query's picture is Graph, and only when its result links |
| 2026-11-01 | One canvas per root and commit; a table's rows draw as a captured set | `graph_canvas` `a_canvas_starts_from_the_graphs_saved_positions`, `a_captured_set_is_known_by_what_it_holds`; offscreen snapshots; a saved root-less canvas restored twice | Spec 020. No blank canvas; a saved one's positions stay for each new canvas of its graph |
| 2026-10-28 | A walk's change that leaves its scene as drawn lays out nothing | `graph_canvas` `a_change_that_moves_no_chip_lays_out_nothing` (a walk laid out, then a change owed with the same scene: no layout; then another root: laid out) | Pass. Before, every change of the walk laid its scene out again off the main thread, as "+K more" did while its page was read; pins and Reset layout still lay the same scene out again |
| 2026-10-21 | A line's end ring sits on the line, at the chip's middle, opened or not | `paint` `a_line_ends_in_a_ring_on_it` (both ends of lines at zooms 0.3 to 3, chips of four sizes either side, an upright end, a piece of no length, and a chip opened to five lines; it fails by exactly 3 px with the old placement); clippy `-D warnings`; offscreen snapshots on `script/dev-server` of a walk (light, dark, zoomed in twice and out twice) and of a result whose second node's name opens to five lines (light and dark) | Before (Ragnor's screenshots): every ring 3 px below its line, and an opened chip's line and ring at its first line, the cut chip's glow showing above the card. After: each ring centred on its line at the chip's middle at every zoom and in both themes; the opened card's line turns down to its middle; no outline above it |
| 2026-10-21 | Leaving a chip stops what was read ahead for it | `graph_canvas` `leaving_a_chip_stops_its_read_ahead` (a relationship pill's first neighbours read ahead, the pointer leaving before they answer; it fails without the stop) | The read is let go and nothing records that it was asked for, as for a tree row (spec 005). Before, it ran on until another chip's read ahead replaced it |
| 2026-10-21 | A changed node cap reaches an open canvas, and showing a node builds its scene once | `graph_canvas` `a_new_node_cap_is_drawn_at_once` (a results canvas of three people under a cap of 2, then 3: two chips and "1 not shown", then three and no banner; it fails with the settings watch removed); `showing_a_shown_node_keeps_the_exploration`; clippy `-D warnings`; design tokens and layers; offscreen snapshots of Show on canvas, once and twice, on `script/dev-server` | Before: a cap changed in settings showed in the banner over the chips drawn under the old one until something else redrew the canvas, and Show on canvas built the whole scene twice on the main thread, once to ask whether the node was in it. After: the cap redraws the canvas at once, and a node already drawn is laid out from the one scene built to look for it; both snapshots are pixel-identical to the build before |
| 2026-10-20 | Nothing on the ontology page is cut off at any width | `layered` `a_line_meets_a_chip_level_with_its_other_end_where_it_can` and the existing route soundness tests; `ontology` tests; workspace clippy `-D warnings`; design tokens and layers; offscreen snapshots on a third scratch server whose 101 types carry made-up properties and descriptions shaped like a personal graph's (enumerations of up to 20 values, descriptions of up to 600 characters), at 1440 and 1100 px wide, light and dark, a row pointed at; the dev graph's map | Before: at 1100 px the focus view's fixed 1,084 px of columns ran off both edges, cutting the type names on each side, and enumerations ran out of the card. After: at 1440 px the sides take 330 px each; at 1100 px the card stands above the two sides, nothing cut; enumeration values wrap under their property; a 600-character description shows five lines and More; the side titles read "Reach it 14" and "It reaches 10"; the dev map's Contributes line runs straight into Person, where it stepped 5 px |
| 2026-10-20 | The ontology page is keyed, packs its columns on first draw and drops a type the schema lost | `ontology` `the_keys_walk_the_types_and_their_lines`; `studio` `keymaps` (every action bound or palette-only); workspace clippy `-D warnings`; offscreen snapshots on the 101-type scratch schema: open, ↓↓↓, ↓↓↵, ↓↓↵ Esc | Four columns on the first render, before any pointer; ↓↓↓ selected the third shared type and dimmed the types it is not related to; ↓↓↵ brought the second into focus; Esc returned to the domains |
| 2026-10-20 | A large ontology reads as domains and one type at a time; a small one as a named map | `graph_canvas` `domains::tests` (cliques around a hub group apart and the hub is shared, every type has one place, a small schema is one group), `ontology::tests` (each relationship once on its side of a type in focus; past twelve types no map), `layered::tests`; `gq_language` `a_type_says_what_it_is`; workspace clippy `-D warnings`; offscreen snapshots on a scratch server holding a 101-type, 555-relationship schema with the structure of a personal graph and made-up names, and on `script/dev-server` | Four shared types and nine domains of 4 to 18 types in four columns; pointing at a type lit the 16 types it is related to; a shared type in focus listed 77 relationships reaching it and 12 it reaches, grouped by domain, long names cut before the type's; the dev graph drawn as three cards with Owns, Contributes, WorksAt and the Knows loop named |
| 2026-10-20 | The canvas's lines are dashed as interfaces.dev's borders, at right angles, drawn once where they share a stretch | `paint` tests (`lines_turn_at_right_angles`, `lines_along_one_stretch_share_their_dashes`, `overlapping_lines_are_drawn_once_in_their_strongest_tone`); a 1 px dashed border in a local page captured at 2x in the browser pane; offscreen snapshot of Donald Dijkstra's walk | Chrome: 6 device pixels on, 4 off; Studio: 3 px on, 2 off, crisp on the pixel grid; Donald's four relationship pills hang from one upright each side, rings at every end, the top left reading "From Donald Dijkstra · 1 node" |
| 2026-10-20 | Chips carry interfaces.dev's shadow | Offscreen 2x ontology snapshots on `script/dev-server` (light, pointed, dark) against the site's `x-height` chip at 2x | Out from the top edge, the site reads 240, 238, 239, 246 and Studio 242, 236, 236, 245; from the bottom 234, 231, 235, 245 and 236, 226, 228, 240 (a chip sits at a fractional position, so its ring spreads over three pixels); pointed, the bottom reads 230, 217, 219; dark, the 8 % white ring alone. Before, a chip showed no ring and a broad, soft shadow |
| 2026-10-11 | Layout and edge costs at 1k/10k/50k | research benchmark on this Mac | 2.2/34/254 ms per layout step; 0.14/1.3/8 ms edges per frame |
| superseded | Pan and zoom p95 < 16 ms at 10k nodes, 30k edges | spike S2 | Answered by the rows below: met at the default cap of 2,000, not at 10,000. First pass 2026-10-13, debug build, not yet the claim: branch `perf-10k` of the dev graph (`dev-seed.py --people 10000`, loaded in chunks of at most 8,000 per type, the server's limit), the `knows` query's 20,534 rows drawn as 9,767 nodes, `canvas_max_nodes` 10,000, the snapshot tool's new `measure:120,drag=8:0` and `measure:120,scroll=40`. Pan p95 24.8 ms, zoom p95 25.0 ms, a notify alone p95 1.6 ms. The canvas stayed blank from about 4,000 nodes: a result's roots sit on one ring sized by their count, which at thousands of roots is wider than the view can zoom out to, so the frame time measures per-frame work over every edge with few chips on screen. Needs a release build and a result layout for many components |
| 2026-10-13 | Pan and zoom p95 under 16 ms at the default cap | `release-fast` build (new profile: release without link-time optimization), the snapshot tool's `measure:300`, `measure:300,drag=8:0`, `measure:300,scroll=40` on the `knows` result of `perf-10k`; the paint function's sections timed temporarily | 2,000 nodes: pan p95 25.9 → 15.4 ms, zoom p95 23.5 → 15.1 ms, a notify p95 12.3 ms. Edges took 4.65 ms a frame as curves and 0.40 ms drawn straight from afar (chips are letter marks there, and a curve is dozens of triangles); marks 8.8 → 7.3 ms with each letter shaped once a frame instead of once a chip. Routes cost 0.3 ms. At 9,767 nodes pan p95 is 95.6 ms (was 299) and zoom 134 ms (was 298): the 2,000 cap stays |
| 2026-10-13 | A result with many roots is drawn, not an empty ring's middle | `layout` test `many_result_roots_stand_in_a_landscape_grid_a_view_can_hold`; offscreen snapshots of the `knows` result on `perf-10k` at the default cap (2,000 of 2,409 nodes) and at 9,767 nodes | Before: blank from about 4,000 nodes (and at the default cap for results of many small groups). After: roots past 12 stand in a 16:9 grid and their columns grow beside them; 2,000 nodes drawn with "Too many nodes to draw · 409 not shown". Debug-build frames at 2,000: notify p95 17.9 ms, pan p95 53 ms, zoom p95 67 ms. At 9,767 visible nodes, pan and zoom took 0.7 to 0.9 s a frame and the screenshot failed to render: the cap is what keeps the canvas usable |
| 2026-10-13 | A result's layout grows about linearly | `layout` instrument `explore_layout_time` (debug build, results shaped as the canvas builds them); `free_room_is_found_as_testing_every_block_finds_it` (2,000 random cases against the first implementation) | Before: 0.40 s at 4,000 nodes, 2.44 s at 10,000: each column listed every chip again and tested every candidate start against every chip. After: 39 ms and 198 ms, chips kept as they are placed and room found from merged gaps |
| 2026-10-12 | Schema landing: types on a circle by sorted name, sized by `/snapshot` counts, one arrowed line per edge type, self-loops | offscreen snapshot, `OpenCanvas`, dev graph | Company 25, Person 200, Project 40; Knows drawn as a loop on Person (superseded by the stress layout below) |
| 2026-10-12 (superseded 2026-10-20) | The ontology reads as a map | `layout` tests (determinism, no overlaps, related types nearer than unrelated by 1.5×, hubs central, landscape, fewer crossings and shorter edges than a ring, unrelated types beside, placed types fixed); a CRM ontology (25 types, 78 relationships) measured locally; the app on that graph | Ring 668 crossings / 78.7k px edges; map 117 / 26.5k; fit shows every type named |
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
| 2026-10-19 | The canvas is named Canvas; a cut chip opens; a node is named by the property its graph chose | `graph_canvas` `an_opened_name_wraps_into_even_lines`; `gq_language` `reads_types_keys_and_properties` (a chosen text property names a node, a list, a date or a missing one does not); `graph_store` `answers_kept_under_one_schema_are_not_given_under_another` (a choice in the settings renames at once with one `SchemaChanged`, a schema read again keeps it, none goes back to the key); `studio_settings` `views_and_node_names_are_saved_per_graph_and_type`; workspace clippy `-D warnings` and suite; offscreen glass snapshots on a scratch dev server with Person named by `email`: a person explored, a hub opened, a 35-character email at rest and pointed at, the chip menu, the table's Views menu, the settings tab searched for "node" | The sidebar row, mode switch and tab read Canvas. Neighbours with an email showed it, the rest their key. The 35-character email opened on one line over its neighbours from the chip's top left; an earlier wrap at 280 px left "m" alone on a second line, so lines are now of even length and the width 320 px. "Show as ›" in the chip menu, "Name rows by ›" in Views, and "Node names · 1 changed" in settings with Person's dropdown on "email" and Reset. Chips and hubs now take the `sm` corner rather than round ends |

## Corrections

- 2026-10-13 · A resize redrew the whole window on the next frame (`window.refresh`), which
  turns off every cached tab and dock, on each frame of a dock's glide. It now asks for the
  canvas view alone (`request_animation_frame`).

- **Dotted lines froze the canvas (2026-10-13).** The dash walk carried its place in the
  pattern from piece to piece in f32. When the step left in a dash fell below the resolution
  of the distance walked (phase 1.9999995 of 2 at 8.14 px into a piece), the walk stopped
  advancing and pushed empty dashes until memory ran out: expanding 20 marked nodes reached a
  159 GB footprint before the process was killed. Curves, which bend every few pixels, hit it
  often (3 of the first 20 random three-point lines in an f32 emulation). Dashes are now
  placed at whole periods along the line in f64; the same expansion peaks at 220 MB.

- **Ontology layout (2026-10-12; superseded 2026-10-20, below).** Types on a circle by name gave position no meaning: on a
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
- **Ontology layout (2026-10-20).** On a 101-type ontology with 555 relationships (445
  related pairs, four types related to a third or more of the rest) the stress map was a
  tangle of diagonal lines, and fitted to the window every type was a letter mark (Ragnor:
  "too messy", "the zoomed out view is useless with these random letters and lines
  everywhere"). A first answer kept the map and laid it out in columns with right-angle,
  dashed lines (a layered layout after Sugiyama, Tagawa and Toda 1981: columns by least total
  distance with related types never sharing one, waypoints shared per hub so its lines ran as
  one trunk, barycentre ordering, a least-squares placement by pooling adjacent violators, and
  the fewest uprights per channel, a smallest vertex cover), with faint lines drawn in full
  under the pointer. It was orderly, and still a web: Ragnor found it ugly and still messy,
  and the fit still showed letters. The ontology became a page of domains and of one type at
  a time (above), and Ragnor asked for it to be a page apart from the canvas, which it shares
  little with. The layered layout draws a small schema's map, every line on its own track;
  its trunk mode, which only a large map used, was removed. Grouping the 101-type schema
  (structure taken, names replaced) gave the four shared types and nine domains of 4 to 18
  types that name recognisable parts of it (money, work, learning and events, health,
  automation, commerce, investments, equity); grouping three levels deep merged them into
  groups of 23 and 32, so it stops at two.
- **Rings off their lines (2026-10-21).** A ring's centre was the line's end moved half a
  ring along the last piece, in the direction `f32::signum` gave each axis; the signum of 0
  is 1, so at a level line's end the ring also moved half a ring (3 px) down, off the line
  (Ragnor: "the circle is still misaligned"). One function (`ring_centre`) now moves it along
  the piece only. An opened chip also kept the cut chip's line ends, at its first line, and
  the cut chip was still drawn under the card; it is now laid out before the lines, which meet
  its sides at its middle, and drawn once ("when it expands it should still center").
- **A changed cap did not reach an open canvas (2026-10-21).** The canvas read
  `canvas_max_nodes` only when it built a scene and did not watch settings, so after a change
  the banner named the new limit over chips drawn under the old one. The canvas now watches
  settings and builds again when the cap differs from its drawn scene's, which records the cap
  it was built under for its banner. Show on canvas also built the explorer's scene twice, once
  only to ask whether the node was in it; a node already drawn is now laid out from that scene,
  and one not drawn is left to the exploration's change, which lays it out.
- **Ontology cut off (2026-10-20).** The focus view's columns were fixed (300, 72, 340, 72 and
  300 px) and centred, so on a page narrower than their 1,084 px (a smaller window, or a
  panel open beside it) both sides ran off the page's edges and their type names were cut
  (Ragnor: "the ontology nodes are cut off"); an enumeration's values ran out of the card
  on one line, and a relationship's name had 40 % of a 300 px row. The sides now take the
  page's width between two bounds and the view stacks below them, enumerations wrap under
  their property, and type and name share a row half and half. The page's width is read for
  every view, not only the domains. The map's cards were 180 px whatever their names and
  could cut one; they are sized to it now. The page's body began flush under its header, and
  its scrolling clipped the top of the card in focus, its ring and shadow (Ragnor: "its cut
  off at top"); the body keeps 8 px above its first row.
- **Canvas lines (2026-10-20).** The explorer's and a query result's lines are dashed 3 on 2
  off, as Chrome draws a 1 px dashed border (measured at 2x: 6 device pixels on, 4 off), and
  turn at right angles: out of a node's side, turning 18 px beyond it (within the narrower of
  the gaps the layout leaves), into the neighbour's side, so a column of neighbours hangs from
  one upright. Each end is a small ring filled with the ground, as interfaces.dev ends its
  connectors; a closed hub hangs on a dotted line (1 on 2 off). A dash sits at a whole number
  of periods from the world's origin on screen, so lines sharing a stretch share their dashes,
  drawn once in the strongest tone among them.
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
- **Starting a walk (2026-10-12).** The canvas, before a node is chosen, says how to start:
  Search nodes (⌘P, which roots the exploration at the node picked) or G on a row in any table
  (`studio_table::ExploreRow`, and in its footer hints). The
  inspector's node header has an Explore button. Until 2026-10-20 the canvas was also the
  ontology, a face chosen with a segmented switch and saved with the tab (`"ontology": bool`,
  now ignored); its top left now says what is drawn (the walk's root and its nodes, or the
  query), beside the search and zoom pills.
- **Relationships first.** A relationship with at most one target is a hub on the canvas like
  any other ("→ WorksAt · 1"), so expanding a node shows its relationships before the nodes
  they reach; the tree still shows it inline, as a field.
- **Fit** waits for the canvas's own bounds and fits again when they change (a dock opening),
  until the person moves the view by hand; Fit hands the camera back to fitting.
- **Hit-testing** is a linear scan over at most the node cap (2,000 by default), not an
  R-tree.
- **Hub labels** sit between the hub and its node, on a backdrop, so they never cover the fan
  of neighbours.
- **Look (2026-10-12):** every node, hub and "+K more" is a chip, a white pill with a
  hairline and a soft contact shadow (since 2026-10-20 interfaces.dev's ring and two drops),
  with a letter mark (the label's initial in white on one of eight muted tints, chosen by a
  hash of the type's name) and a muted count; lines are 1-pixel greys (since 2026-10-20
  dashed, at right angles, each end in a ring: Corrections, "Canvas lines"), 16%, a hub's line
  to its node 22%, its lines to its neighbours 13%, the pointed chip's 45% and the selected
  chip's 28%, dotted while the hub is closed (the canvas stopped fading under the pointer on
  2026-10-19, at Ragnor's request, for now); the selection carries a warm ring; a faint dot
  grid lies behind. Chips grow with the view to their size at zoom 1 and become marks alone
  from afar. Overlay: what is drawn, a "Search nodes…" pill (⌘P) and the zoom controls on
  top, what a click does bottom left. Colour is kept to node types: relationships show a
  neutral arrow wherever a type is listed.
- **Keys:** ←↑→↓ (`canvas::MoveLeft` …, nearest node in a 45° cone, `along + 2 × across`),
  `0`, `=`/`+`, `-`, ↵ expand, ⌫ collapse, Space peek, F focus on the selected node's
  neighbourhood, `[`/`]` step back and forward along the nodes walked. Bound in the `Canvas`
  context of `assets/keymaps/`. Scroll and pinch zoom, drag pans.
- **The cap exit** offers "Open as table" for the type with most hidden nodes and "Show in
  tree" (the Explore panel). The cap is the `canvas_max_nodes` setting, per graph if set.
- **Query results** open as a canvas from the result header's Canvas button
  (`studio_graph::ShowResultsOnCanvas`): each node once, a line between the nodes of a row.
