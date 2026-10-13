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
| Node | type colour dot, label property; one short edge value at the end ("role lead") when it fits whole; the key on the root and the selected row; in review mode `+ ~ −` | its relationship groups |
| Relationship group | `→` or `←`, edge name, other type (muted), count; zero-count groups dimmed or hidden (toggle) | neighbour nodes, 50 at a time |
| Field | an edge with cardinality at most one, shown on its node as `manager → Charles` | nothing (Enter follows it) |
| Value | one value of the property the group is grouped by, and how many neighbours have it; "No value" last | the neighbours with that value, paged like a group |
| More | "Show 50 more of 418 · Filter…" | the next page; a filter box appears above 50 |
| Cycle | a node already on its own path, "↺ already above" | Enter jumps to the ancestor |

Edge properties: the first one shows at the end of the neighbour row when it fits whole
("hours 291"); hover shows the type, key and all of them. Only the root carries a lettered type
mark: each relationship already names the type it leads to, so its nodes carry a small dot in
the type's colour. Rows are the sidebar row height (`ds::size::SIDEBAR_ROW`, 26 px). The
panel's title row holds its actions: hide relationships with none (shown when some are empty)
and Collapse all. Typed filter text shows as a field above the rows, with `esc` to clear.

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

- Neighbour pages: keyset by `@id`, `studio.tree_page_size` per page (50 by default; a graph
  can have its own, spec 002), with edge properties through an edge binding.
- Grouping: a relationship group's button (shown on hover or selection) groups its neighbours
  by a property of the other type or of the edge: text, enum, Bool, integer or date, never the
  key, a list or a vector. The group row reads "by title"; it opens to one row per value from one
  grouped count read (`return { $m.title as value, count($m) as n }`, most common first, at most
  100 values), then "N more in other values" when the read was cut. A value opens to its
  neighbours, read with `$m.title = $value` (or `is null`) in the same keyset pages. The grouping
  belongs to the relationship (edge, direction, other type), so it applies wherever that
  relationship appears in the tree, survives re-rooting, and ends from the same menu. The canvas
  draws a grouped relationship as its hub, with the open values' neighbours on it.
- The menu also groups by a relationship of the other type ("people Ada knows, by company"):
  its last section, "Person's relationships", lists them as `worksAt →`. One read counts the
  neighbours per node at the other end (`return { $v.@id as value, $v.name as label, count($m)
  as n }` over the two hops), and a second, at the same commit, counts the neighbours with none
  (`builder::neighbours_without`). Each value reads as its node's label; the neighbours with none
  read "No Company", last, and only when there are any. A value opens to its neighbours narrowed
  with `exists { $v: Company  $m worksAt $v  $v.@id = $value }`, or `not { … }` for none. A
  neighbour related to two values is under both, so the values' counts can add up to more than
  the group's.
- The whole tree reads one commit; when the branch moves, "Newer data · Refresh" appears.

### Rendering and state

- Expanded paths live in `Exploration` (graph_store), shared with the canvas (spec 006).
- Visible rows are flattened into one list and rendered with `uniform_list`, indent guides, and
  sticky ancestor rows so the path stays visible however deep the walk.
- Each expansion loads lazily: an inline spinner row, or an error row with Retry.
- Reads that finish together are shown in one go: `ExplorationEvent::Loaded` is gathered for up
  to 8 ms before the rows are rebuilt, and an answer for a closed row sends nothing. Opening,
  closing and re-rooting still rebuild at once (`Changed`).
- A closed row the pointer rests on for 120 ms reads ahead what it opens to, as the canvas's
  pills do (spec 006), so it usually opens with its rows already read.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Path tree, cycles marked | De-duplicated node tree | A graph is not a tree; the path is what the person walked |
| Single-cardinality edges as fields | A group with one row | Saves a level for the commonest case |
| Counts per edge type by query | Wait for a neighbourhood route | Works on 0.13; the route is a deferred engine change |

## Deferred

- Engine change E16 (neighbourhood counts per node), [spec 012](../012-omnigraph-engine-changes/plan.md). Until then, one count query per
  edge type, run concurrently and cached for the pinned commit.
- Grouping by floats and datetimes, which need buckets: GQ 2.1 has no function that truncates a
  date or rounds a number, so each bucket would be a read of its own.
- Showing the same model as columns in wide windows (node | relationships | nodes | …): the
  tree lives in a fixed-width dock panel, and the canvas covers the wide view.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Tree primitives exist in Zed `ui` | `git ls-tree -r v1.23.2 crates/ui/src/components` | `tree_view_item.rs`, `indent_guides.rs`, `sticky_items.rs` |
| 2026-10-13 | Count queries per edge type stay under 100 ms on a 1M-edge graph | `script/dev-seed.py --people 230000` (305k nodes, 1.08M edges: Knows 460k, Contributes 346k, WorksAt 230k, Owns 46k) loaded into a second `script/dev-server` (0.13, local file store, Apple M5 Pro); each `studio_degree` query six times, the first cold; the table's `studio_page_degrees` for a 100-row page the same way | Tree counts: 17–39 ms warm, 23–38 ms cold, for out and in groups of Person, Company and Project. A page's relationship column: 34–91 ms warm median, 112 ms worst, for `knows`. A full `count` over Knows: 334 ms |
| 2026-10-12 | `builder::degree`, `builder::neighbours` and the new `builder::node_label` run on 0.13 and a later read accepts `snapshot` | HTTP probe against `script/dev-server` (0.13, port 8802); `cargo test -p gq_language` type-checks every builder | `count($m) as n` returns one row (0 when empty); null edge properties are omitted from rows; `snapshot` reads echo `target.snapshot` |
| 2026-10-12 | Rows: root, groups with counts, zero-count groups dimmed and closed, neighbours with edge properties, indent guides | offscreen snapshot, Person `person-000000` and Company `company-00000` | Contributes 1, Knows 4, WorksAt 1, ← Knows 1; `Contributes 0` dimmed without a chevron |
| 2026-10-12 | Paging: 50 per page with "Show N more of M"; ↵ on it loads the next page and keeps the cursor in place | snapshot after appending 100 `WorksAt` edges to `company-00000` (103 in all) | first page then "Show 50 more of 55" (parallel edges show as one row, see Corrections) |
| 2026-10-12 | Cycles: a node already on its path is a "↺ already above" row; ↵ jumps to the ancestor | snapshot Company → ← WorksAt → Hedy Turing → WorksAt | "↺ Sammet Systems · already above" under Hedy Turing's WorksAt |
| 2026-10-12 | Sticky ancestor rows with Zed's `ui::sticky_items` and `ui::indent_guides` in a `uniform_list` | snapshot scrolled 51 rows down | root and "← WorksAt Person 103" stay pinned above the scrolled rows |
| 2026-10-12 | F re-roots and keeps expansions below; the old root becomes the breadcrumb; ⌥→ expands every group | snapshot Person → Contributes → Project Fable 36, `RerootHere`, `ExpandAll` | breadcrumb "Radia Knuth ›"; Contributes 13 and Owns 1 open |
| 2026-10-12 | A missing root is an error row with Retry, never an empty tree | snapshot `OpenNode` Person `nobody` | "No Person with key nobody at this commit." · Retry |
| 2026-10-12 | Flattening, cycle detection, filtering with ancestors, paging state | `cargo test -p graph_store -p relationship_tree` | 9 + 4 unit tests pass |
| 2026-10-12 | ⌘↵ on a group opens the related table | offscreen snapshot: `OpenNode` person-000000, ↓, ⌘↵ | The left dock switched to Explore; Project opened with "← contributes person-000000", 1 of 40 (project-00036, as the tree shows) |
| 2026-10-12 | Group search reads the server | `gq_language` `relationship_queries_compile_in_both_directions` with a search; `rows` tests keep the "Show more" row under a kept group and hide empty groups | Pass |
| 2026-10-12 | A calmer tree: one type mark, names first, no clipped text | offscreen snapshots before and after: Radia Knuth with every group open, Sammet Systems three levels deep, and filtering by "ed" | Before: a lettered badge on every row, keys and edge properties cut mid-word ("…t-00036 hours…"), the eye toggle and the filter on two loose lines. After: dots, labels, "hours 291" and "role lead" at the row ends, the key only on the selected root, one filter field, actions in the title row |
| 2026-10-12 | Grouped counts by a node or edge property, nulls as their own group | HTTP probe on the dev server: `company-00000`'s ← WorksAt by `$m.title` and by `$e.role`, `person-000000`'s Knows by `$m.born` | Rows such as `{"value": "Engineer", "n": 2}`; neighbours without a value come back as one row with no `value` |
| 2026-10-12 | Group by: values with counts, a value opens to its neighbours, "No value" last; ending it closes the values and keeps the selection on the group | `gq_language` `neighbours_group_by_a_property_of_either_end_and_narrow_to_one_value` (every choice of every dev relationship compiles, with and without a value); `graph_store` `a_grouped_relationship_lists_its_values_and_each_opens_to_its_neighbours`; offscreen snapshots: Sammet Systems → ← WorksAt → group-by menu → title, then the canvas | Menu: No grouping, Person (name, title, email, born), WorksAt (role, since). Rows: Engineer 2, Designer 1, Manager 1, Researcher 1, No value 2; Engineer and Designer opened to their people; the canvas hub showed the three open people and "+4 more" |
| 2026-10-12 | Hover cards with every edge property | `relationship_tree` `labels_read_well` (`edge_value`); offscreen snapshot: `OpenNode` person-000000, WorksAt opened, hover on Allen Labs | The card read "Allen Labs · Company", `company-00002`, then "→ WorksAt" with role lead and since 2000-06-09, names aligned |
| 2026-10-12 | "Newer data · Refresh" after Studio's own write | offscreen snapshot on the scratch branch `drafts-test`: `OpenNode` person-000000, a title edited in the Person table, then the Explore panel | The tree, pinned to its read, showed "Newer data · Refresh" above Radia Knuth |
| 2026-10-13 | Grouping by a relationship of the neighbour | `gq_language` `neighbours_group_by_a_property_of_either_end_and_narrow_to_one_value` (every relationship choice of every dev relationship compiles: values, none-count, narrowing to a value and to none; a relationship of another type is refused); `graph_store` `neighbours_grouped_by_their_own_relationship_carry_labels_and_skip_an_empty_none`; offscreen snapshots: Radia Knuth's Knows by `worksAt →`, Allen Works opened, then Yukihiro Dijkstra's; the same reads on the dev server | Menu sections Person and "Person's relationships"; Radia's 4 people under 6 companies (two work at two), Allen Works opened to Leslie Dahl; Yukihiro's under Allen Works 1 and "No Company" 3, which opened to Frances Liskov, Tim Hopper and Claude Berners-Lee, as the server answers |
| 2026-10-13 | At-most-one edges show as fields on their node | a scratch cluster from `dev/cluster` plus `edge ReportsTo: Person -> Person @card(0..1) { since: Date? }`, the dev seed and 170 `ReportsTo` edges, served by `script/dev-server` on another port; offscreen snapshots of `OpenNode` person-000006 and person-000003 | person-000006 read "reportsTo → Donald Dijkstra" (its manager, person-000001) with the type's dot, between the groups; person-000003, who reports to no one, read "reportsTo → —"; the incoming side stayed a group, "← ReportsTo 4" |

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
- **Key bindings** were first registered in `relationship_tree::init` with `cx.bind_keys`; they
  now live in `assets/keymaps/` (contexts `Tree`, `Tree && !filtering`, `Tree && filtering`).
- **Built since:** Space peeks (`studio_graph::PeekNode`: the inspector only, focus stays in
  the tree); ⌘↵ opens the node's table filtered to its id, or on a relationship group the other
  type's table filtered to "related to" this node (spec 004's relationship filter); the eye
  toggle (or `h`) hides groups with none, shown when a group is empty; with text typed, a
  group not read in full offers "Find “text” among N", which reads that group again on the
  server narrowed to ids starting with or labels containing the text (Esc clears both).
  `OpenNode` from the table, palette or canvas now reaches the tree: the table's handler
  propagates, so Explore shows and the tree reveals the node. A node row's hover card shows
  its label, type and key, then under the relationship that led to it (`→ WorksAt`) every
  property of that edge, one per line; a row shows only the first, when it fits.
- **Fields seen:** the dev schema has no at-most-one edge, so fields were checked on a scratch
  copy of it with `edge ReportsTo: Person -> Person @card(0..1)` (validation log). A field with
  no target reads "—", muted, with no type mark.
