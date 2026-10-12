# 009: History

Status: built on 0.13 · 2026-10-12 (`crates/history`): a History tab with the lane graph and a
commit detail split, `OpenCommit` as its own tab, a History panel in the bottom dock, search by
id prefix or actor, and type tables and query editors pinned to a commit.

## Problem

People need to see how a graph got here: commits by branch, merges, who changed what, and the
exact changes of any commit.

## Principles

II (a commit is an immutable snapshot), VI, IX (virtualized; size warning on long histories),
XI (lane logic forked from Zed with notices).

## What exists today (checked)

- OmniGraph `v0.13.0`: `GET /commits?branch=` returns every commit (no paging) with
  `graph_commit_id`, `parent_commit_id`, `merged_parent_commit_id`, `actor_id`, `created_at`;
  `GET /commits/{id}`; `GET /commits/{id}/changes` (diff against the first parent; 409
  `change_diff_refusal` for parentless or schema-boundary commits; 410 when history was cleaned
  up).
- Zed `v1.23.2` `crates/git_ui/src/git_graph.rs`: lanes assigned as commits stream in, curves
  for merges, rendered with a `canvas` beside a virtualized table, with property tests over random
  commit graphs.

## Design

- Bottom dock panel and a full tab: a lane graph left, then message-free rows (commits have no
  messages): short id, actor, relative time, branch chips, change counts.
- Selecting a commit opens its changes (spec 008's entity cards) in a detail split.
- Search by id prefix or actor with an i/N counter; ↵ and ⇧↵ step through the matches, and
  typing in the field never moves the selection.
- Any commit can open a type table or query editor pinned to it (read-only chip): the commit
  detail's Table menu (the commit's types first) dispatches `OpenType { at_commit }`, its Query
  button `OpenQueryAt`. A pinned table reads that snapshot for every page and count, ignores the
  branch and its head, and refuses edits, inserts and deletes with the reason; a pinned query
  reads the snapshot and refuses writes. Both come back after a restart.
- The bottom-dock panel follows the window's branch and graph; rows only, and a click opens the
  commit in its own tab.
- More than 5,000 commits: a warning that the list loads in full until paging exists.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Fork Zed's lane assignment and its property tests | Write lanes from scratch | GPL accepted; the tests are the specification |

## Deferred

- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E10 paged commit list: the full list loads, virtualized, with a warning above 5,000 commits,
    until then.
  - E9 branch details: branch chips derived from per-branch commit lists until then.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | `git_graph.rs` exists at the pin | `git ls-tree -r v1.23.2 crates/git_ui/src` | Present |
| 2026-10-12 | Lanes connect and never collide | `history::lanes` property test over 2,000 seeded random commit graphs (up to 60 commits, merges, missing parents): every edge is one line, rows join edge to edge, no two lines share a lane, width bounded | Passes; a mutation that leaves ending lanes busy fails at seed 0 |
| 2026-10-12 | Merges draw against a live server | offscreen snapshot of main after merging `feature-y` and fast-forwarding `feature-z` | Both merged lanes join main; heads shown as branch chips |
| 2026-10-12 | Parentless commit | live server: `GET /commits/{genesis}/changes` | 409 "has no first parent"; the detail says the commit cannot be compared |
| 2026-10-12 | Pinned tables and queries read their commit | offscreen snapshot restoring `Person @ 4S3Y6CKK`, `Person @ EP12CTBE` and a query at `4S3Y6CKK`, with the History panel open | Tabs named with the commit, a "Read only · 4S3Y6CKK" chip, footer hints reduced to Peek; the query's result reads "people on commit 4S3Y6CKK" |
| 2026-10-12 | Search matches by id prefix or actor | `history` test `search_finds_commits_by_id_prefix_or_actor` | An id matches only by prefix; actors match anywhere, case-blind |

## Corrections

- 2026-10-12: lanes are a fresh implementation of the idea in Zed's `GraphData::add_commits`, not a
  fork: each row records the lines through it, ending at its commit and leaving it, so every
  row of the virtualized list draws itself with a `canvas`. The property tests are Studio's own.
- 2026-10-12: `GET /commits?branch=` lists one branch's first-parent chain; merged-in commits appear
  only as `merged_parent_commit_id`. History also reads up to 20 other live branches' lists and
  adds every commit reachable through a merged parent (`history::merged`). Merges from deleted
  branches draw as a short stub. E9 and E10 remove this.
