# 040: Why something changed: commit intent and a node's history

Status: built 2026-11-19 · a node's History tab (page, inspector and table), History rows that
say what a commit was, a search that finds a node's commits, and review cards that name who set
each value where more than one actor did. The operation and message of a commit (E60) and one
entity's commits (E62) wait on the engine.

## Problem

The cookbook study (`docs/ux/2026-11-18-cookbook-study.md`, proposal "Why something changed",
on branch `docs/ux-cookbook-study`) walked seven graphs where people ask *who changed this and
why*, and Studio could not say:

- **History is ids and counts.** An agent's `claim`, `link_blocks`, `comment` and `close` on
  beads are four rows of "~1" and "+1" beside `act-writer` (f05). The CLI's receipt names the
  stored query (`query_name: close`); the commit keeps none of it.
- **A node has no history.** "When did the agent last touch `lk-queue`?" and "who changed this
  deal's stage?" (beads B2, pharma R4, vcos) mean opening commits one by one. History's search
  answers "No match" for `lk-acl`: it matches ids and actors only. Neither a node's page nor the
  inspector has a history.
- **A review card mixes authors.** One card merges the admin's priority edit and the agent's
  close, "4 commits", and says nothing of who set which value (beads B4, f02).
- **Everyone is `act-writer`.** The real author lives in an `updatedBy: agent:orion` property,
  or nowhere (dev D3, brain N3).

## Principles

- **I.** Every read goes through `graph_client`; a node's history is read from the server's
  change feed, never from a copy Studio keeps.
- **II.** A node's history is read at one head: the head the commit listing resolved, which the
  feed stops at; older windows answer for the same head. A commit on the branch offers Refresh.
- **VI.** A disabled History view says why (a page or table held at a commit).
- **VIII.** History is an action: the view strip's fourth place (⌘⌥4 / Ctrl+Alt+4),
  `OpenAsHistory` in the palette, `ShowNodeHistory` for the inspector and the UI's buttons, and
  `ReadOlderCommits` (⌘⌥N / Ctrl+Alt+N) in the tab.
- **IX.** A window is the branch's newest 200 first-parent commits, read in at most 12 feed
  pages of at most 8,192 changes; the honest exit is "Read the 200 commits before".
- **X.** What needs the engine (a commit's operation and message, one entity's commits) is
  designed against today's API and recorded as E60 and E62.
- **XIII.** A node's history is a view of one node: a tab opened from the view strip of its
  page, tree, canvas or table row, beside them.

## What exists today (checked)

- **Omnigraph `v0.15.0`.** `CommitOutput` has `actor_id`, `created_at`, `graph_branch`,
  `graph_commit_id`, `graph_manifest_version`, `kind`, `merged_parent_commit_id`,
  `parent_commit_id`, `published`, `root_commit_id`: no operation and no message
  (`crates/graph_client/openapi/omnigraph-0.15.0.json`). A write's receipt (`ChangeOutput`)
  names `query_name`, "the declared mutation's name"; it is not kept with the commit.
- **The change feed.** `GET /graphs/{id}/changes?branch=&start=after:<commit>&type=…` answers one
  block per first-parent commit, each with its `cause` (commit, actor, `authored_at`,
  `authored_branch`, parents) and its changes with before and after images, filtered by type
  (repeatable `type`). A page ends on a whole commit and covers up to 128 commits (E38). No
  filter takes an entity id (E62).
- **A merge in the feed** is one block, by whoever merged, with the merged changes: on beads, an
  agent's `close` on `agent/close-acl` merged by `act-admin` reads as `act-admin`'s change on
  `main` (validation log). The branch's own commits are not in main's feed.
- **Studio** logs every write it makes and every write it sends for the agent's CLI with its
  words and outcome (`GraphSession::outcomes`, the newest 1,000; `external_write` for the
  gateway's), so "Set priority of lk-acl" or the agent's write is known by its commit for the
  session.
- **History** (spec 009) reads each drawn row's first 100 changes for its counts
  (`counts.rs`, `ROW_COUNT_PAGE`); the review (spec 008) folds the branch's feed per entity
  (`graph_store::ChangeFold`), each change with its cause.
- **Views.** `ViewKind::History` exists (a branch's history); a node's page offers Page, Tree,
  Canvas (`type_table/src/node_page.rs`), the inspector Page, Tree, Canvas buttons
  (`peek.rs`), a node table's strip Table, Tree, Canvas (`table.rs`). `OpenViewAt` was bound for
  places 1 to 3 only.

## Design

### A node's History tab

- **Opened from** the node's page (fourth place of its view strip), the inspector's **History**
  button beside Page, Tree and Canvas, and a node table's strip ("Changes to *name*", for the
  row under the cursor). ⌘⌥4 / Ctrl+Alt+4 opens it from any of them, ⌘⌥⇧4 beside;
  `studio_graph::OpenAsHistory` in the palette ("Show the history of what the active tab
  shows: a branch's commits, or the commits that changed a node"). A page or table held at a
  commit offers it disabled: "A page held at a commit has no history; open its node at the
  branch head".
- **Tab.** "History · *name*" (the node's label once read, else its id), the history icon, key
  `node-history:<type>:<id>`, restored as `node_history`. Its strip: Page, Tree, Canvas,
  History. The inspector shows the node at the head read.
- **What it reads** (`GraphSession::node_history`, `graph_store::node_history`): the window's
  branch (it follows a branch switch) at its head. A window is the newest 200 first-parent
  commits (`GET /commits?branch=&limit=200`), then the change feed from just before the oldest
  of them (`start=after:<its parent>`, or `beginning`), filtered to the node's type and every
  relationship type that ends at it (`type=Issue&type=BlockedBy&…`), in pages of 8,192 changes,
  at most 12, read off the main thread. The feed stops at the head the listing resolved. The
  commits that changed the node, or a relationship whose end of the node's type is the node,
  are kept, newest first; a change that ends where it began is left out.
- **An entry** is one commit: the kind's mark (+ ~ −, of the node's own change), the actor, what
  it was (Studio's words when Studio made it this session; "Merge of commit *X*" for a merge),
  how long ago (the UTC time in its tooltip) and the commit's short id, which opens the commit's
  tab. Under it the node's changed properties as a review's card shows them (`change_cards::
  rows_of`), and its relationship lines ("+ comment on ← c-acl-1").
- **The end row** says how far it read: "200 commits read. Older changes are in the commits
  before them." with **Read the 200 commits before** (⌘⌥N / Ctrl+Alt+N, `ReadOlderCommits`),
  which reads the next listing page (for the same head) and the feed up to where the window
  above began; or "N commits read, back to the graph's first". A failed older read keeps what
  was read and offers Try again. A feed that stops short says so: "The read stopped short: … The
  changes shown are those read before it."
- **States.** Reading (the change cards' reading state); "The node's history could not be read"
  with the reason and Try again; "No commit on main changed *id* or its relationships since the
  graph began." Toolbar: the view strip, "*N* changes on *branch* in the newest *M* commits",
  "Newer commits · Refresh" once the branch moved, and the refresh button (⌘R).
- **Drawing.** The entries are a cached view of their own (`NodeHistoryRows`), made with the
  tab; a commit on another branch draws nothing.

### History rows say what a commit was

- A row reads as a sentence: lane, id, counts, branch tags and merge mark, then **what it was**
  (muted, cut to fit), then the actor and the time. What it was, in order:
  1. Studio's own words, for a commit Studio made or sent for the agent this session
     (`GraphSession::own_words`): "Set priority of lk-acl".
  2. A merge whose merged commit is a branch's head: "Merge agent/close-acl".
  3. The node it changed, once its counts are read: "Issue lk-acl", "Issue lk-acl and 2 more";
     a commit that changed only relationships names their ends.
- The row's card and the inspector say the same ("What").
- **Search** finds a commit by its id, its actor, a node its counted changes touched, or
  Studio's words for it. The field reads "Find a commit by id, actor or node". A node's whole
  history is its History tab; the search finds what History has read.

### Review cards name who set each value

- The review's fold keeps, for each property of each entity, the commit and actor that set it
  last (`FoldedChange::set_by`). A card whose shown values were set by more than one actor ends
  each row with the actor ("priority 1 → 0 · act-admin", "status open → closed · act-writer"),
  "Set by *actor* in commit *X* · *time*" in its tooltip. A card set by one actor says nothing
  more: the header names who made the branch's commits. A review read from the net diff (a
  branch its target was merged into) names no commit, so it names no setter.
- A commit's tab and a node's History show one commit per card, so they never name setters.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| A node's history is a tab of its own in the node's view strip | A section on the page; a panel in the inspector | It has reads, a window and a state of its own: constitution XIII makes it a tab. The page stays the node as it is; the inspector opens the tab as it opens the tree and canvas |
| Read a window from the change feed filtered by type | One `GET /commits/{id}/changes` per commit, filtered in Studio (the study's sketch) | 200 requests against two; the feed covers up to 128 commits a page and filters by type on the server. Measured below |
| A window of 200 first-parent commits, with "Read the 200 commits before" | The whole history; a time window | Bounded work with an honest exit (IX); the listing's own page token keeps older windows on the same head |
| Relationships that end at the node are part of its history | Only the node's own row | "Who linked this comment", "who blocked this issue" are the questions of beads and pharma |
| A merge is shown as the merger's commit, with the merged commit's id | Walk the merged branch's commits to find the original author | The feed says only the merger; walking a merged branch per entry is unbounded reads for one row. E62 is to name the commits that changed the entity wherever they were made |
| Operation words come from Studio's own outcome log, this session only | Persist them; parse the server's log; ask the CLI | Constitution I: Studio keeps no copy of graph data it treats as true; words it said of its own writes are not the graph's. E60 makes them the server's |
| Name setters only on a card set by more than one actor | Always; never | Always repeats the header's one actor on every row; mixed cards are the case the study found |
| `ReadOlderCommits` is a new action | Reuse `LoadMoreChanges` | Different thing, different words; both bound to the same key in their own context |
| View places 4 get ⌘⌥4 / ⌘⌥⇧4 | Palette only | A node's views are four; every view place has a key (keymap test) |

## Deferred

Recorded in `docs/deferred-engine-changes.md` and spec 012:

- **E60** ([#1470](https://github.com/ModernRelay/omnigraph/issues/1470)): a commit's operation
  (the stored query's name, `mutate`, `load`, `merge`, `blob put`) and an optional
  writer-supplied message in `CommitOutput` and the feed's `cause`; the actor who created a
  branch in E9's details. With it, a row reads "act-writer · close · Issue lk-acl" for any
  writer's commit, and a node's History names the operation of each entry. Without it, Studio's
  own words for its own writes this session, else the node changed.
- **E62** ([#1472](https://github.com/ModernRelay/omnigraph/issues/1472)): the commits that
  changed one entity, paged, including commits merged in from other branches with their own
  actors. With it, a node's History is one request with no window, and a merged change names
  who made it on the branch. Without it, windows of 200 commits from the feed, and a merge
  names its merger.
- Already deferred and used here: **E20** (delegated credentials recorded beside the actor)
  answers "which agent" when every agent is `act-writer`; **E44** (counts in the listing) would
  give a row its counts and touched nodes without reading its changes.

Not in this spec: the per-field Revert of the review (study, Appendix A), and stamping
`updatedBy` on hand edits (proposal "The graph's own verbs").

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | `CommitOutput` has no operation or message; `ChangeOutput` names `query_name` | `omnigraph-0.15.0.json` schemas `CommitOutput`, `ChangeOutput` | As stated |
| 2026-11-19 | The feed answers one block per first-parent commit with its actor, filters by repeated `type`, and a merge is one block by the merger | Beads cookbook on `omnigraph-server` `v0.15.0` (release binary), seed loaded; `claim lk-acl` and `comment` on main as `act-writer`; `close lk-acl` on `agent/close-acl` as `act-writer`, merged by `act-admin`; `GET /changes?branch=main&start=beginning&type=Issue&type=BlockedBy&type=CommentOn` with curl | Five blocks: the load (act-admin), the claim (act-writer, `Issue lk-acl update`), the comment (act-writer, `CommentOn` insert), and the merge (act-admin, `merged_parent_commit_id` set, the close's `Issue lk-acl update`). `set=merge_lineage=on/off` changes nothing in the feed |
| 2026-11-19 | A node's History is two requests a window and lists who changed the node, newest first | `node_view` tests `a_nodes_history_reads_a_window_of_commits_and_the_one_before` (250 commits; listing then feed from `after:c49` with the node's three types; older window from `page_token=200`, feed from `beginning`, stopping before `c50`) | Pass |
| 2026-11-19 | The entries draw only for what they show | `the_history_draws_its_list_only_for_what_it_shows`: another branch's commit draws the list 0 times; a commit on the branch 1 (Refresh offered); reading older commits 2 | Pass |
| 2026-11-19 | A review card names who set each value when more than one actor did | `graph_store` `a_fold_keeps_who_set_each_property_last` (beads B4's two commits) | Pass |
| 2026-11-19 | A row names the node a commit changed, and a search finds it | `history` `a_counted_commit_names_the_nodes_it_touched`, `a_commit_says_what_it_was`; `graph_store` `own_words` in the counts test | Pass |

## Corrections

None yet.
