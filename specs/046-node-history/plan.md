# 046: A node's history

Status: step 1 built 2026-11-20 · a node's History view (its fourth view, ⌘⌥4), "Now" (who set
each value), changes newest first with runs folded, windows of 200 commits read updates and
deletes first and inserts under a cap, and the page's "Last changed by" line. Compare, Restore
and the "not merged" section are the next steps. One entity's commits (E62) and a commit's
operation (E60) wait on the engine.

Stacked on #161 (spec 014, "The page as a document"), which is not merged yet.

## Problem

People ask *what happened to this node, who changed it, and when*. The cookbook study (#143)
found the question in seven of its eight graphs, and the design study (#160,
`docs/ux/2026-11-20-node-history.md` on `docs/node-history-study`) sorts it into five jobs: who
set this value (H1), what happened to this node (H2), was it like this last week (H3), is anyone
changing it now (H4), where did it go (H5). Studio answers none of them: History lists a
branch's commits, and finding one node's changes means opening commits one by one. The closed
#146 (spec 040) built a first History tab; a teammate closed it, and the study measured what it
missed: a bulk load in the window made one person's six changes cost 13.2 MiB.

Step 1 answers H1, H2 and H5.

## Principles

- **I.** Every read goes through the session and `graph_client` (`GraphSession::node_history_in`,
  `GraphClient::changes_within`). Studio keeps no history of its own: the session holds only the
  node's last change as its history read last found it, for the page, and forgets it with a new
  token.
- **II.** Every window answers for the head the first listing resolved; an older window goes on
  from the listing's page token for that head. A commit on the branch offers Refresh.
- **V.** Nothing is drawn until a window is whole; a newer commit shows "Newer commits ·
  Refresh", never a jump. The page's line appears only once the history is read, and says
  nothing before.
- **VI.** The insert cap says what it skipped; "before <time>" says where the read stopped.
- **VIII.** Every action has a key and a palette entry: ⌘⌥4 (`OpenViewAt` place 4) and
  `studio_graph::OpenAsHistory`, `ReadOlderCommits` ⌘⌥N, ↑/↓ and J/K, ↵, Space, ⌘R.
- **IX.** 200-commit windows, at most 12 feed pages a pass, 8 MiB of inserts a window, 20 merges
  named a window.
- **X.** Designed against contract 0.14 (`omnigraph-0.15.0.json`). Waits on E60 and E62
  (recorded in spec 012 and `docs/deferred-engine-changes.md`).
- **XIII.** One tab, a view of one node, beside its page, tree and canvas. The page and the
  inspector read no history.

## What exists today (checked)

- **Routes** (`crates/graph_client/openapi/omnigraph-0.15.0.json`): `GET /commits` pages a
  branch's first-parent commits (`limit` ≤ 1000, `page_token`, `head_commit_id`);
  `GET /changes` takes `start` (`beginning`, `after:<id>`), `page_token`, repeatable `type`,
  `kind` and `op` (`insert | update | delete`), `limit` ≤ 8192, and answers 409 with
  `change_diff_refusal.reason = schema_boundary` and 410 for a retention gap; `GET
  /commits/{id}` names `graph_branch` (none for main). No route takes an entity id (E62); no
  commit names its operation (E60).
- **The feed's images are whole entities**, so the properties a change set are found by comparing
  them (`FoldedChange::property_diffs`). A merge is one block by the merger, with
  `merged_parent_commit_id` (study, validation log).
- **Studio before this spec:** `graph_client::changes` sent no `op` and read no byte count;
  `change_cards` drew a change's rows only from a folded review (`change_rows`), and kept its
  Blob words private; the node page's view strip had Page, Tree and Canvas; ⌘⌥4 was bound on
  every platform to `OpenViewAt { index: 3 }`, which reached a type table's Gallery (spec 035) and
  nothing on a node.
- **#146** (`feat/ux-p5-node-history`): `graph_store::node_history` (scope, window fold) and
  `history::node_view` (the tab, the cached rows, `own_words`) are reused here, changed as below.

## Design

### Where it lives

| Place | What it shows | Reads |
|---|---|---|
| **History tab**, a node's fourth view (`node-history:<type>:<id>`, "History · *name*", restored as `node_history`) | Everything below | The read plan |
| Node page (#161's meta line) | "· Last changed by *actor* · *time*", muted, once the node's history was read at the commit the page shows; a click opens History; tooltip "In commit *X* · *UTC*. Open its History ⌘⌥4" | None |
| Inspector | A **History** button beside Page, Tree and Canvas | None |
| Tree and canvas of a node | History as their fourth view, of the root | None |

⌘⌥4 is `OpenViewAt { index: 3 }`. On a node's page, tree and canvas the fourth place was empty
and is now History. On a type table with files it stays Gallery (spec 035): this spec takes no
key from another view. A page held at a commit offers History too; it reads at the branch head.

### The tab

```
[Page] [Tree] [Canvas] [History]   9 changes on main in the newest 17 commits       ⟳
Now
 title    Principal Engineer, Graphs                 act-dev · 1 min ago · 4YR71DG3
 slug     person-000000                    no actor · created · 3 min ago · YSJP572K
 born     1978-03-20                                  before 2026-11-18 14:02 UTC
Changes
 ~ act-dev  on main                       2 commits ⌄   2 changes · 00:07–00:07, 1 min ago  4YR71DG3
     title   Principal Engineer → Principal Engineer, Graphs
 ~ act-dev  via merge of feat-title                                   1 min ago  EJS97HB4
 + no actor  created · on main                                        3 min ago  YSJP572K
Read back to YSJP572K, 3 min ago, the graph's first commit. 17 commits read.
```

- **Now (blame).** Every property of the node's type, in the schema's order: its value from the
  newest image read, and the newest entry that set it (an update whose images differ there, or
  the creation, which sets every property): actor, "created" or "via merge of *branch*", relative
  time, short commit; the full commit and UTC time in the tooltip. A property no entry read set
  reads "before *2026-11-18 14:02 UTC*": the oldest commit read, or, when a window's inserts were
  skipped, the newest commit whose inserts were not read, since a creation there would have set
  it. Never a guess past that. Blobs by size. When no commit read changed the node itself the
  values are "—" and a line says they are on its page. A node removed at its newest entry reads
  "Removed by *actor* in *X*, *time*. Its relationships went with it."
- **Changes**, newest first. Each entry: the kind's mark (`change_cards::kind_mark`; `·` for a
  relationship-only entry); the actor; "Studio" when the session made the commit
  (`own_words`), else "agent" when it was authored on `agent/<stamp>` (E20); "created" or
  "removed"; "on *branch*", or "via merge of *branch*" (the merged head's `graph_branch`, one
  `GET /commits/{id}` each, at most 20 a window; "via merge of commit *X*" past them or if the
  read fails); Studio's own words for its own write; the relative time (UTC in the tooltip); the
  short commit, which opens the commit's tab (↵ too). Under it the property rows and relationship
  lines as a review draws them (`change_cards::rows_of`, a Blob by size since #138), each line
  naming the other end whichever end the node is. A removal with no relationship read says its
  relationships went with it.
- **Folding.** Consecutive entries fold into one when they share the actor, the authored branch
  and the kind of author (Studio's own or not), each is less than 10 minutes after the one before,
  and none is a merge, a creation or a removal (the study's rule). A run of more than one starts
  folded: "*N* commits ⌄" and "*N* changes · 14:02–14:09, *time*", with its net change (the
  oldest image before, the newest after, for the node and each relationship). Space or the button
  unfolds it into its entries, the first carrying "Fold ⌃".
- **The end row** says how far it read ("Read back to *X*, *time*. 200 commits read." or "…, the
  graph's first commit."), with **Read the 200 commits before ⌘⌥N**; its tooltip gives requests
  and bytes read. It also says what a window skipped, a schema boundary, cleaned-up history, and
  a read that stopped short.

### Reading

Per window (`GraphSession::node_history_in`, `graph_store::node_history`), each request named in
the read ledger under `ReadView::NodeHistory`, none sent again by itself, no token, query text or
row logged:

1. `node_history_commits`: `GET /commits?branch=&limit=200` (`page_token` for an older window).
2. `node_history_changes`: the feed from just before the window's oldest commit
   (`after:<parent>`, or `beginning`), `type=` the node's type and each relationship type that
   ends at it, `op=update&op=delete`, pages of 8,192 changes, at most 12. The window ends at the
   first block of a commit the listing did not name (#146's review fix).
3. `node_history_inserts`: the same with `op=insert`, under 8 MiB a window
   (`NODE_HISTORY_INSERT_CAP`): each page is read with what is left of the cap as its own cap, so
   a page that would pass it stops after the cap and one byte (`GraphClient::changes_within`,
   `ReadError::TooLarge`). The commits newer than the last block read are said as skipped.
4. `node_history_merge`: `GET /commits/{merged_parent}` for each merge entry, at most 20.

Pages are filtered to the node in the request's own task, off the main thread, so a load's images
never reach it. The newest window keeps the node's last change in the session
(`GraphSession::last_change`, `SessionEvent::NodeHistoryRead`) for the page.

A 409 `schema_boundary` at commit *B* in the window ends the history there (E52): the window is
cut to the commits after *B*, the feed is read again from `after:B`, no older window is offered,
and the end row says "History stops at a schema change to *Type* in *B*". A 410 says the history
before was cleaned up.

### States

| State | What it says | Next |
|---|---|---|
| Reading | "Reading the history…" (`ds::loading_state`) | — |
| Failed | "The node's history could not be read" and the reason | Try again (⌘R) |
| Nothing in the window | "No changes in the last 200 commits" · "No commit in the newest 200 on main changed *name* or its relationships." | Read the 200 commits before ⌘⌥N |
| Nothing since the graph began | "No changes" · "No commit on main changed *name* or its relationships since the graph began." | — |
| Schema boundary, nothing after it | The schema-change state with the commit | — |
| Inserts skipped | On the end row: "New nodes and relationships in the newest *N* commits of a window were not read: they hold more than 8 MB of new rows, such as a load…" | Read older commits |
| Older read failed | The end row's reason, and Try again in place of the button | ⌘⌥N |

### Keys

| Key (macOS; Ctrl on Linux and Windows) | Action | Context |
|---|---|---|
| ⌘⌥4 / ⌘⌥⇧4 | `studio::OpenViewAt` place 4 (bound since 0.4) | Workspace |
| ↓ J / ↑ K | `menu::SelectNext` / `SelectPrevious`, between entries (Now and the end row are passed over) | `NodeHistory` |
| ↵ | `menu::Confirm`: open the selected entry's commit (a run's newest) | `NodeHistory` |
| Space | `studio_changes::ToggleChange`: fold or unfold the selected run | `NodeHistory` |
| ⌘⌥N | `studio_history::ReadOlderCommits` (new) | `NodeHistory` |
| ⌘R | `studio::Refresh` | Workspace |

`studio_graph::ShowNodeHistory { type_name, id, preview }` (new) opens the tab from the
inspector's button, the page's line, and the views' strips; it takes arguments, so it has no key
of its own, as `ShowNodeOnCanvas` has none.

### Drawing

The rows are a cached view of their own (`NodeHistoryRows`), made with the tab, which answers
`Item::caches_its_parts`. A key draws them once; another branch's commit draws nothing.

## Decisions

| Decision | Rejected | Why |
|---|---|---|
| Two feed passes: updates and deletes, then inserts under 8 MiB | One pass of every op (#146); a byte cap on the whole window | Measured: a load cost 13.2 MiB of inserts for 0.3 MiB of edits (study). Edits are what a node's history is mostly made of; the cap falls only on the pass that a load inflates |
| The cap is each page's own read cap, as what is left of 8 MiB | Read whole pages and stop after | A page over the cap is never downloaded past it, and never decoded |
| The skipped commits are the newer part of the window, said as an upper bound | Read the skipped inserts by default; a binary search of point reads for the creation (study) | The feed reads forward only. "Read them anyway" and the binary search are next steps; until then the end row and "before" say exactly what was not read |
| "before" is bounded by the newest skipped commit when inserts were skipped | The oldest commit read | A creation in a skipped commit set every property there, so only that bound is true |
| Blame from the entries read | A point read of the node at the head for its values | The newest image holds every value; with no image read the values are on the page, one click away, and History reads nothing it does not show |
| A schema boundary ends the history after it | Read the commits before it with another feed | Across E52 the type's images do not compare; the study's "Read before the change" is left to E52 |
| ⌘⌥4 keeps meaning "the fourth view" | A History key of its own | It is bound already; on a node the fourth place was empty, and a table's Gallery keeps it |
| The page's line in its meta line, shown only for the head the history read | Under the properties (study sketch); always, from any read | #161 made the meta line the page's quiet facts; a line from another commit would describe a node the page does not show |
| History from a page held at a commit reads at the head | Disabled (#146); marking the held commit (study) | It answers H1 from anywhere; the marker is a next step |
| Agent named only from an `agent/` branch, Studio only from this session's outcome log | A person otherwise (study) | "Person" would be a guess; an unnamed actor is just its id |
| Merges named by one commit read each, at most 20 | The branch list's heads; walking the merged branch for the original author (study) | A head moves and a branch is deleted; a commit's `graph_branch` never changes. The walk is the next step and E62's job |
| `ReadOlderCommits` is a new action, ⌘⌥N in `NodeHistory` | `LoadMoreChanges` | A different thing in different words; ⌘⌥N means "more" in each context that binds it |

## Next steps

1. **Compare** two entries, or an entry and now, as point reads of the node (study, H3).
2. **Restore** a value into the page's draft for the window's branch, saved as any edit (study,
   #158's rules).
3. **Not merged**: branches that change the node, one line each, with "Will conflict with main"
   (study, H4).
4. "Read them anyway" for skipped inserts, and a binary search of point reads for a creation the
   cap skipped.
5. "Who on *branch*?": the bounded walk of a merged branch for the original author, until E62.
6. A filter by property, actor and relationships only; "Hide loads" (E44).
7. Opening History from a commit's tab or a review card with the commit marked.
8. New commits from the session's change log as a chip, with no Refresh.

## Deferred

In spec 012 and `docs/deferred-engine-changes.md`:

- **E62** ([#1472](https://github.com/ModernRelay/omnigraph/issues/1472)): one entity's commits,
  paged, merged-in commits under their own actors; *extended here*: each change names the
  properties it set, a `property=` filter, and a blame answer. With it History is one request
  with no window or cap, and blame is exact at any depth.
- **E60** ([#1470](https://github.com/ModernRelay/omnigraph/issues/1470)): a commit's operation
  and message, and who created a branch.
- Used as they stand: E20 (which agent), E44 (counts in the listing, for "Hide loads"), E52
  (across a schema change), E59 (Blobs in images), E35 and E38.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-20 | The routes, parameters and refusals the design rests on | `omnigraph-0.15.0.json`: `/changes` and `/commits/{id}/changes` (`op`, `type`, `kind`, `limit` ≤ 8192, 409, 410), `/commits` (`limit` ≤ 1000, `page_token`), `CommitOutput.graph_branch`, `ChangeDiffRefusalOutput` | As stated. `graph_client` `every_route_the_client_calls_is_in_the_contract` now calls `changes_within` |
| 2026-11-20 | A filtered feed read names its ops, says its bytes, and stops at its cap | `graph_client` `a_filtered_feed_read_names_its_ops_and_keeps_to_its_cap` | Pass |
| 2026-11-20 | Scope, passes, blame, folding and the boundary as pure functions | `graph_store` `node_history::tests` (6): either end of a relationship, both passes in one entry, a pass ending past its window, blame and "before", a run's net change and Studio's own writes apart, the boundary read from a 409 | Pass |
| 2026-11-20 | The view: entries, folding, relationships at both ends, blame, the window and "before", creation and removal, the insert cap, the empty, reading and failed states, the schema boundary, the keyboard and the draw count | `history` `node_view::tests` (10), FakeHttpClient behind the session: 250 commits paged as Omnigraph pages, the feed in pages of 20 commits filtered by `op` | Pass. A window reads 1 listing, 10 + 10 feed pages and 1 merge, all `View(NodeHistory)` in the ledger; c240+c239 fold to title A → C; ⌘⌥N reads `page_token=200` then the feed from `beginning`, stopping at c50, and the key's blame moves from "before c50" to c5's creation; a 9 MiB load at c220 stops the insert pass after 8 pages and skips c210–c249; a 500 is asked once in 60 s, then Try again and ⌘R each read once; ↓ J K ↑ Space ↵ move, fold and open; each key draws the rows once, another branch's commit none |
| 2026-11-20 | The page says who changed the node only for the commit it shows | `type_table` `peek::page::tests::the_meta_line_says_who_changed_the_node_once_its_history_is_read` | Pass: nothing before a read, nothing for another head, the line for its own |
| 2026-11-20 | Measured reads of opening History on a node of the dev graph | `script/dev-server` on `127.0.0.1:8851` with its own data directory (`omnigraph-server` 0.15.0), history made with the 0.15.0 CLI through a direct target on the stopped server's data (the CLI refuses a token for an `http://` server, E46; the dev policy allows one actor, `act-dev`, so one actor): about 15 commits on `person-000000` and around it (title, email, tags, a `Knows` to her and a `WorksAt` from her, a branch `feat-title` merged after main moved, a person created and removed, `person-000099` removed). 17 commits on main. Debug `snapshot` example, `reads:` step, then `curl` for sizes; Apple M5 Pro, load average 6 to 7 | Opening History on person-000000: 4 requests, settled in 426 ms from the action (`node_history_commits` 12.5 ms, `node_history_changes` 54.6 ms, `node_history_inserts` 40.3 ms, `node_history_merge` 12.5 ms), beside the inspector's own 6 reads of the node. Bytes: listing 6.0 KB, updates and deletes 11.3 KB, inserts 281.8 KB (the seed load's people and relationships), the feed of every op 288.2 KB. 9 changes, in 4 rows: a run of 2, the merge ("via merge of feat-title"), a run of 5 with both relationship ends, the creation (seed load, no actor) |
| 2026-11-20 | It looks right, light and dark | Offscreen snapshots at 1440 × 900, 2x, against the same server: History of person-000000 light and dark, its run of 5 unfolded by ↓ ↓ Space, the page's "Last changed by act-dev · 2 min ago" after History was read, person-000099 removed at the head, and person-999999 (no changes) | As in the PR. Removed: "Removed by act-dev in 382BYEJ6…", the removal with its 9 relationships, its creation |
| — | Not checked | A remote server's latency; the 230,000-person seed and the study's 50,000-person load against the real cap; a real schema boundary; a 0.14 server; Studio's own writes labelled "Studio" in the running app; two actors | Open |

## Corrections

None yet.
