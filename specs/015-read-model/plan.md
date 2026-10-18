# 015: One read model: versions, cache, scheduler

Status: in progress; head reads go as branch reads (02ba803), a relationship group is one read
when it fits (2dfe205), Studio needs 0.14 (911a885), node pages, peeks and tables move past
commits that changed nothing they show (carry-forward, below) · 2026-10-18

## Problem

Opening a Person page on a large graph takes tens of seconds. On the personal graph a Person has
92 relationship groups, and the page sends one node read, then one count per group, then one
neighbour read per group with neighbours: about 185 requests in three waves, all at once, each
pinned to a commit. The server saturates, the groups at the top wait behind the rest, and none
of it is reused: the table the page was opened from read the same node, and every head move
throws everything away.

The cause is structural. Each view (table, peek and page, tree and canvas, history) builds its
own queries, runs its own fetching state machine and keeps its own partial cache. Nothing
schedules reads across views, nothing knows which reads a new commit leaves valid, and nothing
uses what OmniGraph guarantees: a commit never changes.

This plan replaces those per-view mechanisms with one read model owned by the session.

## Principles

- I: the server stays the authority. Cached answers are answers *at a commit*, labelled with it;
  a branch's current state always comes from the server (heads come from the change feed).
- II: one commit per view, unchanged as a rule. The mechanism changes (see Decisions): a view's
  later reads may be branch reads verified to return the view's commit, which is cheaper than a
  pinned read on 0.13. This needs the constitution's wording of II amended.
- III: writes are untouched: no optimistic layer, no patching of results, no retries.
- V: a view never changes under the reader. Carrying a view to a new commit whose changes touch
  nothing it shows is not a change: its content is identical, so the "Newer data" chip appears
  only when the view's data actually differs.
- IX: every read is bounded: a page size or a cap, a concurrency cap, a byte budget for the cache.
- X: Studio's minimum server is OmniGraph 0.14 (Ragnor, 2026-10-17): today's `main`, released as
  0.14 edge builds (`edge-20261017-d883c49`, `0.14.0-edge+d883c49`). It adds the graph diff,
  branch review, merge-base and merge dry run (#966) and paged commit lists (#1013); the engine
  changes filed below are not in it. The HTTP contract header is still `0.13`.
- XII: every claim below was checked against OmniGraph `main` (`d883c49e`) or the 0.13 server on
  the personal graph; see the validation log.

## What exists today (checked)

### The workload (personal graph, OmniGraph 0.13.0 server, 2026-10-17)

Measured with the CLI (10 ms start-up, so the times are server and network), from a Mac to
`omnigraph.ragnor.co`, reads only, Studio's own query text.

| Read | Time |
|---|---|
| `/healthz`, reused connection | 0.16–0.21 s |
| Read returning no rows (`limit 0`) | 0.45 s |
| Node by id, at the branch | 0.56–0.66 s |
| Same, pinned to the head commit | 1.03–1.18 s |
| Same, pinned 200 commits back | 1.43–1.45 s |
| One group's count (`count($m)`) | 1.0–3.4 s |
| One group's first 50 neighbours | 0.9–3.8 s |
| One page's counts for 100 ids, one group | 1.0–2.8 s |
| 8 counts at once / 32 at once | 1.6 s / 7.4 s wall |
| Counts for every Person–Artifact edge type in one wildcard query | 58 s |
| `/snapshot` (653 tables) | 0.5 s, 163 KB |
| `/schema` | 0.5 s, 199 KB |
| `/commits` (4,875 commits, unpaged) | 1.3 s, 1.9 MB |
| Change feed poll, caught up | 0.45 s |
| Change feed poll over the last 5 / 20 commits (306 / 327 changes, 16 types) | 4.7 s, 469 KB / 8.5 s, 540 KB |

Shape of the graph: 101 node types, 552 edge types (149 empty), a Person has 92 relationship
groups (16 on empty edge tables). History: about 20 commits an hour, in bursts (median gap 6 s),
from one writer. So requests, not bytes, are the scarce resource; reads at old commits cost
more; heads move every few minutes.

### OmniGraph (`main` at `d883c49e`; server 0.13.0)

1. Every read answers from one pinned version and reports its `graph_commit_id`
   (`crates/omnigraph-api-types/src/lib.rs`, `ReadOutput.graph_commit_id`).
2. A read at a commit is fixed only together with the schema: "A historical query is written
   against the current schema", and a property added since a commit is refused there, so a
   whole-node `return { $p }` is refused at commits older than a property
   (`docs/user/branching/index.md`, Historical reads). Results also depend on `now()`, on the
   embedding provider behind `nearest($v, "text")`, and on settings.
3. A commit-pinned read resolves the commit first: held commits, the history cache, then a read of
   `__history`, then live branches (`crates/omnigraph/src/db/graph_coordinator.rs`,
   `resolve_commit`). It also skips the server's read caches on purpose: branch reads share a
   session and a held-handle cache ("0 IO on a warm repeat"), snapshot-id reads do not, to stay
   clear of `cleanup` (`crates/omnigraph/src/db/omnigraph.rs`,
   `resolve_target_after_schema_validation`). Together that is the 0.5–0.9 s more than a branch
   read.
4. The change feed (`GET /graphs/{id}/changes`) returns complete commits in first-parent order,
   with each entity change's kind, type, id, operation and images (edge images carry endpoints);
   a durable `cursor` on the terminal page; `caught_up`; an empty block for a commit with no
   entity changes; `409 schema_boundary` across a schema change; `410 change_feed_gap` after
   cleanup. A cursor is bound to the branch's lifetime, so a branch deleted and made again under
   its name is refused, not confused (`docs/user/branching/changes.md`).
5. `/snapshot` reports `published_dataset_version`, which is a table's logical version within one
   branch's lineage (`base + 1` per write), not its content identity: readers open the staged
   version (`docs/dev/writes.md`, `crates/omnigraph/src/engine/mod.rs` `dataset_pin`). Two
   branches forked from one base can both report version 6 with different data.
6. Read policy is per action and branch; nothing filters rows (`docs/user/operations/policy.md`).
7. Indexes, compaction and fragment layout never change results (`docs/dev/invariants.md` #7).
8. A projected `count($m)` expands through the edge index and then scans the far node table
   (explain: `ScanExec $m: Artifact … id_restriction=input`, a 205 MB hash-join build estimated
   for one count); only `count { }` filters are answered from the graph index's degree
   (`crates/omnigraph/src/engine/lower.rs`, `bulk_row_count`).
9. No push channel and no batch route (`openapi.json`). The server admits at most 64 reads with
   bodies at once (`OMNIGRAPH_READ_INGRESS_INFLIGHT_MAX`, `crates/omnigraph-server/src/workload.rs`,
   `docs/user/deployment.md`) and answers `429` with `Retry-After` past that.
10. Wildcard and alternative traversals exist (`$a $e:* $b`, `$e.@type`), but every member must
    join the same two node types, and on 0.13 the one query above took 58 s.

### Studio

- Each view runs its own fetching: `NodePeek::load_groups` and `toggle_group`
  (`crates/type_table/src/peek.rs`), `Exploration::start` (`crates/graph_store/src/exploration.rs`),
  `TypeTable::load_relation_groups` (`crates/type_table/src/table.rs`).
- Shared pieces: `GraphSession::read` memoizes reads at a commit, bounded by entry count
  (`pinned_reads`, `crates/graph_store/src/session.rs`); `degree` batches counts by type, group
  and commit; commit lists and Blob metadata have their own memos; `counted_read` counts reads.
  The client already holds at most 8 reads on the wire per connection (`READ_CONCURRENCY`, a
  FIFO semaphore) and waits out a `429`'s `Retry-After` twice (`crates/graph_client/src/client.rs`).
  So a 185-read page runs 8 at a time, about 185 / 8 × 1.5 s ≈ 35 s: what is missing is fewer
  and cheaper requests, not a cap.
- Heads: every 30 s a branch list read and one `limit 0` probe per watched branch; a head move
  re-reads `/snapshot` and forgets the commit list (`check_heads`, `saw_head`).
- A page opened from a table reads the node and every group again (`NodePeek::page` passes no
  known relations).
- A table shows every relationship column by default (`ColumnLayout` hides nothing) and reads
  each one per page of rows, including columns scrolled off screen.

### Zed (`v1.23.2`), patterns this design reuses

- Results tagged with the version they were asked at and dropped on apply if the version moved,
  with a generation counter against stale writers: `RunningFetch::take_finished`,
  `SemanticTokensData.generation` (`crates/project/src/lsp_store.rs`,
  `crates/project/src/lsp_store/semantic_tokens.rs`).
- In-flight requests shared as `Shared<Task<…>>`; dropping a task cancels it
  (`crates/scheduler/src/executor.rs`).
- Concurrency caps by semaphore: `MAX_CONCURRENT_OBJECT_READS = 16`
  (`crates/project/src/git_store.rs`); Zed's HTTP client has no request limit of its own.
- Byte-budgeted LRU: `CostBudgetedLru` (`crates/language_core/src/highlight_cache.rs`).
- Effects flushed once per outermost update, so observers never see half a change
  (`crates/gpui/src/app.rs`, `flush_effects`).

## Design

Three parts in `graph_store`, owned by `GraphSession`, and one change in how views read.

### 1. Heads: one change cursor per followed branch

- For each branch a window follows, keep one feed cursor and poll it: every 30 s while a window
  of the graph has focus (as the head probe today), every 120 s otherwise, and at once on focus,
  on Refresh and after Studio's own write; one poll in flight per branch, exponential backoff on
  errors up to 5 min, `Retry-After` honoured. This replaces the `limit 0` head probe; the branch
  list is read on its own, every 120 s.
- The poll asks only for the types open views read (the union of their read sets, repeated
  `type` filters), because every change carries its images: the last 20 commits on the personal
  graph were 540 KB and 8.5 s unfiltered. A commit touching only other types still arrives, as
  an empty block, so the head still advances. When the set of watched types grows, the cursor is
  started again after the last commit seen (`start=after:<id>`), since a cursor is bound to its
  filters. Types nobody watched since a commit do not carry forward across it.
- Each poll appends complete commits to a per-branch change log: for each commit its id, its
  parent, and per type the ids it touched (and for edges their endpoints), up to 1,000 ids per
  type; past that the type is marked as touched as a whole. A commit whose feed pages exceed
  8,192 changes is marked as touching everything.
- The log keeps the last 500 commits per branch. A poll that advances the head also reads
  `/schema` (0.5 s, 199 KB on the personal graph); different text, or a `409 schema_boundary`,
  starts a new schema epoch: nothing carries across it. `410 change_feed_gap` clears the log;
  views read again.
- The head of a branch is the last commit the log holds. Every result a read returns also
  reports its commit; a newer commit than the log knows triggers a poll.

### 2. The cache: answers at a commit

- An entry is keyed by a typed resource key, the commit and the schema epoch, and holds the
  decoded answer, its size in bytes and its read set: the node and edge types it reads, and for
  per-node resources the ids it depends on.
- Resource keys: `Node{type, id}`, `Relations{type, id}` (a node's relationship groups),
  `Neighbours{type, id, group, after, limit, narrow}`, `Degrees{type, group, ids}`,
  `TypePage{type, filters, sort, start, limit}`, `Count{type, filters}`,
  `ValueCounts{…}`, `CommitList{branch}`, `BlobHead{cell}`, `Query{text, params}` for the query
  editor. Each key builds its query through `gq_language::builder`, as today.
- An entry is never invalidated, because a commit never changes; it is only evicted.
  Eviction is least-recently-used within a byte budget (64 MB), with retain counts: what a
  view shows is retained, and the last 16 released views' entries are kept before eviction
  (Relay's release buffer).
- **Carry-forward.** A key asked at commit C2 that misses is answered by the same key at an
  earlier commit C1 on the same branch and schema epoch when no commit in (C1, C2] touches its
  read set; the answer is then recorded at C2 too. Per-node resources check ids (a node's
  neighbours change only when an edge of that group touches it, or a neighbour's label changes);
  pages and counts check types.
- Volatile answers never carry forward: queries using `now()`, `nearest` with a text argument,
  or a `limit` without a total order. The query editor's queries are parsed with
  `omnigraph-compiler` to find their read set; a query it cannot classify is volatile.
- **Seeding.** Rows of a `TypePage` at C are `Node` entries at C (both return `$n`), so opening a
  page or a peek from a table reads nothing for the node itself.

### 3. The scheduler: every read through one queue

- The client's cap stays: 8 reads on the wire per connection, `429` waited out. Priorities
  (visible before near before background) and an adaptive cap wait for a measurement showing
  queueing order matters; a node page enqueues its groups in the order it shows them.
- Single flight: identical keys at one commit share one request, and identical branch reads under
  way share one too.
- Coalescing for 20 ms: same-shaped keys merge into one query with list parameters, as
  `degree` batches today (counts of many nodes for one group, nodes by `@id in $ids`).
- Dropping the last subscriber removes a queued read. An in-flight read at a commit finishes,
  since its answer stays reusable, unless it is background.
- **Target choice.** A read for the branch's known head is sent as a branch read, which is
  cheaper, and its returned commit is checked. Same commit: stored at it. Newer: stored at the
  newer commit, which also tells Heads to poll. A read for an older commit is sent pinned.
- Reads retry transport errors, `503` and `429` with backoff (three times); writes never retry.

### 4. Views subscribe; they do not fetch

- A view holds a pin: the commit it shows. It asks the session for resources at that commit and
  renders their states: loading, ready, failed, or reclaimed (the commit was cleaned up).
- When its branch's head moves, the view asks for its resources at the new commit, in the
  background. Resources that carry forward cost nothing; the others are read again. If every
  answer is equal to the one shown (an early cutoff, as Salsa's backdating), the view re-pins
  silently: nothing it shows changed. Otherwise it shows "Newer data · Refresh" as today, and
  Refresh swaps the staged answers in one frame while the old content stayed on screen.
- After the view's own write, it re-pins to the commit the write returned, as today; the
  change set of that one commit (`/commits/{id}/changes`) decides what carries forward from the
  parent.
- A commit cleaned up under a view (`410`, or a read refused as reclaimed) keeps what the view
  already shows and offers "This commit was cleaned up · Show the head".
- A view's later reads that come back at a newer commit than its pin (a branch read raced a
  commit) are carried back if the commits between touch nothing they read, and otherwise
  re-read pinned.
- **Relations** (node page, peek, tree, canvas): the groups from the schema, minus groups on
  edge types empty at the head (from `/snapshot`'s `entity_count`, used only when the view is at
  the head); for each group one neighbour read of `limit + 1`, which gives the exact count when
  the group fits; a count read only for groups that do not. Groups on screen are visible
  priority, the rest background.
- **Tables:** relationship columns read only while on screen; a type with more than 8
  relationship groups shows the first 8 whose edge types are not empty, the rest in the columns
  menu.
- Historical reads of whole nodes: a refusal naming a property added since the commit is
  answered by reading again without that property, once per property, and the property shows as
  "added later".

### Carry-forward as built (2026-10-18)

A first cut of parts 1 and 4, ahead of the shared cache:

- The head probe stays. When it, or one of Studio's writes, moves a branch's head, the session
  reads the change feed after the last commit it logged (`start=after:<id>`), with one `type`
  filter per type name what open views show depends on. With no view of the branch nothing is
  read.
- The log (`graph_store::carry`) keeps, per commit, its first parent, the names asked about, and
  per type the ids written and, for edges, the nodes at either end; past 1,000 ids a type counts
  as changed throughout. 500 commits per branch. A read takes at most 8 pages of 1,000 changes;
  a commit it stops inside, a `409`, a `410` or a failed read leaves the rest unknown, and the
  next read starts at the head.
- After a read that returned commits, `/schema` is read once; a different text means none of it
  counts. The feed filters by type name, so a rename could hide a view's changes, and a name
  the server does not know is accepted without error (validation log).
- `changes_since(branch, from, to)` walks first parents from `to` back to `from`: pending while
  the read is out (views decide on `ChangesRead`), unknown when a commit is missing.
- Each view holds a read scope (`graph_store::scope::ReadScope`) and reads what it shows through
  it (`read_in`, `degree_in`, `blob_meta_in`; `note_in` for what a caller read for it, as the
  table's row and relationships given to its peek). The scope keeps each answer's `Depends`
  from the moment it is asked, and the nodes a list named once it is in. It is the one read
  set: the feed's type filter is the union of live scopes, and `scope_since(scope, from, to)`
  says whether a move changed anything the view shows (a list still being read counts its whole
  listed type). A page, a peek, a table and the tree and canvas move to the head when it says
  unchanged, and otherwise show "Newer data · Refresh" as before. Refresh, or a pin that moves
  for another reason, resets the scope; a view held at a commit has none.
- Tables read a relationship column's neighbours only once rows draw it (the columns in the
  horizontal view and one either side), and each row's once: a Person table on the personal
  graph has 92 relationship columns, each two reads per page. The default cap of 8 columns
  (part 4) is not built; with reads limited to the screen it is a layout choice, not a cost.
- Answers carry across commits too (part 2, first cut). A generated query says what its
  answer depends on (`builder::Depends`): a node, the edges of one type at some nodes plus the
  nodes the answer lists, or whole types for pages, counts, groupings and narrowed lists;
  anything else (a search, a query typed by hand) is `Unknown` and never carried. A read at a commit that misses the memo is answered by the same query and
  parameters at an earlier commit of the branch when the log shows nothing it depends on
  changed in between, and kept at the new commit. So Refresh on a page reads again only the
  groups the commit touched, and a moved view's later reads mostly cost nothing. The memo
  keeps its answers within 64 MB (each weighed by its rows), least recently used first out, up
  to 4,096; the release buffer is not built.
- Seeding: a generated query may say what its rows also answer at the same commit and schema
  (`builder::Seeds`). A whole-node page's rows answer each row's `node_by_id`, so a node page
  or peek opened from a table at the session's known head reads nothing for the node; a page's
  relationship counts answer each node's count. Counts are kept per node, commit and schema
  epoch (16,384 of them), so a peek, the tree and the canvas share the table's, and a count
  carries to a later commit that changed no edge of its group at the node.
- Review reads its fork and head from `branches/merge-base` and folds the feed after the base,
  instead of reading both branches' whole commit lists (each about 1.6 s on the personal graph,
  and 3m47s once right after a restart); its commits come from the feed blocks' causes. A
  base off the branch's first-parent history is refused by the feed (`400`, checked on 0.14),
  and review then reads both lists as before. A merge checks the head and confirms its landing
  through the merge base too. Review still offers Refresh on every move.

- Every answer kept (pinned reads, Blob cells) is keyed by a schema epoch, a number that
  moves whenever the schema's text changes, the first load included. A read at an old commit
  uses the current schema, so a rename, a drop or an addition can change the same query's
  answer at the same commit; nothing kept or carried crosses an epoch, and a new epoch starts
  every change log again at its branch's head. A head move whose feed read meets
  `409 schema_boundary`, or whose `/schema` read after it differs, reads the schema again: the
  session reads under it from then on, and views showing what they read under the old one offer
  Refresh (`SchemaMoved`), which takes it. Nothing on screen changes by itself, an open cell
  edit included; a reload the person starts still changes views at once (`SchemaChanged`). Left open: a schema changed and changed back between two head checks is not seen
  (the server gives no schema identity with a read; omnigraph#1054).
- A node page or peek at a commit made before one of its type's properties was added (or
  renamed) was refused whole (`return { $n }` reads every current property). It now reads the
  node again as fields without the property the refusal names, once per property, and shows it
  as "Added after this commit" or by the name it had. On the personal graph 38 of 101 types are
  refused at the oldest commit; an `Account` there reads after two retries
  (`provider_subtype`, `tax_wrapper`). A table at a commit reads its rows the same way and says
  so in the column's header. A relationship group whose edge type, or other end's type, did not
  exist at the commit had no neighbours then and shows none; one whose neighbour label or edge
  property is newer is read without it. A property whose stored column has no identity (data
  written before OmniGraph 0.10) shows as "not readable at this commit". The server names one
  property per refusal and only in its message (`omnigraph-core` `error.rs`); an engine change
  could name them all, typed.

### Expected effect, to be measured

A Person page on the personal graph: from about 185 requests in three waves, all at once and all
pinned (+0.5 s each), to the node from the table's page (no request), 76 neighbour reads as branch
reads, and counts only for groups over 50, six at a time, visible first. With E16 the counts
become one request.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Cache answers per commit, never invalidate | Time-based staleness (TanStack `staleTime`, SWR `dedupingInterval`, Zero TTLs) | A commit never changes and the feed says exactly when a branch moves; a timer only guesses |
| One answer per key and commit | A normalized entity store shared by all views (Relay, Apollo, urql Graphcache) | Merging records read at different times shows states that never existed, against II; counts and neighbour lists do not normalize |
| Carry answers across commits by read set | Re-read everything on each head move | 20 commits an hour; most touch types a view does not read. Read-set tracking is what Replicache subscriptions, Convex and Salsa do |
| Ask the feed only about the types views read, and read `/schema` after it | The unfiltered feed; trusting type names | Unfiltered, 20 commits were 540 KB and 8.5 s on the personal graph; a filter by name misses a renamed type's changes and accepts a name it does not know |
| Read sets from the feed's change set | From `/snapshot` table versions | `published_dataset_version` is not content identity across branches (fact 5); the feed is exact and cursor-bound to one branch lifetime |
| Branch reads verified by commit for the head | Pin every read to its commit (constitution II as written) | 0.5–0.9 s cheaper per read on 0.13 (fact 3); the commit check keeps one commit per view |
| No optimistic updates | Optimistic layers with rollback (Linear, Replicache, Zero) | Constitution III: an outcome is known only from the server |
| Poll the feed | WebSocket push (Linear, Zero) | The server has none; a caught-up poll costs one request |
| Client-side concurrency cap and priorities | Unbounded concurrency (today; TanStack, Apollo) | 32 reads at once take 7.4 s and slow every other client; the server has no read admission |
| `limit + 1` neighbour reads | A count per group, then neighbours | Halves the requests; the count is exact for every group that fits a page |
| One query across edge types | Wildcard `$a $e:* $b` grouped by `$e.@type` | 58 s on 0.13 against 1–3 s per group; revisit with E16 |
| Treat cached answers as answers at a commit, kept in memory | Datomic-style caching of storage segments in the client | The server owns authentication and policy, and Studio does not run the engine (constitution I); answers are the unit Studio can verify |
| Swap a view only when all its new answers are in | Rendering each answer as it lands | Convex transitions and Electric's `up-to-date` exist for the same reason: a half-updated view mixes commits |
| No local replica | Bootstrapping a graph locally (Linear's early design) | Graphs of hundreds of thousands of rows; Linear itself stopped bootstrapping large workspaces |

## Deferred

Engine changes, to be added to `docs/deferred-engine-changes.md`:

- **E16** (filed): a node's counts per edge type in one request; proposed instead as projected
  `count { }` / `exists { }` blocks in `return` ([omnigraph#1050](https://github.com/ModernRelay/omnigraph/issues/1050)).
- **E28** ([omnigraph#1051](https://github.com/ModernRelay/omnigraph/issues/1051)): a projected
  `count` over one filter-free hop answered from the graph index's degree, as `count { }` filters
  are (fact 8).
- **E29** ([omnigraph#1054](https://github.com/ModernRelay/omnigraph/issues/1054)): read responses report the dataset versions they read (the plan's assumptions), and
  `/snapshot` reports each table's content identity (staged version), so read sets are exact
  without the feed and hold across branches; commits and feed blocks carry the accepted schema
  contract's identity, so a head move needs no `/schema` read to rule out a schema change.
- **E30** ([omnigraph#1052](https://github.com/ModernRelay/omnigraph/issues/1052)): commit-pinned
  reads cost no more than branch reads (fact 3).
- **E31** ([omnigraph#1056](https://github.com/ModernRelay/omnigraph/issues/1056)): a long-poll on the change feed, or an `ETag` on branch reads, so freshness costs less
  than a request every 30 s.
- **E32** ([omnigraph#1055](https://github.com/ModernRelay/omnigraph/issues/1055)): one request carrying several reads, answered at one snapshot, so a view's first paint
  is one round trip (Relay compiles a view's fragments into one query for the same reason).
- **E33** ([omnigraph#1053](https://github.com/ModernRelay/omnigraph/issues/1053)): a footprint answer from the graph diff on `main` (`GET /graphs/{id}/diff`, added in
  #966, not in 0.13). The diff already finds the changed tables from the two manifests alone
  (`changed_table_intervals`, comparing whole pins by stable table identity in `same_state`,
  `crates/omnigraph/src/changes/mod.rs`), then opens both endpoints of every changed table and
  enumerates rows. Two changes make it the head check this plan needs:
  - a mode that returns only the changed types (kind, type, identity) and stops before any table
    opens;
  - the kind and type filters applied before the opens in `plan_intervals`
    (`crates/omnigraph/src/changes/enumerate.rs`), which today opens and fingerprints every
    changed table first.
  Then one request, `diff?from=<view's commit>&to=<branch>`, names the new head
  (`to_commit_id`) and every type changed since the view's commit, across branches too, with no
  change log in Studio; the feed stays for entity-level precision. A feed mode without images
  remains useful for that.

Also filed: a wildcard traversal grouped by `$e.@type` running in indexed-scan mode, about ten
times slower than its members ([omnigraph#1057](https://github.com/ModernRelay/omnigraph/issues/1057));
whole-node reads at an older commit naming the properties they leave out instead of refusing
([omnigraph#1058](https://github.com/ModernRelay/omnigraph/issues/1058)); and new measurements of the
request floor on [omnigraph#752](https://github.com/ModernRelay/omnigraph/issues/752).

Studio, later: keeping answers at a commit on disk across restarts (they cannot change, but a
commit can be cleaned up; measure first).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-18 | The same reads on 0.14 | After the hosted graph moved to `0.14.0-edge+d883c49`: node by id at the branch 0.65–0.87 s, pinned to the head 1.51–1.62 s, pinned 200 back 1.34–1.56 s; a Person's 92 groups read once each (first 51 neighbours), 8 at a time | 74.5 s wall, median 1.25 s, p90 3.3 s; `spokenBy` (about 18k edges into a 208k-row table) took 61.2 s, `bookmarked` 20.8 s, `artifactFromPerson` 16.5 s; 51 groups had neighbours, 18 more than 50. The hub groups are the far-table scan of omnigraph#1051 (evidence added there); the pinned surcharge is omnigraph#1052 (0.14 numbers added there) |
| 2026-10-18 | One read per group that fits | `type_table` `a_group_that_fits_its_page_is_read_once`: 2 neighbour reads, 1 count read only for the group past the page | Passes |
| 2026-10-18 | A head read goes to the branch and checks its commit | `graph_store` `a_read_at_the_head_goes_to_the_branch_and_checks_its_commit` | Passes |
| 2026-10-17 | The workload table above | CLI reads against the personal graph on 0.13.0 (`omnigraph query`, `snapshot`, `commit list`, `changes poll`), Studio's query text, 1–3 runs each, from a Mac | As in the table |
| 2026-10-17 | A count scans the far node table | `explain` of Studio's degree query for `Mentions` | `ScanExec $m: Artifact … id_restriction=input`; `hash_join_build_bytes(node:Artifact)` ≈ 205 MB estimated; `count($e.@id)` and `count($e.@src)` the same plan and time |
| 2026-10-17 | Snapshot versions are lineage-local | `crates/omnigraph-api-types/src/lib.rs` (`published_dataset_version` from the manifest row), `docs/dev/writes.md` (`base + 1`, readers open `staged_version`), `engine/mod.rs` `dataset_pin` | Confirmed |
| 2026-10-17 | Commit cadence | `commit list` on the personal graph | 4,875 commits; 20 in the last hour, 374 in a day; median gap 6 s in the last 200 |
| 2026-10-17 | Pinned reads skip the server's read caches | `crates/omnigraph/src/db/omnigraph.rs` at `d883c49e` | "Snapshot-id reads are deliberately NOT cached"; only `ReadTarget::Branch` gets the shared session and held handles |
| 2026-10-17 | The feed is costly when commits carry changes | `omnigraph changes poll --start after:<id>` on the personal graph | 5 commits: 4.7 s, 469 KB, 306 changes in 16 types (Workflow and Run types); 20 commits: 8.5 s, 540 KB |
| 2026-10-18 | A `type`-filtered feed still returns every commit | `GET /changes?start=after:<id>&type=…` on a 0.14 dev server, three commits on a scratch branch (a Company insert, a WorksAt insert, a Person update) | Commits touching other types come as empty blocks; `type=Nope` answers `200` with three empty blocks; `limit=1` with two types ends after the first change's commit with a cursor and `caught_up: false` |
| 2026-10-18 | A feed poll across a schema apply | `plan_intervals` (`crates/omnigraph/src/changes/enumerate.rs`) and `first_or_boundary` (`changes/feed.rs`) at `d883c49e`; not run live | The gate ignores the request's filters: a type added or removed with rows, or a changed column fingerprint, is `409 schema_boundary` for the whole commit; as a poll's first commit it is the answer, later it ends the page with `caught_up: false`. A rename's block is not known, hence the `/schema` check |
| 2026-10-18 | Schema apply needs a graph with only `main` | `schema_apply.rs` at `d883c49e` ("schema apply requires a graph with only main") | Confirmed |
| 2026-10-18 | What a carry-forward feed read costs | `GET /changes?start=after:<id>` on the personal graph (0.14 edge) with one `type` per name a view reads: a Person page 143 names, an Artifact page 165 | One commit: 0.47–0.68 s, 1–2 KB; five commits: 1.6–2.1 s, 2–5 KB. Every change in them was a `WorkflowRun`, so a Person page moves along with nothing read again. The `/schema` read after it is about 0.5 s and 199 KB |
| 2026-10-18 | Carry-forward | `graph_store` `carry::tests` (5) and `a_head_move_reads_the_change_feed_for_the_types_views_read`; `an_exploration_moves_its_pin_past_commits_to_other_types`; `type_table` `a_page_moves_to_a_commit_that_changed_nothing_it_shows`, `a_table_moves_its_pin_past_commits_to_other_types`, `relationship_columns_read_while_drawn`, `refresh_reads_again_only_the_groups_a_commit_changed`; `a_page_at_a_commit_before_a_property_was_added_shows_the_rest`, `groups_at_a_commit_before_their_edge_or_its_property_existed`, `a_table_at_a_commit_before_a_property_was_added_reads_the_rest`; `graph_store` `a_read_at_a_new_head_is_answered_from_an_earlier_commit_it_still_holds_for`, `answers_kept_under_one_schema_are_not_given_under_another`; `review` `a_review_reads_the_merge_base_and_the_feed_after_it` | Pass |
| 2026-10-17 | Historical reads use the live schema contract | `docs/user/branching/index.md`; `omnigraph.rs` ("a point-in-time read keeps the LIVE contract") | Confirmed: the schema epoch belongs in every key |

## Corrections

- 2026-10-17, before building: the first draft said Studio's reads were unbounded and the server
  had no read admission. The client caps reads at 8 per connection and handles `429`; the server
  admits 64 reads at once. The scheduler shrinks to single flight and the head-read choice.
