# 013: The agent uses the OmniGraph CLI

Status: implemented; the CLI is the only graph tool · 2026-10-13

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
- Policy branch scopes are `any`, `protected`, `unprotected`
  (`crates/omnigraph-policy/src/lib.rs:213-217`). A commit records the actor resolved from the
  bearer token (`handlers.rs:415-437`).
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
  then blocks loopback too (Validation log). Codex's shell-environment filter is off by default
  (`ignore_default_excludes = true`), so shell commands get the agent's whole environment. Its
  stdio MCP servers run outside the sandbox with only a fixed set of variables plus the ACP
  `env` entries. Its shell snapshot may rewrite PATH.
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
| Agent launch | `crates/agent_panel/src/host.rs`, `launch.rs`; `omnigraph-studio launch-agent` in `crates/studio/src/main.rs` | Starts the agent without OmniGraph credentials in its environment: through `env -u` on Unix, through Studio's own launcher on Windows |

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
it by absolute path, so neither an environment filter nor a shell snapshot that reorders PATH
can change what the CLI reads.

The agent learns the path two ways: the conversation's MCP server adds it to its `initialize`
instructions, and a new `omnigraph_cli` tool returns it with the branch, its base and the rules.
`<cwd>/AGENTS.md`, shared by the graph's conversations, gives the rules and says where to find
the command. Codex reads `AGENTS.md`; Claude reads it when there is no `CLAUDE.md`.

### The agent's environment

The ACP crate can add variables to the agent's process but not remove them, and its
`spawn_process` cannot be replaced, so the agent starts through a command that removes them. It
removes every `OMNIGRAPH_*` variable Studio holds, unless `agent.env` sets it: the CLI's
tokens and addressing, and tokens kept for other servers (`OMNIGRAPH_MR_BEARER_TOKEN`) or a local
server's token list alike (in any case on Windows). Cloud variables (`AWS_*`, `AZURE_*`, Google's) stay: the
agent may need them for its model, as Claude Code on Bedrock does. This closes the gap where the
agent inherited the person's graph tokens. With none of these variables set, the agent starts as
it is.

- On Unix the command is `/usr/bin/env -u NAME … PROGRAM ARGS…`. `env` reads a word holding `=`
  as an assignment, so a program path holding one runs through `/bin/sh -c 'exec "$0" "$@"'`.
- Windows has no `env`. There Studio starts its own executable as
  `omnigraph-studio launch-agent NAME… -- PROGRAM ARGS…` (`launch.rs`), as it does for
  `mcp-proxy`. The launcher removes the names, starts the program as the ACP crate would (with
  no window), and exits with its code. Its arguments travel on the command line the standard
  library writes and reads back, so a path or argument arrives as given. Studio stops an agent by
  ending the process it started, which is the launcher, so the launcher puts itself in a job that
  ends every process in it when the launcher ends; the agent and what it starts join that job.
  Windows reads a variable by its name in any case, so there names are matched in any ASCII
  case.

### Choosing the CLI

1. `agent.omnigraph_cli` (Settings → Agent → OmniGraph CLI), then each `omnigraph` on PATH, then
   `~/.local/bin`, `/opt/homebrew/bin` and `/usr/local/bin`, without duplicates.
2. Each says which HTTP contract it speaks, without a server, bounded to 5 s: `omnigraph
   version --json` reports `http_contract` (OmniGraph #987, E23), and 0.13.x, which predates
   it, speaks `0.13`. Studio speaks 0.13 and later only, so an older release, or one that does
   not answer, fits nothing. The first that speaks Studio's contract goes in the wrapper, so a
   slow or unavailable server never decides which CLI a conversation gets.
3. If none fits, the wrapper runs nothing and prints why, so `omnigraph_run` and the agent's
   shell refuse rather than run a CLI the server would refuse. A notice in the thread names the
   contract and each CLI found with its release and what it speaks, and the `omnigraph_cli`
   tool says the CLI is unavailable. The agent keeps the graph tools. A configured
   `agent.omnigraph_cli` that does not fit is passed over with a notice naming the one used.

### Gateway rules

The gateway answers `GET`/`HEAD /healthz` itself with one contract header, before the token
check, as the CLI probes with no token. Every other request needs the session token (else 401)
and exactly one `omnigraph-http-api: 0.13` (else 400 `api_contract_mismatch`, naming the CLI to
use). Request bodies are read whole, at most 64 MiB (413 above, before anything is sent). A
target holding a `#`, or a query parameter given twice, is refused (400) before the rules
read it: forwarding drops a `#` and all that follows it, so the rules would judge a query the
server never sees, and a repeated key could be judged by one value and read as another.

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
- **Ask before writes** (Settings → Agent, `agent.ask_before_writes`): each write waits on its
  card with Allow (⌘Y), Always on this branch (⌘⌥Y) and Reject (⌘⌥N). The CLI waits, since it has
  no timeout. Reject answers 403 and nothing is sent.

### An agent whose shell has no network

Codex's default sandbox blocks loopback, so its CLI calls fail with "server discovery connection
failed". For that agent Studio runs the CLI itself: `omnigraph_run` takes the arguments after
`omnigraph` as a list, runs the conversation's chosen CLI with them (no shell, killed with the
call), and returns the exit code, standard output and standard error. The same gateway, rules
and cards apply, because the CLI still speaks to the gateway.

- The run is outside the agent's sandbox, and the agent can rewrite anything in its working
  directory, its wrapper and `.studio/<branch>/config.yaml` included. So Studio never runs the
  wrapper or reads that home: it starts the chosen CLI itself, with `OMNIGRAPH_HOME` set to a
  copy of the conversation's config and credentials in a private folder outside every working
  directory (`<data dir>/agent/homes/`, `0700`), with every `OMNIGRAPH_*` variable of Studio's
  own removed, and with that folder as its current directory. Run files are staged there too,
  not in the temporary directory, which a sandboxed agent may write. The folder goes with the
  conversation.

- Studio adds `--server studio` and `--graph <graph>` unless the arguments name them, and
  refuses `--server`, `--store`, `--uri`, `--cluster`, `--profile`, `--direct`, `--managed`,
  `--api` and `--as`, so a call cannot leave the gateway.
- Commands: `query`, `mutate`, `load`, `branch`, `snapshot`, `export`, `blob`, `commit`,
  `changes`, `schema show`, `graphs list`, `version` and `lint`. Anything else is refused before
  running. `lint` checks queries against a schema file offline (`--query`, `--schema`, `--json`
  only, no server added): 0.13 refuses `lint` with `--server`, and a positional storage URI
  would make Studio open storage itself.
- Files given to `--query`, `--data`, `--params-file`, `--file`, `--out` and `--schema`, and
  `save_to`, must be inside the working directory (relative, or absolute under it, never
  through `..`, and never under `.studio`, which holds Studio's files for the conversation), so
  the agent can process them in its own sandbox. Studio opens only regular files there, without
  waiting on a pipe. The CLI runs outside that
  sandbox, so it never opens them there: Studio copies each input into a private folder before
  the run and writes each output back after it, opening every folder on the way by handle and
  never through a link (`agent_panel::confined`). A link the agent makes between the check and
  the run is refused on the way in and replaced, not followed, on the way out. The skill,
  `AGENTS.md` and the conversation's CLI home are written the same way.
- Output over 100 KB is cut with a note to use `save_to`; standard output is bounded at 64 MiB,
  and a run at 30 minutes.
- A call made before Studio has chosen a CLI waits for that choice instead of failing.
- A call answers within four minutes. A command still running then (a large load, or a write
  waiting for the person) answers with its run number and keeps going; `omnigraph_run` with
  `wait` set to that number waits for its outcome, again for up to four minutes. A run's
  outcome is returned once; a new session, or the agent stopping, kills runs still going.

Giving Codex network access (a codex-acp mode with network) lets its shell reach the gateway
too, and opens all network, so it stays the person's choice. E24 would let the sandbox allow one
socket instead.

### The skill

Studio writes an `omnigraph-studio` skill into the graph's agent working directory at every
conversation start, beside `AGENTS.md`: `.claude/skills/omnigraph-studio/` for Claude Code and
`.agents/skills/omnigraph-studio/` for Codex, since neither agent reads the other's folder.

- Its `SKILL.md` is Studio's own (`crates/agent_panel/skill/SKILL.md`): what differs in Studio
  (the CLI path and flags, the agent's branch, no schema changes, unknown outcomes, offline
  lint) and which parts of the guide to read or skip.
- The guide is the OmniGraph skill of the release whose contract Studio speaks, copied
  unmodified with its MIT license (`crates/agent_panel/skill/upstream/`, `SOURCE.md` names the
  tag and commit) and embedded in Studio's binary. Its `SKILL.md` is written as `GUIDE.md`, so
  agents do not list it as a second skill.
- The name is not `omnigraph`. With a personal `omnigraph` skill installed, Claude Code hides a
  project skill of the same name, and Codex lists both but loads neither by name.
- Studio replaces files that differ and removes files it no longer ships, only inside its own
  skill folders. `AGENTS.md` names the skill.

When the CLI can export its own skill (being added as `omnigraph skill export`), Studio should
take the guide from the CLI it chose, so the guide always matches that binary, and keep the
embedded copy for CLIs without the command.

### The MCP tools

The CLI is the agent's one way to the graph. Studio's MCP server offers two tools:
`omnigraph_cli` (the command path, flags, branch and rules) and `omnigraph_run`. The graph tools
of spec 011 (`omnigraph_schema`, `omnigraph_branches`, `omnigraph_query`,
`omnigraph_propose_change`) are gone: every one of them is a CLI command, and two ways to the
same write would split the cards and the rules.

- The server's instructions reach the model; Claude Code keeps the first 2,048 characters, so the
  server's and the conversation's instructions together stay under that (tested with a long
  data directory).
- Agents give up on a slow tool call without telling the server. Claude Code aborts a call that
  sends nothing for 30 minutes, and progress resets that; Studio sends `notifications/progress`
  every 60 seconds while a call runs, when the client gave a progress token. Codex abandons any
  call after 300 seconds, ignores progress and sends no cancellation. An agent told its write
  failed while Studio still holds it might write again, so `omnigraph_run` answers before
  Codex's limit with a run number to wait on.
- The thread shows an `omnigraph_run` call with an inline query as a query card titled by its
  subcommand ("Query", "Mutate"), and other calls as tool rows ("Run omnigraph").

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| The agent uses the stock `omnigraph` CLI against a Studio gateway | A `studio` CLI of Studio's own; MCP tools only | One CLI for people and agents. The CLI already covers every served route; a second CLI would repeat its verbs. MCP tools cannot be composed |
| Studio holds the person's token; the agent gets a session token for the gateway only | Give the agent the person's token; mint a server token for the agent | The person's token could write `main` and every branch. 0.13 cannot mint a narrower credential: credentials carry no grants, and branch scopes cannot name a branch (E20) |
| The gateway classifies writes by structured fields and refuses what it cannot classify | Parse statement mutations with `gq_language` | Statement targets live in query text, `commit publish/drop` names no branch, and stored-query mutability is the server's. A second parser of the write language could drift from the engine's. Refusing keeps the gateway closed by default; the agent uses the structured commands instead |
| Session token in a 0600 file | An environment variable | The agent process is shared by the graph's conversations, so one variable cannot hold each conversation's token, and both agents pass their whole environment to every shell command |
| A wrapper per conversation, run by absolute path | Environment variables and a PATH entry on the agent process; one `AGENTS.md` naming the command | The agent process and working directory are shared by the graph's conversations, so per-conversation values cannot live there. Codex's shell snapshot may reorder PATH |
| Remove only OmniGraph's variables from the agent's environment | Also remove `AWS_*`, `AZURE_*`, Google credentials | The agent may need them for its model provider |
| On Windows the agent starts through Studio's own executable as `launch-agent` | `cmd /c set NAME= & PROGRAM ARGS…`; starting the agent's process without the ACP crate | `cmd` expands `%` even inside quotes and does not read the quotes the standard library writes, and the ACP crate passes arguments only through the standard library, so a path or argument could change. Starting the process without the crate would repeat its stderr capture, exit and shutdown handling. Studio already re-runs its executable as `mcp-proxy` |
| Request bodies read whole, at most 64 MiB; answers to reads stream | Stream request bodies too | The rules need small JSON bodies whole; a load larger than 64 MiB is refused before sending, which bounds memory (IX) |
| The gateway passes the upstream contract through and refuses a CLI that speaks another | Translate between contracts | The contract is an exact-match safety property. A translation layer would be a second API that drifts |
| Studio tests each CLI once against the gateway | Map package versions to contracts | The package version does not determine the contract; a real request does, until E23 |
| Codex gets network only when the person allows it | Always enable it; always refuse the CLI to Codex | Enabling it opens all network; refusing it gives up the CLI where the person accepts the risk |
| The CLI is the only graph tool; `omnigraph_run` runs it for agents without network | Keep spec 011's schema, query and propose tools beside the CLI | The CLI covers every served route and composes; the old tools were a subset with their own write path. One path keeps one set of rules and cards |
| `omnigraph_run` takes an argument list and runs the chosen CLI without a shell, from a private home | A command string run by a shell; running the agent's wrapper | A list cannot smuggle a second command or a redirect, and the allowlist and file checks read the same arguments the CLI gets |
| Long calls report MCP progress and answer within four minutes with a run number | Raise the agent's timeout | A timeout is the agent's setting and differs by agent. codex-acp drops a per-server `tool_timeout_sec`; setting it means declaring the server in `CODEX_CONFIG` instead of ACP `mcpServers`. A run number works for every agent |

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
| 2026-10-13 | Claude Code's MCP and shell behave as the design assumes | A minimal ACP client running claude-agent-acp 0.90.0 (Claude Code 2.1.296) with a stub stdio MCP server, plus the package source | Server instructions reach the model, cut at 2,048 characters. `AGENTS.md` loads when no `CLAUDE.md` is in the working directory or above it. Bash and MCP servers inherit the agent's environment; the sandbox is off by default. A silent stdio tool call is aborted after the 30-minute idle timeout; progress notifications reset it. Every MCP tool call asks the client unless pre-allowed. An ACP `mcpServers` entry with `"type": "stdio"` is dropped |
| 2026-10-13 | Codex's MCP and shell behave as the design assumes | codex-acp 2.2.2 and Codex 0.160.1 source, plus live runs of the installed codex-acp with a stub client, a fake model server and throwaway `CODEX_HOME`/`HOME` | Stdio MCP servers run outside the seatbelt sandbox and reach loopback; the shell does not. The MCP tool timeout is 300 s (the docs still say 60 s); on timeout Codex sends no cancellation and drops the late reply. codex-acp passes only `command`, `args` and `env`, so a `tool_timeout_sec` for an ACP server is ignored. The shell-environment filter is off by default: a `*TOKEN*` variable reached the shell. MCP servers start with a fixed environment plus ACP `env`. Server instructions reach the model as the tool namespace's description. `AGENTS.md` loads from the cwd, which codex-acp trusts |
| 2026-10-13 | Each agent finds the skill where Studio writes it | Claude Code: an ACP session (claude-agent-acp 0.90.0) in a non-git folder holding both skill folders, with the person's own `omnigraph` skill installed. Codex: the earlier source and live check of skill discovery | Claude listed `omnigraph-studio` beside the personal `omnigraph`, and `GUIDE.md` was not listed as a skill. Codex loads `<cwd>/.agents/skills` for a non-git working directory and lists every same-named copy |
| 2026-10-13 | Studio writes the skill for each agent and keeps it current | `agent_panel` `writes_the_skill_for_each_agent_and_removes_only_its_own_stale_files`, `every_upstream_file_ships_and_the_entry_point_names_the_skill` | Both folders hold every file; a stale file of an older Studio is removed and a neighbouring skill kept; every file under `skill/upstream` ships; the entry point's name is the folder name |
| 2026-10-13 | `lint` runs offline under the conversation's home | `omnigraph` 0.13.0 with a home whose default server is unreachable | `lint --query --schema` reported the type error; `lint --server` is refused by the CLI |
| 2026-10-13 | A slow run answers with its number and finishes later | `agent_panel` `a_slow_run_answers_with_its_number_and_keeps_going` | No outcome within the limit, the number and the `wait` call in the answer, then the outcome on a later wait |
| 2026-10-13 | `omnigraph_run` checks its arguments | `agent_panel` `run_arguments_stay_on_the_gateway_and_in_the_working_directory` | `--server studio` and the graph added; other servers, stores, profiles, `--as` and unknown commands refused; files outside the working directory refused, including through `..` and symlinks |
| 2026-10-18 | Studio writes into the working directory without following links | `agent_panel` `a_runs_files_are_staged_without_following_links`, `a_skill_folder_linked_elsewhere_is_not_written_through`, `confined::writes_stay_inside_and_links_are_not_followed` | A linked input is refused; an output written over a link replaces the link and leaves its target; a skill folder or `.claude` linked elsewhere is refused and nothing there is written or removed |
| 2026-10-18 | `omnigraph_run` runs only what Studio chose | `agent_panel` `a_run_starts_the_chosen_cli_with_its_private_home`, `confined::writes_stay_inside_and_links_are_not_followed`, `socket::lines_are_read_whole_and_a_line_past_the_cap_ends_the_connection`, `gateway` chunked-size case | With the wrapper and the home's config rewritten, the run started the chosen CLI with the private home as `OMNIGRAPH_HOME` and current directory and no other `OMNIGRAPH_*` variable; `.studio` paths refused; a pipe refused without waiting; a later conversation's token kept when the earlier one ended; a socket line past the cap and a chunk size near `usize::MAX` refused |
| 2026-10-13 | `omnigraph_run` reads and writes through the real gateway | The ignored end-to-end test, extended to call `cli_home::run` | A query inline and with `save_to`, the file in the working directory |
| 2026-10-13 | A long call reports progress until it answers | `agent_panel` `a_long_call_reports_growing_progress_until_it_answers` | Progress notifications with the client's token and growing values, then the answer |
| 2026-10-13 | The instructions fit Claude Code's cut | `a_conversation_home_holds_its_config_token_and_wrapper` | Server and conversation instructions within 2,048 characters for a CLI found, not yet found and missing, with a 120-character data directory |
| 2026-10-13 | The panel shows `omnigraph_run` calls | Offscreen snapshot, the test agent using only `omnigraph_run` on "change please" | Schema and query rows, "Create agent/… from main · new", a "Mutate" card on the agent branch with its query, the "Agent CLI: Mutation · committed" notice and the branch banner with Review |
| 2026-10-13 | The panel shows the agent's CLI writes | Offscreen snapshots with the test agent, which now uses `omnigraph_cli` and the real CLI on "cli" | "Create agent/… from main · Branch created", "Mutation · Committed" with the commit and 1 change, the refusal on main ending "Nothing was sent", the workspace's "Agent CLI: Mutation · committed" notice and the branch banner with Review. With `ask_before_writes`, each card waited with Allow, Always and Reject, and committed after Allow |
| 2026-10-14 | The CLI is chosen by its contract, not a server's answer | `agent_panel` test `the_cli_that_speaks_the_contract_is_chosen_without_a_server` (fake CLIs: 0.11 and 0.13.0 without `version --json`, a 0.14 reporting `http_contract` 0.13, a 0.15 reporting 0.15); `--version` of ~/.local/bin/omnigraph and /opt/homebrew/bin/omnigraph | 0.13 chosen over the 0.11 before it with no server behind the gateway; the 0.14 chosen over the 0.15 before it; with 0.11 and 0.15 the wrapper ran nothing and printed each CLI and what it speaks. Both installed CLIs now report `omnigraph 0.13.0` |
| 2026-10-18 | The launcher starts the agent without the variables and with its arguments as given | `agent_panel` `the_launcher_starts_the_agent_without_the_variables`: the test binary stands in for Studio and for the agent, so both hops cross a real command line. Run on macOS; not yet run on Windows, where it also checks that the agent starts through the launcher (CI runs the suite on `windows-2025`) | `STUDIO_TEST_GONE` absent and `STUDIO_TEST_KEPT` kept in the agent's environment; a space, quotes, an empty argument, `--`, a trailing backslash and `%` arrive as given; the launcher exits with the agent's 7 and writes nothing to stderr |
| 2026-10-18 | Windows matches the variables' names in any case | `agent_panel` `the_agent_starts_without_omnigraph_credentials` | With case folded, `omnigraph_profile` and `Omnigraph_Token_Work` are removed and `omnigraph_home` stays when `agent.env` sets `OMNIGRAPH_HOME`; without it, none of them is removed |
| 2026-10-18 | The launcher's Windows code builds | `cargo clippy --target x86_64-pc-windows-msvc -- -D warnings` on `launch.rs` in a scratch crate with `windows-sys` 0.61.2 (Rust 1.98.1) | Clean. That the agent ends with the launcher rests on `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; no test checks it |

## Corrections

- 2026-10-18 · The agent started through `/usr/bin/env -u …`, which Windows does not have, so on
  Windows any OmniGraph variable in Studio's environment kept the agent from starting. Windows
  now starts it through Studio's own executable as `launch-agent`, and matches the variables'
  names in any case, as Windows reads them.

- 2026-10-18 · The rules read a target's query up to the end, while forwarding dropped a
  `#` and all that followed it. `load/ndjson?mode=overwrite#&branch=<agent branch>` was judged
  a load into the agent's branch and reached the server as a load into main, with the
  person's token; Blob writes the same. Such targets, and repeated query keys, are now
  refused.

- 2026-10-18 · `omnigraph_run` ran the conversation's wrapper from the agent's working
  directory, with Studio's environment. The agent can rewrite that script and its home's config,
  so a sandboxed agent could have run any command outside its sandbox with the person's
  OmniGraph tokens, or pointed the CLI at another address. The run now starts the chosen CLI
  from a private home, without Studio's OmniGraph variables. Ending a conversation also removed
  the token file of a conversation started again on the same branch; it now removes only its
  own.

- 2026-10-14 · A conversation opened while the person's server was slow got Homebrew's
  omnigraph 0.11.0 with ~/.local/bin's 0.13.0 installed, and a notice that no CLI spoke 0.13:
  the choice was a read through the gateway, so any failure (a timeout, a server that did not
  answer) counted against the CLI, and the wrapper kept the last one tried. The CLI's contract
  is now read from the CLI.

- 2026-10-13 · The draft put the session home in `<cwd>/.omnigraph/` and passed `OMNIGRAPH_HOME`
  in the agent's environment. A graph's conversations share one agent process and one working
  directory, so that would have mixed conversations' gateways and branches. Each conversation now
  has its own directory and a wrapper run by absolute path.
- 2026-10-13 · The draft removed `AWS_*`, `AZURE_*` and Google credentials from the agent's
  environment. An agent may need them for its model provider, so only OmniGraph's variables are
  removed.
- 2026-10-13 · The draft made "Ask before writes" a footer toggle and dropped the query tools once
  the CLI answered. The first slice has a setting, and kept every tool; MCP has no way to drop a
  tool mid-session without `listChanged`.
- 2026-10-13 · The first slice assumed Codex strips variables named like `*TOKEN*` from its shell.
  Its filter is off by default; the token file was still the right choice, for the reason the
  Decisions table now gives. It also assumed Codex's MCP timeout was 60 seconds and could be
  set per server; it is 300 seconds, and codex-acp drops the setting.
- 2026-10-13 · The first slice told an agent without network to fall back to the graph tools.
  Codex's default sandbox is such an agent, so the person lost the CLI there. `omnigraph_run`
  now runs the CLI for it, and the graph tools are removed: the CLI is the one path.
