# 046: A node's history

Status: step 1 built 2026-11-20 · a node's History view (its fourth view, ⌘⌥4), "Now" (who set
each value), changes newest first with runs folded, read as **point reads of the node at each
commit** of a window of 200, a search past the window for when it was created and its values
set, and the page's "Last changed by" line. Compare, Restore and the "not merged" section are the
next steps. One entity's commits (E62) and a commit's operation (E60) wait on the engine.

Stacked on #161 (spec 014, "The page as a document"), which is not merged yet.

## Problem

People ask *what happened to this node, who changed it, and when*. The cookbook study (#143)
found the question in seven of its eight graphs, and the design study (#160,
`docs/ux/2026-11-20-node-history.md` on `docs/node-history-study`) sorts it into five jobs: who
set this value (H1), what happened to this node (H2), was it like this last week (H3), is anyone
changing it now (H4), where did it go (H5). Studio answers none of them: History lists a
branch's commits, and finding one node's changes means opening commits one by one. The closed
#146 (spec 040) read a node's history from the change feed filtered by type; the study measured
that this costs what the node's *type* wrote: a bulk load in the window made one person's six
changes cost 13.2 MiB.

Step 1 answers H1, H2 and H5.

## Principles

- **I.** Every read goes through the session and `graph_client` (`GraphSession::node_history_in`,
  `node_history_search`). Studio keeps no history of its own: the session holds only the node's
  last change as its history read last found it, for the page, and forgets it with a new token.
- **II.** Every window answers for the head the first listing resolved; each read is pinned to
  its commit's snapshot. A commit on the branch offers Refresh.
- **V.** Nothing is drawn until a window is whole; a newer commit shows "Newer commits ·
  Refresh", never a jump. The page's line appears only once the history is read.
- **VI.** "before <time>" says where the read stopped; the end row says what was read, what was
  skipped between a window's ends, and where it stops.
- **VIII.** Every action has a key and a palette entry: ⌘⌥4 (`OpenViewAt` place 4) and
  `studio_graph::OpenAsHistory`, `ReadOlderCommits` ⌘⌥N, `FindWhenSet` ⌘⌥F, ↑/↓ and J/K, ↵,
  Space, ⌘R.
- **IX.** Windows of 200 commits; about window × (1 + relationship groups) requests, 4 at once;
  1,000 edges a group a commit; 20 merges named a window; a search lists at most 20,000 commits.
- **X.** Designed against contract 0.14 (`omnigraph-0.15.0.json`). Waits on E60 and E62
  (recorded in spec 012 and `docs/deferred-engine-changes.md`).
- **XIII.** One tab, a view of one node, beside its page, tree and canvas. The page and the
  inspector read no history.

## What exists today (checked)

- **Routes** (`crates/graph_client/openapi/omnigraph-0.15.0.json`): `GET /commits` pages a
  branch's first-parent commits (`limit` ≤ 1000, `page_token`, `head_commit_id`) with
  `actor_id`, `created_at`, `graph_branch` and `merged_parent_commit_id`; `POST /query` takes a
  `snapshot`; `HEAD /blob` with `snapshot` answers a cell's size, its address or 404; a read at a
  snapshot before a type or property existed is refused with a message `GraphClient` reads as a
  `HistoricalGap`. No route takes an entity id (E62); no commit names its operation (E60).
- **A point read is small.** The node at one commit is one row of its non-Blob properties
  (`return { $n }`), about 0.4 KB; its edges through one relationship group are their ids
  (`$e.@id`, `$m.@id`), about 0.2 KB for a few. Edges are never updated (T16), so their ids say
  everything a commit did to them.
- **Studio before this spec:** History listed a branch's commits; `change_cards` drew a change's
  rows only from a folded review; the node page's view strip had Page, Tree and Canvas; ⌘⌥4 was
  bound on every platform to `OpenViewAt { index: 3 }`, which reached a type table's Gallery
  (spec 035) and nothing on a node.
- **#146** (`feat/ux-p5-node-history`): its tab, its cached rows and `own_words` are reused here;
  its feed-based read is not.

## Design

### Where it lives

| Place | What it shows | Reads |
|---|---|---|
| **History tab**, a node's fourth view (`node-history:<type>:<id>`, "History · *name*", restored as `node_history`) | Everything below | The read plan |
| Node page (#161's meta line) | "· Last changed by *actor* · *time*", muted, once the node's history was read at the commit the page shows; a click opens History | None |
| Inspector | A **History** button beside Page, Tree and Canvas | None |
| Tree and canvas of a node | History as their fourth view, of the root | None |

⌘⌥4 is `OpenViewAt { index: 3 }`. On a node's page, tree and canvas the fourth place was empty
and is now History. On a type table with files it stays Gallery (spec 035): this spec takes no
key from another view. A page held at a commit offers History too; it reads at the branch head.

### The tab

```
[Page] [Tree] [Canvas] [History]   11 changes on main in the newest 29 commits       ⟳
Now
 title    After the load, again                       act-dev · 1 min ago · C8VY49Z
 slug     person-000000                    no actor · created · 32 min ago · YSJP572K
 born     1978-03-20                               before 2026-11-18 14:02 UTC
          [Find when it was set ⌘⌥F]
Changes
 ~ act-dev  on main                       2 commits ⌄   2 changes · 00:36–00:36, 1 min ago  C8VY49Z
 ~ act-dev  via merge of feat-title                                   30 min ago  EJS97HB4
 · act-dev  on main                                                   31 min ago  B2K9X2KR
     + works at → company-00004
 + no actor  created · on main                                        32 min ago  YSJP572K
Read back to YSJP572K, 32 min ago, the graph's first commit. 29 commits read.
```

- **Now (blame).** The node at the head (the newest read), each property in the schema's order:
  its value, and the newest entry that set it (a change of that property between two reads, or
  the creation): actor, "created" or "via merge of *branch*", relative time, short commit; the
  full commit and UTC time in the tooltip. A property no entry read set reads "before
  *2026-11-18 14:02 UTC*", the oldest commit read. Blobs by size. A node gone at the head reads
  "Removed by *actor* in *X*, *time*. Its relationships went with it."
- **Find when it was set** (⌘⌥F), offered while something is "before" and older commits remain:
  it lists the commits before the windows (`GET /commits`, pages of 1,000, at most 20,000) and
  binary-searches them with point reads of the node, shared between its questions: the commit
  that created the node, and for each property no commit read changed, the newest commit before
  which it held another value. The row then reads "act-old · 3 d ago · K1GB3NS7"; its tooltip says
  it was found by a search, and that a value that went back to an earlier one may have been set
  later. Used for nothing else.
- **Changes**, newest first. Each entry: the kind's mark (`change_cards::kind_mark`; `·` for a
  relationship-only entry); the actor; "Studio" when the session made the commit (`own_words`),
  else "agent" when it was made on `agent/<stamp>` (E20); "created" or "removed"; "on *branch*",
  or for a merge "via merge of *branch*" (the merged head's `graph_branch`, one
  `GET /commits/{id}` each, at most 20 a window; "via merge of commit *X*" past them); Studio's own
  words for its own write; the relative time (UTC in the tooltip); the short commit, which opens
  the commit's tab (↵ too). Under it the property rows as a review draws them
  (`change_cards::rows_of`, a Blob by size since #138), and a line for each edge added or removed,
  naming the other end whichever end the node is.
- **Folding.** Consecutive entries fold into one when they share the actor, the branch and the
  kind of author (Studio's own or not), each is less than 10 minutes after the one before, and
  none is a merge, a creation or a removal (the study's rule). A run of more than one starts
  folded: "*N* commits ⌄" and "*N* changes · 14:02–14:09, *time*", with its net change. Space or
  the button unfolds it into its entries, the first carrying "Fold ⌃".
- **The end row** says how far it read ("Read back to *X*, *time*. 200 commits read.", or "…, the
  graph's first commit."), with **Read the 200 commits before ⌘⌥N**; its tooltip gives the
  requests sent. It says which relationship groups were not read between a window's ends, a
  group past 1,000 edges, a schema change, and a read that stopped short.

### Reading

Per window (`GraphSession::node_history_in`, `graph_store::node_history`):

1. `node_history_commits`: `GET /commits?branch=&limit=200` (`page_token` for an older window).
2. `node_history_node`: the node at each commit of the window and at the oldest one's parent,
   a point query at that commit's snapshot (`builder::history_node`). An older window reuses the
   read of its newest commit, which the window above read as its base.
3. `node_history_edges`: the node's edges through each relationship group (each edge type at
   each end the node's type can be, `builder::history_edges`, at most 1,000 a group), at both
   ends of the window; between them, every group while the node was created or removed in the
   window, else only the groups with an edge at either end, and only at commits where the node
   exists.
4. `node_history_blob`: `HEAD /blob` at the snapshot for each Blob property, at each commit where
   the node exists: kind and size, never bytes.
5. `node_history_merge`: `GET /commits/{merged}` for each merge entry, at most 20.

Reads run at most 4 at once, each named in the read ledger under `ReadView::NodeHistory`; none is
sent again by itself, and no token, query text or row is logged. Two consecutive reads that differ
are the newer commit's change, by its actor: exact for properties, a value changed and changed
back included. A request costs what the node holds, not what its type wrote, so the cost is about
window × (1 + relationship groups), whatever loads the window holds.

A read refused for a type or property the commit did not have (a schema change, E52) ends the
history at the commit above it: what is known stays, no older window is offered, and the end row
says "History stops at a schema change to *Type* in *X*". Another failure ends the window there
and says so; a failure of the head's read fails the tab.

The newest window keeps the node's last change in the session (`GraphSession::last_change`,
`SessionEvent::NodeHistoryRead`) for the page.

### States

| State | What it says | Next |
|---|---|---|
| Reading | "Reading the history…" (`ds::loading_state`) | — |
| Failed | "The node's history could not be read" and the reason | Try again (⌘R) |
| Nothing in the window | "No changes in the last 200 commits" · "No commit in the newest 200 on main changed *name* or its relationships." | Read the 200 commits before ⌘⌥N |
| Nothing since the graph began | "No changes" · "No commit on main changed *name* or its relationships since the graph began." | — |
| Schema boundary, nothing after it | The schema-change state with the commit | — |
| A value set before the window | "before *time*" | Find when it was set ⌘⌥F |
| Older read failed | The end row's reason, and Try again in place of the button | ⌘⌥N |

### Keys

| Key (macOS; Ctrl on Linux and Windows) | Action | Context |
|---|---|---|
| ⌘⌥4 / ⌘⌥⇧4 | `studio::OpenViewAt` place 4 (bound since 0.4) | Workspace |
| ↓ J / ↑ K | `menu::SelectNext` / `SelectPrevious`, between entries (Now and the end row are passed over) | `NodeHistory` |
| ↵ | `menu::Confirm`: open the selected entry's commit (a run's newest) | `NodeHistory` |
| Space | `studio_changes::ToggleChange`: fold or unfold the selected run | `NodeHistory` |
| ⌘⌥N | `studio_history::ReadOlderCommits` (new) | `NodeHistory` |
| ⌘⌥F | `studio_history::FindWhenSet` (new; in `TypeTable` ⌘⌥F is Filter by selection) | `NodeHistory` |
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
| Read the node itself at each commit of the window | The change feed filtered by type and op (#146, and this spec's first build); `GET /commits/{id}/changes` a commit at a time | The feed and the walk cost what the node's *type* wrote: 13.2 MiB for one person across a load of 50,000 (study), and a byte cap only hides it. A point read costs what the node holds: a window across 10 loads of 5,000 people read 64.9 KiB (validation log). It is exact for a value changed and changed back, which a net diff misses. The feed stays the alternative where a type changes rarely and a window is long; E62 is the real fix |
| Edge ids per relationship group, diffed between reads | Reading edge properties; the neighbours' ids | Edges are never updated (T16), so an id that comes or goes is the whole change; two edges to one neighbour stay two lines. An added edge's properties are on its page, one click away |
| Skip a group between a window's ends when it has no edge at either end of a window the node lived through | Read every group at every commit; never skip | Most nodes have few relationship types in use; the skip halves the reads for a person of the dev graph. It misses an edge added and removed inside the window, which the end row says |
| Binary search only for creation and for blame past the windows, on demand | Binary search for the list of changes; searching by itself | A search finds a commit where an answer flips, so it misses a change undone later; it is labelled where used. On demand, since the windows already answer most questions |
| Blobs by kind and size (`HEAD` at the snapshot) | Their bytes; their `ETag` | Bytes are the whole file; the `ETag` names the type's table version, which moves with any row (E57). Two files of one size read alike, and the spec says so |
| A schema change ends the history | Read past it with a query of the older schema | The node's query is built from the schema Studio holds; across E52 Studio has no other |
| A merge is credited "via merge of *branch*" | Walking the merged branch for who made it (study) | The point reads see the change land on the branch read; who made it on the other branch is the next step, and E62's job |
| ⌘⌥4 keeps meaning "the fourth view" | A History key of its own | It is bound already; on a node the fourth place was empty, and a table's Gallery keeps it |
| The page's line in its meta line, only for the head the history read | Under the properties (study sketch); always | #161 made the meta line the page's quiet facts; a line from another commit would describe a node the page does not show |
| History from a page held at a commit reads at the head | Disabled (#146); marking the held commit (study) | It answers H1 from anywhere; the marker is a next step |
| Agent named only from an `agent/` branch, Studio only from this session's outcome log | A person otherwise (study) | "Person" would be a guess; an unnamed actor is just its id |

## Next steps

1. **Compare** two entries, or an entry and now, as point reads of the node (study, H3).
2. **Restore** a value into the page's draft for the window's branch, saved as any edit (study,
   #158's rules).
3. **Not merged**: branches that change the node, one line each, with "Will conflict with main"
   (study, H4).
4. "Who on *branch*?" for a merge: the merged branch's commits read the same way, bounded, until
   E62.
5. An added edge's properties on its line.
6. A filter by property, actor and relationships only.
7. Opening History from a commit's tab or a review card with the commit marked.
8. New commits read as they land, with no Refresh.

## Deferred

In spec 012 and `docs/deferred-engine-changes.md`:

- **E62** ([#1472](https://github.com/ModernRelay/omnigraph/issues/1472)): one entity's commits,
  paged, merged-in commits under their own actors; *extended here*: each change names the
  properties it set, a `property=` filter, and a blame answer. With it History is one request a
  page, and blame is exact at any depth. This is the real fix.
- **E60** ([#1470](https://github.com/ModernRelay/omnigraph/issues/1470)): a commit's operation
  and message, and who created a branch.
- Used as they stand: E20 (which agent), E52 (across a schema change), E57 (a Blob validator).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-20 | The routes, parameters and refusals the design rests on | `omnigraph-0.15.0.json`: `/commits` (`limit` ≤ 1000, `page_token`, `graph_branch`, `merged_parent_commit_id`), `/query` (`snapshot`), `HEAD /blob` (`snapshot`); `GraphClient::historical_gap`'s messages | As stated |
| 2026-11-20 | The node and edge reads compile against the dev schema | `gq_language` `a_nodes_history_reads_compile`; by hand against the dev server: `return { $n }` 0.37 KB, two edges 0.3 KB | Pass |
| 2026-11-20 | Reads, diffs, blame, folding and the search as pure functions | `graph_store` `node_history::tests` (6): each group at each end; consecutive reads making entries, a value changed and changed back as two, edges at either end, creation and removal; blame with the newest setter, a search's find and "before"; folding and net change; the binary search in ≤ 9 probes over 464 commits; Blobs by kind and size | Pass |
| 2026-11-20 | The view | `history` `node_view::tests` (9), FakeHttpClient answering point reads at each snapshot over 250 commits | Pass. A window reads 1 listing, 201 node reads (c249 to c50 and c49), 6 + 2 × 199 edge reads (the outgoing Knows had no edge at either end) and 1 merge, all `View(NodeHistory)` in the ledger, and no change feed; c240 + c239 fold to title A → C; Grace's Knows reads as incoming and Ada's WorksAt as outgoing; Zed removed at the head shows his removal with its edge, his edge added, his creation; ⌘⌥F finds Ada's creation at c5 in a listing and ≤ 6 point reads, then ⌘⌥N reads it as an entry; a schema change before c201 stops the history at c201 with 48 commits and no older window; a 500 is asked once in 60 s, then Try again and ⌘R each read once; ↓ J K ↑ Space ↵ move, fold and open; each key draws the rows once, another branch's commit none |
| 2026-11-20 | The page says who changed the node only for the commit it shows | `type_table` `peek::page::tests::the_meta_line_says_who_changed_the_node_once_its_history_is_read` | Pass |
| 2026-11-20 | Measured reads of opening History on a node of the dev graph | `script/dev-server` on `127.0.0.1:8851` with its own data directory (`omnigraph-server` 0.15.0); about 15 commits made with the 0.15.0 CLI through a direct target on the stopped server's data (the CLI keeps no token for an `http://` server, E46; the dev policy allows one actor, `act-dev`): title, email, tags of `person-000000`, a `Knows` to her and a `WorksAt` from her, a branch `feat-title` merged after main moved, a person created and removed, `person-000099` removed; 17 commits on main. The debug `snapshot` example's `reads:` step for Studio's requests and times; `measure_points.py` (the same requests one at a time, with `urllib`) for bytes; Apple M5 Pro, load average 6 to 7 | Opening History on person-000000: **92 requests** (1 listing, 17 node reads, 68 edge reads over its 4 relationship groups, 1 merge, plus the inspector's own 5), settled **929 ms** after the action, 4 at once; **37.2 KiB** for the 86 reads, 0.55 s one at a time. The change feed of Person and its relationships from the graph's start was 288 KB |
| 2026-11-20 | The cost no longer follows the node's type | The same server after 10 loads of 5,000 `Person` (50,199 people) and two more title edits of person-000000 through `POST /mutate`; 29 commits on main | **152 requests**, settled **1,408 ms**; **64.9 KiB** for the 146 reads: 2.2 KiB a commit before and after the loads. The loads draw nothing (the node did not change); the two edits fold into one entry. The feed's first page of the same types was 2.1 MB (8,192 changes of 50,000) |
| 2026-11-20 | It looks right, light and dark | Offscreen snapshots at 1440 × 900, 2x, against the same server after the loads: History of person-000000 light and dark, a run of commits unfolded by ↓ ↓ ↓ Space, the page's "Last changed by" line after History was read, person-000099 removed at the head, and person-999999 (no changes) | As in the PR |
| — | Not checked | A remote server's latency; the 230,000-person seed; a real schema boundary; a Blob-holding type against a server; a 0.14 server; Studio's own writes labelled "Studio" in the running app; two actors | Open |

## Corrections

- 2026-11-20, before review: the first build read the change feed filtered by type, updates and
  deletes first and inserts under 8 MiB a window. Ragnor asked for point reads of the node at
  each commit instead, so a read costs what the node holds, not what its type wrote; the feed
  read was taken out, not kept beside it.
