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
- Since OmniGraph main `2c9eb1e0` (#1090, 2026-10-20), after 0.14's edge build: `--server`,
  `--store`, `--cluster`, `--profile`, `--managed`, `--direct`, `--api`, `--organization`, graph
  `--uri` and `use` are removed. `--target NAME|ADDRESS`, or `defaults.target`, selects one
  backend, and the resolved target is printed to stderr. The operator file is
  `target_config_version: 1` with `targets`; a server target has an `endpoint`, a
  `default_graph` and one `credential` owner, whose token is `OMNIGRAPH_TOKEN_<OWNER>` or what
  `omnigraph login --target` stored in `credentials`, bound to the endpoint. No command reads
  `OMNIGRAPH_BEARER_TOKEN` (#1030). `version --json` reports `protocols.target`. A `unix:///…`
  endpoint is a server (E24, #988), and a refused write is reported as not started (E22, #981).
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
| Gateway | `crates/agent_panel/src/gateway.rs`, `requester.rs` | A loopback HTTP/1.1 server per conversation (`127.0.0.1:0`, one request per connection). Checks the session token and contract header, applies the rules, forwards, and reports writes to the panel. On Windows it holds the processes a write's CLI was started under while the write waits |
| CLI home | `crates/agent_panel/src/cli_home.rs` | The conversation's directory, config, credentials and wrapper; the shared `AGENTS.md`; finding a CLI that answers |
| Panel | `crates/agent_panel/src/panel.rs` | Starts the gateway and home with the conversation, shows each write as a card, and records outcomes in the session's write log (`GraphSession::note_write`) |
| Agent launch | `crates/agent_panel/src/host.rs`, `launch.rs`; `omnigraph-studio launch-agent` in `crates/studio/src/main.rs` | Starts the agent without OmniGraph credentials in its environment: through `env -u` on Unix, always through Studio's own launcher on Windows |

### One directory per conversation

A graph's conversations share one agent process and one working directory
(`<data dir>/agent/graphs/<graph>/`; on Windows under `%LOCALAPPDATA%`, below), so nothing per
conversation can travel in the agent's environment or in one instructions file. Each
conversation gets `<cwd>/.studio/<agent branch with / as ->/` (mode `0700`):

```yaml
# config.yaml (0600)
target_config_version: 1
defaults:
  target: studio
targets:
  studio:
    kind: server
    endpoint: "http://127.0.0.1:<gateway port>"
    credential: studio
    default_graph: "<graph id>"
```

```ini
# credentials (0600), removed when the conversation ends
[studio]
token = <32 random bytes as hex>
endpoint = http://127.0.0.1:<gateway port>
```

Studio writes the credentials file in the CLI's own format, so the token never passes through a
process; the CLI sends it only to requests under `endpoint`. The CLI must have targets
(OmniGraph #1090): Studio writes no other form.

`omnigraph` (`0700`) is a wrapper: it unsets every `OMNIGRAPH_TOKEN_*`,
`OMNIGRAPH_BEARER_TOKEN`, `OMNIGRAPH_PROFILE` and `OMNIGRAPH_CONTROL_*`, sets `OMNIGRAPH_HOME` to
the directory, puts `127.0.0.1,localhost` in `NO_PROXY`, and runs the chosen CLI. The agent runs
it by absolute path, so neither an environment filter nor a shell snapshot that reorders PATH
can change what the CLI reads.

On Windows the wrapper is the batch file `omnigraph.cmd`, written from the same account of what
it does as the shell script (`cli_home::Wrapper`), so only the syntax differs. It removes the
same variables (`set OMNIGRAPH_TOKEN_` lists each of that prefix, in any case), sets
`OMNIGRAPH_HOME` to its own folder (`%~dp0`), prefixes `NO_PROXY` (one variable with `no_proxy`
there) and runs the CLI with `%*`. Windows starts a batch file through `cmd /c`, which reads the
caller's command line first: it expands `%NAME%` and, outside double quotes, acts on `&`, `|`,
`<`, `>` and `^`. A caller that escapes for cmd, as Rust's standard library does, gets every
argument through as given; a shell that quotes only for programs (Git Bash, PowerShell) leaves
the text between two quotes inside one argument outside cmd's quotes. That holds for every
`.cmd`, npm's shims included, so on Windows the instructions and `AGENTS.md` tell the agent to
put such a query or its parameters in a file, or to use `omnigraph_run`. The batch file reads
nothing more itself: `%*` is expanded once, delayed expansion is off, each path it names is in
double quotes with `%` doubled, and its lines end with CRLF. cmd reads a batch file in the
console's code page, so the file is ASCII only: its home is `%~dp0`; the CLI is named by its
path when that is ASCII, else from `%~dp0` when that way is (both under a profile whose name is
not); otherwise the wrapper says so and points at `omnigraph_run`. The agent is then told that
its shell cannot run the CLI and to use `omnigraph_run`, which starts the CLI by its path and
still runs it, and the thread says why; it is not told the wrapper is ready. A refusal turns
delayed expansion off too, so a `!` in its text stays even where a person's registry turns it
on for every batch file.

cmd cannot work in a network folder: started in one, it says so and works in the Windows
folder instead, so every relative path a batch file passes on (`--query`, `--data`, `--out`)
would be read from there. Studio's data folder on Windows is the roaming one (`%APPDATA%`),
which folder redirection can put on a `\\server\share`, so the agent's working directories go
under the local application folder (`%LOCALAPPDATA%\OmniGraph Studio\agent\graphs\…`), which is
never redirected; the agent keeps its own sessions on the machine anyway. A `pushd` of the
working directory in the wrapper was rejected: cmd has lost the caller's folder before the batch
file runs, so it could only guess the working directory's root, and a drive letter it maps can
be left mapped when the command is stopped before its `popd`. Known limit: a working directory that is
still a network path (a profile on a share, or `OMNIGRAPH_STUDIO_DATA` set to one) keeps cmd's
behaviour, and the conversation's instructions then tell the agent to give the CLI every file by
its absolute path, or use `omnigraph_run`, which reads relative paths itself.

The agent learns the path two ways: the conversation's MCP server adds it to its `initialize`
instructions, and a new `omnigraph_cli` tool returns it with the branch, its base and the rules.
Both say what Studio chose, so both wait for the choice (since 2026-10-21): the instructions are
made when `initialize` arrives, from the CLI chosen by then, which the panel sends the socket
(`socket::Instructions`); past 5 s still choosing, they say so and to call `omnigraph_cli`, and
name no CLI. An `omnigraph_cli` call made before the choice is answered once it is made.
The config makes the gateway and the graph the default, so they tell the agent to give no
`--target` or `--graph`.
`<cwd>/AGENTS.md`, shared by the graph's conversations, gives the rules and says where to find
the command. Codex reads `AGENTS.md`; Claude reads it when there is no `CLAUDE.md`.

### The agent's environment

The ACP crate can add variables to the agent's process but not remove them, and its
`spawn_process` cannot be replaced, so the agent starts through a command that removes them. It
removes every `OMNIGRAPH_*` variable Studio holds, unless `agent.env` sets it: the CLI's
tokens and addressing, and tokens kept for other servers (`OMNIGRAPH_MR_BEARER_TOKEN`) or a local
server's token list alike (in any case on Windows). The prefix is matched in one place,
`agent_panel::scrub` (since 2026-10-21), which the agent's launch calls; a variable OmniGraph
adds later is removed without a list to keep. Cloud variables (`AWS_*`, `AZURE_*`, Google's) stay: the
agent may need them for its model, as Claude Code on Bedrock does. This closes the gap where the
agent inherited the person's graph tokens. With none of these variables set, the agent starts as
it is.

- On Unix the command is `/usr/bin/env -u NAME … PROGRAM ARGS…`. `env` reads a word holding `=`
  as an assignment, so a program path holding one runs through `/bin/sh -c 'exec "$0" "$@"'`.
- Windows has no `env`. There Studio starts its own executable as
  `omnigraph-studio launch-agent NAME… -- PROGRAM ARGS…` (`launch.rs`), as it does for
  `mcp-proxy`, with no names to remove as well. The launcher removes the names, starts the
  program as the ACP crate would (with no window), and exits with its code. Its arguments travel
  on the command line the standard library writes and reads back, so a path or argument arrives
  as given. Studio stops an agent by ending the process it started, which is the launcher, so
  the launcher puts itself in a job that ends every process in it when the launcher ends; the
  agent and what it starts join that job. On Windows the ACP crate ends only the process it
  started (its process group is Unix's), which for `npx` is `cmd`, not the agent under it, so
  every agent goes through the job. A program named without a folder is found as a shell finds
  it, on PATH in its order (by PATHEXT when it has no extension), and never in the working
  directory: the standard library looks only for `NAME.exe`, and npm installs `npx` as
  `npx.cmd`. An empty or relative PATH entry (`a;;b`, a trailing `;`, `bin`) names a folder
  under the working directory and is passed over; the standard library's own search reads such
  entries too, so a name on no absolute entry does not start ("could not find … on PATH").
  Windows reads a variable by its name in any case, so there names are matched in any ASCII
  case.
- Studio's files in the agent's working directory are written by handle on both
  (`agent_panel::confined`): Unix opens each name relative to its folder's descriptor; Windows
  holds each folder open without sharing deletion, so it cannot be renamed, removed or swapped
  for a junction while Studio works in it, and opens each part with
  `FILE_FLAG_OPEN_REPARSE_POINT`, so a link or a junction is refused, never followed. A link to
  a folder, a junction included, is a directory to Windows, so Studio removes a stale one as a
  directory, which removes the link itself and never its target.

### Choosing the CLI

1. `agent.omnigraph_cli` (Settings → Agent → OmniGraph CLI), then each `omnigraph` on PATH, then
   `~/.local/bin`, `/opt/homebrew/bin` and `/usr/local/bin`, without duplicates.
2. Each says which HTTP contract it speaks and whether it has targets, without a server,
   bounded to 5 s: `omnigraph version --json` reports `http_contract` (OmniGraph #987, E23) and
   `protocols.target` (#1090). An older release, or one that does not answer, fits nothing; its
   `--version` only names it in the notice. The first that speaks Studio's contract and target
   protocol 1 goes in the wrapper (edge `edge-20261020-7fd9cc9` is the first), so a slow or
   unavailable server never decides which CLI a conversation gets.
3. If none fits, the wrapper runs nothing and prints why, so `omnigraph_run` and the agent's
   shell refuse rather than run a CLI the server would refuse. A notice in the thread names the
   contract and each CLI found with its release and what it speaks, or that it has no CLI
   targets, and the `omnigraph_cli`
   tool says the CLI is unavailable. The agent keeps the graph tools. A configured
   `agent.omnigraph_cli` that does not fit is passed over with a notice naming the one used.

### Gateway rules

The gateway answers `GET`/`HEAD /healthz` itself with one contract header, before the token
check, as the CLI probes with no token. Every other request needs the session token (else 401)
and exactly one `omnigraph-http-api: 0.14` (else 400 `api_contract_mismatch`, naming the CLI to
use). Of the request's headers only `accept`, `content-type`, `if-match`, `if-none-match`,
`omnigraph-if-actor`, `omnigraph-if-graph-commit` and `range` are passed on; the gateway sets the
token and contract itself. `omnigraph-if-actor` is the CLI's `--as`, which can only narrow a
request: the server refuses it unless it names the person's actor. Request bodies are read
whole, at most 64 MiB (413 above, before anything is sent). A
target holding a `#`, or a query parameter given twice, is refused (400) before the rules
read it: forwarding drops a `#` and all that follows it, so the rules would judge a query the
server never sees, and a repeated key could be judged by one value and read as another.

| Request | Rule |
|---|---|
| `GET`/`HEAD` anything; `POST /query`, `/export`, `/changes/baseline` | Forward, any graph, branch or snapshot; the answer streams back |
| `POST /mutate`, `/mutate/if-graph-commit` | Only when `branch` is the agent branch. `branch: null` (branch and commit statements) is refused, and so is a declaration written `as commit` or `as unattached branch`, read from its head with `gq_language`'s lexer: it moves no branch |
| `POST /load/ndjson`, `/load` | Only when `branch` is the agent branch; `from`, when given, must be the base (it creates the branch when the branch is missing; the answer's `branch_created` says whether it did) |
| `PUT`/`DELETE /blob` | Only when `branch` is the agent branch |
| `POST /branches` | Only `{name: <agent branch>, from: <base>}`; one with `from_commit` is refused, since a branch started at a commit holds what that commit held |
| `DELETE /branches/{name}` | Only the agent branch |
| `POST /branches/merge` | Only into the agent branch, and never with `delete_branch: true` or `as_commit`. A `dry_run` is a read, on any graph, as the other reads are |
| `POST /queries/{name}` | Sent with `expect_mutation: false` (`omnigraph query <name>`), a read anywhere, the agent's branch and other graphs included: the server refuses it if it writes (since 2026-10-21). Otherwise, on the agent branch, a write, whose answer says whether it wrote: a read's rows (`ReadOutput`) are "Read; nothing written", never a commit; elsewhere forwarded with `expect_mutation: false`. Its text is not in the request and the catalog (`GET /queries`) says only whether it writes, so one held as an unpublished commit is known from its answer alone |
| `POST /commits/{id}/publish`, `DELETE /commits/{id}`, non-`GET` `/cluster/…`, anything else | Refused |

Writes are checked on the conversation's graph only. A refusal is a 403 `ErrorOutput` with
`code: forbidden` and the contract header; its message ends "Nothing was sent." and says what to
do instead. A CLI with E22 (OmniGraph #981) reports it as not started; an older one still says
`effects: unknown`, so `AGENTS.md` says a Studio refusal never wrote anything.

A forwarded write's answer is read whole (at most 8 MiB) so the card can show it, then sent on.
When the server's answer is lost, the gateway closes the connection without one, and the CLI
reports the outcome as unknown, as it is.

### Cards and approval

Each write is a card in the thread, like a proposal: the summary ("Mutation", "Load (append)
into …", "Create … from main"), the query when there is one, the branch, and the outcome:
Committed with the commit and the change count, Done or "Branch created" for a write without a
commit, Not on the branch for one the server held as a commit no branch shows (`kind`
`unpublished` or `unattached`), Refused with the server's message, or Outcome unknown, which a
success whose body cannot be read is too, as Studio's own writes judge it. A success is a write's
receipt only in the write's own shape (a `commit` key, null or the commit; a branch's `name` for
a create or delete); a commit there of a kind this build does not know is Outcome unknown,
"whether the branch moved is not known", and is logged so (since 2026-10-21; before, a kind not
said held was taken as committed). A write already sent when the agent stops keeps running on its
card until its outcome comes, through what records it, which the gateway calls however the write
ends; the agent's branch then learns whether it exists. A load given `from`
is tagged new only when its answer says it created the branch. A committed write is logged in the
session (the workspace's "Agent CLI: … committed" notice) and the agent branch banner counts it.
A write the server held as a proposal or a snapshot is logged as held, with its commit, by the
commit's kind, as Studio's own writes are (since 2026-10-21; before, as committed).

- **Sandbox** (default): writes to the agent branch go through at once.
- **Ask before writes** (Settings → Agent, `agent.ask_before_writes`): each write waits on its
  card with Allow (⌘Y), Always on this branch (⌘⌥Y) and Reject (⌘⌥N). The CLI waits, since it has
  no timeout. Reject answers 403 and nothing is sent. The keys answer the write card waiting
  longest, which alone shows them, never an agent's own permission request (spec 011).

A write still waiting is given up unsent, and its card says so, once nobody waits for it: when
the CLI closes its connection (its command timed out or was stopped), since the person could
otherwise allow a write the agent was told had failed. On Unix the wrapper `exec`s the CLI, so
stopping the command by its process id stops the CLI. On Windows the CLI runs under the batch
file's `cmd`, and an agent that stops the command by that process id, not by its process tree,
ends `cmd` and leaves the CLI waiting with its connection open. The agent's job does not cover
this: it ends what the agent started only when the launcher, and with it the agent, ends. So on
Windows the gateway, when a write arrives, finds the process holding the connection's other end
(`GetExtendedTcpTable`) and holds open each process it was started under, from its parent up to
Studio (a parent that is already gone, or whose number a later process took, ends the walk; a
CLI Studio started itself has none). When one of them ends while the write waits, the write is
given up as if the CLI had gone, and the CLI, still listening, hears a 403 that nothing was
sent (`requester`).

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

- Studio adds `--target studio` and `--graph <graph>` unless the arguments name a graph, and
  refuses `--target`, even naming `studio`, since Studio adds it, and `--as`, so a call cannot
  leave the gateway. The flags #1090 removed (`--server`, `--store`, `--profile` and the rest)
  are refused by the CLI itself. `--config` passes: on a CLI with targets it names source files, for `cluster`
  commands only, which `omnigraph_run` does not run.
- Commands: `query`, `mutate`, `load`, `branch`, `snapshot`, `export`, `blob`, `commit`,
  `changes`, `schema show`, `graphs list`, `version` and `lint`. Anything else is refused before
  running. `lint` checks queries against a schema file offline (`--query`, `--schema`, `--json`
  only, no target added): `lint` takes no target, and a positional storage URI
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
too, and opens all network, so it stays the person's choice. E24 has landed (a `unix:///…`
server, OmniGraph #988), so a sandbox can allow one socket instead. To adopt: the gateway also
listens on a Unix socket in the conversation's folder, and a target-capable CLI's home points at
it, so Codex reaches the gateway without network.

### The skill

Studio writes an `omnigraph-studio` skill into the graph's agent working directory at every
conversation start, beside `AGENTS.md`: `.claude/skills/omnigraph-studio/` for Claude Code and
`.agents/skills/omnigraph-studio/` for Codex, since neither agent reads the other's folder.

- Its `SKILL.md` is Studio's own (`crates/agent_panel/skill/SKILL.md`): what differs in Studio
  (the CLI path and flags, the agent's branch, no schema changes, unknown outcomes, offline
  lint) and which parts of the guide to read or skip.
- The guide is the OmniGraph skill of the build whose CLI Studio requires, copied
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
  subcommand ("Query", "Mutate"), and other calls as tool rows ("Run omnigraph"). A write whose
  card shows its text is the title line alone ("Mutate retitle"), so the query shows once, on
  the card with its branch and outcome (since 2026-10-21).

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
| On Windows the agent's wrapper is a batch file, `omnigraph.cmd`, written from the same account as the shell script | A native launcher in each home; a PowerShell script | A launcher means a second executable to build and ship, or Studio's own: that opens its window when run as `omnigraph`, is large to copy into each home, and a hard link to it would let the agent write Studio's executable. PowerShell's default execution policy on Windows 10 and 11 refuses scripts, and Git Bash cannot run one. A batch file's cost is cmd's reading of the caller's line, which no batch file can undo; the agent is told, and `omnigraph_run` takes a list |
| Request bodies read whole, at most 64 MiB; answers to reads stream | Stream request bodies too | The rules need small JSON bodies whole; a load larger than 64 MiB is refused before sending, which bounds memory (IX) |
| The gateway passes the upstream contract through and refuses a CLI that speaks another | Translate between contracts | The contract is an exact-match safety property. A translation layer would be a second API that drifts |
| Studio tests each CLI once against the gateway | Map package versions to contracts | The package version does not determine the contract; a real request does, until E23. Superseded by `version --json` (see "Choosing the CLI") |
| The agent's CLI must have targets; Studio writes only the target home and adds `--target studio` | Write a `servers` home for a CLI before targets, chosen by `protocols.target` | OmniGraph main refuses a `servers` file (#1090). Two forms meant two homes, two flag sets, and instructions written before the choice that could name neither; Studio supports one CLI line, 0.14 with targets |
| The agent keeps Studio's gateway, not the server's MCP | Point the agent at the server's MCP, which has read and opt-in write tools since #994 | Its writes would go wherever the agent's token may write; until E20 only the gateway keeps them on the agent's branch, and only the gateway shows each write as a card |
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
  403 or a contract 400 on a write as not started. Landed (#981); `AGENTS.md` keeps its note
  for older CLIs.
- **E23** ([#977](https://github.com/ModernRelay/omnigraph/issues/977)): `omnigraph version --json`
  with the HTTP contract. Landed (#987) and used to choose the CLI.
- **E24** ([#979](https://github.com/ModernRelay/omnigraph/issues/979)): Unix-socket transport,
  so a sandbox can allow one socket. Landed (#988), not used yet (see the Codex note above).
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
| 2026-10-20 | The agent's CLI home takes the chosen CLI's form | `agent_panel` `the_cli_that_speaks_the_contract_is_chosen_without_a_server`, with a stand-in CLI reporting `protocols.target` | Both homes' `config.yaml` become the target form (`target_config_version: 1`, `studio` a server target with `credential: studio` and the graph, `defaults.target: studio`, no `servers`), and back to `servers` when a 0.14 CLI is chosen; Studio's own reader of the CLI's files lists `studio` with the session token, the saved endpoint covering the target's |
| 2026-10-20 | `omnigraph_run` names the gateway as the chosen CLI does | `agent_panel` `run_arguments_stay_on_the_gateway_and_in_the_working_directory` | `--target studio` for a CLI with targets, `--server studio` otherwise, with the graph; `--target` (another address, or `studio`), `--server` and `--organization` from the agent are refused |
| 2026-10-20 | The 0.14 CLI works with no destination flags and an `endpoint` in its credentials | `agent_panel` `the_stock_cli_reads_and_writes_its_branch_through_the_gateway` (ignored by default) with `omnigraph` 0.14.0-edge+0a62bb7 against `script/dev-server` on `127.0.0.1:8791` | Passed: `omnigraph_run` reads, a saved read, a read on main, the refused writes to main and branch statements, branch creation and writes on the agent's branch, all through the home's defaults. A CLI with targets was not run against the gateway: no edge build with them is installed here (`edge-20261020-b692610` is the first) |
| 2026-10-20 | A CLI without targets is not chosen | `agent_panel` `the_cli_that_speaks_the_contract_is_chosen_without_a_server` | A 0.14 CLI that speaks the contract but reports no `protocols.target` is passed over for the one that does; with none, the notice says "0.14.0, which has no CLI targets"; both homes are the target form |
| 2026-10-20 | The CLI with targets works through the gateway with the home's defaults | `agent_panel` `the_stock_cli_reads_and_writes_its_branch_through_the_gateway` (ignored by default) with `omnigraph` 0.14.0-edge+b692610 against `script/dev-server` on `127.0.0.1:8792`; by hand, that CLI with a home as Studio writes it | Passed (0.95 s): `omnigraph_run` reads, whole and saved to a file, a read on main, writes to main and branch statements refused, the branch created and written, all with no destination flags. By hand: `query` with no flags resolved "target studio (default) … graph=knowledge" and read 200 people; the same home in the `servers` form was refused (`target_config_invalid`), and a token saved without `endpoint` (`credential_scope_missing`) |
| 2026-10-20 | The Windows wrapper is written from the shell script's account, and names only what cmd reads as written | `agent_panel` `both_wrappers_are_written_from_one_account`, `the_batch_file_names_hostile_paths_and_refuses_what_it_cannot_name` (pure, run on macOS) | The shell script byte for byte as before; the batch file in CRLF and ASCII only, with the same names removed; `%` doubled inside quotes and `&`, `^`, `(`, `)`, `!`, `'`, `;`, `=` left plain there; a CLI under a profile named `Jürgen` (in another case too) named from `%~dp0`; one on another drive or under another non-ASCII folder refused, pointing at `omnigraph_run`; a refusal's `"`, `%`, `&|<>^()` and `ü` escaped or replaced |
| 2026-10-20 | On Windows the batch file passes every argument as given, Studio's folders cannot be swapped, and an agent named `npx` starts | `the_batch_file_runs_the_cli_with_every_argument_as_given`, `a_held_folder_cannot_be_swapped_and_a_junction_is_not_followed` (Windows only); `a_bare_program_name_is_found_by_its_extensions_in_path_order` (run on macOS); `cargo clippy --target x86_64-pc-windows-msvc --all-targets -- -D warnings` on macOS over a scratch crate that includes the real `cli_home`, `confined`, `host`, `launch`, `mcp`, `socket` and `tools` files | Clippy clean once an unused `mut` in `cli_home::Stage::new`, which fails Windows's `-D warnings`, was fixed. `cargo check --workspace --target x86_64-pc-windows-msvc` cannot run on macOS: `zstd-sys`'s build script needs an MSVC C compiler. The Windows-only tests have not run yet; CI runs them on `windows-2025` |
| 2026-10-20 | The gateway refuses a mutation held off the branch, reads a merge's dry run anywhere, and reads a write's answer as Studio does | `gateway_rules` `mutations_need_the_agent_branch_in_their_branch_field`, `branches_merges_and_blobs_stay_on_the_agent_branch`, `loads_name_their_branch_and_base_in_the_query_string`; `gateway` `a_writes_success_answer_says_what_it_did` | `as commit` and `as unattached branch [on …]` refused, after an annotation, with a comment between the words and after a settings line; the words in a string, a comment or a declaration not run let through; a dry run on another graph read, with `as_commit` refused; a load's `branch_created` read from its answer; `kind` `unpublished` or `unattached`, or `published: false`, read as not on the branch; an empty or unreadable success body unknown |
| 2026-10-20 | The launcher never finds a program through an empty or relative PATH entry | `agent_panel` `a_relative_path_entry_is_never_searched` (new), `a_bare_program_name_is_found_by_its_extensions_in_path_order` (run on macOS; the search is the same code on every platform) | Before the fix the new test failed: with PATH `;bin;<abs>\npm` it returned the relative `npx.cmd`, which resolves against the working directory. After it only absolute entries are looked in; `npx.cmd` and `agent.js` are found there as named; a name on no absolute entry is `None`, which the launcher reports as not on PATH |
| 2026-10-20 | A CLI the batch file cannot name is not offered to the agent's shell, and a refusal keeps its `!` | `agent_panel` `a_cli_the_batch_file_cannot_name_is_not_offered_to_the_shell`, `network_folders_are_told_apart`, `the_batch_file_names_hostile_paths_and_refuses_what_it_cannot_name`, `a_conversation_home_holds_its_config_token_and_wrapper` (run on macOS); `a_refusal_keeps_its_text_with_delayed_expansion_on` (Windows only, `cmd /V:ON`; not run here) | Before the fix the refusal had no `setlocal` line (the exact-text test failed), and `use_cli` returned `Ok(())` for a refusing wrapper, so `choose_cli` reported it chosen and ready (read in the code). After it `use_cli` returns whether the wrapper runs the CLI, the instructions say the shell cannot run it and point at `omnigraph_run` (which still has the CLI) without the wrapper's path, a network working directory adds its note, and every variant stays within 2,048 characters |
| 2026-10-20 | A junction in the skill folder is removed as a link, and later skill writes go on | `agent_panel` `a_link_left_in_the_skill_folder_is_removed_and_not_followed` (a symbolic link on macOS, run here; a junction on Windows, not run here), `a_held_folder_cannot_be_swapped_and_a_junction_is_not_followed` (Windows only, now removing its junctions through `Confined::remove`) | On macOS the link is removed and its target kept. On Windows `std::fs::remove_file` fails on a junction (`DeleteFileW`, then a reopen without backup semantics, in the 1.98 standard library's `sys/fs/windows.rs` `unlink`), which failed every later skill write; `remove` now takes a link to a folder away with `remove_dir` (`RemoveDirectoryW` removes the junction, not its target) |
| 2026-10-20 | On Windows a write whose command was stopped by the wrapper's process id is not sent | `agent_panel` `the_client_is_the_owner_of_the_connections_client_end` (the TCP table read as Windows writes it; run on macOS); `a_command_stopped_by_its_id_is_seen_though_the_cli_waits_on` and `gateway` `a_write_whose_command_was_stopped_is_not_sent` (Windows only: this test binary as the CLI under the real `omnigraph.cmd`, the wrapper killed by its id; not run here) | The launcher's job ends the CLI only when the agent ends, so it does not cover this. Expected on Windows: the CLI keeps its connection after `cmd` ends, the held `cmd` is seen to end within a poll, and the gateway sends `Abandoned` and nothing upstream |
| 2026-10-20 | The Windows code builds | `cargo clippy --target x86_64-pc-windows-msvc --all-targets -- -A dead_code -D warnings` on macOS over a scratch crate that includes the real `cli_home`, `confined`, `host`, `launch`, `mcp`, `requester`, `skill`, `socket` and `tools` files (`dead_code` allowed only because the panel and gateway that use them are not in it) | Clean, once a Windows-only test's missing `mut` and a `chunks_exact` lint were fixed. `gateway.rs` and `sessions.rs` need `graph_client` and `gpui` and were checked by reading; CI builds them on `windows-2025` |
| 2026-10-21 | A stored read on the agent's branch is a read, and only a write's receipt is a commit | `gateway_rules` `a_stored_read_is_a_read_anywhere_and_commits_are_never_published`; `gateway` `a_writes_success_answer_says_what_it_did` (now per write kind), `the_gateway_checks_the_session_then_forwards_reads_and_approved_writes` (a stored read on the agent's branch streams back with no card); the receipts' shapes in `crates/graph_client/openapi/omnigraph-0.14.0-edge.json` (`ChangeOutput`, `IngestOutput`, `GraphBatchLoadOutput`, `BlobWriteOutput`, `BranchMergeOutput` carry `commit`; `BranchCreateOutput`, `BranchDeleteOutput` carry `name`; `ReadOutput` carries `rows` and the `graph_commit_id` it read at) | Before, `omnigraph query people --branch agent/x` (`expect_mutation: false`) was a write: Ask mode waited for Allow, and the read's `graph_commit_id` was taken for a commit, so the card said Committed, ✎ rose and the session logged "committed". Now it is forwarded as a read, and a write's answer is a commit only as its own receipt; a stored query sent without `expect_mutation` that reads is "Read; nothing written", logged as writing nothing |
| 2026-10-21 | A commit of a kind Studio does not know leaves the outcome unknown | `gateway` `a_writes_success_answer_says_what_it_did`; `agent_panel` `an_agents_held_write_is_logged_as_held` | Supersedes the earlier 2026-10-21 row's "an unknown kind not said to be held is on the branch": the gateway keeps the commit's kind (`Seen::Written`), so every kind goes through `OutcomeKind::written` as Studio's own writes do, and the card says Outcome unknown with the commit. A receipt naming a commit without its id is unknown too |
| 2026-10-21 | `initialize` says what Studio chose, not what it guessed at launch | `socket` `initialize_answers_with_what_studio_chose_when_it_asks` (instructions sent before and after the agent asked, none within the wait, and a conversation that ended); `mcp` `initialize_echoes_a_known_revision_and_offers_the_newest_otherwise` | Before, the instructions were made at launch as `instructions(None)`, before the CLI was chosen, so with no CLI found, or on Windows with a CLI at a path the batch file cannot hold, the agent was told the wrapper was its CLI. Now they are made when `initialize` arrives, from the choice (`instructions(Some(chosen))`), and while Studio is still choosing past 5 s they say so |
| 2026-10-21 | An approval the gateway never took says nothing was sent | `gateway` `an_approval_the_gateway_never_took_records_that_nothing_was_sent` | An approval handed to a gateway that stopped before reading it was dropped with it, and recorded as unknown. The gateway's `Decision` now records "nothing was sent" for it, and for one taken as the CLI gave up, once |
| 2026-10-21 | The agent's launch removes OmniGraph's variables through one shared match | `agent_panel` `scrub::every_name_with_omnigraphs_prefix_is_omnigraphs`, `scrub::what_is_removed_is_every_omnigraph_variable_but_those_kept`, `the_agent_starts_without_omnigraph_credentials` | `OMNIGRAPH_` matched whole at the start of the name, in any ASCII case only on Windows (`OMNIGRAPHX`, `XOMNIGRAPH_TOKEN` and `OMNIGRAPHé_` are not); `omnigraph_run` (`cli_home`) still reads it through `host::is_omnigraph_variable`, a re-export of `scrub`'s |

## Corrections

- 2026-10-21 · A stored read on the agent's branch (`omnigraph query <name> --branch agent/…`,
  `expect_mutation: false`) was taken for a write, and its answer's `graph_commit_id`, the commit
  it read at, for a commit it made: the card said Committed and the session logged it. It is now a
  read, and only a write's receipt names a commit. A commit of a kind Studio does not know, taken
  as committed unless said held, now leaves the outcome unknown.
- 2026-10-21 · The MCP instructions were made at launch, before the CLI was chosen, and told the
  agent the wrapper was its CLI when none fit or its shell could not run the one chosen. They are
  now made when the agent asks, from the choice.
- 2026-10-20 · The launcher's search for a program read every PATH entry, and an empty or
  relative one (`a;;b`, a trailing `;`) resolved against its working directory, which the doc
  said was never searched. It now reads only absolute entries, and a name found on none does
  not start.
- 2026-10-20 · A stale junction in the skill folder could not be removed (`remove_file` refuses
  a directory link on Windows), so every later skill write failed. It is now removed as a
  directory, which removes the link and not its target.
- 2026-10-20 · When the batch file could not name the CLI, the wrapper refused but the agent was
  told its CLI was ready, and the refusal left delayed expansion as the registry set it. The
  agent is now told its shell cannot run the CLI and to use `omnigraph_run`, and the refusal
  turns delayed expansion off.
- 2026-10-20 · A command stopped by the wrapper's process id left the CLI under `cmd` waiting, so
  the person could still allow its write after the agent was told it failed; the gateway relied
  on the CLI's connection closing. It now also gives the write up when a process the CLI was
  started under ends.
- 2026-10-20 · The agent's working directory on Windows was under `%APPDATA%`, which folder
  redirection can make a network path cmd cannot work in, so relative file arguments through
  `omnigraph.cmd` were read from the Windows folder. It is now under `%LOCALAPPDATA%`.

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
