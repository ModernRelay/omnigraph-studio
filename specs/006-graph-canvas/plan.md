# 006: Graph canvas

Status: draft · 2026-10-11

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

- GPU compute layout above 50k nodes (spike S3, later).
- Saved layouts per graph.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Layout and edge costs at 1k/10k/50k | research benchmark on this Mac | 2.2/34/254 ms per layout step; 0.14/1.3/8 ms edges per frame |
| pending | Pan and zoom p95 < 16 ms at 10k nodes, 30k edges | spike S2 | |

## Corrections

None yet.
