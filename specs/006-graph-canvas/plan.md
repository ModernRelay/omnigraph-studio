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
- Tower's canvas (its spec 081): schema landing; hubs per edge × direction × other type with
  counts; 8 neighbours then "+K more"; each node drawn once; cap at 600 chips with "Too many
  nodes to draw" and a way out; zoom ×1.25 steps clamped to [min(0.2, 0.8 × fit), 2]; arrow keys
  move to the nearest node in a 45° cone, scored `along + 2 × across`.

## Design

- **Landing:** the schema graph (types as nodes, edge types as lines) laid out by type names
  only, with counts from `/snapshot` added after layout. Hovering a type highlights its edges.
- **Exploring:** expanding a node adds one hub per relationship group with its count; opening a
  hub adds 8 neighbours, then "+K more". State lives in `Exploration`, shared with the tree.
- **Rendering:** one custom element. Nodes as quads with labels shown above a zoom threshold;
  edges batched by color in one paint layer with cached geometry; hit-testing through an
  R-tree. Layout runs on a background thread and streams positions; the main thread only paints.
- **Caps:** a legibility cap (start at 2,000 visible nodes, tune with spike S2) with "Too many
  nodes to draw · Open as table · Show in tree".
- **Keys (context `Canvas`):** arrows move to the nearest node; ↵ expand; Space peek; ⌫ collapse;
  `0` fit; `+`/`-` zoom; F focus; `[`/`]` walk the expansion path.
- **Query results:** a query whose result has node or edge columns can open as a canvas.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Custom GPUI canvas | WebView with a JS renderer | WebView embedding needs a GPUI fork; not on Zed's lineage |
| Grow by relationship group | Show the whole neighbourhood | Counts first keep large hubs from flooding the view |

## Deferred

- Engine change E16 (neighbourhood counts per node), [spec 012](../012-omnigraph-engine-changes/plan.md), for hub counts; shared with the
  tree's interim count queries until then.
- GPU compute layout above 50k nodes (spike S3, later).
- Saved layouts per graph.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Layout and edge costs at 1k/10k/50k | research benchmark on this Mac | 2.2/34/254 ms per layout step; 0.14/1.3/8 ms edges per frame |
| pending | Pan and zoom p95 < 16 ms at 10k nodes, 30k edges | spike S2 | |
| 2026-10-12 | Schema landing: types on a circle by sorted name, sized by `/snapshot` counts, one arrowed line per edge type, self-loops | offscreen snapshot, `OpenCanvas`, dev graph | Company 25, Person 200, Project 40; Knows drawn as a loop on Person |
| 2026-10-12 | Exploring shares the tree's `Exploration`: hubs per group with counts, 8 neighbours then "+K more", each node drawn once, selection mirrored | snapshot Company `company-00000` with 103 incoming `WorksAt` | "← WorksAt · 103", eight people, "+95 more"; the tree's selected row is the ringed node |
| 2026-10-12 | Canvas actions change the shared state | snapshot: tree `ShowOnCanvas` (G) on Frances Lovelace, then `canvas::ExpandNode` | Frances gets four hubs on the canvas and is expanded in the tree |
| 2026-10-12 | Layout off the main thread is deterministic, keeps drawn nodes in place, and bounds cost at the cap | `cargo test -p graph_canvas` (`layout::tests`) | 1,999 nodes in 464 ms in an unoptimized debug build, Apple M5 Pro; same input gives the same positions |
| 2026-10-12 | Errors and loading show in the canvas, not a blank view | snapshot `OpenNode` Person `nobody` then `OpenCanvas` | banner "No Person with key nobody at this commit." · Retry |
| 2026-10-12 | Keys walk and focus | offscreen snapshot: Allen Labs expanded, → → `[` F Space | `[` returned to Allen Labs, F centred it, Space showed it in the inspector |
| 2026-10-12 | Query results as a canvas | `gq_language` `whole_node_columns_carry_their_type`; `query_editor` `node_columns_become_a_graph_of_distinct_nodes_linked_by_row`; snapshot of `return { $p, $c }` | The result offers Canvas; a results canvas draws each node once and links the nodes of a row; it is a tab of its own and is not restored |

## Corrections

- **Layout.** A deterministic radial seed (each node fanned away from where the walk came from)
  plus 60 iterations of grid-based overlap relaxation, on `cx.background_spawn`; not `fjadra`.
  Nodes already drawn never move, so expanding does not reshuffle the view. Revisit with spike
  S2 if larger neighbourhoods need a force layout.
- **Hit-testing** is a linear scan over at most 2,000 nodes, not an R-tree.
- **Hub labels** sit between the hub and its node, on a backdrop, so they never cover the fan
  of neighbours.
- **Mode.** "Schema" and "Exploration" toggle in the canvas; the schema view never clears the
  shared exploration.
- **Keys:** ←↑→↓ (`canvas::MoveLeft` …, nearest node in a 45° cone, `along + 2 × across`),
  `0`, `=`/`+`, `-`, ↵ expand, ⌫ collapse. Space peek, F focus and `[`/`]` are not built.
  Bindings are registered in `graph_canvas::init`; they still need entries in
  `assets/keymaps/`. Scroll and pinch zoom, drag pans.
- **The cap exit** offers "Open as table" for the type with most hidden nodes; "Show in tree"
  is not built. Query results opening as a canvas is not built.
