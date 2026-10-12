# 009: History

Status: draft · 2026-10-11

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
- Search by id prefix or actor with an i/N counter.
- Any commit can open a type table or query editor pinned to it (read-only chip).
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

## Corrections

None yet.
