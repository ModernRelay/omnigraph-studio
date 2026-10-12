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
- Tower's review (its spec 088): one summary sentence; changes to existing data first; groups
  of an entity plus the relationships created with it; fixes bound to the reviewed head and never
  retried.
- Zed `v1.23.2` `crates/git_ui/src/`: `project_diff.rs`, `commit_view.rs`, `conflict_view.rs`;
  `buffer_diff` and multibuffers present diffs as one scroll of excerpts.

## Design

### Branches

- Sidebar list with current branch marked; ⌘⇧B switch; create from the current branch or a
  commit; delete by typing the branch name. Fork point and last commit derived from
  `GET /commits?branch=`.

### Review (`review` crate)

- Opens as a tab "Review <branch> → <target>".
- Scope selector: since the fork (default), or one commit (its own changes, read from
  `GET /graphs/{g}/commits/{id}/changes`).
- Search: a field narrows the cards to those whose id, type or a before or after value
  contains the text; J/K move among the cards it leaves.
- Header: one sentence ("17 entities and 22 relationships changed in 3 commits by act-alice"),
  then per-type counts (+ added, ~ changed, − removed).
- Body: one scroll of entity cards grouped by type, changes to existing data first. A changed
  entity shows only its changed properties, before and after; a removed one stays as a real,
  searchable row; edges show endpoint changes. A card's header is the kind's symbol (+ ~ −)
  and the entity's id; its type is the group header's, never repeated per card. Cards are
  hairline frames on the document, values in the UI font.
- Act in place: Revert (restore the before image), Edit, Mark reviewed (card turns hollow but
  stays). Keys: J/K between cards, ↵ edit, ⌘Y accept and next, ⌥⌘Z revert and next.
- Edit (a node that is added or changed): a form of its editable properties filled with what
  the branch holds; Save writes only the fields that changed, each bound to that value, as one
  mutation on the reviewed branch bound to the reviewed head. After a fix the card's other
  fixes wait for Refresh.
- Revert of a relationship: an added one is deleted by `@id`; a removed one is inserted again
  between its before endpoints with its before values (an unkeyed one gets a new id: a `.gq`
  insert cannot supply one). A changed relationship is not reverted here: edges cannot be
  updated, and the delete and insert it needs cannot share one mutation, so it would publish
  twice. Its card says so and points to the query editor.
- Every fix is a write on the branch, bound to the reviewed head with
  `/mutate/if-graph-commit`; a 412 says the branch moved and offers Refresh.
- Data: change feed from the fork, folded per entity (first before, last after), capped at
  20,000 changes with "Partial: showing the first 20,000 changes" when capped.

### Merge

- "Merge into <target>" names both branches. Before merging, the button checks the branch head
  still equals the reviewed head.
- Outcome shown plainly: fast-forwarded, merged (with commit), already up to date.
- A 409 opens a conflicts view: one row per conflict with its kind in words ("Changed on both
  branches", "Deleted on main, changed here", "Relationship points to a deleted node"), the
  entity, and the message. Resolution happens on the source branch with ordinary edits; then
  merge again.
- After merging, Studio confirms the target's history contains the merge (Tower's "verified
  merge").

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Review from the change feed | Wait for a diff route | Works on 0.13; the route is deferred |
| Stat cards removed from the header | Tower's first design | "Read as cluttered" in Tower 088 |
| Conflicts resolved by editing the source branch | Ours/theirs buttons | 0.13 has no resolution API; edits are ordinary writes |

## Deferred

- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E6 paged branch-to-branch diff: the change feed is folded from the fork, capped at 20,000
    changes, until then.
  - E7 merge base: found by walking `GET /commits` parents until then.
  - E8 merge preview with no effects: merge on request and show the 409 conflicts until then.
  - E9 branch details: derived from `GET /commits?branch=` until then.
  - E21 a branch from a commit: 0.13 forks only a branch's head (`from` names a branch).
  - R2 unpublished commits: `as_commit` merge proposals only when the server advertises
    `cluster_publishCommit`.
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
  shows the merged-in changes too. E6 removes this.
