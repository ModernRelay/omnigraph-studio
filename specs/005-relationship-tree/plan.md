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
- An earlier web client's canvas exploration: hubs per edge × direction × other type with counts,
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

- Group counts: one count per relationship of the node's type, pinned to the tree's snapshot
  and cached for it. Counts asked for in the same turn (several nodes expanded at once, a
  refresh, a restored exploration) go out together: one `builder::page_degrees` read per node
  type and relationship for up to 100 nodes, sorted by id (`graph_store::degrees`,
  `GraphSession::degree_in`), so a node's peek and the tree share the read at the commit. GQ has
  no single query that counts several relationship groups. Sketch of one count:

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
- The whole tree reads one commit; when the session sees the branch's head elsewhere (spec 003:
  Studio's commit, or another writer's at the next poll), "Newer data · Refresh" appears. It is
  read and flattened under one schema too, the one its reads were made under: another writer's
  schema offers Refresh as a head move does, and the groups stay as read (a dropped
  relationship's still there, an added one not yet) until Refresh takes the new schema at the
  head (since 2026-10-20). ⌘R where the tree has the keyboard is that Refresh, whose tooltip
  shows the key, and goes on to the workspace's reload of the graph (since 2026-10-21).

### Rendering and state

- Expanded paths live in `Exploration` (graph_store), shared with the canvas (spec 006).
- Visible rows are flattened into one list and rendered with `uniform_list`, indent guides, and
  sticky ancestor rows so the path stays visible however deep the walk. The panel scrolls
  itself (`Workspace::set_panel_scrolls_itself`): its host gives it the dock's size, and the
  list takes what its header leaves, in whichever dock and layout, so the row the keyboard
  selects scrolls into what shows.
- Each expansion loads lazily: an inline spinner row, or an error row with Retry.
- Reads that finish together are shown in one go: `ExplorationEvent::Loaded` is gathered for up
  to 8 ms before the rows are rebuilt, and an answer for a closed row sends nothing, reads
  nothing more and leaves the flattening as it was made, so it walks no tree (since
  2026-10-21). Opening, closing and re-rooting still rebuild at once (`Changed`).
- A closed row the pointer rests on for 120 ms reads ahead what it opens to, as the canvas's
  pills do (spec 006), so it usually opens with its rows already read: the first 8 reads it
  needs (`PREFETCH_LIMIT`, one connection's reads on the wire), in the order its rows show.
  One row is read ahead at a time: the pointer leaving it, or resting on another, stops what
  is still out unless the row was opened since, and that is read again if it opens later
  (since 2026-10-21; before, a Person's 92 counts went out for every row rested on and ran to
  the end). A read stopped before it answered shows nothing, so it makes no change count
  toward "Newer data" and the feed is not asked about its types (spec 015's read scope), and
  it takes no answer's place in the session's memo (since 2026-10-22, spec 015).
- What the exploration holds stays bounded: a finished read lets go of its task, and once it
  holds more counts, neighbour lists, values and labels than its bound, those no open path
  reaches (rows read ahead and never opened, what was below a subtree closed whole) are let
  go, to be read again at the pin, mostly from the session's answers kept. The bound is 4,096,
  or twice what the last letting go kept when more stayed, so a large walk that is all open
  is not gone through at every answer; it is 4,096 again whenever the walk starts over (a new
  root, a re-root, going back along the breadcrumb, Refresh, a new token), and a re-root or
  going back lets go at once of what the new root cannot reach (since 2026-10-21).
- The pin moves past another writer's commit that changed nothing the tree reads (spec 015);
  a read still out at the old pin answers there and stands, since nothing it reads changed in
  between. An answer at a commit the pin never was at is still a failure (since 2026-10-21).
- A new token (`omnigraph login`, or one typed in) may see less than the old: everything the
  tree read is let go at once and read again under the new token at the same commit, paths
  left open or closed as they were (since 2026-10-21).

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Path tree, cycles marked | De-duplicated node tree | A graph is not a tree; the path is what the person walked |
| Single-cardinality edges as fields | A group with one row | Saves a level for the commonest case |
| Counts per edge type by query | Wait for a neighbourhood route | Works on 0.13; the route is a deferred engine change |

## Deferred

- Engine change E16 (neighbourhood counts per node), [spec 012](../012-omnigraph-engine-changes/plan.md). Until then, one count read per
  edge type for the nodes asked for together, cached for the pinned commit.
- Grouping by floats and datetimes, which need buckets: GQ 2.1 has no function that truncates a
  date or rounds a number, so each bucket would be a read of its own.
- Showing the same model as columns in wide windows (node | relationships | nodes | …): the
  tree lives in a fixed-width dock panel, and the canvas covers the wide view.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-01 | The tree is a tab, one per root and commit | `relationship_tree` `a_hidden_tree_makes_no_rows_and_a_walk_is_shared_by_root_and_commit`; `graph_store` `a_held_walk_stays_at_its_commit`; an offscreen snapshot beside a table | Spec 020 moves it from the left dock. Re-rooting opens a tab, and Back replaces the breadcrumb |
| 2026-10-22 | Reads left by a read ahead take no answer's place in the memo | `graph_store` `memo::a_read_nobody_waits_for_goes_before_any_answer` (spec 015) | Five reads each left as the next started kept a 2-entry memo's answer, one read held at the end. Before, each stayed until the entry cap let it go after every answer |
| 2026-10-21 | A read out when the pin moved past a commit that changed nothing it reads stands | `graph_store` `an_answer_at_a_commit_the_pin_was_carried_past_stands` (root at c1; another writer changes a Company at c2, so the pin moves to c2; a page and a count answered at c1, sent at c1; a page answered at c9, sent at c1) | The c1 page shows its neighbour and the c1 count 5, with the pin at c2; the c9 answer fails. Before, `pinned_at` refused every answer not at the current pin, so a slow group expanded before another writer's unrelated commit showed "The server answered at commit c1, not at c2" until Retry |
| 2026-10-21 | A new token reads what the tree shows again | `graph_store` `a_new_token_reads_what_the_exploration_shows_again` (a server answering the root by token: the old one sees Ada, the new one nothing; `Connection::set_token`) | At once the label Ada is gone and the root is not loaded; then one node read goes under the new token and the root reads "No Person with key ada at this commit.", the pin still c1. Before, the exploration had no arm for `SessionEvent::CredentialChanged`: what the old token read stayed beside what the new one read |
| 2026-10-21 | Reads ahead are bounded and stop when the pointer moves on | `graph_store` `what_is_read_ahead_stops_when_the_pointer_moves_on` (20 relationships; a server that never answers reads about p2), `what_nothing_open_reaches_is_let_go`; `relationship_tree` calls `Exploration::end_prefetch` when the pointer leaves a row | Resting on p2: 8 counts Loading and 8 reads out; leaving: 0 out, no count kept, no task. p3's 8 answered counts stay after leaving, its tasks gone, the flattening the same `Arc`; resting on p4 stops p2's still out. The unreachable counts, lists and labels go and the root's, an open group's and one being read stay. Before, all 20 went out, ran on, kept their tasks, and each answer re-flattened the tree |
| 2026-10-21 | A relationship read ahead and left before it answered changes nothing shown | `graph_store` `a_relationship_left_before_it_answered_changes_nothing_shown` (its neighbours held unanswered, the pointer leaves, then another writer changes Company k9 at c2), `scope` `a_list_stopped_before_it_answered_shows_nothing`; the same test with the scope counting every entry, as before | The pin moves to c2 with no Refresh offered. Counting every entry, it stayed at c1 with "Newer data" offered: the stopped page's scope entry had no answer, so every Company counted as shown. A count stopped first now shows nothing either (`GraphSession::degree_in` holds its read open until it answers); a Blob cell still counts from when it is asked |
| 2026-10-21 | A row's hover stays with the row when rows move | `relationship_tree` `a_rows_key_follows_the_row_not_its_place`; the row's element id is a hash of what it is (`row_key`: path, kind, and for loading and error rows what they read), not its index | The same row at two places has one key; a group's count error and page error at one path have two. Before, the id was the index, so after rows shifted under a resting pointer, leaving stopped the new row's reads ahead and not the old one's. Not checked with a pointer in a running window |
| 2026-10-21 | A parked graph's tree reads nothing for new node names until shown | `graph_store` `a_parked_graph_reads_under_new_names_only_when_shown` (`GraphSession::take_labels` naming Person by title while parked) | Parked: no `SchemaChanged`, no read; shown: one `SchemaChanged`, and the root read again under the new names. Before, the settings' change reached the parked graph's tree at once and it read again |
| 2026-10-21 | The bound starts over with the walk | `graph_store` `a_reroot_starts_the_bound_over` (4,196 counts no path reaches under a bound raised to 16,384, then a re-root) | The bound is 4,096 again and the unreachable counts are gone. Before, the bound only rose, so after one large walk the state could grow to twice its size before anything went |
| 2026-10-21 | ⌘R reads the tree again | `relationship_tree` `cmd_r_reads_the_tree_again` (another writer changes the root at c2, the tree offers newer data, then `studio::Refresh` is dispatched with the tree focused) | The rows are read again and the offer goes. Before, nothing in the tree handled `Refresh`: ⌘R reloaded the session at the workspace and "Newer data · Refresh" stayed |
| 2026-10-21 | One count reader | `GraphSession::degree`, used only by tests, removed; the tests read through `degree_at`, which the tree's `degree_in` wraps | The `graph_store` count tests pass on `degree_at`, one of them now also checking that each count names the commit it was asked at |
| 2026-10-21 | A relationship count is held to the exploration's pin too | `graph_store` `every_read_of_an_exploration_is_at_its_roots_commit`, extended: `spawn_count` given a count of 9 at c3, sent unpinned, then sent at the pin c2, then with no pin; `GraphSession::degree_at` returns each count with the commit its read answered at (`ReadOutput.graph_commit_id`, or the commit asked for) | Unpinned and answered at c3: read again at c2 and shown as 0, its answer there; sent at c2 and answered at c3: a failure that names both commits; with no pin (the root's read failed): c3 becomes the pin and 9 shows. Before, `spawn_count` applied any count it was given, so after a failed root read counts read at the head could stand beside pages pinned at another commit |
| 2026-10-21 | Every read of an exploration is at the commit its root pinned | `graph_store` `every_read_of_an_exploration_is_at_its_roots_commit` (a fake holding the root read while a group is searched, then moving the head to c2 before releasing it; `Exploration::apply` given an answer at c3 sent unpinned, then pinned); the same test against the earlier code, by reading: `search_group` called `start(Page)` with no pin while the root was out | Nothing but the root read went out while it was out; after it, the pin was c2 and every count and neighbour read went at c2. The unpinned answer at c3 was read again at the pin (the session's kept answer, so nothing was sent) and its neighbour not shown; the pinned one at c3 failed. Before, the search's page was read at the head beside the root, so a commit between them showed two commits at once |
| 2026-10-11 | Tree primitives exist in Zed `ui` | `git ls-tree -r v1.23.2 crates/ui/src/components` | `tree_view_item.rs`, `indent_guides.rs`, `sticky_items.rs` |
| 2026-10-13 | Counts asked for together are read together | `graph_store` `counts_asked_together_are_read_together` (three nodes' Knows and two nodes' WorksAt asked one by one: two reads, a node without a row counts 0, asked again at the commit: none), `askers_gather_by_type_relationship_and_commit_in_id_order`; offscreen snapshots through a request-counting proxy on Liskov Systems' canvas: mark all, Expand all (its 2 projects and the first 8 of 18 people), mark all, Expand all | The second expansion read 4 `studio_page_degrees` of 8 people and 2 of 2 projects, 6 reads for 36 counts that were one `studio_degree` read each; opening a node still reads its 4 counts once for the tree and the peek together |
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
| 2026-10-20 | The rows fit the tree's dock, and the row the keyboard selects shows | `relationship_tree` `the_rows_fit_the_dock_and_the_selected_row_shows` (a 320 px host in a taller window, as the workspace hosts a panel that scrolls itself; 25 rows of 26 px); the same test with the list at the old `viewport − 94` | The list is 320 px from the dock's top and the last row, selected, is drawn inside it. With the old height the list was 986 px in the 320 px dock (`Bounds … 260px × 986px`), so `scroll_to_item` put the selected row below what shows |
| 2026-10-20 | The tree keeps the schema it was read under until Refresh | `graph_store` `an_exploration_keeps_the_schema_it_was_read_under_until_refresh` (another writer replaces `Knows` with `Follows` at `c2`, seen at a head check); the same test with the rows flattened under the session's schema, as before | Before Refresh: the session reads under `Follows`, the exploration's rows still show `Knows` out and in with their counts (0), and Refresh is offered; after: `Follows` out and in, counted at the head. Flattened as before, the rows were `Follows` out and in with no count (`None`), the dropped `Knows` gone. The canvas's hub menu still reads the session's schema for its grouping choices (`graph_canvas.rs`, `chip_menu`) |

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
