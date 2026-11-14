# 030: The read ledger: how each read was answered, how long a view took, and whether a kept answer was right

Status: in progress; steps 1 and 4 built (2026-11-12); part 5's first seed, a group counted empty, built (2026-11-12) · drafted 2026-11-12, research added and claims checked against Studio and Omnigraph `7711e4a` the same day

## Problem

Spec 015 built a cache that keeps answers at a commit, carries them to later commits and seeds
nodes from table pages. Nobody can say how well it works. The plan's "Expected effect" (a Person
page from about 185 requests to about 76) was never measured. The tests check single mechanisms,
not what a person waits for. When we change the cache to make it faster, nothing checks that a
carried answer still equals what the server would say at that commit.

Three questions need numbers that stay comparable over time:

1. **Cost.** For each view, how many reads it asked for, and how each one was answered: kept,
   carried, seeded, shared with a read already under way, or sent (to the branch or pinned).
2. **Latency.** How long until a view shows its first answer, and until it shows everything it
   asked for.
3. **Correctness.** Whether an answer given without a request equals the server's answer at the
   same commit.

## Principles

- I: the server stays the authority. The audit compares against it, and never changes what a
  view shows.
- III and the agent guide: never log tokens, query text or row data. The ledger records query
  names Studio gave its own reads, routes, outcomes, times and sizes. It records no parameters
  and no ids.
- IX: bounded. A ring of events, aggregates of fixed size, and an audit with its own small cap on
  reads.
- The lesson of #90 (closed 2026-11-06, `docs/perf/20261111-report.md`): no new crate, nothing
  every view depends on, and each part lands as its own small change in an existing layer.

## What exists today (checked)

### Studio (main at `29fc1636`)

- **Where a read with a commit is decided.** `GraphSession::read` (`crates/graph_store/src/session.rs`)
  works in this order:
  1. A page's rows answered it (`seeded_reads`), or, since 2026-11-12, a count of 0 held at
     the commit answers a group's first neighbours with no rows (part 5, "Seeding before
     prefetch").
  2. Else it is carried from an earlier commit (`carried`), and the carried answer is then
     **inserted into `pinned_reads`** like an answer read.
  3. Else `share` either returns a held answer or joins a read under way.
  4. Else it sends `read_at_commit`. A read at the head's commit goes as a branch read, and the
     commit it answers at is checked.

  A read with no commit (`snapshot: None`) skips all of this and is always sent.
- **Relationship counts have their own path.** They use `degree_kept` and carry per node, so the
  ledger records there too.
- **Every request** passes through `counted_read`.
- **Scopes cover only some views.** Tables, peeks and node pages (`type_table`) and the tree and
  canvas (`graph_store::exploration`) read through a `ReadScope` while they follow a branch. A view
  held at a commit reads with no scope (`read_scoped(None, …)`). History, review, the query editor,
  the command palette and the agent panel read with no scope at all. So a scope is not the same as
  a view.
- **Studio names its own queries.** Every generated query carries a name (`QueryRequest.name`):
  `studio_node`, `studio_page`, `studio_page_neighbours`, `studio_page_degrees`,
  `studio_neighbours`, `studio_head`, `studio_count`, `studio_required_dependents`, and 20 more in
  `gq_language::builder`.
- **Reads that are never carried.** `Depends::Unknown` is the default. A person's query, a search
  and anything not described are never carried.
- **The client's slots.** It keeps 8 reads on the wire, and of those at most 4 that may wait past
  60 s (reads of changes: `LONG_READ_CONCURRENCY`). A slot is taken inside the read's own future,
  and `counted_read` drops that future when every asker has let go. So a read still queued for a
  slot is never sent.
- **429s from the personal server are never retried.** A 429 is retried twice, but only when its
  `Retry-After` is at most 30 s (`MAX_RETRY_AFTER`).
- The snapshot example (`crates/studio/examples/snapshot.rs`) drives real views offscreen; its
  `,live` option lets reads land and times them. `script/measure-frames` runs it.
- `graph_store::test_server` is a fake server with commits and a change feed by type.
- Telemetry (spec 018) has `DurationBucket` and `CountBucket`. There is no share bucket yet.

### Omnigraph (the personal server runs `0.14.1-edge+7711e4a`, read at that commit)

- **Read admission.** Every read takes a read observer, 128 at most
  (`operations.rs` `DEFAULT_READ_OBSERVERS`). A read with a body, such as `POST /query`, also
  takes a read-ingress lease. Leases allow 64 at once and 64 MiB in all; each reserves 1 MiB and
  then shrinks to its body (`workload.rs`, `ingress.rs`). The per-actor caps (16 in flight) apply
  to writes, not reads (`handlers.rs`, "bound concurrent in-flight mutations").
- **Every 429 says `Retry-After: 60`.** It is a constant (`lib.rs` `RETRY_AFTER_SECONDS`). Since
  Studio waits at most 30 s, Studio never retries a 429 from this server and shows it as a
  refusal. Spec 015 says Studio "waits out a 429's Retry-After twice"; against this server it
  does not.
- **No `Server-Timing` header, and no `priority` header honoured** (searched in
  `omnigraph-server/src`).
- **A read runs inside its request.** `server_query` awaits `run_query` in the handler, while a
  write is moved into a task of its own (`owned_write`). So a read whose request is dropped is
  dropped at its next await. Whether the engine's own tasks stop with it, and whether a client's
  disconnect reaches the server at all, cannot be read from the source: the personal server sits
  behind Railway's edge proxy (`server: railway-hikari`), and Studio speaks HTTP/2 to that proxy.
  This needs a live check.
- **The only time-dependent function in the query language is `now()`** (`query.pest`). A query
  that calls it, or `nearest` with a text argument, can answer differently at the same commit.
  Studio's generated queries use neither, and anything else is `Unknown` and never carried.

### Measured

- A one-off count on 2026-11-12, made with a temporary counter in `graph_client` and not kept:
  Studio restoring one table on the personal graph sent 55 requests. These were 20
  `studio_page_degrees`, 13 `studio_page_neighbours`, 4 `studio_head`, 1 `studio_page`,
  1 `studio_count`, and the session's setup reads (`/schema`, `/snapshot`, `/access`,
  `/queries`, `/branches`, `/commits`), including one refused with 401 while the token was
  renewed. The median request took 0.69 s, p90 1.93 s and the slowest 3.66 s.
- On 2026-10-18 (spec 015), reading each of a Person's 92 groups once: the hub groups' neighbour
  reads took 61.2 s (`spokenBy`), 20.8 s and 16.5 s. The median was 1.25 s and p90 3.3 s.

## Design

### 1. One record per read asked: `ReadEvent`

Recorded in `graph_store` where the session decides how to answer:

```rust
pub struct ReadEvent {
    pub asker: Asker,              // who asked: a scope, or a view kind given by the caller
    pub scope: Option<u64>,        // the scope's id, when it read through one
    pub what: ReadKind,            // the query name Studio gave it, or the route; `query` for a person's own
    pub at: AtCommit,              // Head, or Older
    pub answer: Answered,
    pub asked: Instant,
    pub done: Option<Instant>,
}

pub enum Answered {
    Kept { origin: Origin },       // the memo held an answer at this commit
    Carried { commits: u32, origin: Origin },  // from an earlier commit the change log cleared
    Seeded,                        // a page's rows answered it
    Joined,                        // shared a read under way
    Sent { target: Target, status: Status, queued: Duration, wire: Duration, bytes: u64 },
    Dropped,                       // every asker let go before it answered
}

pub enum Origin { Read, Seeded, Carried }  // how the answer held first came to be
```

- **Origin.** A carried answer is filed in `pinned_reads` beside answers read, so a later hit on
  it is `Kept` though no server ever gave it at that commit. Each entry keeps its origin, and the
  ledger and the audit go by the origin, not by the path of the last lookup.
- **Who asked.** Only tables, peeks, pages, the tree and the canvas read through a scope, and only
  while they follow a branch. The read methods therefore take an `Asker`: the scope when there is
  one, and otherwise a view kind the caller names (`History`, `Review`, `QueryEditor`, `Palette`,
  `Agent`, `Session` for the session's own reads). A read nobody names counts as `Unnamed`, and a
  test fails on any `Unnamed` read, so new code cannot read unattributed.
- **Counts** are recorded on their own path (`degree_kept`, `degree_at`) with the same events.

Two parts:

- **Aggregates, always on.** Counts per `(scope kind, what, answer)` in a fixed table, plus a
  latency histogram per kind, with power-of-two buckets. Each update is one hash lookup on the
  main thread.
- **The ring, on when asked.** The last 4,096 events, kept when `OMNIGRAPH_STUDIO_READS=1`, in a
  test's capture, or in the snapshot tool.

### 2. A view's load, from its scope

For the views that read through a scope, the scope already knows when its first read was asked,
when its first answer landed, and when no read is out. It records three times:

- `began`: its first read was asked.
- `first`: its first answer landed.
- `settled`: no read is out, and none has been for one frame.

Those views change nothing. A Refresh or a new pin starts a new load. Views without a scope
(history, review, the query editor) get load times only when they are given a scope of their own,
which is a later step.

### 3. The audit: kept answers checked against the server

A carried or seeded answer claims to equal the server's answer at one commit. That commit never
changes, so a check read at the same commit cannot race a write: any difference is a bug in the
read-set check, the change feed, seeding or decoding, never noise. That is a stronger position
than the systems this borrows from (Figma's LiveGraph verification, Meta's Polaris, GitHub's
Scientist; see Sources), which have to tell bugs from races.

- **What is checked, in order of risk:** answers whose origin is `Carried`, then `Seeded`,
  however they were last looked up. Answers whose origin is `Read` are the server's own and are
  not checked.
- **Which answers.** A deterministic sample, as rustc's incremental verification picks: a hash
  of the key, modulo N, against a number that changes each session. A failure then happens again
  on the next run, and a different slice is checked next session.
  - `OMNIGRAPH_STUDIO_AUDIT=all` checks every answer. Tests and the scenario runner use it.
  - Debug builds check 1 in 8.
  - Releases check none unless the person turns it on.
- **How.** The read is pinned at the same commit and under the same schema epoch. It never goes
  through the memo and never goes ahead of a view's read: it uses the speculative lane (part 5),
  and only while nothing visible waits. It never reads the slow class (part 5). It is a read, not
  a write, and it is not retried.
- **Comparison.** Exact. Rows are compared as a set when the query has no total order. Only
  generated queries are ever carried or seeded, and none calls `now()` or `nearest` with text, so
  the server's answer at a commit cannot change. An answer that differs is a bug.
- **Skipped, not failed:** the commit was cleaned up (`410`), the schema epoch moved before the
  check read was sent, or the credential changed.
- **Report.** Matched, mismatched and skipped, per answer kind, given as nines ("carried answers:
  4 nines correct over 12,000 checked"), as Polaris reports. A mismatch records what replays it:
  the query name, both commits, the read set's types, and the schema epoch. It records no
  parameters, ids or rows. A test fails on any mismatch.
- **Offline too.** A property test drives the fake server (`test_server.rs`) with random commits
  and checks every carried answer against a fresh read. It costs no requests and runs in CI.

### 4. Where the numbers go

- **Tests.** `session.reads().capture()` in a `#[gpui::test]`, with `assert_eq!` on
  `(what, answer)` counts. These become budget tests per view. Counts are deterministic against
  the fake server, so they fail a build exactly, as Figma ties each regression to a commit. Wall
  time is reported as a trend only. Examples: a Person page opened from
  a table sends no `studio_node`; a Refresh after a commit that touched other types sends nothing.
- **Scenarios.** Steps in the snapshot example: open a table, open a row's page, wait for a head
  move, Refresh. It prints one table per step (reads by answer, first and settled times, any
  audit mismatch) and writes JSON, so two builds can be compared with
  `script/measure-frames --reads`. It runs against `script/dev-server`, or read-only against a
  real graph with `--target`.
- **Telemetry (spec 018).** One `ViewSettled { view, settled: DurationBucket, sent: CountBucket,
  from_cache: ShareBucket }` event, and an `AuditMismatch { answer }` count where the audit is
  turned on. Both are added to the catalog and `docs/telemetry.md`. They are anonymous, as the
  catalog's other events are.
- **Later: a reads panel** in the developer menu, showing the ring live. It is not needed for the
  rest.

### 5. What the ledger measures next: scheduling and prefetch

The ledger lands first, so that each of the following is a change with a before and an after.
Each is its own plan when it is built.

- **Lanes inside the 8 slots.** Use shares, as Netflix's concurrency-limits partitions do, not a
  strict priority queue. Visible reads get most of the slots and near reads some. The
  speculative lane (prefetch, the audit) gets only slots nobody else is using.
- **A cap on the slow class: measured, not kept (2026-11-12).** A hub group's read took 61 s on
  the personal graph (spec 015), so one holds a slot as long as 15 to 120 ordinary reads. The
  idea was a lane like the one for reads of changes (`LONG_READ_CONCURRENCY`, 4 of the 8): reads
  known to be slow get at most 2 slots. Measured against the fake server
  (`a_pages_hub_groups_hold_slots_its_ordinary_groups_wait_for`, validation log), it does not pay:
  - **Little to free.** A page has few hubs (3 of a Person's 92 groups), and while fewer than
    8 slow reads are out the ordinary ones still have slots. Capping 3 hubs at 2 frees one slot of
    five: the 40 ordinary groups settle at 7 s, not 8.
  - **It costs the hubs.** The third hub waits for a slow slot and is answered at 120 s, not 60.
  - **Nothing is known on the first open.** No read says it is slow before it is sent: a group's
    count comes from the same slow read, or from a table, and a page opened from a table does not
    open a large group by itself. History only helps the same node's page read again.
  - **Order matters more than the cap.** With the hubs taking slots as others freed them (the
    test executor's own order), the ordinary groups settled at 7 s with no cap.
  
  What would change this: a page or canvas that has 6 or more slow reads out at once, which the
  ledger's slowest time per name would show in the scenarios (step 4), or a lane beside the 8
  instead of inside them. A lane beside them is a different bound: it lets 10 reads onto the wire.
  Separately, Studio sends a query with a 60 s timeout (`READ_TIMEOUT`, set in `send_now`), so a
  hub read as slow as `spokenBy` (61.2 s) would fail in Studio rather than finish. This was read in
  the source, not checked live.
- **Already true:** a read every asker dropped is dropped from the client's queue before it is
  sent (`counted_read`). The ledger counts these as `Dropped`, which shows how much a view
  abandons.
- **429s first.** The personal server answers every 429 with `Retry-After: 60`, and Studio waits
  at most 30 s, so a 429 is never retried there. Decide this before any adaptive limit: wait
  60 s, or treat the 429 as the signal to take fewer slots. The ledger counts 429s by query name,
  which says how often it happens.
- **An adaptive limit** that backs off on `429` and timeouts and grows back slowly (AIMD), never
  above 8. Latency is no signal here, because the cost of a read depends on its query. The
  server's cap is across clients (64 reads with a body, 128 reads in all), and 8 per Studio is
  far below it, so this matters only when several clients share one server.
- **Seeding before prefetch.** A seed costs no request. Peeks and node pages should take
  everything a held answer already says.
  - *Built (2026-11-12): a group counted empty lists no neighbours.* A node page reads each
    group's first neighbours (`studio_neighbours`), and before this it read them for every
    group, the ones the table it was opened from had just counted 0 included. Now a first page
    narrowed by nothing but its limit carries `builder::EmptyWithout` (the node, the group and
    the projection's column names), and `GraphSession::read` answers it with no rows when the
    session holds that node's count through the group at the same commit and schema epoch, read
    or carried, and it is 0. The ledger records it as `Seeded`. Why the answer is the server's:
    the count's read (`studio_page_degrees`) and the page's match the same node type, the node
    by id and the same traversal, so a node without a match in the one has no row in the other.
    The count is the session's own, forgotten with the token as every count is.
  - *Measured, not taken: a table's chips for a page's neighbours.* `studio_page_neighbours`
    returns the neighbour and its label but not the edge, while a page and a peek list each
    edge with its id (`edge_id`) and the edge's properties, so no row of it is a row of
    `studio_neighbours`. Seeding them would need the table to read the edges too.
  - *Next: one group's first neighbours at another limit.* After a page, a peek of the same row
    sends `studio_neighbours` again for each group with neighbours (3 in the fixture below),
    because a peek reads 9 rows and a page 51, so their keys differ. The order (`$m.@id`, then
    `$e.@id`) is total, so a read of limit `a` answers a read of limit `b` with its first `b`
    rows when `b <= a`, or when it returned fewer than `a`. That is the next class to seed.
- **Prefetch** in the speculative lane, at the commit the view shows, never at a moving head:
  - the next table page as the scroll nears the end;
  - a node page's or peek's cheap reads on hover or keyboard focus after a short dwell;
  - never the slow class.
  
  At a fixed commit a prefetched answer can only be unused, never wrong. The ledger reports
  prefetched answers that were used and that were wasted, and that ratio decides whether a
  trigger stays.
- **No hedged reads.** Sending a slow read twice halves tail latency only where requests are
  cheap; here they are what is scarce.

### Step 1 as built (2026-11-12)

- **Where it lives.** `graph_store::ledger` holds `ReadLedger`, `ReadEvent`, `Answered`,
  `Origin`, `Asker`, `ReadView`, `Target` and `Status`. The session owns one ledger
  (`GraphSession::ledger`, `ledger_mut`).
- **Answers from the cache.** `GraphSession::read` records `Seeded`, `Carried(origin)`,
  `Held(origin)` and `Joined`. A carried answer is marked (`carried_keys`) until a read keeps
  one at its key. Relationship counts record `Held`, `Carried` or `Batched` under the name
  `count`.
- **Requests.**
  - Every `counted_read` records its route's name (`access`, `commit_page`, `commit_changes`,
    `changes`, `diff`, `merge_base`, `blob_head`, …). Each query records the name Studio gave
    it.
  - The session's own reads (`branches`, `snapshot`, `stored_queries`, `studio_head`) are
    recorded through `ledgered`.
  - The time is from asking for a slot to the answer, on the executor's clock.
  - Status is ok, refused (with its HTTP status), failed, or dropped while every asker let go.
- **Targets.** There are four: `Branch` (no commit; kept at the commit its answer names, below),
  `HeadCommit` (a commit that is its branch's head, sent as a branch read; kept), `Pinned` and
  `Route`.
- **A read with no commit is kept** (2026-11-12, found by the ledger). `GraphSession::keeping`
  files a branch read's answer in `pinned_reads` at the commit it names (`graph_commit_id`), as a
  read at that commit, so a later read there is `Held(Read)` and one at a later commit can be
  `Carried`. Only when the schema epoch and the credential it was sent under still stand, only
  for a read whose dependencies are known (`Depends::Unknown` is neither kept nor carried),
  never over what is already held there, and the head stays the head watch's to move.
- **Who asked.** `read_scope` takes the `ReadView` (table, peek, page, exploration), and the
  scoped reads (`read_in`, `degree_in`, `blob_meta_in`) name it, including when a head-commit
  read is sent again pinned. Every other read is `Unnamed` for now.
- **Not yet:**
  - The schema reads, which run inside larger background tasks.
  - Named askers for unscoped views, and the test that refuses `Unnamed`.
  - Queue and wire time apart, and bytes: `graph_client` would have to report them.
  - The ring's switch by environment variable. Tests use `keep_events` and the tallies.
- **Tests.**
  - `graph_store`: `ledger::tests` (2) and `the_ledger_says_how_each_read_was_answered`.
  - Budgets added to `type_table`'s `a_peek_reads_only_what_the_table_did_not` and
    `refresh_reads_again_only_the_groups_a_commit_changed`.

### Step 4 as built (2026-11-12)

- **The runner.** `reads:<path>` in the snapshot example (`crates/studio/examples/snapshot.rs`)
  turns on the window session's ledger events (`keep_events`) for the run. The start and then
  each action, `click:` and `head:` step clears the ledger before it acts and lasts until the
  session has no read out (`reads_in_flight`) and has answered none for 1 s (at most 120 s).
  Real time passes and GPUI's clock is moved by as much, so the ledger's times are wire times.
  After each step it prints `ReadLedger::table()` and the time to the first answer and to the
  last, and at the end writes every step's tallies as JSON.
- **New steps.** `run:<command>` runs a command and waits for it (another writer).
  `head:<ms>` waits for the session's branch head to move, as the 30 s head poll sees it.
- **The scenario.** `script/measure-frames --reads <out.json>` against `script/dev-server`:
  open Person, open person-000001's page, another writer gives that person a new title, wait
  for the head to move, and click the page's "Newer data · Refresh". With two files it
  compares them (`script/compare-reads.py`).
- **Wire time.** `script/latency-proxy.py` (Python standard library only) holds each request
  `STUDIO_READS_LATENCY_MS` before passing it on, 700 ms by default, the personal graph's
  median. Set it to 0 to send requests straight to the server.
- **Not yet:**
  - `--target`, a read-only run against a real graph.
  - The audit's mismatches (step 3 is not built).
  - The schema read, which the ledger does not record (step 1, "Not yet").
  - A step's name is its action's name, `click` or `head`, and never its arguments.

### Baseline (2026-11-12)

Linux 4.19 under gVisor, 2 cores, no GPU (lavapipe). `release-fast` build of main at `3466c50`
with this change. `omnigraph-server` `v0.15.0` (contract 0.14) on the development graph: Person
has 200 rows and 6 relationship columns. 700 ms a request. Two runs gave the same counts, and
times within 10 ms of each other.

| Step | Asked | Sent | How | First answer | Settled |
|---|---|---|---|---|---|
| Start, until the schema is read | 5 | 5 | `access`, `branches`, `head_commit`, `snapshot`, `studio_head`, each sent once | 859 ms | 1,573 ms |
| Open Person | 13 | 13 | 1 `studio_page`, 6 `studio_page_degrees`, 6 `studio_page_neighbours`, all `Sent(HeadCommit)` by the table | 784 ms | 2,289 ms |
| Open person-000001's page | 5 | 4 | `studio_node` `Seeded`; 4 `studio_neighbours` sent | 26 ms | 755 ms |
| Another writer's commit, until it is seen | 4 | 4 | the head poll: `studio_head`, `branches`, `snapshot`, `head_commit` | 23,242 ms | 24,668 ms |
| The page's Refresh | 6 | 2 | `studio_head` sent, then `studio_node` sent; the 4 neighbour groups `Carried(Read)` | 724 ms | 1,451 ms |

With no added latency the same counts: open Person settles in 232 ms, the page in 50 ms, its
Refresh in 68 ms.

## Decisions

| Decision | Rejected | Why |
|---|---|---|
| Measure where the session decides how to answer | Count HTTP requests in `graph_client` | Only the session knows a read was kept, carried, seeded or joined, and the cache works through those answers; requests alone measure what it failed to save |
| Attribute by scope where there is one, and by a named asker otherwise | Treat the scope as the view | Only the views that follow a branch read through a scope; history, review, the query editor and the agent panel do not |
| Keep each answer's origin | Classify by the lookup's path | A carried answer is filed among answers read, so the path alone would hide it from the audit |
| Inside `graph_store` | A `studio_perf` crate every view depends on (#90) | #90 was closed; the 2026-11-11 report asks for small instruments in existing layers |
| Audit kept answers against a pinned read | Trust the change log's tests | The cache's correctness rests on the feed, the epoch and `Depends`; only the server's answer at the commit proves an answer right |
| Fixed aggregates always on, the ring on demand | Everything on demand | Telemetry and a later panel need numbers from ordinary use, at no measurable cost |
| Query names and routes only | Hashed parameters or ids | Ids are row data; a hash of an id is still a stable identifier |
| A deterministic, rotating audit sample | Random sampling | A failure repeats on the next run, and every slice is checked over sessions (rustc) |
| Audit in the speculative lane, never the slow class | Audit inline, or every read | Requests are the scarce resource; a check read must never delay what a person waits for |
| Request-count budgets fail CI; wall time is a trend | Wall-time thresholds in CI | Counts are deterministic against the fake server; server time is not |

## Build order

1. `ReadEvent`, the aggregates and `capture`, recorded at `share`, the carry, `seed` and
   `counted_read`. Three budget tests: a page from a table, a Refresh after an unrelated commit,
   and a table's first page.
2. The scope's load times.
3. The audit, on in every `graph_store` and `type_table` test that carries or seeds.
4. The scenario steps and their JSON in the snapshot example, and `--reads` in
   `script/measure-frames` (built 2026-11-12, against the development graph). The first live
   baseline on the personal graph, with `--target`, goes in spec 015's validation log.
5. The telemetry events.

## Deferred

Server changes that would make this cheaper; none is needed for the design:

- **Does a dropped read stop on the server?** The handler drops a read when its request goes
  (above), but Railway's proxy sits between Studio and the server. Before prefetch is built,
  measure it on a scratch server: drop a slow read after 2 s and watch the server's
  read-observer count. If the read keeps running, file it in spec 012.
- **A `Server-Timing` header** (time queued against time working), so the ledger can split the
  wire time. Not in `omnigraph-server` today.
- **The `priority` header** (RFC 9218) honoured across clients. Not in `omnigraph-server` today.
- **A `Retry-After` that says how long the wait really is.** Today it is always 60 s.
- **E32** (one request carrying several reads) would let the near lane batch different reads, as
  DataLoader and Linear's batch route do. E32 and E16 (counts in one request) change what "sent"
  costs, and the ledger shows how much each would save before they are built.

## Sources

The research (2026-11-12) is summarized here; a claim not checked in a primary source is marked.

- Zero's inspector (per-query `hydrate` times, row counts, update p50/p95) and its preload caps:
  zero.rocicorp.dev/docs/debug/inspector, /docs/queries.
- PowerSync's sync priorities (a higher priority interrupts a lower one):
  docs.powersync.com/usage/use-case-examples/prioritized-sync.
- Figma: scripted interactions on every commit, regressions fixed or reverted
  (figma.com/blog/figma-faster); LiveGraph's sampled online verification of its cache
  (figma.com/blog/livegraph-real-time-data-at-scale).
- Meta Polaris, consistency reported as nines within a window
  (engineering.fb.com/2022/06/08/core-infra/cache-made-consistent).
- GitHub Scientist: sampling, `compare`, `clean`, `raise_on_mismatches` (github.com/github/scientist).
- rustc's `-Zincremental-verify-ich` and its rotating subset (secondary: rustc docs and
  `verify_ich.rs`).
- Netflix concurrency-limits, partitioned limits (github.com/Netflix/concurrency-limits);
  Envoy's adaptive concurrency filter.
- DataLoader's per-tick batching (github.com/graphql/dataloader).
- Hedged requests, "The Tail at Scale" (secondary summaries only).
- Linear's per-model load strategies and batch route (reverse-engineered, secondary).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-14 | The runner measures a dense graph's Expand all, and finds the main thread, not the server, slow | `script/measure-expand-all` (the snapshot example's `reads:` steps) against `script/dense-server`, main at `5c5c8ad` and this branch; spec 006's validation log | The server answered the 58 `studio_page_degrees` and 10 `studio_neighbours` of 1,763 counts within about 0.6 s, but main showed the first answer at 12.1 to 12.4 s: each count's answer flattened the whole walk. Taken a batch at a time, the first and last answer land at about 0.6 s. Counts were the same in every run |
| 2026-11-12 | Restoring a table sends 55 requests on the personal graph | A temporary counter in `graph_client::send_now`, Studio debug build, not committed | As in "What exists today" |
| 2026-11-12 | Where a read with a commit is decided, and the origin of what is kept | `session.rs` `read` (seeded, then `carried`, which inserts into `pinned_reads`, then `share`, then `read_at_commit`), `degree_kept` | As in "What exists today". Spec 030 first said a `Kept` answer was the server's own; a carried answer is kept too, hence `Origin` |
| 2026-11-12 | Which views read through a scope | Calls of `read_in`, `read_scoped`, `degree_in`, `blob_meta_in` and `note_in` outside `graph_store`, and inside it | Only `type_table` (`table.rs`, `peek.rs`) and `exploration.rs`. Spec 030 first said every view did; hence `Asker` |
| 2026-11-12 | Studio's query names | `"studio_*"` strings in `gq_language` and `graph_store` | 28 names, `studio_node` among them |
| 2026-11-12 | A read dropped while queued is never sent | `graph_client` `read_raw`: the slots are taken inside the read's future; `counted_read` drops it when every asker has | Confirmed |
| 2026-11-12 | 429 handling against the personal server | Studio `MAX_RETRY_AFTER` 30 s, `READ_RETRIES` 2; Omnigraph `7711e4a` `lib.rs` `RETRY_AFTER_SECONDS = "60"` on every 429 and retryable 503 | A 429 from this server is never retried; spec 015's "waits out a 429's Retry-After twice" holds only for a server asking 30 s or less |
| 2026-11-12 | The server's read caps | `7711e4a` `operations.rs` `DEFAULT_READ_OBSERVERS = 128`; `workload.rs` `DEFAULT_READ_INGRESS_INFLIGHT_MAX = 64`, `DEFAULT_READ_INGRESS_BYTES_MAX` 64 MiB, `DEFAULT_REQUEST_BODY_LIMIT_BYTES` 1 MiB reserved per read; per-actor caps on mutations only (`handlers.rs`) | Confirmed; spec 015's "64 reads" is the body-bearing lane, under a cap of 128 reads in all |
| 2026-11-12 | A read runs inside its request | `7711e4a` `handlers.rs` `server_query` awaits `run_query`; writes use `owned_write`, which spawns | Confirmed in the source. Whether it stops through Railway's proxy is not checked |
| 2026-11-12 | What stands between Studio and the personal server | `curl -sI https://omnigraph.ragnor.co/healthz`: `server: railway-hikari`, `x-railway-edge: sjc1`; HTTP version 2 | Railway's edge proxy over HTTP/2 |
| 2026-11-12 | Time-dependent functions | `7711e4a` `omnigraph-compiler/src/query/query.pest`: `now_call` is the only one | Confirmed |
| 2026-11-12 | No `Server-Timing` or `priority` handling | `git grep -i` for both in `7711e4a` `crates/omnigraph-server/src` | Neither found |
| 2026-11-12 | The ledger records how each read was answered | `graph_store` `the_ledger_says_how_each_read_was_answered`: two asks of a page (one `Sent(HeadCommit, Ok)`, one `Joined`), the node `Seeded` with no request, the neighbours sent once, `Carried(Read)` to a commit that touched another person, then `Held(Carried)`; two reads attributed to the page's scope. `type_table`: a peek of a table's row sends no `studio_node` and three `studio_neighbours`, all asked by the peek; Refresh carries knows out and in and sends likes out and in | Pass; workspace 1,380 tests pass, clippy clean |
| 2026-11-12 | Found by the ledger: a page opened before the session knows its branch's head reads its node with no commit, and that answer is not kept, so Refresh sends the node again though nothing of it changed | `type_table` `refresh_reads_again_only_the_groups_a_commit_changed`, ledger before Refresh: `studio_node … Sent(Branch, Ok)`; after: `Sent(HeadCommit, Ok)`, nothing carried | Open at the time; fixed in the next row |
| 2026-11-12 | A read with no commit, kept at the commit its answer names, then holds and carries | Fake servers only. `type_table` `refresh_reads_again_only_the_groups_a_commit_changed`, ledger for Refresh. Before: 4 sent (`studio_neighbours` 2 `Sent(HeadCommit)`, `studio_head` 1, `studio_node` 1 `Sent(HeadCommit)`), 2 carried. After: 3 sent (`studio_neighbours` 2, `studio_head` 1), 3 carried (`studio_neighbours` 2, `studio_node` 1 `Carried(Read)`). `graph_store` `a_read_with_no_commit_is_kept_at_the_commit_it_answered`: a branch read, then the same read at its commit (`Held(Read)`, before: sent again) and at a commit to another person (`Carried(Read)`): `studio_node` sent once in all (`Sent(Branch)`), before twice (`Sent(Branch)`, then `Sent(HeadCommit)` at c1). The guards: an answer naming a commit the head watch has not seen is kept there and the head stays; a schema epoch or credential changed while the read was out, or `Depends::Unknown`, keeps nothing; removing any one guard fails the test | Pass. Two `type_table` tests read a first page again at the commit a branch read answered at, and now send nothing (`a_first_read_pins_the_head_the_session_holds`, `a_read_out_when_a_commit_to_another_type_moves_the_pin_lands`: 1 request each before, 0 after); their assertions changed with a note. Not covered: a live graph, and branch reads outside `GraphSession::read` (`read_on` another branch, `read_request` called directly), which are still not kept |
| 2026-11-12 | A node page opened from a table reads no neighbours of a group the table counted empty | `type_table` `the_ledger_of_a_table_a_rows_page_and_its_peek`: a Person table over a small graph that answers Studio's queries as a server would (`test_support::Graph`; Ada has 6 groups, 3 with neighbours), then Ada's page, then her peek. Table: 1 `studio_page`, 1 `studio_count`, 6 `studio_page_degrees`, 6 `studio_page_neighbours`. Page before: `studio_node` `Seeded`, 6 `studio_neighbours` sent. Page after: `studio_node` `Seeded`, 3 `studio_neighbours` `Seeded`, 3 sent. Peek after the page: 3 `studio_neighbours` sent, for the groups with neighbours; it reads none for the empty ones, which the table hands it as `KnownRelation` counts, so this change does not touch it. Each of the 6 page queries, answered by the session at `c1`, has the same rows, byte for byte, and row count as the fake's answer to the same query text and parameters; the 3 it sent are among them. `graph_store` `a_group_counted_empty_lists_no_neighbours_without_a_read`: a count of 0 answers with no request, a count of 1, a search and a node not counted are sent | Pass. A page's requests at open fall from 6 to 3 in the fixture. On the personal graph a table counts only the relationship columns it draws (20 `studio_page_degrees` in the count above), so a page saves the empty ones of those for its row; a group no view counted is read as before. Not proven against the fake: `columns` (the fake answers `["n"]` to every read); the session gives the projection's names, and no reader of these answers reads them |
| 2026-11-12 | A cap of 2 slots on reads known to be slow lets a node page's ordinary groups settle much sooner (part 5) | `graph_store` `a_pages_hub_groups_hold_slots_its_ordinary_groups_wait_for`: a fake server (`test_server`'s new `wait`, on the executor's clock) answers 3 hub groups' `studio_neighbours` in 60 s and 40 others in 1 s; the hubs are asked first and the ledger's `took` times each read. A prototype added a slow lane to `graph_client` (2 slots taken before one of the 8) and, in the session, sent a read in it when the same name, text and parameters had taken 15 s or more | **Not kept.** Before, on the first read and again: ordinary groups settled at 8 s (mean 4.5 s), hubs 60, 60, 60 s. With the cap, first read: unchanged, since nothing is known yet. Read again: 7 s (mean 3.85 s), hubs 60, 60, **120 s**. With the executor's own order and no cap, ordinary groups settled at 7 s (mean 3.55 s), hubs 60, 63, 64 s. The test keeps the 8 s baseline |
| 2026-11-12 | The scenario runner measures a table, a page, another writer's commit and the page's Refresh, headless on Linux | `script/measure-frames --reads` twice and `--reads a.json b.json` on the development graph, lavapipe, 700 ms a request | Pass. Counts the same in both runs, times within 10 ms: open Person 13 sent of 13 asked (settled 2,289 ms), the page 4 of 5 (`studio_node` seeded, 755 ms), the head move 4 of 4 (seen after 23.2 s), the page's Refresh 2 of 6 (4 groups carried, 1,451 ms). Table in "Baseline" |
| 2026-11-12 | `ReadLedger::table()` lines up its columns | `ledger::tests::the_table_lines_up_its_columns` | Fixed: `SharedString`'s `Display` ignores width, so names longer or shorter than others pushed the columns along; the name is now padded as a `str` |
| 2026-11-12 | Found by the runner: the page's Refresh reads the head before the node, one after the other | The Refresh step: `studio_head` answers at 724 ms, and only then is `studio_node` sent, settled at 1,451 ms. The head poll read the same head about 1 s before | Open. Two round trips where one could do: the next hypothesis to measure with this runner |
| 2026-11-12 | Found by the runner: Studio's Refresh (`studio::Refresh`, ctrl-r) does not read again the page in front, though its description says "and the view in focus" | The same scenario with `studio::Refresh` in place of the page's button: `branches`, `snapshot`, `head_commit` and `studio_head` sent, no `studio_node` or `studio_neighbours`, and the page still offers "Newer data · Refresh" | Open. The scenario clicks the page's button |
