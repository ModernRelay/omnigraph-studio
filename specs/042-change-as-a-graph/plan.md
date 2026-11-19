# 042: A branch's change as a graph

Status: built 2026-11-19 · the review's Canvas view (`graph_canvas`, `Mode::Change`) and the
other end's facts on a relationship line (`review`, `change_cards`); proposal 6 of the
[cookbook study](../../docs/ux/2026-11-18-cookbook-study.md) ("A branch's change as a graph:
anchors and the other end of every edge"), handed over as proposal 7

## Problem

Reviewers judge a change by what it hangs on, and the review lists changes by type. The
cookbook study found it four times:

- **vcos V2.** An ingest agent's 4 nodes and 7 edges are listed as Meeting, Artifact, Signal,
  Commitment. The partner thinks "one call, hung on `deal-helix-series-a`, attended by Elena,
  supporting `asmp-helix-onprem-margin`" (study screenshot f12).
- **company C2.** The new claim contradicts `provider-certifies-nov-6`; to read the old claim's
  quote beside the agent's, Alice left the review for a page tab.
- **pharma R2.** "FDA approves orforglipron → about VK2735 (subcutaneous)" was caught only
  because the target's name gave it away; its owner and phase were one hop further.
- **spike S2.** To judge "forms → Sovereign AI" the analyst needs that pattern's other facts.

Two things are missing: a picture of the change with the nodes it attaches to, and, on each
changed relationship, what its other end is beyond its name.

## Principles

- **II, one snapshot per view.** The change's canvas holds the reviewed head, read-only, as a
  view opened from a tab holding a commit does (XIII); a node the branch removed is read at the
  fork, where it still is. The facts on a card are read at the reviewed head, with the names.
- **V, nothing moves under you.** Facts land on the line they describe, after its type, so a
  card's height never changes when they arrive. A change read again at a new head is drawn in
  the same tab with the nodes drawn before where they were.
- **VI, VIII.** The Canvas view is a link of the review's view strip, disabled with its reason
  while the changes are read or when there are none; `g` in the review and the palette's
  "Open as canvas" open it.
- **IX, work is bounded.** The canvas's node cap (spec 006) bounds the change's chips, with
  "Too many nodes to draw · N not shown · Open as table"; the facts read is one read per type,
  of at most `NAME_BATCH` ids, the first `NAME_LIMIT` ends, as the names read is.
- **X.** Built on contract 0.14 alone; no engine change waits (Deferred).
- **XIII.** A view of one thing: the review's change, from its strip, with the review's strip.
- **033, one way to show a change.** The marks are the change vocabulary's + ~ −, in the added,
  changed and removed colours, never colour alone.

## What exists today (checked)

- The review folds the branch's changes since its fork into `EntityGroups`
  (`crates/graph_store/src/entity_groups.rs`): one group per node, with the relationship
  changes reviewed on it (`GroupRelationship`: the other end's type and id, the direction, the
  other end's group when the review has one). A node the review knows only by id (an end the
  branch did not change) is listed by `EntityGroups::unnamed_ends`, and its name is read at the
  reviewed head by `GraphSession::node_names` (`studio_node_labels`, one read per type).
- The canvas draws a query's result as nodes and links (`GraphCanvas::with_results`,
  `model::results_scene`, spec 006 "Query results"), laid out by `layout::explore_positions`,
  each chip's text made once at layout (`paint::chip_text`, guarded by
  `long_names_wrap_and_drawing_them_wraps_nothing`). An explore walk draws a relationship as a
  pill (`SceneKind::Hub`) between a node and its neighbours.
- `studio_graph::OpenAsCanvas` ("Show what the active tab shows on a canvas") is offered while
  the active tab's strip has a Canvas link (`Workspace::register_view_action`), and has no
  default binding.
- `builder::nodes_fields_by_id` reads chosen properties of nodes of one type by id, each in
  its `field_column` (`v_<name>`).
- Omnigraph `v0.15.0` (contract 0.14): the review's reads are the change feed and
  `GET /diff`; endpoints come in each edge change's image (`endpoints.from`, `endpoints.to`).
  Nothing in the contract returns an edge's endpoint properties; a node's properties at a
  commit come from `POST /query` (`crates/graph_client/openapi/omnigraph-0.15.0.json`).

## Design

### The review's Canvas view

- The review's view strip is Review · **Canvas** · History. Canvas is disabled with
  "The changes are still being read" until the review is read, and with "No changes to draw"
  for a review with none; its tooltip is "The change as a graph, with what it attaches to".
  `g` in the review (context `Review && !searching`) and the palette's Open as canvas
  (`studio_graph::OpenAsCanvas`) open it, as a click on the link does.
- Opening it sends `studio_graph::ShowChangeOnCanvas` with the change as a graph
  (`change_cards::change_graph`):
  - **nodes**: every node the branch changed, with what it did (`ChangeMark`: Added, Changed,
    Removed), named as its card names it; then every **anchor**, a node the branch left as it
    was that a changed relationship reaches, unmarked, named by the name the review read for
    it, else its id. Each node once.
  - **links**: every relationship the branch added, changed or removed, from its source to its
    target, in the words a card uses for it ("about", "attended by"), with its mark.
  - the reviewed head (`at_commit`), the fork, and how many changes it cannot draw: a
    relationship whose type the schema does not name, which has no end to stand under.
- One canvas tab per review (`canvas-change:<graph>:<branch>-><target>`). Opened again, the tab
  comes to the front; a review read again at another head redraws it in place, the nodes drawn
  before where they were (`GraphCanvas::show_change`); the same change again lays out nothing.
  The tab is not restored with the window, as a result's canvas is not.

### What the change's canvas draws (`Mode::Change`)

- **Chips.** Each node once, as the canvas draws a node: its type's mark and its name, led by
  what the branch did, `+`, `~` or `−` in its colour. An anchor carries no mark. A removed
  node's name is muted: it is gone at the head the canvas holds.
- **Relationships as pills.** Each changed relationship is a pill between its ends (the line
  reads node, relationship, node), as a walk's relationship hub is: `+ → about`, the arrow the
  way it runs from the chip it hangs off. A removed relationship's pill is muted and hangs on
  a dotted line.
- **Layout.** The walk starts from the node the most changed relationships meet (the call an
  ingest hung everything on), changed nodes before anchors, then as listed; each pill hangs off
  the end it was reached from and the other end off the pill. A node a second relationship
  reaches is drawn once, with a line to it. A part no relationship joins starts its own walk;
  the starts stand on a ring, or a grid for many (`layout::result_root_positions`).
- **Cap.** At most the node cap's chips (spec 006); a pill is drawn only with room for its
  other end. The nodes left out are counted in the cap's banner, with Open as table for the
  type most of them are, at the reviewed head.
- **First view.** The canvas fits the change when it opens, but never below zoom 0.8, where
  its chips still show their names: a wider change opens at 0.8 on the node its walk starts
  from. Fit, asked for, fits it whole.
- **Summary pill.** "tentative/ingest · 4 nodes changed · 7 relationships · 3 ends as they
  were", and "· N not drawn" when some cannot be.
- **Caption.** The legend, then what a click does: "+ new  ~ changed  − removed  unmarked: as
  it was · click to select · double-click to open · ⇧-drag to select". Each mark is drawn in
  its colour beside its word.
- **Clicks and keys.** As a result's canvas: a click selects, a double click or ⌘↵ opens the
  node's page, the inspector shows the selected node. A node is read, inspected and opened at
  the reviewed head; a removed one at the fork. A pill selects and shows nothing in the
  inspector: the relationship is on its review card. The chip menu offers Open, Canvas from
  here (a walk from the node at the same commit, for its whole neighbourhood) and the rest of a
  result's chip menu.
- **Tab.** "Change: tentative/ingest @ 01m7s9x" with the branch icon; its strip is the
  review's, Review · Canvas · History, the Review link opening the branch's review.
- **UI tools.** The canvas's outline names each changed relationship as an edge between its
  ends, and a selected pill as `changed_relationship` (spec 032).

### Facts on a relationship line

- A relationship line in the review names its other end, its type, and now what tells it
  apart: "→ Elena Vargas  Person · role: founder · city: Oakland". The line keeps its height:
  the facts follow the type on the same line, cut to the room left, whole in their tooltip.
- **Which properties** (`builder::fact_properties`): the other end's type's enumerations first
  (a phase, a stage, a status), then its numbers and flags (a value, a confidence), then its
  dates, then its text (a quote), each class in schema order, at most three. Never the key or
  the name (the line shows them), a list, a Blob, a vector, nor bookkeeping: a name ending
  `_at`, `At`, `_by` or `By`.
- **Words** (`graph_store::facts_of`): "name: value" joined by " · "; a text's first non-empty
  line, cut at a word to 48 characters with "…"; a property with no value is left out.
- **Where they come from.** For an other end the branch changed too, its own image (after the
  change, or before a removal). For an anchor, a read at the reviewed head with the names
  (`GraphSession::node_facts`, `studio_nodes_fields`, one read per type), read again when the
  review is read at another head, kept while it is read at the same one, and left out for a
  node the read did not find or a read that failed (which is read again at the next ask).
- A mirror line (a relationship reviewed on the other end's card) shows none: its other end is
  the card it is reviewed on.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| The change's graph is the canvas, a link of the review's strip named Canvas | A new view kind "Graph", as the study sketched; a toggle inside the review | Studio calls a picture of nodes and relationships a canvas everywhere, and constitution XIII makes a "how" with its own state, selection and reads a tab from the strip; the canvas already draws sets (spec 006) |
| Anchors are the ends of changed relationships, read nowhere else | Every changed node's whole one-hop neighbourhood | The study's cases (V2, C2, R2, S2) all hang on the relationships the branch changed. A whole neighbourhood costs one count read per edge type per changed node until E16, and would bury the change; Canvas from here walks any node's neighbourhood at the same commit in one click |
| Relationships are pills between their ends | Labels painted along the lines; unlabelled lines, as a result's canvas has | The relationship's name and its mark are what the reviewer reads (about, attended by, supports); a pill reuses the walk's chip, its hit-testing and its layout, and keeps the mark a symbol, not a line colour alone |
| The walk starts where most changed relationships meet | The first changed node; every node on a ring | An ingest hangs everything on one call or one deal: starting there draws the study's sketch, the call in the middle and what it hangs on around it |
| Removed nodes are read at the fork | At the reviewed head, where they are gone | The inspector and the page then show what the branch removed, not "not found" |
| Facts on the same line as the other end | A line of their own below it | Facts land after the card is drawn; a second line would push every card below it down (constitution V) |
| Facts chosen by type of property | Annotated key fields; the first three properties | The schema has no "key field" annotation (E27 would allow one). Enumerations, numbers and dates are short and say what kind of node it is (phase, stage, confidence); text comes last since it is long |
| Facts read for anchors only; a changed end's from its own image | A read for every end | The changed end's values are in the review already, and at the same head |
| The review builds the graph (`change_cards::change_graph`) and the canvas draws it from an action | The canvas reading the review's entity | Feature crates do not depend on each other (`docs/architecture.md`); the action carries ids, names and marks only, as `ShowResultsOnCanvas` does |
| A change's canvas opens no further out than 0.8 | A whole fit, as a walk opens | A reviewer reads names; a fit of a seven-relationship ingest drew letter marks only. Fit still fits the whole |
| The data action has no binding of its own | A `studio_review::ShowOnCanvas` | `OpenAsCanvas` already means "show what the tab shows on a canvas"; `g` binds it in the review as `g` shows a row or a tree node on a canvas. `ShowChangeOnCanvas`, like `ShowResultsOnCanvas`, carries data and is not built from the palette |

## Deferred

- **E16** (neighbourhood counts per node, `docs/deferred-engine-changes.md`) would make a
  changed node's own anchors, beyond the relationships the branch changed, cheap enough to draw
  with the change. Studio today draws the ends of changed relationships and leaves the rest to
  Canvas from here. No new engine change is needed.
- **E27** (annotation-only schema changes) would let a schema mark the properties that tell a
  node apart; until then they are chosen by kind.
- A commit's change as a graph (History's commit tab) uses the same `change_graph` and canvas;
  it waits for the commit tab's strip to offer Canvas.
- The facts on a commit's cards (History) wait for the commit view to read them, as the review
  does its names.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | A change draws each node once, marked, its anchors unmarked, each relationship a pill between its ends, from where most relationships meet; a removed node reads at the fork; pointing and zooming make no chip text; the same change lays out nothing and a newer one keeps the nodes' places | `graph_canvas` `a_change_draws_its_nodes_and_relationships_marked` | Passes |
| 2026-11-19 | A change past the cap draws what fits, every pill with both ends, and counts the rest | `graph_canvas` `a_change_past_the_cap_counts_what_it_leaves_out` | Passes |
| 2026-11-19 | The review's graph holds its changed nodes, its anchors named, its relationships source to target in a card's words, and counts what it cannot draw | `change_cards` `a_change_is_a_graph_of_its_nodes_their_anchors_and_its_relationships` | Passes |
| 2026-11-19 | Canvas waits for the review; read, it sends the change at the reviewed head and fork; a line shows its anchor's facts read at the head | `review` `the_change_opens_as_a_graph_and_its_lines_say_what_they_reach` | Passes |
| 2026-11-19 | It works against a real server on a cookbook graph | Omnigraph `v0.15.0` release binaries (`omnigraph-server`, `omnigraph`), the vc-os cookbook applied as a cluster, its seed loaded with `load --mode append`, and the study's V2 ingest replayed as `act-writer` on `tentative/ingest-mtg-helix-margin-deep-dive` (11 stored mutations: a Meeting, an Artifact, a Signal, a Commitment and 7 relationships); Studio's offscreen snapshot (`crates/studio/examples/snapshot.rs`, debug build, 1600x1000) opening the branch's review, then Canvas, then a click on the deal | The review's line "about → Helix Runtime — Series A  Deal · round_type: series-a · stage: in-diligence · outcome: open"; the canvas "4 nodes changed · 7 relationships · 3 ends as they were", the call in the middle with the deal and Elena Vargas unmarked beside it; the deal selected shows in the inspector at the reviewed head. The first fit zoomed to about 0.3, where every chip was a letter mark (the study's f16): the first view now stops at 0.8 |
| 2026-11-19 | Facts are a type's enumerations, numbers, dates, then text, three at most, never key, name, list or bookkeeping; their words are cut at a word | `gq_language` `a_nodes_facts_are_its_enumerations_numbers_dates_then_text`; `graph_store` `facts_read_as_names_and_values_cut_to_a_line` | Pass |

## Corrections

None yet.
