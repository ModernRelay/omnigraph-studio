# 005: Relationship tree

Status: draft · 2026-10-11

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

- One-call neighbourhood counts (`docs/deferred-engine-changes.md`).
- Grouping neighbours by a property ("by company").

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Tree primitives exist in Zed `ui` | `git ls-tree -r v1.23.2 crates/ui/src/components` | `tree_view_item.rs`, `indent_guides.rs`, `sticky_items.rs` |
| pending | Count queries per edge type stay under 100 ms on a 1M-edge graph | measure against a 0.13 server | |

## Corrections

None yet.
