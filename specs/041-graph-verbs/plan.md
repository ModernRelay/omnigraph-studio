# 041: The graph's own verbs: result rows are nodes, stored mutations are node actions

Status: built 2026-11-19 (cookbook study, proposal "The graph's own verbs"; the study numbers
it 5)

## Problem

The cookbooks speak through stored queries, and Studio shows them as text grids and forms that
are not connected to the nodes they are about ([the study](../../docs/ux/2026-11-18-cookbook-study.md),
"The graph's own verbs"; friction log B1, B3, C3, P1, S2, V1).

- A row of beads' `ready` is "Row 3 of 8" with `i.slug` and `i.title`, not the issue: no page,
  no relationships, no menu, no claim (f01). The same in the program's `budget_rollup`, whose
  `violated` column is cut off at the grid's edge with nothing saying there is more (P1).
- Company's `verify_claim` takes `$slug` as free text, with no Claim picker (f14). The claim's
  page has no Verify or Reject: the graph's verbs are rows in the Queries panel.
- A hand edit stamps nothing: beads' `edit` and the program's `revise_requirement` compare and
  swap on `updated_at`, so an agent working from a stale read can overwrite a person's edit made
  in Studio, which left `updated_at` as it was (B3, S3).
- The Queries panel lists 60 to 70 stored queries alphabetically, reads and writes mixed, with
  each description only on hover; a read with no parameters opened from the list waits for Run
  (f04).

## Principles

- I: every value is the server's. A row is a node only when the query names one; Studio never
  guesses which node a value is, and asks the person once where the engine cannot say (E43,
  E64).
- III, IV: a node action is the stored mutation's tab with values filled. It is confirmed with
  the bar that names the branch, sent once, and its outcome is the server's.
- V: results keep their place. A row that becomes a node changes nothing in the grid but what a
  click, ↵ and the menu do.
- VI: each new control shows its key; a node action says which stored mutation it runs; a
  stamped property is named where the setting is.
- VIII: Open the row's node, Node actions and Pin are actions with bindings, in the palette.
- IX: no new read runs per row. Opening rows as nodes reads nothing; a picker reads at most 12
  nodes each time its menu opens, through node search (spec 031).
- X: what waits on the engine is designed against today's API, with the fallback the study
  names: the person maps an action, or a stored query's rows, once per graph (E43, E64, E65).
- XIII: a result row selects its node, and the inspector, the page and the view strip follow.

## What exists today (checked)

Studio at `98c7cea` (origin/main), Omnigraph `v0.15.0`.

- **Result columns.** `gq_language::ResultColumn` gives each column's answer name, kind and,
  for `return { $p }`, the node type (`crates/gq_language/src/check.rs:63`). A projection's
  expression is `Expr::PropAccess { variable, property }` or `Expr::Variable` in the compiler's
  AST, and `executed_column_name` names the answer's column `<var>.<prop>`
  (`check.rs:335-360`).
- **Rows and the inspector.** `ResultTable` keeps `node_types` per column; a cell holding a whole
  node is `Picked::Node`, any other cell `Picked::Row`, which the inspector shows as every column
  in full (`crates/query_editor/src/view.rs:104`, `:3763`). `ResultTable::graph` makes the
  status bar's Tree and Canvas from whole-node columns only (`results.rs:76`). Results are
  read-only and no hidden id is added to the query sent (spec 024, Decisions, 2026-11-09).
- **A keyed node's id.** Omnigraph derives a keyed node's id from its key; a one-column key
  keeps its scalar's spelling (`canonical_key_id`, `crates/omnigraph/src/loader/mod.rs` at
  `v0.15.0`). On a `v0.15.0` server, `return { $i.@id, $i.slug }` over beads' `Issue` answers
  equal values (`lk-sync`, `lk-sync`), and names the columns `i.@id` and `i.slug`.
- **Stored queries.** `GET /graphs/{id}/queries` answers each entry's name, description,
  instruction, `mutation` and typed parameters, never its text, its file or its written type
  (`crates/graph_client/src/api.rs:174`). beads' `claim` takes `id: string`, `close` takes
  `ids: list<string>`, company's `verify_claim` takes `slug: string` (read from the server,
  2026-11-19). A stored query opens in a tab with a field per parameter; `open_stored` takes the
  values to fill (`crates/query_editor/src/query_editor.rs:108`). A stored mutation asks before
  it writes, naming the branch.
- **The Queries panel** lists saved files, then Stored, alphabetically, with "writes" on a
  mutation and the description as a tooltip (`crates/query_editor/src/queries_panel.rs:201`).
- **Per-graph settings** are `GraphOverrides` under `graphs."<target>/<graph>"`
  (`crates/studio_settings/src/studio_settings.rs:225`), written through
  `studio_settings::update`.
- **Hand edits.** Every property write from a table, a page, a peek or the text editor is built
  by `gq_language::builder::update_rows` or `update_cells` and sent by
  `type_table::write::send_bound` (`crates/type_table/src/write.rs`); a new node by
  `builder::insert_node`. The session knows the credential's actor (`GraphSession::actor`).

## Design

### 1. A result row is a node

A column **identifies** a node when it is a whole node (`return { $i }`), its `@id`
(`$i.@id`), or its type's key where the key is one `String` property (`$i.slug` on `Issue`,
whose `slug` is `@key`). Since a one-column key is the node's id (checked above), the cell's
value is the id: Studio reads nothing to find the node.

Every other column **belongs** to a node when it reads a property of a variable whose identity
is in the result (`$i.title` with `$i.slug`). An aggregate or a computed column (`allocated`,
`violated`) belongs to none.

- **Local `.gq` queries:** `gq_language` names each column's variable and whether it identifies
  its node (`ResultColumn::variable`, `ResultColumn::identifies`), from the compiler's AST and
  the schema. Nothing is added to the query sent.
- **Stored queries (until E43):** the server names columns `<var>.<prop>`, but not the type.
  The person says once which type a column names, from the result's status bar: "Rows as
  nodes…" opens a menu with a section per column that could name one, offering the node types
  whose one-property key, or `@id`, the column's name ends in (`i.slug` → Comment, Issue and
  Memory in beads), those named like the column's variable or the query first, and "Rows are
  not nodes". The choice is kept per graph and stored query
  (`graphs."<key>".row_nodes."ready"."i.slug" = "Issue"`), and the button then reads "Rows:
  Issue". The other columns named `<var>.…` belong to it. The button shows only on a stored
  read's result with such a column.

What a row that is a node does, in results of either kind:

- A click on a cell that identifies or belongs to a node selects that node: the inspector shows
  it, read where the rows were (spec 020). Any other cell selects the row, as today.
- ↵ (`studio_query::OpenRowNode`) opens the node's page; a page of rows held at a commit opens
  it at that commit.
- Right-click on a row, or ⌥↵ (`studio_node::ShowActions`), opens the row's menu: Open page,
  Show in tree, Show on canvas, the node's actions (below), Copy cell, Copy row. A row whose
  cell is no node's offers Copy cell and Copy row.
- The status bar's Tree and Canvas take the nodes keyed columns identify too, each named by its
  label property's column in the same row when the result has one.
- **Bool cells** show a state chip: `true` in the tag box (`ds::chip`), `false` muted, null as
  "—". A flag like `violated` reads at a glance.
- **A grid wider than its view** shows a fade at its right edge while columns go on past it
  (`ds::trailing_edge_fade`, painted after layout from the scroll range), so `violated` is not
  lost off the edge (polish, P1); scrolled to the end, the fade goes.

### 2. Node actions from stored mutations

A stored mutation is a **node action** of a type when one of its parameters names a node of
that type: a `String` parameter takes the node's id, a list of `String` takes a list with it.

- **Until E64**, the catalog does not say which type a mutation writes, so the person maps a
  parameter once, in the stored query's tab: beside each `String` or `[String]` parameter, a
  "Names a node…" button opens a menu, "$slug names", listing "a Claim", "an Issue", … (the
  node types whose key is one `String` property, those named in the parameter's or the
  query's name first: `verify_claim` → Claim) and "No node". The choice is kept per graph
  (`graphs."<key>".node_params."verify_claim"."slug" = "Claim"`), and the button then reads
  "Claim".
- A mapped parameter **picks**: the same menu starts with "Pick a Claim", the type's nodes that
  the field's text finds (node search, spec 031: at most 12, read at the branch head), each by
  its name and id; a pick fills the id, or adds it to a list. The field still takes typed text.
  A stored read's parameters are mapped the same way, for their pickers.
- **Where actions show.** A node's page has an Actions menu in its toolbar listing every
  mutation mapped to its type, by its name in words ("Verify claim", from `verify_claim`), each
  with its description as the tooltip and "Runs the stored mutation verify_claim on main". A
  result row that is a node lists the same in its menu. ⌥↵ opens the menu on a page.
- **What an action does** (`studio_graph::RunNodeAction { query, param, type_name, id }`):
  opens the stored mutation's tab with the parameter filled with the node's id (a list with
  it), other mapped parameters as pickers, and runs it, which shows the confirm bar naming the
  branch ("Run on main", ↵, Esc cancels) when every required parameter has a value; otherwise
  the first empty one takes the keyboard. Nothing is written until ↵ (constitution III, IV).
- An action whose mutation the catalog no longer lists is not shown; the mapping stays.

### 3. A catalog shaped by use

- **Pinned** stored queries come first, in the order pinned, under "Pinned", each with a pin
  for its icon. "Pin to the top" and "Unpin" are in a row's menu, and `p`
  (`studio_queries::TogglePin`) pins or unpins the selected row; the selection stays on it.
  Pins are kept per graph (`graphs."<key>".pinned_queries`).
- Then the stored queries **by what they do**, under "Reads" and "Writes" (until E64 gives the
  source file, when they group by file). The first heading shown carries the refresh button
  "Stored" carried; while the catalog is read, has failed or is empty, "Stored" stays.
- Each stored row shows its **description under its name** (`ds::SidebarRow::note`), one line,
  cut at the panel's width; the tooltip keeps the whole, and the instruction after it.
- A **read with no parameters runs when it is opened** from the panel, the palette or a pin
  (once, when its tab is made; a tab restored at start runs nothing).

### 4. Write conventions per graph

`graphs."<key>".stamps` maps a property name to what Studio writes into it on every hand edit
and new node of a type that declares it: `"now"` for a `DateTime` (the server's `now()`) and
`"actor"` for a `String` (the credential's actor, as the access route answers it). beads:
`{"updated_at": "now"}`; the dev graph: `{"updatedAt": "now", "updatedBy": "actor"}`.

- A stamp is added to the `set` of the update Studio builds for an edit, never to a property
  the edit sets itself. It is not bound in the `where`: a stamp is what changed, not what was
  read. A new node takes it unless the person filled it.
- A type that does not declare the property, or declares another type for it, is not stamped.
  An `actor` stamp with no actor known (an access route not answered) is left out.
- Nothing is stamped by default. The settings document's description of `stamps` names both
  values; until E65 the convention is Studio's alone, so CLI and agent writes keep their own.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| A key column is the node's id, so a row needs no read to be a node | A read per result to resolve keys to ids; adding `$v.@id` to the query sent | The engine keeps a one-property key's spelling as the id (checked). Spec 024 rules out hidden ids in sent queries (Ragnor, 2026-11-09), and a read per result costs a request where none is needed |
| Only one-property `String` keys and `@id` identify | Any key, composite keys | A composite key's id is a JSON array of canonical strings, and a non-`String` scalar's canonical spelling is the engine's; neither is worth guessing |
| A stored query's rows are nodes only once the person says so | Guess from the column name; test candidate types against the data | Three of beads' types key on `slug`; a guess that opens the wrong page breaks principle I. The study's fallback for E43 and E64 is a mapping made once |
| Mappings and pins are per graph, in settings | Per server; in the graph | The same stored query name means different things in different graphs; the server has no place for them until E42/E64 |
| A node action opens the mutation's tab and asks there | A dialog of its own; run at once | The tab already has typed fields, the confirm bar naming the branch, outcomes and the activity log; a second form would drift from it |
| Results stay read-only | Editing cells of node rows | Spec 024's rule holds: values are edited in a type's table or the node's page, which ↵ now opens |
| Stamps are a person's setting, applied by Studio | Detect `updated_at` columns; stamp by default | A property named `updated_at` is not always a write stamp; the graph's owner decides, and E65 is where it belongs |
| Reads and Writes groups until E64 | Folders from query name prefixes | The catalog has no file; a prefix guess would mis-group |

## Deferred

- **E43** (spec 012): a stored query's result columns with their variable, type and property.
  *Today:* the person maps a column to a type once per graph.
- **E64** (spec 012, [omnigraph#1474](https://github.com/ModernRelay/omnigraph/issues/1474)): a
  stored query's source file and tags in its catalog entry, and a mutation's written type and
  target parameter. *Today:* Reads and Writes groups; the person maps an action's parameter
  once per graph.
- **E65** (spec 012): properties the engine stamps on every write (a schema annotation such as
  `@stamp(now)` and `@stamp(actor)`), so every writer keeps the graph's rules. *Today:* Studio's
  per-graph `stamps` setting, for Studio's own writes.
- **E40** (spec 012): arithmetic in `return`, so a roll-up's margin is a server column.
- "Watch" and "check" marks on stored queries belong to proposal 1.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | A keyed node's id is its one-property key | `canonical_key_id` at `v0.15.0` (`crates/omnigraph/src/loader/mod.rs`); `POST /graphs/beads/query` `return { $i.@id, $i.slug }` on a `v0.15.0` server serving the beads cookbook | Equal values; columns `i.@id`, `i.slug` |
| 2026-11-19 | The catalog says nothing of a mutation's type or file | `GET /graphs/beads/queries`, `GET /graphs/company/queries` on `v0.15.0` | Entries carry `name`, `tool_name`, `description`, `instruction`, `mutation`, `params` only |

| 2026-11-19 | A result row that returns a key is its node; a stored read's rows are nodes once a column is linked; a node action fills its node and asks before it writes; a read with no parameters runs as it opens; pins and groups; stamps on hand edits | `gq_language` `columns_name_their_variable_and_what_names_a_node`, `writes_stamp_what_the_graph_asks_and_the_type_declares`; `graph_store` `verbs::tests`; `studio_settings` `a_graphs_verbs_rows_pins_and_stamps_are_kept_per_graph`; `query_editor` `a_row_that_returns_a_key_is_the_node_it_names` (and the row menu's draw count), `a_stored_reads_rows_are_nodes_once_the_person_says_which`, `a_node_action_fills_its_node_and_asks_before_it_writes`, `a_read_with_no_parameters_runs_when_it_opens`, `pinned_queries_lead_then_reads_then_writes`; `type_table` `a_pages_actions_are_the_mutations_said_to_act_on_its_type`, `a_hand_edit_carries_the_graphs_stamps`; `studio` keymap tests | Pass |
| 2026-11-19 | It works against a real server | Offscreen snapshots (`crates/studio/examples/snapshot.rs`) and the debug binary under Xvfb, against `omnigraph-server` `v0.15.0` serving the beads, company-brain and hardware-program cookbooks in one cluster (seeds loaded with `load/ndjson?mode=append`) | beads `ready` ran as it opened; "Rows as nodes…" offered Comment, Issue and Memory for `i.slug`; linked to Issue, a row picked the issue in the inspector and its menu offered Claim… and Close…; Claim… filled `$id` with `lk-acl`, `$who` took the keyboard, Run showed "Run on main", ↵ committed one node, and `lk-acl` left `ready`. The page of `lk-sync` had Actions (⌥↵). Its kind set on the page committed `updated_at` at the server's time with `{"updated_at": "now"}`. company's `verify_claim` offered "a Claim" first, and "provider" picked `provider-certifies-nov-6`. program's `budget_rollup` faded at the cut and, scrolled, showed `violated` as a chip on `dc-pl-mass`. In the binary, one click on `blocked` in the Queries panel ran it |

## Corrections

- 2026-11-19 · A tab opened before the catalog was read (from the palette or a link as Studio
  starts) did not run as it opened: it now runs when the catalog arrives and says it is a read
  with no parameters (`Stored::run_when_known`).
- 2026-11-19 · The row menu, when it closed, put the keyboard back on the rows even after its
  action opened another tab, so `$who` lost it; it now does so only while the keyboard is still
  in the menu.
- 2026-11-19 · The fade at a grid's cut edge was decided as the rows were laid out, from the last
  frame's scroll range, which the first frame of a result has not set; it is now painted after
  layout, from this frame's range.
- 2026-11-19 · A trigger's tooltip covered its own open menu ("Rows as nodes…", "Names a
  node…"); it is hidden while the menu is open, as the table's toolbar menus do.
