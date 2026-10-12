# 008: Branches, review and merge

Status: draft · 2026-10-11

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
- Scope selector: since the fork (default), or one commit.
- Header: one sentence ("17 entities and 22 relationships changed in 3 commits by act-alice"),
  then per-type counts (+ added, ~ changed, − removed).
- Body: one scroll of entity cards grouped by type, changes to existing data first. A changed
  entity shows only its changed properties, before and after; a removed one stays as a real,
  searchable row; edges show endpoint changes.
- Act in place: Revert (restore the before image), Edit, Mark reviewed (card turns hollow but
  stays). Keys: J/K between cards, ⌘Y accept and next, ⌥⌘Z revert and next.
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

- Merge preview with no effects and `as_commit` proposals: used when the server advertises
  `cluster_publishCommit`.
- Paged branch-to-branch diff and merge base (deferred engine changes).
- Approvals and comments on reviews.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Merge outcomes and conflict kinds in 0.13.0 | parsed `openapi.json` at `v0.13.0` | As listed |
| 2026-10-11 | Change feed and commit list parameters | parsed `openapi.json` at `v0.13.0` | `/changes`: branch, cursor, start, page_token, limit, kind, type, op, set; `/commits`: branch |
| pending | Folding 20,000 changes stays under 1 s off the main thread | measure | |

## Corrections

None yet.
