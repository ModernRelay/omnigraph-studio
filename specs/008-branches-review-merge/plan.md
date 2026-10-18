# 008: Branches, review and merge

Status: built on 0.13 · 2026-10-12 (`crates/review`, shared logic in `graph_store::changes` and
`graph_store::fixes`, cards in `studio_ui`). Approvals and comments are deferred (below).

## Problem

Branches are how changes to a graph get reviewed. Studio needs to create and switch branches,
show what a branch changed in a form a reviewer can read, fix mistakes in place, and merge with
conflicts explained.

## Principles

III (merge outcomes), IV (branch named; typed confirmation for delete), V (review items stay in
place), X (designed on 0.13; the better routes are deferred).

## What exists today (checked)

- OmniGraph `v0.13.0` HTTP: `GET /branches` (names only), `POST /branches {name, from}`,
  `DELETE /branches/{branch}`, `POST /branches/merge {source, target, delete_branch}` with
  outcome `already_up_to_date`, `fast_forward` or `merged`; a 409 carries `merge_conflicts[]`
  with kind `divergent_insert`, `divergent_update`, `delete_vs_update`, `orphan_edge`,
  `unique_violation`, `cardinality_violation` or `value_constraint_violation`.
- Changes: `GET /changes?branch=&start=after:<commit>&limit=&page_token=` (blocks of entity
  changes with `op` insert, update or delete and before/after images); `GET /commits/{id}/changes`
  (one commit against its first parent). `GET /commits?branch=` lists commits with parents.
- Not in 0.13.0: merge `as_commit`, commit publish and drop, a branch-to-branch diff, a merge
  preview, branch details (all in `docs/deferred-engine-changes.md`).
- An earlier web client's review: one summary sentence; changes to existing data first; groups
  of an entity plus the relationships created with it; fixes bound to the reviewed head and never
  retried.
- Zed `v1.23.2` `crates/git_ui/src/`: `project_diff.rs`, `commit_view.rs`, `conflict_view.rs`;
  `buffer_diff` and multibuffers present diffs as one scroll of excerpts.

## Design

### Branches

- Sidebar list with current branch marked; ⌘⇧B switch; create from the current branch or a
  commit. Fork point and last commit derived from `GET /commits?branch=`.
- Delete asks once, saying what goes with the branch, read from its merge base with `main` as
  the dialog opens: a branch whose head is in `main` loses nothing and is deleted with ↵; one
  with commits only it holds, or one the check could not read, is deleted by typing its name
  (constitution IV). The dialog holds that one sentence, the name field when it is needed,
  and Cancel and Delete; the CLI command is the Delete button's tooltip.

### Review (`review` crate)

- Opens as a tab "Review <branch> → <target>".
- Scope selector: since the fork (default), or one commit (its own changes, read from
  `GET /graphs/{g}/commits/{id}/changes`).
- Search: a field narrows the cards to those whose own change or a relationship on them has an
  id, type, relationship end, or before or after value containing the text; J/K move among
  the cards it leaves.
- Header: one sentence ("17 entities and 22 relationships changed in 3 commits by act-alice"),
  then per-type counts (+ added, ~ changed, − removed).
- Body: one scroll of entity cards (`graph_store::EntityGroups`) in two sections: changes to
  what the target has (changed, removed, or unchanged with relationships that changed), then
  what the branch added. Inside a section, cards run by type under one header counting what the
  run shows. A card is one node: its changed properties, before and after (a removed one stays
  as a real, searchable row), then the relationship changes that came with it.
- A relationship change is on one card, its home: the end the branch added, else an end it
  changed or removed, else its source, which then stands unchanged for its relationships.
  There it is counted, reviewed and reverted. When the other end has a card too, the
  relationship shows there as well, muted, "reviewed there", so either entity tells its whole
  story. A line reads as the relationship in words (`WorksAt` is "works at", `NoteAboutPerson`
  from Note to Person is "about"), an arrow for its direction and the other end by its label
  property, else its id; "new" marks an end the branch added, and a changed relationship's
  properties follow it. A relationship whose type the schema lacks stands alone as its own
  card. A card's header is the kind's symbol (+ ~ −, · for unchanged), the entity's name and
  its id; its type is the run's header's. Cards are hairline frames on the document, values in
  the UI font.
- Act in place: Revert (restore the before image), Edit, Mark reviewed (every change on the
  card; the card turns hollow but stays, and says how many are not reviewed when some are).
  Each relationship line has its own Revert, shown on hover. Keys: J/K between cards, ↵ edit,
  ⌘Y accept the card and go to the next, ⌥⌘Z revert the selected change (the card's own, or
  the relationship line clicked) and go to the next.
- Edit (a node that is added or changed): a form of its editable properties filled with what
  the branch holds; Save writes only the fields that changed, each bound to that value, as one
  mutation on the reviewed branch bound to the reviewed head. After a fix the card's other
  fixes wait for Refresh.
- Revert of a relationship: an added one is deleted by `@id`; a removed one is inserted again
  between its before endpoints with its before values (an unkeyed one gets a new id: a `.gq`
  insert cannot supply one). A changed relationship is not reverted here: edges cannot be
  updated, and the delete and insert it needs cannot share one mutation, so it would publish
  twice. Its card says so and points to the query editor.
- Every fix is a write on the branch, bound with `/mutate/if-graph-commit` to the reviewed head
  or to the commit of the fix before it; a 412 says the branch moved and offers Refresh. Fixes
  go one at a time, so a second one asked for quickly is bound to the first's commit, and each
  outcome finds its card by the change's key, also after a Refresh. A reviewed mark is kept by
  the change and its last commit: changed again since, it is reviewed again.
- Data: change feed from the fork, folded per entity (first before, last after), capped at
  20,000 changes with "Partial: showing the first 20,000 changes" when capped.

### Merge

- "Merge into <target>" names both branches. Before merging, the button checks the branch head
  still equals the reviewed head. It merges only a review on screen, with no fix out: never
  while the review is being read (a head nobody has seen), and never from the branch list or
  the menu, which open the review instead.
- "Delete <branch>", beside the button and on by default, deletes the branch once the merge
  lands (`delete_branch`): the server reports the deletion on the answer and never undoes the
  merge for it, and the outcome says "Deleted <branch>." or why it was not. A session on the
  deleted branch moves to the target. Not offered for `main`.
- Outcome shown plainly: fast-forwarded, merged (with commit), already up to date.
- A 409 opens a conflicts view: one row per conflict with its kind in words ("Changed on both
  branches", "Deleted on main, changed here", "Relationship points to a deleted node"), the
  entity, and the message. Resolution happens on the source branch with ordinary edits; then
  merge again.
- After merging, Studio confirms the target's history contains the merge (a verified
  merge).

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Review from the change feed | Wait for a diff route | Works on 0.13; the route is deferred |
| Stat cards removed from the header | Stat cards above the changes | They read as cluttered in an earlier design |
| A node's card carries its relationship changes, mirrored at the other end | Sections by type with edges in their own; a relationship on one card only | A reader joined a node and its edges by hand, from keys alone; a mirror lets either end tell the whole story while each change is counted and reverted once |
| Conflicts resolved by editing the source branch | Ours/theirs buttons | 0.13 has no resolution API; edits are ordinary writes |

## Deferred

- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E6 paged branch-to-branch diff: `GET /diff` since 0.14. A review folds the change feed
    after the merge base, for each commit's author and the reviewed marks; a branch its target
    was merged into, whose base the feed refuses as a start, folds the net diff from the merge
    base to the reviewed head instead (both pinned), capped at 20,000 changes either way.
  - E7 merge base: `GET /branches/merge-base` since 0.14 (fork, head, and a merge's
    confirmation); walking `GET /commits` parents remains for a review of one commit and for
    the commit list of a branch its target was merged into.
  - E8 merge preview with no effects: merge on request and show the 409 conflicts until then.
  - E9 branch details: derived from `GET /commits?branch=` until then.
  - E21 a branch from a commit: 0.13 forks only a branch's head (`from` names a branch).
  - R2 unpublished commits: `as_commit` merge proposals. The minimum server (0.14) serves
    them; Studio does not use them yet.
  - E28 a merge bound to the reviewed source head: until then a commit landing between the
    head check and the merge is merged too, and the outcome says so.
- Approvals and comments on reviews.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Merge outcomes and conflict kinds in 0.13.0 | parsed `openapi.json` at `v0.13.0` | As listed |
| 2026-10-12 | Edge mutations Studio builds compile on 0.13 | `gq_language` `writes_compile`, `graph_store` `fixes` tests type-check `insert_edge`/`delete_edge` against the 0.13 compiler; `docs/user/mutations/index.md` at `v0.13.0` (`from`/`to` assignments; no edge update; inserts and deletes cannot mix) | Pass |
| 2026-10-12 | Relationship reverts land on the branch | dev server: branch `review-edges` adds `WorksAt person-000000 → company-00003` and deletes `Knows person-000000 → person-000005`; review restored from a workspace file; ⌥⌘Z on both cards; queries on the branch | Two commits, each bound to the head before it; the `Knows` edge is back, the `WorksAt` edge is gone |
| 2026-10-12 | Edit writes only what changed, on the reviewed branch | dev server: `review-edges` retitles `person-000001`; ↵ on its card, " Jr" added to the name, ⏎ | One commit; the branch has "Donald Dijkstra Jr" and the retitled title unchanged; the card reads "Edited in ARZJVH2D" |
| 2026-10-12 | Search narrows the cards | `review` test `search_matches_ids_types_and_values_case_blind` | Ids, type names and before/after values, case-blind |
| 2026-10-11 | Change feed and commit list parameters | parsed `openapi.json` at `v0.13.0` | `/changes`: branch, cursor, start, page_token, limit, kind, type, op, set; `/commits`: branch |
| 2026-10-12 | Folding 20,000 changes stays under 1 s off the main thread | `graph_store` `fold_instrument` (ignored instrument), debug build, Apple M5 Pro, 48 GB: 20,000 changes into 15,000 entities | 175 ms |
| 2026-10-12 | Change-feed paging | live `omnigraph-server` 0.13.0: a terminal page carries `cursor` and `caught_up: false` but no `next_page_token` when more commits wait | `GraphClient::changes` takes no `cursor`, so review continues with `start=after:<last folded commit>` and stops at the reviewed head; a 20,500-change branch shows "Partial" at 20,000 |
| 2026-10-12 | Merge outcomes | live server: `feature-y` merged as a new commit; `feature-z` fast-forwarded; `feature-x` refused with 409 | 409 listed `divergent_update` and `delete_vs_update` with `entity_id`; nothing moved on main |
| 2026-10-12 | Target history after merges | live server: `GET /commits?branch=main` after both merges | Lists main's own chain only; merged branch commits appear only as `merged_parent_commit_id` |
| 2026-10-12 | Fast-forward shape | live server: main's head after fast-forwarding `feature-z` | A new main commit whose `merged_parent_commit_id` is the branch head, not the branch head itself |
| 2026-10-12 | A revert of several properties publishes once | live server, scratch branch: two `update` statements joined by `graph_store::fixes::compose` | One commit, `affected_nodes: 2` |
| 2026-10-12 | Fixes bound to the reviewed head | offscreen snapshot: Revert on `feature-x` `person-000002` through `/mutate/if-graph-commit` | Committed; the value read back as before; the card stays, hollow, "Reverted in …" |
| 2026-10-12 | Merge refuses a moved branch | offscreen snapshot: a second writer committed on `feature-v` after the review was read | "feature-v moved to …"; no merge request sent; main unchanged |
| 2026-10-18 | Relationships are reviewed with the entity they came with | `graph_store` `entity_groups` tests (home rule, mirrors, self relationships, a type the schema lacks, edge types in words); `review` test `a_search_keeps_the_cards_whose_changes_match_and_headers_count_what_shows` | Each change on exactly one card; mirrors name their card; headers count what shows |
| 2026-10-18 | Entity cards render | offscreen snapshot against a scratch dev server on edge `0a62bb7`: branch `review-demo` adds Person `person-900000` with `WorksAt` and `Knows`, retitles `person-000001` and adds its `Contributes`, removes `WorksAt person-000000 → company-00002`, adds `Knows person-000155 → person-000005` | Donald Dijkstra's card carries his new `contributes`; Theo Brandt's carries `works at` and `knows`, mirrored on `person-000000`'s card with the removed `works at` struck through; the scratch server and its data were deleted after |

## Corrections

- 2026-10-12: a target's `GET /commits` holds only its own first-parent chain, and a fast-forward
  publishes a new target commit whose merged parent is the branch head. The fork point is the
  newest branch commit that is in the target's list *or* is a merged parent there
  (`graph_store::fork_point`); merge verification and settling an unknown outcome look for the
  merge commit or a commit whose merged parent is the reviewed head (`contains_merge_of`).
- 2026-10-12: relationships cannot be reverted from the review: `gq_language::builder` has no edge
  insert or delete builder. Their Revert is disabled with the reason.
- 2026-10-12: a revert that sets several properties joins the builder's statements into one
  mutation (`graph_store::fixes::compose`, renaming parameters). It belongs in
  `gq_language::builder` next to the builders it composes.
- 2026-10-12: the review's keys (j/k, ↑/↓, ⌘Y, ⌥⌘Z, ⌘⇧↩ for Merge) are bound in `review::init` in
  the `Review` context; they move to `assets/keymaps/` with the next keymap change.
- 2026-10-12: a fresh window focuses nothing, so window-level actions never reached the
  workspace's handlers. `review::init` focuses the workspace when nothing is focused; the fix
  belongs in `studio_workspace`.
- 2026-10-12: the change feed folds first-parent history, so a branch that merged its target in
  showed the merged-in changes too, each with Revert, which would undo the target's own change
  on merge. Since 2026-10-18 such a review folds the net diff from the merge base (E6) and says
  "Since main was last merged in at …".
