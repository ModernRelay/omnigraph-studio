# 009: History

Status: built on 0.13 · 2026-10-12 (`crates/history`): a History tab with the lane graph and a
commit detail split, `OpenCommit` as its own tab, a History panel in the bottom dock, search by
id prefix or actor, and type tables and query editors pinned to a commit.

## Problem

People need to see how a graph got here: commits by branch, merges, who changed what, and the
exact changes of any commit.

## Principles

II (a commit is an immutable snapshot), VI, IX (virtualized and paged), XI (lane logic forked
from Zed with notices).

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
  messages), built from `studio_ui::ds` like every list: the short id (code text), the change
  counts in a fixed column, branch heads as tags with a merge mark, then the actor and the
  relative time, muted and right-aligned. No column header; rows are inset pills at the data
  row height. The graph draws the branch's own lane in the accent and merged lanes in grey.
  The tab's toolbar is the shared one: title, then branch and commit count as meta, search and
  a refresh icon.
- Selecting a commit opens its changes (spec 008's entity cards) in a detail split, headed like
  the inspector: a small "Commit" label, the short id as the title (click copies the full id),
  then one meta row (actor, time, branch, parent and merged parent as links, change count).
  The per-type breakdown is the group headers over the cards, not a second summary.
- Search by id prefix or actor with an i/N counter; ↵ and ⇧↵ step through the matches, and
  typing in the field never moves the selection.
- Any commit can open a type table or query editor pinned to it (read-only chip): the commit
  detail's Table menu (the commit's types first) dispatches `OpenType { at_commit }`, its Query
  button `OpenQueryAt`. A pinned table reads that snapshot for every page and count, ignores the
  branch and its head, and refuses edits, inserts and deletes with the reason; a pinned query
  reads the snapshot and refuses writes. Both come back after a restart.
- The bottom-dock panel follows the window's branch and graph; rows only, and a click opens the
  commit in its own tab.
- Built (paging, E10, 2026-10-20): History reads its branch's newest 200 commits, and the
  next page when the rows drawn come within a screen of the list's end; every later page
  answers for the head the first page resolved (`head_commit_id`), so the pages stay one
  history while the branch moves. A commit landing on the branch reads the first page again:
  when it reaches the head the pages were read from, the new commits go on top and the pages
  read stay, else it replaces them. A server that pages no history answers the whole list,
  read as one page. When a later page fails, the list's last row says why and offers Try
  again, which reads the listing from its first page when the server refused the page's
  continuation. Search finds the commits read so far, and a page read finds them again.
- Built (row counts): a drawn row without counts reads its commit's first 500 changes and nets
  them as the detail does ("+ more" when there are more); answers are kept, since a commit's
  changes never change, and the detail's full count replaces a first-page one. A commit
  without a first parent shows none.

Other branches' tags come from each branch's newest commit, read alone (a page of one) and
kept by the session until a commit lands on that branch (spec 003), for up to 20 branches; a
Refresh reads them again for every view. A branch's listing names a merged-in commit only as a
`merged_parent_commit_id`, so History reads each by id (`GET /commits/{id}`), then the parents
it names, as far back as the oldest commit of the pages read: at most 200 in one walk, 8 at
once, the rows drawn first. A walk follows each page read and starts again while a drawn row
names one not read; a read that failed is asked again 5 s later, while its row is drawn. That
route checks `read` on no branch, so it refuses (403) a token whose read grant names branches:
History then reads up to 20 other branches' whole lists once, until the next load or Refresh. A
merged parent not read yet, or not had, draws as a stub.
A row's change count is read once scrolling has paused for 120 ms, and reads for rows scrolled
away are dropped.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Fork Zed's lane assignment and its property tests | Write lanes from scratch | GPL accepted; the tests are the specification |

## Deferred

- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E10 paged commit list (OmniGraph #1013): built 2026-10-20 (Design). Two parts differ from
    the plan here before: a commit landing reads the first page again and keeps the pages it
    reaches, rather than showing "N newer commits · Refresh"; and merged-in commits are read by
    id rather than from other branches' paged lists, which start at those branches' heads now.
  - E21 a branch from a commit: landed (OmniGraph #1016), adopted with the same bump. A commit's
    menu and its detail offer "New branch here…", which starts a branch at that commit
    (spec 008).
  - E9 branch details: branch chips derived from per-branch commit lists until then.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | `git_graph.rs` exists at the pin | `git ls-tree -r v1.23.2 crates/git_ui/src` | Present |
| 2026-10-12 | Lanes connect and never collide | `history::lanes` property test over 2,000 seeded random commit graphs (up to 60 commits, merges, missing parents): every edge is one line, rows join edge to edge, no two lines share a lane, width bounded | Passes; a mutation that leaves ending lanes busy fails at seed 0 |
| 2026-10-12 | Merges draw against a live server | offscreen snapshot of main after merging `feature-y` and fast-forwarding `feature-z` | Both merged lanes join main; heads shown as branch chips |
| 2026-10-12 | Parentless commit | live server: `GET /commits/{genesis}/changes` | 409 "has no first parent"; the detail says the commit cannot be compared |
| 2026-10-12 | Pinned tables and queries read their commit | offscreen snapshot restoring `Person @ 4S3Y6CKK`, `Person @ EP12CTBE` and a query at `4S3Y6CKK`, with the History panel open | Tabs named with the commit, a "Read only · 4S3Y6CKK" chip, footer hints reduced to Peek; the query's result reads "people on commit 4S3Y6CKK" |
| 2026-10-12 | Change counts on rows | offscreen snapshot of a scratch branch with two edits on top of main | `~3` and `~1` on the edits, `+500 + more` on the seed load, none on the first commit |
| 2026-10-12 | Search matches by id prefix or actor | `history` test `search_finds_commits_by_id_prefix_or_actor` | An id matches only by prefix; actors match anywhere, case-blind |
| 2026-10-20 | History reads a page at a time, merged-in commits by id | `history` tests `history_reads_its_branch_a_page_at_a_time`, `an_old_server_answers_the_whole_history_as_one_page`, `a_commit_landing_on_the_branch_keeps_the_pages_read`, `try_again_after_a_refused_page_starts_from_the_first`, `merged_in_commits_are_read_by_their_ids`, `a_merged_branch_is_read_as_far_as_the_pages_read`, `a_long_merged_branch_is_read_as_its_rows_are_drawn`, `merged_reads_go_to_the_rows_drawn_first`, `a_failed_merged_read_is_asked_again_after_a_while`, `a_token_refused_commits_by_id_reads_merges_from_the_listings`, `tags_sit_at_each_branchs_newest_commit`, `a_commit_to_another_branch_moves_its_tag_alone`, `a_search_finds_matches_a_later_page_brings`, against a fake server that pages as #1013 does | Pass; no listing is read whole but a 0.14 server's that pages none, and the other branches' after a 403 by id |
| 2026-10-20 | A deleted branch's commits by id | OmniGraph `GraphCoordinator::resolve_commit` at `0a62bb7` (source read) | `GET /commits/{id}` serves them once they have moved into the graph's `__history`; its latest commits, still buffered in the branch's retired `__manifest`, answer 404 and draw as stubs |

## Corrections

- 2026-10-12: lanes are a fresh implementation of the idea in Zed's `GraphData::add_commits`, not a
  fork: each row records the lines through it, ending at its commit and leaving it, so every
  row of the virtualized list draws itself with a `canvas`. The property tests are Studio's own.
- 2026-10-12: `GET /commits?branch=` lists one branch's first-parent chain; merged-in commits appear
  only as `merged_parent_commit_id`. History also reads up to 20 other live branches' lists and
  adds every commit reachable through a merged parent (`history::merged`). Merges from deleted
  branches draw as a short stub. Superseded on 2026-10-20 (below).
- 2026-10-20: History reads merged-in commits by id, not from the other branches' lists, which
  it reads only for a token refused commits by id (Design). A merge from a deleted branch draws
  as a lane once its commits are in the graph's `__history`; its latest commits, still buffered
  in the branch's retired `__manifest`, answer 404 and draw as a stub.
