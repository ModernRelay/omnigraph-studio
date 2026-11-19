# 039: Conflicts with three sides and a resolution

Status: built 2026-11-19 · `crates/review/src/view/resolve.rs` (the sides, the decisions, the
cards), `graph_store::fixes::take_side`, `gq_language::builder::edges_of`; waits on E53 and E61
for the exact, one-step form.

## Problem

The cookbook study ([docs/ux/2026-11-18-cookbook-study.md](../../docs/ux/2026-11-18-cookbook-study.md),
proposal "Conflicts with three sides and a resolution", 7 of 8 cookbooks) found that a review's
conflict shows one side. A conflicting card shows the branch's values against its fork and never
what the target holds now, who set it, or the base:

- **vcos V4.** "in-diligence → dead · Conflicts with main". Main is `ic-ready`, set by the scout
  agent's merged branch; neither the value nor its author shows (f06).
- **program P2.** The carbon card shows 24.5 → 22.6 while main already holds 21.4.
- **beads B5.** The conflicts list says "divergent update for id lk-crash-empty" and nothing more.
  The study resolved it by writing main's row onto the branch with the CLI.
- **company C4.** "More relationships than the schema allows · SupersededBy · checkout-launch-nov-13
  has 2 edges (max 1)": the two rival successors (Nov 20 on main, Nov 27 here) are not shown (f07).
- **spike S4, brain N4.** A divergent insert and a `HappenedAt` race: the same gap.

The only fix offered was "Fix each on the branch, then merge", with ordinary edits made by hand
from values the reviewer could not see.

## Principles

- I: Studio reads the sides itself, over HTTP, as snapshot reads; it keeps nothing as true.
- II: each side is read at a commit by id: the target's head and the head the review's fixes
  bind to, named by the merge base read with them.
- III: every decision is an ordinary write with three outcomes, never retried; an unknown one
  says to check the history of the branch it wrote to.
- IV: every decision names the branch it writes to; one that writes to the target, before any
  merge, is confirmed with ↵ (it destroys nothing the target's history does not keep). A
  credential the access route refuses changes on a branch sees that branch's decisions disabled,
  saying why.
- V: the cards keep their place while a decision is out; the list is replaced only by the merge
  check read again.
- VI, VIII: each decision is an action with a key, in the palette while conflicts are shown, and
  its button names the key and the branch it writes.
- IX: sides are read for the first 200 conflicts, who set the target's side from at most four
  feed pages, and each `@card` conflict's relationships 50 a side; each cap says so.
- X: E53 (the three images from the server) and E61 (a merge that takes a side) are deferred;
  the design works on contract 0.14 today.

## What exists today (checked)

- Studio `main` at `98c7cea`. The review's conflicts list (`crates/review/src/view.rs`,
  `render_conflicts`, `render_conflict`) showed one row per conflict: the kind in words, the type,
  the id, the server's message and "Show change". A conflicting card in the feed said "Conflicts
  with main" (spec 033). The strip said "N conflicts stop a merge into main. Fix each on the
  branch, then merge."; the merge's banner the same with `resolution_hint`.
- `MergeConflictOutput` (`crates/graph_client/openapi/omnigraph-0.15.0.json`): `entity_kind`,
  `type_name`, `kind`, `message`, `entity_id` (nullable). No values (E53).
- Omnigraph `v0.15.0` `exec/merge.rs` `run_three_way_classification`: a row conflicts when the
  source and the target both changed it from the base and the two rows differ (`rows_equal`, the
  whole row); a row the two sides hold alike merges. `classify_merge_conflict` names the row's id
  for `divergent_insert`, `divergent_update` and `delete_vs_update`.
  `validate/kernels.rs`: `unique_violation` and `orphan_violation` name a row id;
  `cardinality_violation` names none (`row_id: None`), its message "@card violation on edge
  {edge}: source '{src}' has {count} edges ({bound} {limit})"; `value_violation` none.
- `ChangeCauseOutput` carries `actor_id`, `authored_branch`, `authored_at`,
  `merged_parent_commit_id`; `GET /changes` takes `type` filters but no entity (E62,
  [#1472](https://github.com/ModernRelay/omnigraph/issues/1472)).
- A v0.15.0 server with the eight cookbook graphs (validation log, 2026-11-19): a merged change's
  feed block is the merge commit's (`actor_id` the merger, `authored_branch` main,
  `merged_parent_commit_id` the merged head); `GET /commits/{merged parent}` names the branch
  (`graph_branch`) and its author. `graph_branch` is null for commits made on main.
- `MergeBaseOutput` answers the base and both heads in one read, which the sides are read at.
- `POST /mutate/if-graph-commit` binds a write to a commit (spec 008); the review's fixes go one
  at a time, each bound to the head the one before left (`bound_head`).

## Design

### Where

The conflicts list (Show conflicts in the merge strip or the merge's banner, or ⌥⌘C) becomes
the place a conflict is resolved: one card per conflict, the selected one the keys act on. The
feed's "Conflicts with main" and its Show change stay as spec 033 made them.

### A card

```
 ⚠ Changed on both branches                                              [Show change]
 ▣ Deal  Helix Runtime — Series A  deal-helix-series-a
                 At the fork        main now                       pipeline/2026-11-17
                                    act-writer on tentative/scout-  act-writer · 2 h ago
                                    helix, merged by act-admin · …
 stage           in-diligence       ic-ready                       dead
                 3 other properties the sides agree on
 [Keep main's] [Keep pipeline/2026-11-17's…] [✎ Edit…]          Wrote main's version to …
```

- **Header**: the kind in words (`conflicts::conflict_wording`, in `Color::Conflict`), Show change
  when the review holds the entity's change; then the type's swatch, the entity's name (its label
  property read at the target's head) and its id.
- **Three columns** for a node or relationship: "At the fork", "<target> now", "<branch>". Under
  the target's and the branch's titles, who last changed it on that side since the fork
  ("act-writer · 2 h ago"; a value a merge brought: "act-scout on tentative/scout, merged by
  act-admin"). Then each property, in the schema's order, whose values the sides do not all hold
  alike (`same_value`, so 5 and 5.0 agree), a value that differs from the fork's in the body
  colour and one as at the fork muted; a side without the entity reads "deleted" (the fork held
  it), "not there" or, for the fork, "not there yet"; a side not read, "not read". Then "N other
  properties the sides agree on".
- **Too many relationships** (`cardinality_violation`): "<source> may hold fewer <Edge>
  relationships than the two sides hold together:", then each relationship from that source on
  either side, "<source> → <other end>", where it is ("on main · act-alice · 2 h ago", "on
  <branch>", "on both"), and "Remove on main" or "Remove here" on the side that alone holds it.
- **Words only**: a conflict naming no entity Studio can read (a value constraint) shows the
  server's message, as before.
- **Decisions**, for a node or relationship whose sides were read:
  - **Keep main's** (⌥⌘[, Ctrl-Alt-Shift-← on Linux and Windows) writes the target's version onto
    the branch: `take_side` makes the branch's row the target's (every differing property in one
    `update` bound to each value the branch holds; a row only the target holds inserted, which
    needs a key; one only the branch holds deleted). Sent as a fix: one at a time, bound to the
    head the review's fixes bind to, its outcome under the card. Verified in the study's B5.
  - **Keep <branch>'s…** (⌥⌘], Ctrl-Alt-Shift-→) writes the branch's version onto the target, the
    same way the other round, bound to the target's head the sides were read at (each such
    write's commit moves it on), after a confirmation: "Write <branch>'s Deal Helix to main?
    This writes <branch>'s values onto main now, before any merge, so the two agree and the merge
    takes them. main's history keeps what it held." ↵ Write to main. A merge that takes a side
    (E61) would make this the merge itself.
  - **Edit…** (↵ in the list) opens the review's edit form for the entity on the branch, each
    field with the target's value beside it ("main: ic-ready") and Use, which puts it in the
    field. Shown for a node the branch holds and the review shows the change of.
  - A relationship is only removed: one both sides changed cannot be made alike without a delete
    and an insert under a new id (E26), and says so.
- **Outcome**, at the end of the decisions: "Writing main's version to <branch>…", "Wrote main's
  version to <branch> in <commit>. Checking the merge again.", "Not written: a value changed
  since the sides were read. Refresh.", "Not written: <branch> moved. Refresh, then try again.",
  "Refused: …", "Not sent: …. Nothing was written.", "Outcome unknown. Check <branch>'s history
  before trying again." A decision that landed turns a merge's own conflicts back to the dry run,
  which is read again once nothing is out; its answer replaces the list. With no conflict left the
  list says "Nothing stops the merge now", "<branch> merges into <target> without a conflict.
  Review the changes, then merge." and Show changes.
- **Disabled**, saying why, as the review's fixes are: a merge that may land, the branch deleted
  by the merge, a credential that may not change the branch (or, for the target's decisions, the
  target), a write to the target already out, sides not read.

### Reading the sides

Once the list is shown for a conflicts list it has not read (each list made by a dry run or a
merge has an id; `resolve::next_list_id`), after the frame:

1. `GET /branches/merge-base` for the base and both heads. The branch's side is read at the
   review's `bound_head` (the reviewed head, then each fix's commit), the target's at its head.
2. For each type, the conflicting entities by id in one read at each head:
   `builder::nodes_fields_by_id` with every property a projection can read, or
   `builder::edges_of(EdgesOf::Ids)` for relationships. An id a head does not return is
   absent there.
3. The fork's image is the review's change's before image; for an entity the review shows no
   change of (a whole review, of the whole branch), the branch's, since the branch has not
   changed it; otherwise not read.
4. For a `cardinality_violation`, the source named in the message (`card_source`), and
   `builder::edges_of(EdgesOf::From)` at each head, at most 50 a side.
5. Who set the target's side: `GET /changes?branch=<target>&start=after:<fork>&type=…` of the
   conflicts' types, at most 4 pages of 8,192; the newest change to each entity keeps its cause.
   A cause that is a merge (`merged_parent_commit_id`) is read by id (at most 20) for the branch
   and author it came from. A fork off the target's first-parent chain is refused as a start:
   who is then not said. The branch's side's author is the review's newest commit to it.
6. The names of the conflicting nodes and the relationships' other ends
   (`GraphSession::node_names`) at the target's head, then at the branch's for those only it
   holds.

A failure reads "What each side holds could not be read: …" under the heading, and the cards
show the server's words.

### Keys and drawing

New actions in `studio_actions::review`: `ShowConflicts` (⌥⌘C, Ctrl-Alt-C), `KeepTarget`,
`KeepSource`; registered while a merge or its dry run has conflicts (the decisions while the list
is shown), so the palette lists them only there. J and K move among the conflicts while the list is
shown, ↵ opens Edit…. The list is a cached view of its own (`resolve::ConflictRows`), as the
changes' list is (`docs/drawing.md`): keys in the review's search draw it no times.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Read the target's and the branch's images at their heads, the fork's from the review | Read all three at commits; wait for E53 | The review already holds the fork's image of every entity the branch changed, and a read at the fork under today's schema is refused for a property added since. Reading both heads with the same query keeps the two columns a decision compares in one encoding |
| Make the row alike, every differing property | Only the properties both sides changed | Omnigraph compares a conflicting row whole: a row still differing in one property the other side alone changed still conflicts |
| "Keep this branch's" writes to the target, confirmed | Disable it until E61; write a merge commit by hand | The study's own fallback ("two steps"). It is the person's choice to change the target, so it is asked, named, and bound to the head read; E61 makes it the merge |
| Bind "Keep main's" to the head the fixes bind to, through the fix queue | Its own queue | A decision and a fix both write the branch: one queue keeps every write bound to the commit the one before left |
| Who set the target's side from its type-filtered feed, bounded | A walk of commits' changes; nothing until E62 | One read answers every conflict of a type, with the merge it came by; the bound says when it stopped |
| The `@card` source from the server's words | The branch's added edges of that type | The conflict names no entity; the message is the only place the source is named, as E54 reads a bound from its words. Without it the card shows the words |
| Decisions on the conflicts list, not the feed's cards | Three columns on every conflicting feed card | A `@card` conflict has no card in the feed; the list is where a merge's own 409 leads, and the feed stays the review of what the branch did |
| A cached view for the list | Draw it with the review | `docs/drawing.md`: the search field is the review's |

## Deferred

- Engine ([spec 012](../012-omnigraph-engine-changes/plan.md),
  [index](../../docs/deferred-engine-changes.md)):
  - **E53** the base's, source's and target's images in the conflict itself: one exact answer,
    no reads at the heads, and the base for an entity the review cannot show.
  - **E61** ([#1471](https://github.com/ModernRelay/omnigraph/issues/1471)) a merge that takes a
    side for named entities, bound to the heads its dry run read (with E28): "Keep this branch's"
    becomes one merge rather than a write to the target.
  - E62 ([#1472](https://github.com/ModernRelay/omnigraph/issues/1472), proposal 4's) would let
    who set the target's side be read by entity rather than through a type's feed.
- In Studio: a divergent insert of two rows whose keys differ in case; a relationship conflict's
  ends named on its card; resolving every conflict of a list at once; the feed card's own three
  columns.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | What a merge conflict names, and how a row conflicts | Omnigraph `v0.15.0` `exec/merge.rs` `classify_merge_conflict`, `run_three_way_classification`; `validate/kernels.rs` `cardinality_violation`, `orphan_violation`, `unique_violation`; `validate.rs` `value_violation` | As listed under "What exists today" |
| 2026-11-19 | The study's conflicts on a real server | `omnigraph-server` `v0.15.0` (release binary, sha256 checked) serving the eight cookbooks in one cluster, seeds loaded with `--mode append`; dry runs `POST /graphs/{g}/branches/merge` `dry_run: true` | vcos `pipeline/2026-11-17`: Deal `deal-helix-series-a` `divergent_update`; company `agent/slack-oct-14`: `cardinality_violation`, `entity_id` null, "@card violation on edge SupersededBy: source 'checkout-launch-nov-13' has 2 edges (max 1)"; beads `agent/b`: Issue `lk-crash-empty` `divergent_update`, Memory `crdt-choice` `delete_vs_update`, Memory `offline-index-fix` `divergent_insert`; program `trade-optics-carbon`: Parameter `p-mass-optics` `divergent_update`. Each a 409 `conflict` with `effects: none` |
| 2026-11-19 | Who set the target's side, through a merge | `GET /graphs/vcos/changes?branch=main&start=after:<base>&type=Deal`, then `GET /commits/<merged parent>` | The block's cause is the merge commit (`act-admin`, `authored_branch: main`, `merged_parent_commit_id`); the merged head names `tentative/scout-helix` and `act-writer`. A commit made on main lists `graph_branch: null`. A type filter still answers an unrelated commit's block, with no changes |
| 2026-11-19 | A relationship is read by its source or its id at a commit | `POST /graphs/company/query` of `edges_of`'s shape, at main, at the branch head and with `snapshot` | Main: the Nov 20 edge; the branch: the Nov 27 edge; main before the merge: none. A keyed edge's `@id` is its key tuple as JSON text |
| 2026-11-19 | The builders and the decision's write | `gq_language` `edges_are_read_by_id_or_from_their_source` (type-checked by `omnigraph-compiler`); `graph_store` `taking_a_side_makes_the_other_rows_alike` | Pass |
| 2026-11-19 | The sides, the decisions and their writes, headless | `review` `a_conflict_shows_three_sides_and_keeps_mains`, `keeping_this_branchs_writes_to_the_target_bound_to_its_head`, `a_card_conflict_lists_each_sides_relationships`, and the unit tests in `view::resolve` | Pass: the fork's `in-diligence`, main's `ic-ready` set by act-scout on `tentative/scout` merged by act-admin, the branch's `dead`; Keep main's sends `update Deal set { stage }` on the branch to `/mutate/if-graph-commit`, then a dry run; Keep this branch's sends the branch's value on main; a `@card` conflict lists main's and the branch's edge and removes the branch's on the branch |
| 2026-11-19 | A key in the search leaves the conflicts list | `review` `typing_in_the_search_leaves_the_conflicts_as_they_were` | Pass: two keys draw it 0 times; J once |
| 2026-11-19 | The cards against the real server | `crates/studio/examples/snapshot.rs` (debug build, Linux, 1440×900) on the server above as act-admin, each conflicting branch's review with ⌥⌘C | vcos: stage and `updatedAt` in three columns, main's "act-writer on tentative/scout-helix, merged by act-admin", 14 properties agreeing. company: both successors, "on main · act-notes-bot on agent/notes-oct-14, merged by act-alice", Remove on main and Remove here. beads: all three kinds; a query row leaves out a null column, so a side's image is filled with nulls for every property read (it read "not read" before the fix). program: the parameter's three values |
| 2026-11-19 | A decision resolves the conflict on the server | The same, vcos: ↵ (Edit… with main's values and Use), ⌥⌘] (the confirmation), Esc, then ⌥⌘[; then the branch read with curl and a dry run | Keep main's committed on `pipeline/2026-11-17` (stage `ic-ready`); the review checked the merge again and said "Nothing stops the merge now"; the server's dry run answers `merged` |

## Corrections

- 2026-11-19, review of #151 (Devin):
  - A write to the target was bound to the target head held when the confirmation was
    answered, which a read of the sides in between could have moved. Now it is bound to the head
    its values were read at, for the list they were read in; one answered after either moved
    sends nothing and says "Not written: main moved while you confirmed. Decide again."
    (`a_confirmation_answered_after_main_moved_sends_nothing`).
  - A `@card` conflict whose relationships could not all be read (a read failed, or a side
    reached 50) took a missing relationship as absent and offered to remove a shared one. Now
    such a card says not every relationship could be read, and offers no Remove.
  - Keep main's was sent under a key of its own, so the entity's card in the feed still offered
    fixes from the values it replaced. It is now sent as that card's fix, which turns it fixed
    until Refresh.
  - Who set the target's side stopped at a feed page that stopped short of the head with no
    page token. It now reads on after that page's last commit, as the review's own feed does
    (`a_conflict_shows_three_sides_and_keeps_mains`).
  - Use in Edit… put the target's value in the field as text, which cannot tell an empty string
    from null. The form now saves the value Use took while the field still reads it.
