# 030: The read ledger: how each read was answered, how long a view took, and whether a kept answer was right

Status: draft · 2026-11-12 (research added, claims checked against Studio and Omnigraph `7711e4a` the same day)

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
  1. A page's rows answered it (`seeded_reads`).
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
- **A cap on the slow class.** A hub group's read took 61 s on the personal graph (spec 015), so
  one holds a slot as long as 15 to 120 ordinary reads. The client already keeps such a lane for
  reads of changes (`LONG_READ_CONCURRENCY`, 4 of the 8). They get at most 2 slots, so a node
  page's neighbours never queue behind them. The ledger's latency per query name decides which
  names are slow.
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
- **Prefetch** in the speculative lane, at the commit the view shows, never at a moving head:
  - the next table page as the scroll nears the end;
  - a node page's or peek's cheap reads on hover or keyboard focus after a short dwell;
  - never the slow class.
  
  At a fixed commit a prefetched answer can only be unused, never wrong. The ledger reports
  prefetched answers that were used and that were wasted, and that ratio decides whether a
  trigger stays.
- **No hedged reads.** Sending a slow read twice halves tail latency only where requests are
  cheap; here they are what is scarce.

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
   `script/measure-frames`. The first live baseline on the personal graph goes in spec 015's
   validation log.
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
