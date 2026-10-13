# 013: The agent uses the OmniGraph CLI

Status: first slice implemented · 2026-10-13

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
| Forwarding | `GraphClient::forward` (`crates/graph_client/src/client.rs`) | Sends a request on as it came, with the person's token and the contract header. Bodies stream both ways, no total timeout, never retried, and a path that would leave the server root is refused. The one client crate keeps every request (I) |
| Rules | `crates/agent_panel/src/gateway_rules.rs` | `classify(request, sandbox) -> Verdict`: health, read, read with a rewritten body, a write for the agent's branch, or a refusal |
| Gateway | `crates/agent_panel/src/gateway.rs` | A loopback HTTP/1.1 server per conversation (`127.0.0.1:0`, one request per connection). Checks the session token and contract header, applies the rules, forwards, and reports writes to the panel |
| CLI home | `crates/agent_panel/src/cli_home.rs` | The conversation's directory, config, credentials and wrapper; the shared `AGENTS.md`; finding a CLI that answers |
| Panel | `crates/agent_panel/src/panel.rs` | Starts the gateway and home with the conversation, shows each write as a card, and records outcomes in the session's write log (`GraphSession::note_write`) |
| Agent launch | `crates/agent_panel/src/host.rs` | Starts the agent without OmniGraph credentials in its environment |

### One directory per conversation

A graph's conversations share one agent process and one working directory
(`<data dir>/agent/graphs/<graph>/`), so nothing per conversation can travel in the agent's
environment or in one instructions file. Each conversation gets
`<cwd>/.studio/<agent branch with / as ->/` (mode `0700`):

```yaml
# config.yaml (0600)
servers:
  studio: { url: "http://127.0.0.1:<gateway port>" }
defaults:
  server: studio
  default_graph: "<graph id>"
```

```ini
# credentials (0600), removed when the conversation ends
[studio]
token = <32 random bytes as hex>
```

`omnigraph` (`0700`) is a wrapper: it unsets every `OMNIGRAPH_TOKEN_*`,
`OMNIGRAPH_BEARER_TOKEN`, `OMNIGRAPH_PROFILE` and `OMNIGRAPH_CONTROL_*`, sets `OMNIGRAPH_HOME` to
the directory, puts `127.0.0.1,localhost` in `NO_PROXY`, and runs the chosen CLI. The agent runs
it by absolute path, so neither Codex's environment filter (it may drop names like `*TOKEN*`)
nor a shell snapshot that reorders PATH can change what the CLI reads.

The agent learns the path two ways: the conversation's MCP server adds it to its `initialize`
instructions, and a new `omnigraph_cli` tool returns it with the branch, its base and the rules.
`<cwd>/AGENTS.md`, shared by the graph's conversations, gives the rules and says where to find
the command. Codex reads `AGENTS.md`; Claude reads it when there is no `CLAUDE.md`.

### The agent's environment

The agent process is started through `/usr/bin/env -u …` because the ACP crate can add variables
but not remove them. It removes `OMNIGRAPH_TOKEN_*`, `OMNIGRAPH_BEARER_TOKEN`,
`OMNIGRAPH_PROFILE`, `OMNIGRAPH_HOME`, `OMNIGRAPH_CONTROL_TOKEN`, `OMNIGRAPH_CONTROL_API` and
`OMNIGRAPH_STUDIO_TOKEN` when Studio holds them, unless `agent.env` sets them. Cloud variables
(`AWS_*`, `AZURE_*`, Google's) stay: the agent may need them for its model, as Claude Code on
Bedrock does. This closes the gap where the agent inherited the person's graph tokens.

### Choosing the CLI

1. `agent.omnigraph_cli` (in `settings.json`), then each `omnigraph` on PATH, then
   `~/.local/bin`, `/opt/homebrew/bin` and `/usr/local/bin`, without duplicates.
2. Each is tried through the conversation's gateway with
   `omnigraph snapshot --server studio --graph <id> --json`, bounded to 20 s. The first that
   answers stays in the wrapper. With `omnigraph version --json` (E23) this can read the contract
   instead.
3. If none answers, a notice in the thread names the API the graph speaks and the setting, and
   the `omnigraph_cli` tool says the CLI is unavailable. The agent keeps the graph tools.

### Gateway rules

The gateway answers `GET`/`HEAD /healthz` itself with one contract header, before the token
check, as the CLI probes with no token. Every other request needs the session token (else 401)
and exactly one `omnigraph-http-api: 0.13` (else 400 `api_contract_mismatch`, naming the CLI to
use). Request bodies are read whole, at most 64 MiB (413 above, before anything is sent).

| Request | Rule |
|---|---|
| `GET`/`HEAD` anything; `POST /query`, `/export`, `/changes/baseline` | Forward, any graph, branch or snapshot; the answer streams back |
| `POST /mutate`, `/mutate/if-graph-commit` | Only when `branch` is the agent branch. `branch: null` (branch and commit statements) is refused |
| `POST /load/ndjson`, `/load` | Only when `branch` is the agent branch; `from`, when given, must be the base (it creates the branch) |
| `PUT`/`DELETE /blob` | Only when `branch` is the agent branch |
| `POST /branches` | Only `{name: <agent branch>, from: <base>}` |
| `DELETE /branches/{name}` | Only the agent branch |
| `POST /branches/merge` | Only into the agent branch, and never with `delete_branch: true` |
| `POST /queries/{name}` | On the agent branch, a write; elsewhere forwarded with `expect_mutation: false` |
| `POST /commits/{id}/publish`, `DELETE /commits/{id}`, non-`GET` `/cluster/…`, anything else | Refused |

Writes are checked on the conversation's graph only. A refusal is a 403 `ErrorOutput` with
`code: forbidden` and the contract header; its message ends "Nothing was sent." and says what to
do instead. Until E22, the CLI still reports it as `effects: unknown`; `AGENTS.md` says a Studio
refusal never wrote anything.

A forwarded write's answer is read whole (at most 8 MiB) so the card can show it, then sent on.
When the server's answer is lost, the gateway closes the connection without one, and the CLI
reports the outcome as unknown, as it is.

### Cards and approval

Each write is a card in the thread, like a proposal: the summary ("Mutation", "Load (append)
into …", "Create … from main"), the query when there is one, the branch, and the outcome:
Committed with the commit and the change count, Done or "Branch created" for a write without a
commit, Refused with the server's message, or Outcome unknown. A committed write is logged in the
session (the workspace's "Agent CLI: … committed" notice) and the agent branch banner counts it.

- **Sandbox** (default): writes to the agent branch go through at once.
- **Ask before writes** (`agent.ask_before_writes` in `settings.json`): each write waits on its
  card with Allow (⌘Y), Always on this branch (⌘⌥Y) and Reject (⌘⌥N). The CLI waits, since it has
  no timeout. Reject answers 403 and nothing is sent.

### Codex

Codex's default sandbox blocks loopback, so its CLI calls fail with "server discovery connection
failed". `AGENTS.md` tells the agent what that means: tell the person, and use the graph tools.
Giving Codex network access (a codex-acp mode with network) lets it through, and opens all
network, so it stays the person's choice. E24 would let the sandbox allow one socket instead.

### The MCP tools

The tools of spec 011 stay: they are the path for agents without network, and
`omnigraph_cli` is new. When every supported agent reaches the gateway, the query and propose
tools and the `mcp-proxy` subcommand can go.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| The agent uses the stock `omnigraph` CLI against a Studio gateway | A `studio` CLI of Studio's own; MCP tools only | One CLI for people and agents. The CLI already covers every served route; a second CLI would repeat its verbs. MCP tools cannot be composed |
| Studio holds the person's token; the agent gets a session token for the gateway only | Give the agent the person's token; mint a server token for the agent | The person's token could write `main` and every branch. 0.13 cannot mint a narrower credential: credentials carry no grants, and branch scopes cannot name a branch (E20) |
| The gateway classifies writes by structured fields and refuses what it cannot classify | Parse statement mutations with `gq_language` | Statement targets live in query text, `commit publish/drop` names no branch, and stored-query mutability is the server's. A second parser of the write language could drift from the engine's. Refusing keeps the gateway closed by default; the agent uses the structured commands instead |
| Session token in a 0600 file | An environment variable | Codex's environment filter matches names like `*TOKEN*` |
| A wrapper per conversation, run by absolute path | Environment variables and a PATH entry on the agent process; one `AGENTS.md` naming the command | The agent process and working directory are shared by the graph's conversations, so per-conversation values cannot live there. Codex's shell snapshot may reorder PATH and its filter may drop `*TOKEN*` names |
| Remove only OmniGraph's variables from the agent's environment | Also remove `AWS_*`, `AZURE_*`, Google credentials | The agent may need them for its model provider |
| Request bodies read whole, at most 64 MiB; answers to reads stream | Stream request bodies too | The rules need small JSON bodies whole; a load larger than 64 MiB is refused before sending, which bounds memory (IX) |
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
| 2026-10-13 | Forwarding keeps Studio's credentials and stays under the root | `graph_client` `a_forward_carries_studios_credentials_and_stays_under_the_root` | Token and contract added, the caller's content type and body kept, the answer streamed; `/…`, `../`, `graphs/../..`, a full URL and a backslash refused before sending |
| 2026-10-13 | The rules classify every route the CLI uses | `agent_panel` `gateway_rules` tests (5) | Reads anywhere; mutate, load, Blob, branch, merge and stored-query writes only on the agent branch; statements, other branches, other graphs, commit publish/drop and cluster changes refused; a stored query elsewhere gets `expect_mutation: false` |
| 2026-10-13 | The gateway checks the session, then forwards reads and approved writes | `agent_panel` `the_gateway_checks_the_session_then_forwards_reads_and_approved_writes` (real loopback sockets, a fake upstream) | Health without a token; 401 for a wrong token and 400 for no contract, nothing sent; a read forwarded with the person's token; a write to main refused; a rejected write not sent; an approved write sent once, its commit reported; a chunked body decoded |
| 2026-10-13 | A conversation's home holds its config, token and wrapper | `agent_panel` `a_conversation_home_holds_its_config_token_and_wrapper` | Modes `0700`/`0600`; the wrapper unsets other OmniGraph tokens, prefixes `NO_PROXY`, sets `OMNIGRAPH_HOME`, quotes paths; the token is removed with the conversation |
| 2026-10-13 | The agent starts without OmniGraph credentials | `agent_panel` `the_agent_starts_without_omnigraph_credentials` | OmniGraph tokens, profile, home and Studio's token are removed unless `agent.env` sets them; `ANTHROPIC_API_KEY` and `AWS_PROFILE` stay |
| 2026-10-13 | The stock CLI reads and writes its branch through the real gateway | `agent_panel` `the_stock_cli_reads_and_writes_its_branch_through_the_gateway` (ignored; run with `STUDIO_GATEWAY_E2E_*` against `script/dev-server` and `~/.local/bin/omnigraph` 0.13.0) | Passed: a read on main; mutate on main and a branch statement refused; branch create, a mutation (1 node) and a load on the agent branch; a merge into main refused; the branch deleted |
| 2026-10-13 | The panel shows the agent's CLI writes | Offscreen snapshots with the test agent, which now uses `omnigraph_cli` and the real CLI on "cli" | "Create agent/… from main · Branch created", "Mutation · Committed" with the commit and 1 change, the refusal on main ending "Nothing was sent", the workspace's "Agent CLI: Mutation · committed" notice and the branch banner with Review. With `ask_before_writes`, each card waited with Allow, Always and Reject, and committed after Allow |

## Corrections

- 2026-10-13 · The draft put the session home in `<cwd>/.omnigraph/` and passed `OMNIGRAPH_HOME`
  in the agent's environment. A graph's conversations share one agent process and one working
  directory, so that would have mixed conversations' gateways and branches. Each conversation now
  has its own directory and a wrapper run by absolute path.
- 2026-10-13 · The draft removed `AWS_*`, `AZURE_*` and Google credentials from the agent's
  environment. An agent may need them for its model provider, so only OmniGraph's variables are
  removed.
- 2026-10-13 · The draft made "Ask before writes" a footer toggle and dropped the query tools once
  the CLI answered. The first slice has a setting, and keeps every tool; MCP has no way to drop a
  tool mid-session without `listChanged`.
