# 005: Relationship tree

Status: first slice implemented · 2026-10-12 (`crates/relationship_tree`, `graph_store::exploration`)

## Problem

People want to walk the graph without drawing it: a node, its relationships, the nodes at the
other end, their relationships, as deep as needed. A canvas gets crowded after two hops; a tree
does not.

## Principles

II (the tree reads one commit), V (rows do not jump), VIII (keyboard), IX (paged groups).

## What exists today (checked)

- GQ at `v0.13.0`: directed traversal `$a worksAt $b`, edge binding `$a $e:worksAt $b` with edge
  properties, `$n.@id` filters, `count($m) as n` in `return`, `order`, `limit`. A single-hop,
  filter-free `count { }` block over a directed edge is answered from the graph index's degree
  (`docs/user/queries/index.md`, "Correlated blocks").
- Edge shapes from `compile_schema_shape`: name, from type, to type, cardinality, properties.
- Zed `v1.23.2` `ui` components: `tree_view_item.rs`, `indent_guides.rs`, `sticky_items.rs`;
  rows rendered by GPUI's `uniform_list`.
- Tower's canvas exploration (its spec 081): hubs per edge × direction × other type with counts,
  8 neighbours then "+K more", each node drawn once.

## Design

### Rows

| Row | Shows | Opens to |
|---|---|---|
| Node | type color dot, label property, key; in review mode `+ ~ −` | its relationship groups |
| Relationship group | `→` or `←`, edge name, other type, count; zero-count groups dimmed or hidden (toggle) | neighbour nodes, 50 at a time |
| Field | an edge with cardinality at most one, shown on its node as `manager → Charles` | nothing (Enter follows it) |
| More | "Show 50 more of 418 · Filter…" | the next page; a filter box appears above 50 |
| Cycle | a node already on its own path, "↺ already above" | Enter jumps to the ancestor |

Edge properties appear muted on the neighbour row ("since 2021"); hover shows all of them.

### Keys (context `Tree`)

↑/↓ move; → expand or step into the first child; ← collapse or go to the parent; ↵ open in the
inspector; Space peek; ⌘↵ open a table filtered to this node; ⌥→ expand every relationship of
this node; ⌥← collapse the subtree; F re-root here (the old path becomes the breadcrumb); G show
on the canvas. Typing filters the visible rows.

### Data

- Group counts: one count query per relationship of the node's type, run concurrently, pinned to
  the tree's snapshot and cached for it. Sketch:

  ```
  query n($id: String) {
    match { $p: Person  $p.@id = $id  $p worksAt $c }
    return { count($c) as n }
  }
  ```

- Neighbour pages: keyset by `@id`, 50 per page, with edge properties through an edge binding.
- The whole tree reads one commit; when the branch moves, "Newer data · Refresh" appears.

### Rendering and state

- Expanded paths live in `Exploration` (graph_store), shared with the canvas (spec 006).
- Visible rows are flattened into one list and rendered with `uniform_list`, indent guides, and
  sticky ancestor rows so the path stays visible however deep the walk.
- Each expansion loads lazily: an inline spinner row, or an error row with Retry.
- Wide windows can show the same model as columns (node | relationships | nodes | …).

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Path tree, cycles marked | De-duplicated node tree | A graph is not a tree; the path is what the person walked |
| Single-cardinality edges as fields | A group with one row | Saves a level for the commonest case |
| Counts per edge type by query | Wait for a neighbourhood route | Works on 0.13; the route is a deferred engine change |

## Deferred

- Engine change E16 (neighbourhood counts per node), [spec 012](../012-omnigraph-engine-changes/plan.md). Until then, one count query per
  edge type, run concurrently and cached for the pinned commit.
- Grouping neighbours by a property ("by company").

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Tree primitives exist in Zed `ui` | `git ls-tree -r v1.23.2 crates/ui/src/components` | `tree_view_item.rs`, `indent_guides.rs`, `sticky_items.rs` |
| pending | Count queries per edge type stay under 100 ms on a 1M-edge graph | measure against a 0.13 server | |
| 2026-10-12 | `builder::degree`, `builder::neighbours` and the new `builder::node_label` run on 0.13 and a later read accepts `snapshot` | HTTP probe against `script/dev-server` (0.13, port 8802); `cargo test -p gq_language` type-checks every builder | `count($m) as n` returns one row (0 when empty); null edge properties are omitted from rows; `snapshot` reads echo `target.snapshot` |
| 2026-10-12 | Rows: root, groups with counts, zero-count groups dimmed and closed, neighbours with edge properties, indent guides | offscreen snapshot, Person `person-000000` and Company `company-00000` | Contributes 1, Knows 4, WorksAt 1, ← Knows 1; `Contributes 0` dimmed without a chevron |
| 2026-10-12 | Paging: 50 per page with "Show N more of M"; ↵ on it loads the next page and keeps the cursor in place | snapshot after appending 100 `WorksAt` edges to `company-00000` (103 in all) | first page then "Show 50 more of 55" (parallel edges show as one row, see Corrections) |
| 2026-10-12 | Cycles: a node already on its path is a "↺ already above" row; ↵ jumps to the ancestor | snapshot Company → ← WorksAt → Hedy Turing → WorksAt | "↺ Sammet Systems · already above" under Hedy Turing's WorksAt |
| 2026-10-12 | Sticky ancestor rows with Zed's `ui::sticky_items` and `ui::indent_guides` in a `uniform_list` | snapshot scrolled 51 rows down | root and "← WorksAt Person 103" stay pinned above the scrolled rows |
| 2026-10-12 | F re-roots and keeps expansions below; the old root becomes the breadcrumb; ⌥→ expands every group | snapshot Person → Contributes → Project Fable 36, `RerootHere`, `ExpandAll` | breadcrumb "Radia Knuth ›"; Contributes 13 and Owns 1 open |
| 2026-10-12 | A missing root is an error row with Retry, never an empty tree | snapshot `OpenNode` Person `nobody` | "No Person with key nobody at this commit." · Retry |
| 2026-10-12 | Flattening, cycle detection, filtering with ancestors, paging state | `cargo test -p graph_store -p relationship_tree` | 9 + 4 unit tests pass |

## Corrections

- **Group order.** `compile_schema_shape` returns edge types sorted by name, so groups list
  outgoing first, then incoming, each alphabetical, not in `.pg` source order.
- **Counts are edges, rows are nodes.** The degree query counts edges, so parallel edges to one
  neighbour count twice; the tree shows one row per neighbour (the first edge's properties) so
  every path stays unique. Keyset paging by `@id` alone can skip a parallel edge that falls on a
  page boundary. "Show N more of M" is therefore approximate when parallel edges exist.
- **F, G and typing.** Typing filters the rows, which collides with F and G. They are bound in
  `Tree && !filtering`: they act while the filter is empty and type once it is not. Escape
  (`tree::ClearFilter`) clears the filter.
- **`OpenNode` is shared.** GPUI stops at the first bubble handler that does not propagate. The
  tree's workspace handler reveals the node and calls `cx.propagate()`; a crate whose handler
  runs earlier and does not propagate should call `Exploration::reveal` itself. The exploration
  lives in `graph_store`, so no feature crate depends on another.
- **Nothing focused.** With no focused element, GPUI dispatches actions from the window's root
  dispatch node, above the workspace's `register_action` handlers, so window actions do nothing
  until something has focus. The tree takes focus when a graph opens and nothing else holds it.
  The frame (spec 002) should own this.
- **Key bindings** are registered in `relationship_tree::init` with `cx.bind_keys`; they still
  need entries in `assets/keymaps/`.
- **Not built yet:** Space peek, ⌘↵ table filtered to the node, the zero-count toggle, the
  filter box above 50 rows, hover cards with every edge property (a tooltip shows them), the
  "Newer data · Refresh" chip is wired to `SessionEvent::Committed` but not seen in a snapshot,
  and fields (at-most-one edges) are covered by unit tests only: the dev schema has none.
