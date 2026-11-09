# 026: Commits panel

Status: in progress · step 1 built 2026-11-09

## Problem

Ragnor wants to see what happens on a graph as it happens (2026-11-09): the newest commits on
every branch in one list in the sidebar, each opening the commit, so changes made by people and
agents can be reviewed live. History (spec 009) is a tab per branch, with lanes; it answers
"how did this branch get here", not "what just landed anywhere".

## Principles

V (nothing moves under you: a commit that lands adds a row at the top, the rows below keep
their order), IX (only what is shown is read), and spec 020's pick rules: a click previews the
commit's tab, a double click keeps it.

## What exists today (checked)

- `GraphSession::commit_page(branch, limit, token)` reads one branch's commits, newest first
  (`GET /graphs/{id}/commits?branch=&limit=`), `CommitOutput` names the branch that made each
  (`graph_branch`, `None` for main), its actor and its time.
- `GraphSession::watch_head(branch)` adds a branch to the heads the session asks after every
  30 s while the window is active (`HEAD_POLL`); a moved head emits `SessionEvent::Committed`.
  `BranchesChanged` says the branch list changed.
- `commit_view::open_in` opens a commit's tab, with the commit as listed.
- Left-dock panels are added with `Workspace::add_panel` (Data, Queries, Branches).

## Design

- **A left-dock panel, "Commits"** (id `commits`), beside Data, Queries and Branches. Spec 020
  keeps primary content out of the bottom dock; this list is a way into commits, as the Data
  panel is into types, so it sits on the left.
- **Rows:** the newest commits of every branch, newest first, one row each: the commit's short
  id and who made it, then its branch and how long ago. A commit listed on several branches
  (a branch's first-parent chain holds main's commits before it began) shows once, under the
  branch that made it. The tooltip gives the full id, the actor, the branch and the time.
- **Reading:** each branch's newest page, `PAGE` commits, read when the panel is drawn; a
  hidden panel reads nothing. At most `BRANCHES` branches are watched, in the session's branch
  order (main first), so a graph with many branches does not cost a head read each per poll.
- **Live:** while the panel holds them, its watches keep every branch's head asked after; a
  commit landing on one reads that branch's newest page again, and a branch created or deleted
  is read or dropped. The time column is drawn again each minute while the panel shows.
- **Change counts:** each drawn row shows what its commit added, changed and removed, read as
  History reads them (`counts::RowCounts`, shared): one page of up to 100 changes per commit,
  once scrolling pauses, kept for the panel's life; past 100 the count says "+". The graph's
  first commit, and one that cannot be compared (a schema change) or is past cleanup, shows none.
- **Open:** a click previews the commit's tab, a double click keeps it (spec 020).

### Build order

1. The panel: rows across branches, live, click to open, with change counts. *Built 2026-11-09.*
2. Keyboard (↑ ↓ ↵), a filter by branch or actor, and a mark on the commits that landed since
   the panel was last looked at.

## Decisions

- **One list across branches, not a lane graph.** The lanes need one branch's whole chain;
  this list wants only the newest of each. History stays the place for a branch's shape.
- **Polling, not a feed.** The server has no stream of commits across branches; the session's
  30 s head poll is what tells Studio a commit landed, as it tells every other view.

## Deferred

- **E44** (spec 012): the counts in the commit listing, exact and with no read of the changes.
- A stream of commits across branches from the server, so the panel learns of a commit when it
  lands rather than at the next poll.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-09 | The panel lists the newest commits of every branch once each, newest first, and a branch that could not be read after them | `history` `commits_panel::tests::rows_are_every_branchs_newest_commits_once_each` | Pass. Not yet checked in the running app |
| 2026-11-09 | History and the panel read change counts through one `RowCounts`: the drawn rows', once, never the first commit's | `history` `the_first_commits_changes_are_never_asked_for` (History's rows, through `RowCounts`) | Pass. The panel's counts not yet checked in the running app |

## Corrections
