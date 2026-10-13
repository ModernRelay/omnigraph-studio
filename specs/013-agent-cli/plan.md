# 013: The agent uses the OmniGraph CLI

Status: draft · 2026-10-13

## Problem

Spec 011 gives the agent four MCP tools: schema, branches, a read query, and a proposed change.
An agent cannot compose them. It cannot pipe a result into `jq`, loop over rows, load a file it
wrote, export a branch, or use any `omnigraph` command the tools do not wrap. People want the
agent to work the graph the way they do, with the `omnigraph` CLI in a shell. The guarantees of
spec 011 must still hold: the person's token stays in Studio, the agent writes only to its own
branch, and changes reach `main` only through review.

## Principles

- I: the agent's CLI speaks to Studio, and Studio forwards over its one client crate. The contract
  header is checked both ways.
- III: Studio never retries a forwarded write, and it refuses before effects or not at all.
- IV: Cedar decides. The gateway is a guard Studio keeps for the agent's sake, not policy. It
  only narrows what the person's token may do, and it goes away when the server can scope a
  credential (E20).
- IX: forwarded bodies stream, and each session's requests are bounded.
- X: designed against the 0.13 contract. What the server or CLI should own is filed as engine
  changes (Deferred).
- XII: every claim below was checked in source or by running it (Validation log).

## What exists today (checked)

OmniGraph at `d8d2e3a8` (main), and the released 0.13.0 binaries at `~/.local/bin`:

- `--server` takes a literal URL with no config (`crates/omnigraph-cli/src/helpers.rs:321-325`).
  `OMNIGRAPH_HOME` moves both `config.yaml` and `credentials` (`operator.rs:272-281, 355-357`).
- `defaults.server` plus `defaults.default_graph` make commands run with no flags
  (`scope.rs:128-141`). An `OMNIGRAPH_PROFILE`, a `defaults.store`, or a `./.omnigraph/context`
  in the working directory changes or refuses that. Only explicit flags or `--direct` bypass a
  context file (`managed/data.rs:320-375`).
- The token is chosen by the request URL's longest prefix match against `servers:` entries, not
  by name (`operator.rs:174-192`). The order is `OMNIGRAPH_TOKEN_<NAME>`, then `credentials`
  `[<name>] token =`, then `OMNIGRAPH_BEARER_TOKEN` (`helpers.rs:286-306`). A `credentials`
  file with any group or other permission bit is a hard error (`operator.rs:516-531`).
- Every data request is preceded by `HEAD {root}/healthz`, sent with no bearer and no contract
  header, bounded to 5 s. The response must be 2xx with exactly one `omnigraph-http-api: 0.13`.
  Data requests carry the header, and every response is checked for it
  (`graph_http.rs:10, 133-170`).
- Remote data requests have no total, connect or read timeout, follow no redirects and are never
  retried (`graph_http.rs:94-118`). The system proxy variables apply to loopback unless
  `NO_PROXY` covers it (reqwest 0.12.28 `auto_sys_proxy`).
- After a write is sent, every failure except a typed 429 or a verified 412 is reported as
  `execution: unknown, effects: unknown, action: reconcile`. That includes 401, 403 and the
  contract 400 (`command_outcome.rs:165-258`).
- Where a write names its branch:
  - `ChangeRequest.branch` on `/mutate` and `/mutate/if-graph-commit`.
  - `branch`, `from` and `mode` in the query string of `/load/ndjson`; `from` creates the branch.
  - `branch` in the query string of Blob `PUT`/`DELETE`.
  - `{name, from}` on `POST /branches`; the name in the path of `DELETE /branches/{name}`.
  - `{source, target, delete_branch}` on `/branches/merge`; `delete_branch` removes the source.
  - Statement mutations (`branch create|delete|merge`, `commit publish|drop`) go to `/mutate`
    with `branch: null` and name their targets only in the query text. `commit publish|drop`
    names no branch at all.
  - `/queries/{name}` takes `branch`; whether the stored query writes is known only to the
    server's registry (`InvokeStoredQueryRequest.expect_mutation` is the caller's claim).
  - `schema plan|apply` have no HTTP path and refuse `--server` (`planes.rs:279-296`).
- `--store`, the direct-storage commands and `cluster` without `--server` never go through
  HTTP (`planes.rs:152-153`).
- Policy branch scopes are `any`, `protected`, `unprotected`; "protected" is exact membership
  in `protected_branches` (`crates/omnigraph-policy/src/lib.rs:213-217, 592-593`). A commit
  records the actor resolved from the bearer token (`handlers.rs:415-437`).
- `omnigraph --version` and `omnigraph version` report the package and storage versions, never
  the HTTP contract, and have no JSON form (`main.rs:152-158, 612-620`; `cli.rs:428`).
- On this Mac, `~/.local/bin/omnigraph` is 0.13.0 (contract 0.13) and
  `/opt/homebrew/bin/omnigraph` is 0.11.0, which sends no contract header. The personal server
  and the dev server answer `omnigraph-http-api: 0.13`.

Studio at `3df9307`:

- `graph_client` has typed methods only. It reads every body whole and has no load, export,
  `changes/baseline` or stored-query routes (`crates/graph_client/src/client.rs:250-301`).
- The agent process inherits Studio's whole environment. `agent.env` can add or override
  variables but cannot remove any, and the ACP crate's `spawn_process` has no `env_clear`
  (`crates/agent_panel/src/host.rs:228-230`; `agent-client-protocol-2.2.0/src/acp_agent.rs:265-291`).
- The MCP proxy is Studio's own executable re-run as `omnigraph-studio mcp-proxy <socket>`,
  once per conversation (`crates/studio/src/main.rs:8-21`, `panel.rs:651-687`). The debug
  executable is 490 MB.
- The bundle registers `omnigraph-studio://` and handles `graph`, `node` and `query` links;
  `connect` is refused (`script/bundle-mac:75-85`, `crates/studio/src/deep_link.rs:1-9`).
- New conversations get `cwd` = `<data dir>/agent/graphs/<slug>-<hash>`, and nothing is written
  there (`sessions.rs:289-302`).

Agents (claude-agent-acp 0.89.1 with Claude Agent SDK 0.3.293; codex-acp 2.2.2 with Codex
0.160.1):

- Claude's Bash tool inherits the agent's environment and keeps a prepended PATH: its shell
  snapshot freezes the parent's PATH after sourcing `~/.zshrc`. Its sandbox is off by default.
  When on, it can allow localhost only as all ports (`allowLocalBinding`).
- codex-acp's default `agent` mode sends a workspace-write sandbox policy with
  `networkAccess: false` (`codex-acp/dist/index.js:34264-34319`), and Codex's seatbelt sandbox
  then blocks loopback too (Validation log). Codex's shell-environment filter has default
  exclude patterns `*KEY*`, `*SECRET*` and `*TOKEN*` (strings in the 0.160.1 binary). Whether
  they apply by default inside a session was not verified, so the design assumes they do. Its
  shell snapshot may rewrite PATH.
- Both read `AGENTS.md` in the working directory. Claude reads it when there is no `CLAUDE.md`.

## Design

### Pieces

| Piece | Where | Role |
|---|---|---|
| Gateway | `crates/agent_panel/src/gateway.rs` | A loopback HTTP server per agent session. Checks the session token and the contract, applies the sandbox rules, and forwards through `graph_client` |
| Raw forwarding | `crates/graph_client` | `forward(method, path, query, headers, body stream) -> response stream` with the person's token, and the contract header added and checked. The one client crate keeps every request (I) |
| Session home | `<cwd>/.omnigraph/` | The `OMNIGRAPH_HOME` the agent's CLI reads |
| Launcher wrapper | `crates/agent_panel/src/host.rs` | Starts the agent with a clean environment |
| Instructions | `<cwd>/AGENTS.md` | How to reach the graph, read by Claude and Codex |

### The session home

Written when the session starts, in a directory with mode `0700`:

```yaml
# <cwd>/.omnigraph/config.yaml
servers:
  studio: { url: "http://127.0.0.1:<port>" }
defaults:
  server: studio
  default_graph: <graph id>
```

```ini
# <cwd>/.omnigraph/credentials, mode 0600
[studio]
token = <32 random bytes, base64url>
```

The token lives in a file, not in the environment, because Codex may strip variable names
containing `TOKEN`. It is valid only for this gateway and this session, and it dies with them.
It is never the person's token. The agent's environment gets `OMNIGRAPH_HOME=<cwd>/.omnigraph`
and `NO_PROXY=127.0.0.1,localhost`.

### The agent's environment

Studio starts the agent through `/usr/bin/env` with explicit unsets, because the ACP crate
cannot remove variables. It removes every `OMNIGRAPH_TOKEN_*`, `OMNIGRAPH_BEARER_TOKEN`,
`OMNIGRAPH_PROFILE`, `OMNIGRAPH_CONTROL_*`, `AWS_*`, `AZURE_*` and `GOOGLE_APPLICATION_CREDENTIALS`
variable present in Studio's environment. The person's model-provider keys pass through, since
the agent needs them. This closes the gap where the agent inherited the person's graph tokens
today.

### Choosing the CLI

1. A configured path (`agent.omnigraph_cli`), else `omnigraph` on PATH, else `~/.local/bin`,
   then `/opt/homebrew/bin`.
2. Each candidate is tested once, with `omnigraph snapshot --server <gateway> --graph <id> --json`
   in the session environment, and the result is kept per path, size and modification time.
   With `omnigraph version --json` (E23), the check reads `http_contract` instead.
3. The first candidate that answers is written into `AGENTS.md` by absolute path. Codex's shell
   snapshot may reorder PATH, so the path, not a PATH entry, is what the agent is told.
4. If none answers, the panel says: "This graph speaks OmniGraph API 0.13. Install omnigraph
   0.13 to let the agent use the CLI." The agent still has the read tools of spec 011.

### Gateway rules

The gateway answers `HEAD /healthz` itself, with one header for the upstream's contract and no
body. It never adds a second header to a forwarded response. A request with a missing or
different `omnigraph-http-api` value gets a 400 `api_contract_mismatch` that names the CLI to
use. A wrong or missing session token gets a 401.

| Request | Rule |
|---|---|
| `GET`/`HEAD` routes, `POST /query`, `POST /export`, `POST /changes/baseline`, `GET /schema`, `GET /snapshot`, `GET /commits…`, `GET /branches`, Blob `GET`/`HEAD` | Forward, any branch or snapshot |
| `POST /mutate`, `POST /mutate/if-graph-commit` | Forward only when `branch` is the session's agent branch. Refuse `branch: null` (statement mutations) |
| `POST /load/ndjson`, `POST /load` | Forward only when `branch` is the agent branch. `from` is allowed only while the agent branch does not exist yet, and must be the session's base branch |
| `PUT`/`DELETE /blob` | Forward only when `branch` is the agent branch |
| `POST /branches` | Forward only `{name: <agent branch>, from: <base>}` |
| `DELETE /branches/{name}` | Forward only the agent branch |
| `POST /branches/merge` | Forward only when `target` is the agent branch, so the agent can merge `main` in. `delete_branch: true` only when `source` is also the agent branch |
| `POST /queries/{name}` | Forward with `expect_mutation: false` unless `branch` is the agent branch |
| `POST /commits/{id}/publish`, `DELETE /commits/{id}` | Refuse |
| Anything else | Refuse |

A refusal is a 403 `{"error": "...", "code": "forbidden"}` with the contract header. Its message
says what the agent may do instead, for example: "Studio: the agent writes only to
`agent/20261013-101500`. Pass `--branch agent/20261013-101500`." Until E22, the CLI reports
these refusals as `effects: unknown`. `AGENTS.md` tells the agent that a Studio refusal never
wrote anything.

The agent branch is chosen as in spec 011 (`agent/<YYYYMMDD-HHMMSS>` from the person's branch).
The gateway also lets the agent create it, so a `load --from` or `branch create` is how it
starts.

### Approval

- **Sandbox** (default): writes to the agent branch are forwarded at once. Each one is a card
  in the thread showing the command's route, the branch, and the outcome Studio saw (spec 011's
  three outcomes). Merging into `main` stays the person's, in review (spec 008).
- **Ask before writes**: the gateway holds each write until the person allows or rejects it on
  its card. The CLI has no client timeout, so a held request waits. Checked by holding a
  mutation 150 s, which then committed. A rejection is a 403 sent before forwarding.

The mode is a toggle in the panel's footer, kept per conversation.

### Codex

Codex's default sandbox blocks loopback, so its `omnigraph` calls cannot reach the gateway.
Studio shows this once per Codex conversation: "Codex runs commands without network access, so
it can't reach the graph through the CLI. Give it network access for this conversation, or keep
using the graph tools." Giving it network access sets the codex-acp session to workspace-write
with network enabled. That opens all network, not only the gateway, which is why the person
chooses. A Unix-socket gateway (E24) would let the sandbox allow one socket instead.

### The MCP tools

The tools of spec 011 stay for agents that cannot reach the gateway, Codex without network
among them. Once a conversation's CLI answers, `omnigraph_query` and `omnigraph_propose_change`
are left out of its tool list. `omnigraph_schema` and `omnigraph_branches` stay as cheap
context. When every supported agent reaches the gateway, the tools and the `mcp-proxy`
subcommand are removed.

### AGENTS.md

Written at session start and rewritten when the agent branch is created:

- the graph, the server name `studio`, the agent branch, and the base branch;
- the CLI's absolute path, with explicit `--server studio --graph <id>` in every example, so a
  `.omnigraph/context` in another folder is never used;
- reads anywhere; writes only with `--branch <agent branch>`;
- batch writes with `load`, because one load of many rows costs about one commit while each
  `mutate` is a commit of its own;
- "A Studio refusal never wrote anything. An `effects: unknown` from a write Studio forwarded
  means the server's answer was lost; check `commit list` before retrying."

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| The agent uses the stock `omnigraph` CLI against a Studio gateway | A `studio` CLI of Studio's own; MCP tools only | One CLI for people and agents. The CLI already covers every served route; a second CLI would repeat its verbs. MCP tools cannot be composed |
| Studio holds the person's token; the agent gets a session token for the gateway only | Give the agent the person's token; mint a server token for the agent | The person's token could write `main` and every branch. 0.13 cannot mint a narrower credential: credentials carry no grants, and branch scopes cannot name a branch (E20) |
| The gateway classifies writes by structured fields and refuses what it cannot classify | Parse statement mutations with `gq_language` | Statement targets live in query text, `commit publish/drop` names no branch, and stored-query mutability is the server's. A second parser of the write language could drift from the engine's. Refusing keeps the gateway closed by default; the agent uses the structured commands instead |
| Session token in a 0600 file | An environment variable | Codex's environment filter matches names like `*TOKEN*` |
| The CLI's absolute path in `AGENTS.md` | A PATH entry Studio prepends | Codex's shell snapshot may reorder PATH; Claude keeps it |
| The gateway passes the upstream contract through and refuses a CLI that speaks another | Translate between contracts | The contract is an exact-match safety property. A translation layer would be a second API that drifts |
| Studio tests each CLI once against the gateway | Map package versions to contracts | The package version does not determine the contract; a real request does, until E23 |
| Codex gets network only when the person allows it | Always enable it; always refuse the CLI to Codex | Enabling it opens all network; refusing it gives up the CLI where the person accepts the risk |

## Deferred

Engine changes, in [spec 012](../012-omnigraph-engine-changes/plan.md) and
[`docs/deferred-engine-changes.md`](../../docs/deferred-engine-changes.md):

- **E20** ([#978](https://github.com/ModernRelay/omnigraph/issues/978)): delegated, branch-scoped
  credentials. The server would enforce the sandbox for any client and record the agent in
  history. The gateway then only provisions a credential and shows cards.
- **E22** ([#976](https://github.com/ModernRelay/omnigraph/issues/976)): the CLI reports a 401, a
  403 or a contract 400 on a write as not started.
- **E23** ([#977](https://github.com/ModernRelay/omnigraph/issues/977)): `omnigraph version --json`
  with the HTTP contract.
- **E24** ([#979](https://github.com/ModernRelay/omnigraph/issues/979)): Unix-socket transport,
  so a sandbox can allow one socket.
- **E1, E4** ([#980](https://github.com/ModernRelay/omnigraph/issues/980)): `omnigraph studio`. The
  launcher would pass its own CLI path and contract, so agent sessions on that graph use it.

Studio work not in this plan: per-port loopback rules for Claude's sandbox (Claude allows all of
localhost or none), and a Studio-bundled CLI (the CLI is not small, and the person's installed
CLI is the one they trust).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-13 | The stock CLI works against a gateway with a literal URL or a session home | A Python stand-in gateway on `127.0.0.1:8790` forwarding to the dev server; `omnigraph` 0.13.0 run with `env -i`, `OMNIGRAPH_HOME` and a session token | Literal `--server` with `OMNIGRAPH_BEARER_TOKEN`: rows. A session home with `servers.studio` and `OMNIGRAPH_TOKEN_STUDIO`, no flags: rows. No token: the gateway's 401 printed as JSON |
| 2026-10-13 | The probe and the data request look as source says | The gateway's request log | `HEAD /healthz` with no contract and no bearer, then `POST /graphs/knowledge/query` with `omnigraph-http-api: 0.13` |
| 2026-10-13 | A held write waits and commits | Gateway holding `POST /mutate` for 150 s | Exit 0 after 150 s, committed on `gw-probe` as `act-dev` |
| 2026-10-13 | Writes name their branch in structured fields | Gateway log of `branch create`, `mutate --branch`, `load --branch --from` | `{"from":"main","name":"gw-probe"}`; `ChangeRequest.branch = "gw-probe"`; `/load/ndjson?branch=gw-load&mode=append&from=main` |
| 2026-10-13 | A gateway refusal reaches the agent, but as an unknown outcome | Gateway answering `branch merge` with 403 `forbidden` | Exit 1, the message shown, `command_outcome: {"execution":"unknown","effects":"unknown","action":"reconcile"}` (E22) |
| 2026-10-13 | A contract mismatch sends nothing | Gateway advertising `0.14` on `/healthz` | "could not establish omnigraph-http-api: 0.13 … this data request was not sent"; only the probe reached the gateway |
| 2026-10-13 | An older CLI is refused before execution | `omnigraph` 0.11.0 through the gateway | No probe, no contract header; the server answered 400 `api_contract_mismatch` |
| 2026-10-13 | Proxy variables capture loopback | `HTTP_PROXY=http://127.0.0.1:9`, with and without `NO_PROXY` | Without: "server discovery connection failed … not sent". With `NO_PROXY=127.0.0.1,localhost`: rows |
| 2026-10-13 | Destructive commands need `--yes` with no terminal, even on loopback | `branch delete gw-probe < /dev/null` | Refused, `not_started`; `--yes` deleted it |
| 2026-10-13 | Codex's sandbox blocks loopback | `codex sandbox` (0.160.1, a throwaway `CODEX_HOME`) running `curl http://127.0.0.1:8790/healthz` | Default: no connection, `CODEX_SANDBOX_NETWORK_DISABLED=1`. Workspace-write without network: none. With `sandbox_workspace_write.network_access=true`: 200. Danger-full-access: 200 |
| 2026-10-13 | A `unix://` server is not a server address today | `omnigraph query --server unix:///tmp/x.sock …` | `storage_root_invalid … unsupported storage URI scheme 'unix'` (E24) |

## Corrections
