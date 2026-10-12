# 003: Connections

Status: draft · 2026-10-11

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

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Read the CLI's config and credentials | Ask for every token again | The CLI is how people already reach their servers |
| Capabilities from `/openapi.json` | Compare version strings | A server on main serves routes 0.13.0 lacks; Tower learned the same (its spec 051) |
| Never retry writes | Retry with backoff | A lost response may have committed; only history can tell |

## Deferred

- The `omnigraph studio` launcher, loopback serving and single-use code handoff
  (`docs/deferred-engine-changes.md`).
- OIDC and signed data credentials: supported by the server, but Studio starts with static
  bearer tokens.
- A second contract (`0.14`) once OmniGraph ships it.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Contract header and value | `git show v0.13.0:crates/omnigraph-api-types/src/lib.rs` | `omnigraph-http-api`, `0.13` |
| 2026-10-11 | Routes in 0.13.0 vs main | parsed `openapi.json` at `v0.13.0` and main | Blob PUT/DELETE, commit publish/drop only on main |
| 2026-10-11 | CLI token resolution | `git show v0.13.0:crates/omnigraph-cli/src/operator.rs` | env `OMNIGRAPH_TOKEN_<NAME>`, then credentials file section; refuses over-permissive file |

## Corrections

None yet.
