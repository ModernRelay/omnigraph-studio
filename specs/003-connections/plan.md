# 003: Connections

Status: in progress · 2026-10-13. Built: servers from the CLI config, the environment and
Studio's own list; token resolution; the contract check; bounded reads; and replacing a refused
token (validation log). OIDC waits (Deferred).

## Problem

Studio has to find OmniGraph servers, authenticate, confirm it speaks the server's contract,
discover which graphs and routes are there, and show the state of all of that at a glance,
without the `omnigraph studio` launcher (deferred).

## Principles

I (HTTP only; contract header), III (unknown outcomes), IV (Cedar decides), X (capabilities
from the served document).

## What exists today (checked)

- OmniGraph `v0.13.0`, `crates/omnigraph-api-types/src/lib.rs:30-32`: contract header
  `omnigraph-http-api`, value `0.13`. Every protected request must carry it; a mismatch is
  `400 api_contract_mismatch`; responses echo it.
- Public routes: `GET`/`HEAD /healthz`, `GET /readyz`, `GET /openapi.json`. The CLI checks the
  contract with `HEAD /healthz` before a data request.
- `GET /graphs` needs the `graph_list` action, which is denied without a cluster policy, even to
  a valid token. `GET /graphs/discovery` returns ids and names but accepts only identity
  credentials, not static tokens.
- The CLI's operator config: `$OMNIGRAPH_HOME` or `~/.omnigraph/config.yaml` with `servers`,
  `profiles`, `aliases`, `defaults` (`default_server`, `default_graph`). Token for server
  `<name>`: env `OMNIGRAPH_TOKEN_<NAME>` (dashes become underscores, upper case), then
  `[<name>] token = …` in `$OMNIGRAPH_HOME/credentials`, which must not be group or world
  readable (`crates/omnigraph-cli/src/operator.rs:350-420` at `v0.13.0`).
- Routes on OmniGraph main but not in `v0.13.0`: Blob `PUT`/`DELETE`, commit publish and drop.

## Design

### Sources of servers

1. Every server in the operator config, labelled with its config name. Tokens are resolved
   exactly as the CLI does; Studio never writes the CLI's files.
2. Servers added in Studio (URL, optional token). Tokens go to the OS keychain through GPUI's
   credentials API; the server list goes to Studio's settings.
   An address is made a server root before it is used (`graph_client::server_root`): trimmed,
   without a trailing slash, with `http://` added for a loopback host (`localhost`, `127.*`,
   `[::1]`) and `https://` for any other when no scheme is given; anything but an `http(s)`
   URL with a host, or one holding a user name or password, is refused with the reason. The
   same rule applies to addresses from the CLI config, the environment and a saved list, so
   an entry saved before it (`localhost:8080`) loads as `http://localhost:8080`, its token
   copied to the new address. `GraphClient::new` refuses an unusable root rather than
   building URLs on it.
3. Graph ids for a server come from, in order: `GET /graphs` when allowed; the operator config's
   profiles and `default_graph`; graph ids the person typed before. A 403 on `/graphs` is not an
   error state; Studio says "This server does not list graphs for your token. Enter a graph id."

### Connecting

1. `HEAD /healthz` with a 5 s bound. Read the contract header.
2. Compare with the contracts `graph_client` speaks (only `0.13` today). Mismatch: state
   `contract mismatch`, with the server's version and "Studio speaks 0.13".
3. `GET /openapi.json` once per connection; record which operation ids exist (for example
   `cluster_putBlob`, `cluster_publishCommit`). Surfaces ask the capability set, never the
   version string.
4. `GET /readyz` for serving state; `GET /graphs` when allowed, for per-graph state
   (`ready`, `loading`, `transitioning`, `blocked`, `stopping`) and `read_available` /
   `write_available`.
5. Re-check health every 30 s while a window is open and on wake from sleep; back off to 2 min
   after failures.

### States (status-bar chip)

| State | Chip | What the person can do |
|---|---|---|
| connecting | grey, "Connecting…" | wait; cancel |
| ready | green, "server · 0.13" | everything the token allows |
| contract mismatch | amber | open in the CLI; nothing else |
| unauthorized (401) | red | update token |
| unreachable | red, last error | retry; requests in flight are marked unknown, never resent |
| graph unavailable (503) | amber, graph state from `/graphs` | wait; reads resume when ready |

### Requests (`graph_client`)

- Built on Zed's `http_client::HttpClient` (Apache-2.0): `reqwest_client` in the app, the fake
  client in tests.
- Every request sets `Authorization: Bearer`, `omnigraph-http-api: 0.13`, and a bounded
  timeout. Every response is checked for the contract header; a write response without it is an
  **unknown** outcome.
- 429 honours `Retry-After` for reads only. Writes are never retried (constitution III).
- Error bodies are parsed into the typed `ErrorOutput` (code, diagnostic, merge conflicts,
  precondition failure, resource limit, …) and surfaced as such.
- Tokens, query text and row data are never logged.
- A connection keeps at most 8 reads on the wire (`READ_CONCURRENCY`, shared by every clone of
  its client); the rest queue. A server admits a bounded number of reads across all clients
  (0.13 refuses past its read ingress cap, 64 by default, with "read ingress count cap 64
  exceeded"), and a view that counts every relationship of a node at once would otherwise be
  refused for some of them. A read waiting out a 429 does so without its slot.
- A read at a commit is answered once per session: the answer is kept (256 reads, 2,048 Blob
  cells, least recently used out first) and an identical read under way is shared, since a
  commit never changes. Failures are not kept, and reads at the branch head always go out.
- Each branch's commit list is read once and shared by the views that show it (the history,
  the branches panel, every query tab's snapshot picker); a commit Studio makes on a branch
  drops that branch's list, reading the branch list again drops them all (its first read, as
  the window opens, keeps what was asked for meanwhile), and Refresh reads its branch again. A
  write used to set off one full commit list per branch in each of those views, one after
  another. Review and merge checks read their branches' heads themselves, since they decide on
  the live head.
- The footer's "N queries running" and "Writing…" count requests actually in flight. A read
  whose caller stops waiting (a search superseded by the next keystroke, a closed tab) is
  stopped and counted out. A write runs to its outcome whoever still waits for it, and the
  outcome is logged.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Read the CLI's config and credentials | Ask for every token again | The CLI is how people already reach their servers |
| Capabilities from `/openapi.json` | Compare version strings | A server on main serves routes 0.13.0 lacks; Tower learned the same (its spec 051) |
| Never retry writes | Retry with backoff | A lost response may have committed; only history can tell |

## Deferred

- Engine changes E1–E5, [spec 012](../012-omnigraph-engine-changes/plan.md): the `omnigraph studio` launcher, a supported local embedding
  API, the loopback guard in the launcher, owner discovery, and writer-safety rules for local
  sessions. Until then Studio connects to running servers.
- OIDC and signed data credentials: supported by the server, but Studio starts with static
  bearer tokens.
- A second contract (`0.14`) once OmniGraph ships it.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-12 | A server added as `localhost:8080` no longer crashes Studio | crash report: `GraphClient::url` panicked on `path_segments_mut().expect(..)` from the health probe, since `Url::parse("localhost:8080")` reads `localhost` as the scheme and cannot be a base; `graph_client` tests `a_server_address_gets_a_scheme_and_a_bare_word_is_refused`, `a_client_refuses_a_root_that_cannot_take_a_path` | The address becomes `http://localhost:8080`; a root without an http(s) host, or with credentials, is refused with its reason |
| 2026-10-11 | Contract header and value | `git show v0.13.0:crates/omnigraph-api-types/src/lib.rs` | `omnigraph-http-api`, `0.13` |
| 2026-10-11 | Routes in 0.13.0 vs main | parsed `openapi.json` at `v0.13.0` and main | Blob PUT/DELETE, commit publish/drop only on main |
| 2026-10-11 | CLI token resolution | `git show v0.13.0:crates/omnigraph-cli/src/operator.rs` | env `OMNIGRAPH_TOKEN_<NAME>`, then credentials file section; refuses over-permissive file |
| 2026-10-12 | 429 honours `Retry-After` for reads only | `graph_client` tests `a_rate_limited_read_waits_and_goes_again_a_bounded_number_of_times`, `a_rate_limited_write_is_never_sent_again` (counted requests) | A read waits as told (≤ 30 s, 1 s when unnamed) and goes again at most twice; a write is sent once |
| 2026-10-12 | A 503 graph is unavailable until it answers | `graph_store` test `a_graph_that_answers_503_is_unavailable_until_it_answers_again` (simulated clock) | Marked with the server's reason; asked again after 5 s, doubling to 60 s; any answer clears it. The document footer shows an amber dot by the graph with the reason |
| 2026-10-12 | Abandoned reads leave the running count; abandoned writes still finish | `graph_store` test `dropped_reads_are_counted_out_and_dropped_writes_still_finish` (three reads that never answer, dropped; a write whose task is dropped before it ran); the same test against the earlier session code | Count back to 0 and the write sent once with its refusal logged; the earlier code left the count at 3, as the app showed "100 queries running" after four node searches over crm's 25 types |
| 2026-10-12 | A connection keeps a bounded number of reads on the wire | `graph_client` test `a_connection_keeps_a_bounded_number_of_reads_on_the_wire` (40 reads through two clones against a server that holds each 5 ms) | At most 8 at once, all answered. Seen before the fix in the app on a person with dozens of relationship types: "read ingress count cap 64 exceeded" with Retry on several rows |
| 2026-10-12 | A refused token can be replaced in place | offscreen snapshot with a wrong `OMNIGRAPH_STUDIO_TOKEN`, Open Graph | "The server refused the token" with a masked field and Update token; the connection reconnects with it. A Studio server keeps it in the keychain; a CLI server for this run only. Retry when unreachable, Cancel while connecting |
| 2026-10-13 | Opening a window reads each branch's commit list once | `graph_store` test `commit_lists_are_read_again_only_for_the_branch_a_commit_lands_on` (main asked for while the branch list loads, the branches panel asking again on its arrival: one read; it read twice before the fix); offscreen snapshots through a request-counting proxy on the dev graph's four branches: an empty window; three new queries and the history | `GET /commits` 4 times in both, once per branch (before: 5 for the empty window, main twice, and one more per query tab) |

## Corrections

- 2026-10-12 · A read or write counted itself out only when it answered, inside the task its
  caller held. The palette's node search drops one read per node type at each keystroke, so
  the footer climbed to "100 queries running" on crm and stayed there; a write dropped the
  same way would have been cut off with its outcome unlogged. Reads now stop and are counted
  out when dropped, and writes run detached to their outcome.

- 2026-10-12 · The client followed redirects: reqwest's default follows up to ten, and nothing
  overrode it. Every request now sets `RedirectPolicy::NoFollow`, so a Blob's external
  reference (302 with its URI) is reported and never fetched.
