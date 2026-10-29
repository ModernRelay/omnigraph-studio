# 013: The agent uses the Omnigraph CLI

Status: implemented; the CLI is the only graph tool · 2026-10-13. The CLI home is Omnigraph's
version-1 operator configuration over loopback TLS since 2026-10-21. The agent's CLI is found as
Studio's own is, by `graph_store::cli`, since 2026-10-22, and is Omnigraph `v0.14.0` or later,
the first release to report protocol 2, since 2026-10-24 (see "Choosing the CLI").

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

Omnigraph at `d8d2e3a8` (main), and the released 0.13.0 binaries at `~/.local/bin`. The first
three items held there and are gone; the items for #1090 and #1115 below say what replaced them:

- `--server` took a literal URL with no config (`crates/omnigraph-cli/src/helpers.rs:321-325`).
  `OMNIGRAPH_HOME` moved both `config.yaml` and `credentials` (`operator.rs:272-281, 355-357`);
  it still names the operator home, which holds no `credentials` file since #1115.
- `defaults.server` plus `defaults.default_graph` made commands run with no flags
  (`scope.rs:128-141`). An `OMNIGRAPH_PROFILE`, a `defaults.store`, or a `./.omnigraph/context`
  in the working directory changed or refused that. Only explicit flags or `--direct` bypassed a
  context file (`managed/data.rs:320-375`). `defaults.target` does this since #1090.
- The token was chosen by the request URL's longest prefix match against `servers:` entries, not
  by name (`operator.rs:174-192`). The order was `OMNIGRAPH_TOKEN_<NAME>`, then `credentials`
  `[<name>] token =`, then `OMNIGRAPH_BEARER_TOKEN` (`helpers.rs:286-306`). A `credentials`
  file with any group or other permission bit was a hard error (`operator.rs:516-531`). Since
  #1126 a server target's one `auth` provider names its token.
- Every data request is preceded by `HEAD {root}/healthz`, sent with no bearer and no contract
  header, bounded to 5 s. The response must be 2xx with exactly one `omnigraph-http-api` naming
  the CLI's own contract (`0.13` there; `0.14` for the CLI Studio drives now).
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
- Since Omnigraph main `2c9eb1e0` (#1090, 2026-10-20), after 0.14's edge build: `--server`,
  `--store`, `--cluster`, `--profile`, `--managed`, `--direct`, `--api`, `--organization`, graph
  `--uri` and `use` are removed. `--target NAME|ADDRESS`, or `defaults.target`, selects one
  backend, and the resolved target is printed to stderr. The operator file was then
  `target_config_version: 1` with `targets`, a server target with an `endpoint`, a
  `default_graph` and one `credential` owner, whose token was `OMNIGRAPH_TOKEN_<OWNER>` or what
  `omnigraph login --target` stored in `credentials`, bound to the endpoint, until #1115 (next
  item). No command reads `OMNIGRAPH_BEARER_TOKEN` (#1030). `version --json` reports
  `protocols.target`. A `unix:///…` endpoint is a server (E24, #988), and a refused write is
  reported as not started (E22, #981).
- Since Omnigraph main `f7db38da` (#1115) and `ff542202` (#1126), both 2026-10-21, checked at
  main `97788d37`: the operator file is `version: 1` (`operator_schema.rs`). A document holding
  `target_config_version` is refused ("legacy target_config_version is unsupported"), and the
  keyed `credentials` file and `OMNIGRAPH_TOKEN_*` are gone. Unknown fields, nulls outside alias
  parameters, YAML tags, anchors, aliases and merge keys are refused (`operator_schema/yaml.rs`).
  A server target is `{kind: server, endpoint, auth, transport?, default_graph?}` with exactly
  one `auth` provider; `defaults.target` names the target used without `--target`
  (`scope.rs`), and `default_graph` the graph without `--graph`. The endpoint must be HTTPS
  unless `auth` is `{provider: anonymous}` and the host is loopback (`validation.rs` `endpoint`);
  the CLI adds a trailing `/`. `auth: {provider: token, source: {file: F}}` reads `F` once per
  invocation: a regular file of at most 64 KiB, one trailing LF or CRLF dropped, the rest a bearer
  token of `A-Za-z0-9-._~+/` and trailing `=` (`credential_sources.rs` `acquire_file`,
  `BearerSecret`); its permissions are not checked. A relative path resolves beside the operator
  file. `transport.tls.ca: {mode, file}` is a PEM bundle of at most 1 MiB holding at least one
  certificate; `replace` turns off reqwest's built-in and native roots (`operator_transport.rs`;
  reqwest 0.12.28 over rustls), whose WebPKI check matches an IP endpoint against IP names only,
  as Omnigraph's own TLS fixture does with `127.0.0.1` (`tests/support/tls_gateway.rs`).
  `transport.proxy` defaults to `{mode: direct}`, which ignores the proxy variables.
  `config validate [--operator-config F]` is offline (`planes.rs`: a local command, no target
  resolved): it parses and validates the whole file and reads no token, CA or helper. A command
  that uses a token keeps `auth-state/<sha256>.json` and a lock under `OMNIGRAPH_HOME`, whose
  folder must be the user's own and closed to others (`operator_routing.rs` `lock_state`).
  `version --json` still reports `protocols.target: 1` (filed as #1127, to report 2).
  `0.14.0-edge+50283ce`, the build installed when this was first checked, is from before #1115:
  `config validate` is "unrecognized subcommand 'config'" (run here). `edge-20261021-888223b`
  (published 2026-10-22 00:21 UTC) has #1115 but not #1126: its server target takes `auth` as a
  map of named entries with `default_auth`, so it refuses the single `auth` Studio writes (read
  in the diff `888223b7..ff542202` of `operator_schema.rs`). `edge-20261022-7fff9df`
  (`0.14.0-edge+7fff9df`, main `7fff9dfd`, with `ff542202`) reads it, and is the CLI installed
  at `~/.local/bin` now (checked 2026-10-22: `version --json` reports contract 0.14 and
  protocols target, credential and access 1). The `v0.14.0` release (2026-10-22, commit
  `60bfd19d`) reports all three as 2 (#1133, #1135; its `version --json` is
  `{"version":"0.14.0","channel":"stable","http_contract":"0.14","protocols":{"target":2,"credential":2,"access":2},…}`).
- Policy branch scopes are `any`, `protected`, `unprotected`
  (`crates/omnigraph-policy/src/lib.rs:213-217`). A commit records the actor resolved from the
  bearer token (`handlers.rs:415-437`).
- In 0.13.0, `omnigraph --version` and `omnigraph version` report the package and storage
  versions, never the HTTP contract, and have no JSON form (`main.rs:152-158, 612-620`;
  `cli.rs:428`). `version --json`, with the contract and the protocols, came with #987 (E23).
- On this Mac, when first checked, `~/.local/bin/omnigraph` was 0.13.0 (contract 0.13) and
  `/opt/homebrew/bin/omnigraph` 0.11.0, which sends no contract header, and the personal server
  and the dev server answered `omnigraph-http-api: 0.13`. On 2026-10-22 `~/.local/bin` holds
  `0.14.0-edge+7fff9df` and `/opt/homebrew/bin` 0.13.0.

Studio at `3df9307`:

- `graph_client` has typed methods only. It reads every body whole and has no load, export,
  `changes/baseline` or stored-query routes (`crates/graph_client/src/client.rs:250-301`).
- The agent process inherits Studio's whole environment. `agent.env` can add or override
  variables but cannot remove any, and the ACP crate's `spawn_process` has no `env_clear`
  (`crates/agent_panel/src/host.rs:228-230`; `agent-client-protocol-2.2.0/src/acp_agent.rs:265-291`).
- The MCP proxy was Studio's own executable re-run as `omnigraph-studio mcp-proxy <socket>`,
  once per conversation, until 2026-10-28. The debug executable is 490 MB.
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
- codex-acp (2.3.0) ignores the sandbox in Codex's `config.toml`: every session, new, loaded or
  resumed, starts in its `agent` mode unless `INITIAL_AGENT_MODE` names another
  (`AgentMode.getInitialAgentMode`), and each turn sends that mode's sandbox policy, which
  overrides Codex's permission profiles. Its modes are `read-only`, `workspace-write` and `agent`
  (both without network) and `agent-full-access`.
- Both read `AGENTS.md` in the working directory. Claude reads it when there is no `CLAUDE.md`.

## Design

### Pieces

| Piece | Where | Role |
|---|---|---|
| Forwarding | `GraphClient::forward` (`crates/graph_client/src/client.rs`) | Sends a request on as it came, with the person's token and the contract header. Bodies stream both ways, no total timeout, never retried, and a path that would leave the server root is refused. The one client crate keeps every request (I) |
| Rules | `crates/agent_panel/src/gateway_rules.rs` | `classify(request, sandbox) -> Verdict`: read, read with a rewritten body, a write for the agent's branch, or a refusal. The health probe never reaches them: the gateway answers it |
| Gateway | `crates/agent_panel/src/gateway.rs`, `gateway_tls.rs`, `requester.rs` | A loopback HTTPS (HTTP/1.1 over TLS) server per conversation (`127.0.0.1:0`, one request per connection), with a certificate it makes when it starts. Checks the session token and contract header, applies the rules, forwards, and reports writes to the panel. On Windows it holds the processes a write's CLI was started under while the write waits |
| CLI finder | `graph_store::cli` (`crates/graph_store/src/cli_tokens.rs`) | Shared with Studio's own servers (spec 003): where Studio looks for the CLI (`cli::candidates`, the `omnigraph_cli` setting first), the probe and what it remembers (`cli::probe`, `cli_probes.json`), Studio's one turn at the CLI (`cli::take_turn`, `cli::run`), and the Omnigraph variables no process Studio starts inherits (`cli::omnigraph_variables`) |
| CLI home | `crates/agent_panel/src/cli_home.rs` | In the conversation's folder: the wrapper, the config, token and certificate, and `AGENTS.md`; choosing, of the CLIs the finder takes, the first that reads the home |
| Panel | `crates/agent_panel/src/panel.rs` | Starts the gateway and home with the conversation, shows each write as a card, and records outcomes in the session's write log (`GraphSession::note_write`) |
| Agent launch | `crates/agent_panel/src/host.rs`, `launch.rs`; `omnigraph-studio launch-agent` in `crates/studio/src/main.rs` | Starts the agent without Omnigraph credentials in its environment: through `env -u` on Unix, always through Studio's own launcher on Windows |

### One folder per conversation

A graph's conversations share one agent process, so nothing per conversation can travel in the
agent's environment. Each conversation works in a folder of its own instead (since 2026-10-28),
`<agent dir>/graphs/<graph>/<agent branch with / as ->-<8 random hex>/`
(`sessions::working_dir`; `<agent dir>` is `<data dir>/agent`, on Windows under
`%LOCALAPPDATA%`, below), which ACP gives the session as its `cwd`. Everything the conversation's
agent needs is there at the same names in every conversation, so `AGENTS.md` is all it reads:

- `omnigraph` (`omnigraph.cmd` on Windows, `0700`), the wrapper the agent runs as `./omnigraph`;
- `.studio/` (`0700`), the CLI's home: `config.yaml`, `token` and `ca.pem`;
- `AGENTS.md`: the graph, the agent's branch and base, the command and the rules;
- the `omnigraph-studio` skill (below).

Ended, the conversation removes the wrapper, the token, the certificate and the config, and
`.studio` once empty. A conversation saved before 2026-10-28 keeps the folder it ran in, the
graph's shared directory, since the agent finds its session by it; two such conversations open
at once there write over each other's `AGENTS.md` and wrapper, which is accepted for the few
that exist.

```yaml
# config.yaml (0600): Omnigraph's version-1 operator configuration
version: 1
targets:
  studio:
    kind: server
    endpoint: "https://127.0.0.1:<gateway port>"
    auth:
      provider: token
      source:
        file: "<this folder>/token"
    transport:
      tls:
        ca:
          mode: replace
          file: "<this folder>/ca.pem"
      proxy:
        mode: direct
    default_graph: "<graph id>"
defaults:
  target: studio
```

- `token` (`0600`): the gateway's session token, 32 random bytes as hex, with no line end. The
  CLI reads it at each command, so the token never passes through a process or an environment.
- `ca.pem` (`0600`): the certificate the gateway serves, the only one the CLI trusts for the
  target (`mode: replace`, so neither the system's roots nor any other certificate apply).
- Both files are removed when the conversation ends.
- `auth-state/` is the CLI's own: at its first command with the token it records the target's
  credential binding there (identifiers, no secret), in a folder it makes `0700` and refuses
  when anyone else could enter it.

Each path is the home's own, absolute. The config is written by hand in the plain YAML
the CLI reads strictly: no null, tag, anchor or key it does not know. The proxy is named
`direct` though that is the default, so a later default cannot route loopback through a proxy.
Studio writes only this form: a CLI that cannot read it is not chosen (below). On Windows the
files are private as the home's other files are: the folder is under the person's profile
(`%LOCALAPPDATA%`, below), and Windows has no mode bits.

`omnigraph` (`0700`) is a wrapper: it unsets every `OMNIGRAPH_*` variable, the prefix
(`graph_store::cli::VARIABLE_PREFIX`) matched in any ASCII case, then sets `OMNIGRAPH_HOME` to
`.studio`, puts `127.0.0.1,localhost` in `NO_PROXY`, and runs the chosen CLI. The agent runs it
by its path in the folder, so neither an environment filter nor a shell snapshot that reorders
PATH can change what the CLI reads. Until Studio has chosen the CLI it runs nothing and says
Studio is still choosing; when no CLI fits, it says why.

Known first-run cost on macOS: the wrapper is a newly written executable, so its first run
waits for Gatekeeper's scan of it before the shell starts it. `syspolicyd` logs
`GK performScan` for the new script (unsigned, `-67062`), asks XProtect's analysis service to
match it against the XProtect rules, and records its provenance; later runs are not scanned.
Measured with an unsigned probe that writes a two-line script and runs it twice: 0.17 s and
0.26 s for the first run, 6-12 ms for the second (2026-10-21, Apple M5 Pro, macOS 26.5.2).
While builds were running, the live test's first wrapper run took 7-23 s and a fresh script
from the same process as long, every later run about 20 ms; why the scan took that long then
was not isolated. The agent's first command of each conversation can pay it. No workaround for
now.

On Windows the wrapper is the batch file `omnigraph.cmd`, written from the same account of what
it does as the shell script (`cli_home::Wrapper`), so only the syntax differs. It removes the
same variables (`set OMNIGRAPH_` lists each of that prefix, in any case), sets
`OMNIGRAPH_HOME` to `.studio` in its own folder (`%~dp0.studio`), prefixes `NO_PROXY` (one variable with `no_proxy`
there) and runs the CLI with `%*`. Windows starts a batch file through `cmd /c`, which reads the
caller's command line first: it expands `%NAME%` and, outside double quotes, acts on `&`, `|`,
`<`, `>` and `^`. A caller that escapes for cmd, as Rust's standard library does, gets every
argument through as given; a shell that quotes only for programs (Git Bash, PowerShell) leaves
the text between two quotes inside one argument outside cmd's quotes. That holds for every
`.cmd`, npm's shims included, so on Windows `AGENTS.md` tells the agent to put such a query or
its parameters in a file. The batch file reads
nothing more itself: `%*` is expanded once, delayed expansion is off, each path it names is in
double quotes with `%` doubled, and its lines end with CRLF. cmd reads a batch file in the
console's code page, so the file is ASCII only: its folder is `%~dp0`; the CLI is named by its
path when that is ASCII, else from `%~dp0` when that way is (both under a profile whose name is
not); otherwise the wrapper runs nothing and says to set `omnigraph_cli` to a CLI at a plain
ASCII path, and the thread says so too. A refusal turns
delayed expansion off too, so a `!` in its text stays even where a person's registry turns it
on for every batch file.

cmd cannot work in a network folder: started in one, it says so and works in the Windows
folder instead, so every relative path a batch file passes on (`--query`, `--data`, `--out`)
would be read from there. Studio's data folder on Windows is the roaming one (`%APPDATA%`),
which folder redirection can put on a `\\server\share`, so the agent's working directories go
under the local application folder (`%LOCALAPPDATA%\Omnigraph Studio\agent\graphs\…`), which is
never redirected; the agent keeps its own sessions on the machine anyway. A `pushd` of the
working directory in the wrapper was rejected: cmd has lost the caller's folder before the batch
file runs, so it could only guess the working directory's root, and a drive letter it maps can
be left mapped when the command is stopped before its `popd`. Known limit: a working directory that is
still a network path (a profile on a share, or `OMNIGRAPH_STUDIO_DATA` set to one) keeps cmd's
behaviour, and `AGENTS.md` then tells the agent to give the CLI every file by its absolute path.

`AGENTS.md` names the command, the branch and its base, and says to give no `--target` or
`--graph`, since the config makes the gateway and the graph the default. Codex reads
`AGENTS.md`; Claude reads it when there is no `CLAUDE.md`. Until 2026-10-28 the graph's
conversations shared one directory, each with its home in `.studio/<branch>-<random>/`, and the
agent learned the wrapper's path from Studio's MCP server (its instructions and an
`omnigraph_cli` tool).

### The gateway's TLS

The CLI reaches the gateway over HTTPS on `127.0.0.1`, as the version-1 configuration requires of
a target with a token (HTTP is for anonymous loopback servers alone), and the session token still
travels with every request but the health probe, as before. `gateway_tls` makes the certificate
when the gateway is bound, once per gateway: a new ECDSA P-256 key in memory, never written; a
self-signed certificate for `IP:127.0.0.1` alone (the CLI's WebPKI check matches an IP endpoint
against IP names only, so a DNS name would not do), `serverAuth`, valid from the day before (UTC,
for a clock hours off) for a year. The PEM goes in the home's `ca.pem`; the key stays in the
rustls server configuration and dies with the gateway. A gateway started again (a restart, a new
conversation) makes a new key and certificate.

The stack is rustls, which Studio's HTTP client already builds, over smol through
`futures-rustls`, with ring as the provider (Studio's graph builds both ring and aws-lc-rs, so
none is the process default), and `rcgen` makes the certificate. The server offers only
`http/1.1` and no session resumption: each CLI command is a new process. One 10-second bound,
counted from the connection's opening, covers its handshake and then its request's line and
headers together, so a process that need not trust the certificate to finish a handshake holds a
connection no longer than that without the token. One that does not complete the handshake,
plain HTTP among them, is closed with no answer. The whole request, a body read only with the
token, must be in within 60 seconds of the opening. After the handshake the stream is split: the
request is read and the "CLI went away" watch runs on the read half while the answer goes out on
the write half, and the connection ends with TLS's close. An answer's end is still its framing (a
length or the last chunk), so a cut answer stays cut. The connection's TCP ends are taken before
the handshake, for `requester`.

### The agent's environment

The ACP crate can add variables to the agent's process but not remove them, and its
`spawn_process` cannot be replaced, so the agent starts through a command that removes them. It
removes every `OMNIGRAPH_*` variable Studio holds, unless `agent.env` sets it: the CLI's
tokens and addressing, and tokens kept for other servers (`OMNIGRAPH_MR_BEARER_TOKEN`) or a local
server's token list alike, the prefix matched in any ASCII case on every system. The prefix is
matched in one place, `graph_store::cli` (`omnigraph_variables`, since 2026-10-22; before,
`agent_panel::scrub`, which matched any case on Windows alone), which the agent's launch,
the wrapper and Studio's own CLI (spec 003) use; a variable Omnigraph adds later
is removed without a list to keep. Cloud variables (`AWS_*`, `AZURE_*`, Google's) stay: the agent
may need them for its model, as Claude Code on Bedrock does. This closes the gap where the
agent inherited the person's graph tokens. With none of these variables set, the agent starts as
it is.

- On Unix the command is `/usr/bin/env -u NAME … PROGRAM ARGS…`. `env` reads a word holding `=`
  as an assignment, so a program path holding one runs through `/bin/sh -c 'exec "$0" "$@"'`.
- Windows has no `env`. There Studio starts its own executable as
  `omnigraph-studio launch-agent NAME… -- PROGRAM ARGS…` (`launch.rs`), with no names to
  remove as well. The launcher removes the names, starts the
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
  Windows reads a variable by its name in any case, which is why names are matched in any ASCII
  case, there as on every other system.
- Studio's files in the agent's working directory are written by handle on both
  (`agent_panel::confined`): Unix opens each name relative to its folder's descriptor; Windows
  holds each folder open without sharing deletion, so it cannot be renamed, removed or swapped
  for a junction while Studio works in it, and opens each part with
  `FILE_FLAG_OPEN_REPARSE_POINT`, so a link or a junction is refused, never followed. A link to
  a folder, a junction included, is a directory to Windows, so Studio removes a stale one as a
  directory, which removes the link itself and never its target.

### Choosing the CLI

The agent's CLI is found with Studio's own finder (`graph_store::cli`, spec 003), so the two
look in the same places, probe alike, and share one turn at the CLI.

1. `omnigraph_cli` (Settings → Connection → Omnigraph CLI, one setting for Studio's CLI and the
   agent's; a leading `~` is the home folder), then each `omnigraph` on PATH, then
   `~/.local/bin`, `/opt/homebrew/bin` and `/usr/local/bin` (`cli::candidates`), each program
   once: a place that links to a program listed before it is that program (`cli::installed`).
2. Each is probed as Studio probes its own CLI (`cli::probe`), without a server: `omnigraph
   version --json` must report `http_contract` 0.14 (Omnigraph #987, E23) and target,
   credential and access protocol 2, bounded to 10 s. Omnigraph `v0.14.0` is the first release
   to report 2 (#1133, #1135), and every CLI that does reads the version-1 configuration Studio
   writes, so the probe is the whole check: no `--help` of a command and no `config validate`
   of the home. What a program was found to be is kept in `cli_probes.json` with the
   requirements it was judged against, until its file or those requirements change, so a later
   conversation, or a launch, runs no probe of it again; a program that could not be run or did
   not answer in time is asked again at the next look. The first that fits goes in the wrapper,
   so a slow or unavailable server never decides which CLI a conversation gets. An older
   release, an edge build that reports protocol 1, or one that reports another number or none,
   fits nothing.
3. The look waits for Studio's turn at the CLI up to 120 s, ahead of the asks no window waits
   on, since a person waits on the conversation. When Studio's other commands hold the CLI past
   that (a token's export waiting on its credential helper), no CLI is asked: the wrapper runs
   nothing and says the CLI was busy with another of Studio's commands, and to start a new
   conversation to try again.
4. If none fits, the wrapper runs nothing and prints why, so the agent's shell refuses rather
   than run a CLI that cannot reach the gateway. A notice in the thread names
   what the graph and the home need, and each CLI found with why it does not fit, naming its
   release where it reported one: it answers no `version --json`, speaks another contract,
   reports no target, credential or access protocol, or one other than 2, or could not be run or
   did not answer in time. It ends with what to install: Omnigraph `v0.14.0` or later, with
   Omnigraph's installer (`curl -fsSL
   https://raw.githubusercontent.com/ModernRelay/omnigraph/main/scripts/install.sh | bash`;
   `install.ps1` through PowerShell on Windows), or `omnigraph_cli` set to such a CLI. The
   wrapper's refusal names the same release. A configured `omnigraph_cli` that does not fit is passed over with a notice
   naming the one used.

The edge builds are refused: `0.14.0-edge+50283ce` reports no access protocol, and
`0.14.0-edge+7fff9df`, the build installed here when this was written, reports protocol 1. The
ignored `gateway` test `a_live_cli_reaches_the_gateway_with_what_its_home_holds` passed with
`OMNIGRAPH_STUDIO_LIVE_CLI` pointing at `0.14.0-edge+7fff9df` (validation log, 2026-10-21); it
has not been run against a `v0.14.0` CLI, which is not installed here. The ignored
`the_stock_cli_reads_and_writes_its_branch_through_the_gateway` also needs a server and has not
been run with either.

### Gateway rules

The gateway answers `GET`/`HEAD /healthz` itself with one contract header, before the token
check, as the CLI probes with no token. Every other request needs the session token (else 401)
and exactly one `omnigraph-http-api: 0.14` (else 400 `api_contract_mismatch`, naming the CLI to
use). Of the request's headers only `accept`, `content-type`, `if-match`, `if-none-match`,
`omnigraph-if-actor`, `omnigraph-if-graph-commit` and `range` are passed on; the gateway sets the
token and contract itself. `omnigraph-if-actor` is the CLI's `--as`, which can only narrow a
request: the server refuses it unless it names the person's actor. Request bodies are read
whole, at most 64 MiB (413 above, before anything is sent), and only once the token has been
checked: a request without it is refused (401) from its headers, its body unread. The probe
carries no body, so one that declares a body (a `content-length` other than 0, or any
`transfer-encoding`) is refused (400) unread too, and any local process that finishes a
handshake cannot make Studio read one. A body whose end cannot be told (a `content-length`
that is not one number, a transfer coding that does not end with `chunked`) is refused (400). A
target holding a `#`, or a query parameter given twice, is refused (400) before the rules
read it: forwarding drops a `#` and all that follows it, so the rules would judge a query the
server never sees, and a repeated key could be judged by one value and read as another. The
gateway serves 64 connections at once; full, it closes the oldest that has not yet given the
token to take the next, so connections another local process holds open never keep the CLI
out (since 2026-10-22; before, a full gateway closed the next connection unread, so 64 idle
ones, each let go within 10 s and opened again, locked the CLI out).

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
do instead. A CLI with E22 (Omnigraph #981) reports it as not started; an older one still says
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

### Both agents run unsandboxed

The agent's shell runs `./omnigraph`, so it must reach the gateway on loopback. Claude's Bash
sandbox is off by default. Codex runs unsandboxed too (`agent_panel::codex`): codex-acp starts
every session, new or loaded, in a mode whose sandbox has no network and ignores Codex's own
configuration, so when the agent command runs codex-acp Studio starts it with
`INITIAL_AGENT_MODE=agent-full-access`. An `INITIAL_AGENT_MODE` in the agent's settings is kept,
and a mode chosen in the panel still applies to its conversation; in a sandboxed mode Codex's
shell cannot reach the gateway, and the CLI says so ("server discovery connection failed").
Without a sandbox the agent's shell runs as the person and could read their own CLI
configuration, as Claude's can by default: the gateway's rules hold for the CLI Studio gives it,
and are not a boundary against an agent that leaves it.

Until 2026-10-28 Studio ran the CLI for a sandboxed agent itself, as the MCP tool
`omnigraph_run`: an argument list checked against a table of each command's flags, files
copied through a private folder outside the working directory, and a run number for calls past
four minutes. It went with the MCP server, about 2,000 lines, once neither agent needed it.

E24 has landed (a `unix:///…` server, Omnigraph #988), but a Unix socket does not help Codex
yet: Codex can allow one only through a permission profile's network table, which also opens
the rest of the network under `codex sandbox`, and codex-acp's per-turn sandbox policy
overrides profiles in any case (Validation log).

### The skill

Studio writes an `omnigraph-studio` skill into the conversation's folder at every start, beside
`AGENTS.md`: `.claude/skills/omnigraph-studio/` for Claude Code and
`.agents/skills/omnigraph-studio/` for Codex, since neither agent reads the other's folder.

- Its `SKILL.md` is Studio's own (`crates/agent_panel/skill/SKILL.md`): what differs in Studio
  (the CLI path and flags, the agent's branch, no schema changes, unknown outcomes, offline
  lint) and which parts of the guide to read or skip.
- The guide is the Omnigraph skill of the build whose CLI Studio requires, copied
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

### No MCP server

The CLI is the agent's one way to the graph, and Studio serves no MCP server (since
2026-10-28; before, `omnigraph_cli` and `omnigraph_run`, above). The graph tools of spec 011
(`omnigraph_schema`, `omnigraph_branches`, `omnigraph_query`, `omnigraph_propose_change`) went
earlier: every one of them is a CLI command, and two ways to the same write would split the
cards and the rules. Every permission request the agent sends is the person's to answer, and
the thread shows each tool call by its title; a write's query and outcome show on its card.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| The agent uses the stock `omnigraph` CLI against a Studio gateway | A `studio` CLI of Studio's own; MCP tools only | One CLI for people and agents. The CLI already covers every served route; a second CLI would repeat its verbs. MCP tools cannot be composed |
| Studio holds the person's token; the agent gets a session token for the gateway only | Give the agent the person's token; mint a server token for the agent | The person's token could write `main` and every branch. 0.13 cannot mint a narrower credential: credentials carry no grants, and branch scopes cannot name a branch (E20) |
| The gateway classifies writes by structured fields and refuses what it cannot classify | Parse statement mutations with `gq_language` | Statement targets live in query text, `commit publish/drop` names no branch, and stored-query mutability is the server's. A second parser of the write language could drift from the engine's. Refusing keeps the gateway closed by default; the agent uses the structured commands instead |
| Session token in a 0600 file | An environment variable | The agent process is shared by the graph's conversations, so one variable cannot hold each conversation's token, and both agents pass their whole environment to every shell command |
| A folder per conversation holding `./omnigraph` and `AGENTS.md` (since 2026-10-28; before, a wrapper per conversation in a shared directory, its path told by Studio's MCP server) | Environment variables and a PATH entry on the agent process; keep the shared directory and the MCP server; put the path in the first message | The agent process is shared by the graph's conversations, so per-conversation values cannot live in its environment, and Codex's shell snapshot may reorder PATH. A folder of its own gives each conversation the same names, so `AGENTS.md` alone says what to run, with no server and nothing in the conversation's history to lose to compaction |
| Remove only Omnigraph's variables from the agent's environment | Also remove `AWS_*`, `AZURE_*`, Google credentials | The agent may need them for its model provider |
| On Windows the agent starts through Studio's own executable as `launch-agent` | `cmd /c set NAME= & PROGRAM ARGS…`; starting the agent's process without the ACP crate | `cmd` expands `%` even inside quotes and does not read the quotes the standard library writes, and the ACP crate passes arguments only through the standard library, so a path or argument could change. Starting the process without the crate would repeat its stderr capture, exit and shutdown handling. Studio already re-runs its executable as `mcp-proxy` |
| On Windows the agent's wrapper is a batch file, `omnigraph.cmd`, written from the same account as the shell script | A native launcher in each home; a PowerShell script | A launcher means a second executable to build and ship, or Studio's own: that opens its window when run as `omnigraph`, is large to copy into each home, and a hard link to it would let the agent write Studio's executable. PowerShell's default execution policy on Windows 10 and 11 refuses scripts, and Git Bash cannot run one. A batch file's cost is cmd's reading of the caller's line, which no batch file can undo; `AGENTS.md` tells the agent |
| Request bodies read whole, at most 64 MiB; answers to reads stream | Stream request bodies too | The rules need small JSON bodies whole; a load larger than 64 MiB is refused before sending, which bounds memory (IX) |
| The gateway passes the upstream contract through and refuses a CLI that speaks another | Translate between contracts | The contract is an exact-match safety property. A translation layer would be a second API that drifts |
| Studio tests each CLI once against the gateway | Map package versions to contracts | The package version does not determine the contract; a real request does, until E23. Superseded by `version --json` (see "Choosing the CLI") |
| The agent's CLI must have targets; Studio writes only the target home and adds `--target studio` | Write a `servers` home for a CLI before targets, chosen by `protocols.target` | Omnigraph main refuses a `servers` file (#1090). Two forms meant two homes, two flag sets, and instructions written before the choice that could name neither; Studio supports one CLI line, 0.14 with targets. Superseded 2026-10-21 by the version-1 operator configuration (next rows) |
| Studio writes only Omnigraph's version-1 operator configuration and refuses a CLI that cannot read it, the installed edge `0.14.0-edge+50283ce` included (Ragnor, 2026-10-21) | Keep the `target_config_version` home for CLIs before #1115; write both forms and choose by CLI | Omnigraph main refuses the former form, and its keyed `credentials` file is gone. Two forms would mean two homes and two token files again, for a CLI line that is ending. The cost: no agent CLI until an edge build with `ff542202` is installed, which the refusal says, naming the commit |
| A CLI is taken when its own `config validate` passes the file Studio wrote, after Studio's own probe takes it (`version --json` says contract 0.14, target and credential protocols 1 or 2 and access protocol 1, and the commands Studio runs answer `--help`) | Choose by `protocols.target`; by release name or date | `protocols.target` is 1 before and after the change (#1127 asks for 2), and builds of one date differ (`edge-20261021-888223b` has #1115 and refuses the home; the first that reads it is from `ff542202`). Only the CLI's parser knows what it reads, and the check is offline |
| The gateway serves HTTPS on loopback with a certificate of its own, which the home trusts alone (`mode: replace`); the session token stays on every request | Plain HTTP with `auth: anonymous`; the system's roots; a Unix socket | A token over HTTP is refused by the version-1 configuration, and anonymous would drop the per-request token, so any local process could use the gateway. `replace` keeps every other CA out. A Unix socket (E24) is not a version-1 target transport |
| One certificate per gateway, made when it starts and valid for a year, its key in memory only; never renewed (Ragnor, 2026-10-21) | A short validity, renewed at half-life with both certificates in `ca.pem` | The key never leaves the process and dies with it, and a gateway started again makes a new one, so a shorter validity protects nothing, and renewal is machinery carried forever (a bundle rewritten under a running CLI, and a CLI that read the old one). A gateway that outlived its certificate, which a conversation does not in practice, would let the CLI's TLS error reach the agent |
| Token and certificate in files named by absolute paths in each home | Relative paths beside the config; the certificate inline | The CLI resolves either; absolute paths read the same in both homes' files and in an error. The version-1 configuration takes a CA only as a file |
| The agent keeps Studio's gateway, not the server's MCP | Point the agent at the server's MCP, which has read and opt-in write tools since #994 | Its writes would go wherever the agent's token may write; until E20 only the gateway keeps them on the agent's branch, and only the gateway shows each write as a card |
| Codex runs unsandboxed, as Claude does | Network only when the person allows it; mirror the sandbox in the person's Codex `config.toml`; always refuse the CLI to Codex | One way for both agents to reach the graph, their shell, and no code to read Codex's configuration. Claude's sandbox is off by default already, so a sandboxed Codex protected nothing Claude did not already expose |
| The CLI is the only graph tool, run in the agent's shell | Keep spec 011's schema, query and propose tools beside the CLI; keep `omnigraph_run` for a sandboxed agent (until 2026-10-28) | The CLI covers every served route and composes; the old tools were a subset with their own write path. With both agents unsandboxed, `omnigraph_run` (an argument allowlist, staged files, run numbers past a tool timeout) served no agent Studio starts by default, and was the most code in the crate |

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
  so a sandbox can allow one socket. Landed (#988), not used yet (see the Codex note above); the
  version-1 operator configuration has no Unix-socket target.
- **#1127** ([ModernRelay/omnigraph#1127](https://github.com/ModernRelay/omnigraph/issues/1127)):
  `version --json` to report `protocols.target: 2` for the version-1 operator configuration.
  Studio takes 2 already and still validates the home; with it, the notice could say "too old"
  without running `config validate`.
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
| 2026-10-28 | A folder per conversation replaces the MCP server and `omnigraph_run` | `agent_panel` `sessions::tests::each_conversation_has_a_folder_of_its_own`, `cli_home::tests::agents_md_names_the_cli_the_branch_and_the_rules`, `a_conversation_home_holds_its_config_token_and_wrapper` (the wrapper refuses while Studio chooses, then runs the CLI with `.studio` as its home; the token, certificate, wrapper and `.studio` go with the conversation), `both_wrappers_are_written_from_one_account` (`%~dp0.studio`), `a_cli_the_batch_file_cannot_name_is_not_offered_to_the_shell`, `codex::tests::codex_starts_unsandboxed_unless_its_settings_say_otherwise`; `cargo test -p agent_panel -p studio`, clippy `-D warnings`, `cargo deny check` | See the result columns of the run in the session that made the change. Not yet run with a live agent: the test agent now runs `./omnigraph` in its session's folder, as Claude and Codex would |
| 2026-10-28 | Codex cannot allow one Unix socket to an ACP session | `codex sandbox` (0.164.0, a throwaway `CODEX_HOME`, profile `-P`) running `nc -U` to a listening socket, `nc -z 1.1.1.1 443`; codex-acp 2.3.0's `dist/index.js` | Default and workspace-write: socket refused. A profile with `[permissions.p.network] unix_sockets = { "<dir>" = "allow" }`: refused. With `enabled = true` too (any `mode`, any `domains`): the allowed socket, another socket and the internet all open. codex-acp sends each turn a legacy sandbox policy (`runTurn`, `sandboxPolicy`) that has no socket field and overrides profiles |
| 2026-10-13 | A `unix://` server is not a server address today | `omnigraph query --server unix:///tmp/x.sock …` | `storage_root_invalid … unsupported storage URI scheme 'unix'` (E24) |
| 2026-10-13 | Forwarding keeps Studio's credentials and stays under the root | `graph_client` `a_forward_carries_studios_credentials_and_stays_under_the_root` | Token and contract added, the caller's content type and body kept, the answer streamed; `/…`, `../`, `graphs/../..`, a full URL and a backslash refused before sending |
| 2026-10-13 | The rules classify every route the CLI uses | `agent_panel` `gateway_rules` tests (5) | Reads anywhere; mutate, load, Blob, branch, merge and stored-query writes only on the agent branch; statements, other branches, other graphs, commit publish/drop and cluster changes refused; a stored query elsewhere gets `expect_mutation: false` |
| 2026-10-13 | The gateway checks the session, then forwards reads and approved writes | `agent_panel` `the_gateway_checks_the_session_then_forwards_reads_and_approved_writes` (real loopback sockets, a fake upstream) | Health without a token; 401 for a wrong token and 400 for no contract, nothing sent; a read forwarded with the person's token; a write to main refused; a rejected write not sent; an approved write sent once, its commit reported; a chunked body decoded |
| 2026-10-13 | A conversation's home holds its config, token and wrapper | `agent_panel` `a_conversation_home_holds_its_config_token_and_wrapper` | Modes `0700`/`0600`; the wrapper unsets other Omnigraph tokens, prefixes `NO_PROXY`, sets `OMNIGRAPH_HOME`, quotes paths; the token is removed with the conversation |
| 2026-10-13 | The agent starts without Omnigraph credentials | `agent_panel` `the_agent_starts_without_omnigraph_credentials` | Omnigraph tokens, profile, home and Studio's token are removed unless `agent.env` sets them; `ANTHROPIC_API_KEY` and `AWS_PROFILE` stay |
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
| 2026-10-21 | The agent's launch removes Omnigraph's variables through one shared match | `agent_panel` `scrub::every_name_with_omnigraphs_prefix_is_omnigraphs`, `scrub::what_is_removed_is_every_omnigraph_variable_but_those_kept`, `the_agent_starts_without_omnigraph_credentials` | `OMNIGRAPH_` matched whole at the start of the name, in any ASCII case only on Windows (`OMNIGRAPHX`, `XOMNIGRAPH_TOKEN` and `OMNIGRAPHé_` are not); `omnigraph_run` (`cli_home`) still reads it through `host::is_omnigraph_variable`, a re-export of `scrub`'s |
| 2026-10-21 | `omnigraph_run` passes only the flags Studio lists, wherever an agent puts another | `agent_panel` `run_passes_only_the_flags_studio_knows`, `every_flag_studio_knows_passes`, `run_arguments_stay_on_the_gateway_and_in_the_working_directory`; Omnigraph `origin/main` at `ff542202` (`cli.rs`, `commands/*/args.rs`); the installed `omnigraph` 0.14.0-edge+50283ce, its `--help` for each command compared with the table by a script; by hand, the same CLI with a home as Studio writes it | `--operator-config`, `--profile`, `--as`, `--target` and `--config`, with a value or after `=`, refused before the command, between its words and after them; as another flag's value and after `--` too; an unknown flag, another command's flag and `--page-token` refused, each named. Every listed flag passes in both forms. The table has every flag of each command the CLI's help lists, but the global ones and `--page-token`, and the same number of positional arguments. The CLI refused `version --target` and `graphs list --graph` (`argument_conflict`), which Studio had added, so both failed before; it reads a following `--json` as a flag, not `--params`'s value, and text joined to `-e`, `--operator-config=…` included, as the query |
| 2026-10-21 | The wrapper removes every Omnigraph variable, as `omnigraph_run` does | `agent_panel` `a_conversation_home_holds_its_config_token_and_wrapper`, `both_wrappers_are_written_from_one_account`; `the_batch_file_runs_the_cli_with_every_argument_as_given` (Windows only, with `omnigraph_foo` in another case; not run here) | `OMNIGRAPH_FOO`, a token and an `OMNIGRAPH_HOME` of the caller's gone, the wrapper's own home set, another variable kept; the batch file lists `set OMNIGRAPH_` |
| 2026-10-21 | The CLI home is Omnigraph's version-1 operator configuration, as its schema and parser require | `agent_panel` `the_home_is_omnigraphs_version_1_operator_configuration`: both homes' `config.yaml` parsed and checked against Omnigraph's `docs/user/cli/operator-config.schema.json` at main `97788d37`, vendored as `crates/agent_panel/fixtures/omnigraph-operator-config.schema.json`, by `json_schema` (a test-only check that fails on a keyword it does not know; `the_check_reads_the_keywords_it_knows_and_refuses_others`), and against the parser's rules read in `operator_schema/` | `version: 1`, no `target_config_version`; `studio` a server target at `https://127.0.0.1:<port>` with the graph and the default target; `auth` a token from the home's `token` (the session token alone, a bearer token as `BearerSecret` reads one), `ca.pem` with `mode: replace`, both by absolute path and `0600` with the config; `proxy: direct`; no null; only plain mappings, each value a word or a quoted string. The schema refuses the former form, a token over `http://` and an unknown default, so the check is not empty |
| 2026-10-21 | A CLI is chosen only when it reads that home | `agent_panel` `the_cli_that_reads_the_home_studio_writes_is_chosen_without_a_server` (fake CLIs: 0.13 with no `version --json`; 0.14 without targets; one that answers "unrecognized subcommand 'config'" as 50283ce does; one whose `config validate` refuses the home as 888223b would; one that validates, checking it was given the private home's file with that home as `OMNIGRAPH_HOME`; target protocols 2 and 3; contract 0.15; a hung one), and the installed `~/.local/bin/omnigraph` 0.14.0-edge+50283ce itself | The validating CLI is chosen over every earlier one, and a protocol-2 CLI only when it validates; with none, each is named with its reason and the refusal names main `ff542202`, says `edge-20261021-888223b` is older, and gives the edge install command. The installed CLI is refused as having no `config validate`. Not yet run against a real CLI that reads the home: none is published |
| 2026-10-21 | The gateway serves HTTPS alone, and the CLI reaches it with what its home holds | `agent_panel` `gateway` `the_cli_reaches_the_gateway_over_https_with_what_its_home_holds` (a rustls client trusting only the home's `ca.pem`, as the CLI's `mode: replace` does); `the_gateway_checks_the_session_then_forwards_reads_and_approved_writes` and `an_approved_write_ends_and_reports_though_the_gateway_stops`, now over TLS | With the token from the home's `token`, a read is forwarded with the person's token; without it, 401; a client trusting another certificate fails its handshake; plain HTTP gets no HTTP answer and nothing is forwarded. The earlier cases hold over TLS: health without a token, a wrong token and a missing contract refused, a held write given up when the CLI hangs up, a chunked body read whole |
| 2026-10-21 | The certificate names 127.0.0.1 and lasts a year | `agent_panel` `gateway_tls` `the_certificate_is_for_the_loopback_address_for_a_year` (rustls's WebPKI verifier, which the CLI's reqwest uses, with the certificate as its one root) | Valid for `127.0.0.1` now and in 360 days; refused in 368 days, three days before it was made, and for `localhost`, `127.0.0.2`, `::1` and a DNS name; the PEM holds no private key; two gateways make two certificates |
| 2026-10-21 | `--page-token` passes where a command pages | `agent_panel` `every_flag_studio_knows_passes`, `run_passes_only_the_flags_studio_knows`; `commands/history/args.rs` at `97788d37` | `commit list`, `branch diff` and `commit changes` take it in both forms; `changes poll`, which has none, refuses it by name |
| 2026-10-21 | The Windows stand-in CLI speaks TLS to the gateway | `requester::stand_in` (now a rustls client trusting its home's `ca.pem`) for `gateway` `a_write_whose_command_was_stopped_is_not_sent` | Windows only; not run here. CI runs it on `windows-2025` |
| 2026-10-21 | A real CLI that reads the home reaches the gateway | `agent_panel` `gateway` `a_live_cli_reaches_the_gateway_with_what_its_home_holds` (ignored: `needs: OMNIGRAPH_STUDIO_LIVE_CLI`): `choose_cli` must take the CLI, then `branch list --json` through `omnigraph_run` and through the wrapper must each reach a fake server with the person's token | Not run: no build made from main at `ff542202` or later is published or installed, and building Omnigraph here is too heavy for the disk. Run it, and the server end-to-end test, with the next edge build |
| 2026-10-21 | The gateway reads a body only for a request with the token, and one bound covers the handshake and the head | `agent_panel` `gateway` `only_a_request_with_the_token_has_its_body_read` (new: a stream that gives the head, then body on demand, counting it), `a_connection_is_let_go_one_bound_after_it_opens` (new; the head's bound is 3 s in tests, 10 s in Studio), `the_gateway_checks_the_session_then_forwards_reads_and_approved_writes` (a probe declaring a 64 MiB or chunked body, over TLS, must be answered within 5 s); each run red first against the old order (health before the token check, the body read for it; a bound of its own for the head) | Before: `GET /healthz` with a declared 64 MiB body and no token had all 64 MiB read and was answered 200; over TLS such a probe whose body never came got no answer within 5 s (the gateway waited up to 60 s for it); a connection that finished its handshake 2 s in and sent nothing was held 5.0 s. After: the probe declaring any body, or an unreadable length, is refused 400 and a request without the token 401, each with 0 body bytes read; the probe without a body is answered; the token's request has its body read; a `content-length` of two values or a `transfer-encoding: gzip` is refused 400 unread; the late-handshake connection is let go within 4 s of opening |
| 2026-10-21 | `omnigraph_run` removes an Omnigraph variable Studio holds | `agent_panel` `a_run_starts_the_chosen_cli_with_its_private_home`, now run again by itself in a process of its own started with `OMNIGRAPH_TOKEN_STUDIO_TEST` set (so no other test's environment changes) | Before, the test set no `OMNIGRAPH_*` variable, so the 2026-10-18 row's "no other `OMNIGRAPH_*` variable" was true of an environment that held none. Now the fake CLI's `env` shows `OMNIGRAPH_HOME` alone, neither the variable's name nor its value |
| 2026-10-21 | `.studio` is refused by any name that opens it | `agent_panel` `the_studio_folder_is_refused_by_any_name_that_opens_it` (new) | `.Studio`, `.STUDIO`, `.studio.`, `.Studio . `, `.ſtudio` and `.studıo` (both upper-case to `.STUDIO`), and an absolute `<cwd>/.Studio/…`, are refused; `.studios`, `studio` and a nested `data/.studio` pass. Before, only the exact `.studio` was refused, and on APFS `--out .Studio/…` opens the folder. Not covered: an NTFS 8.3 short name of `.studio` (`STUDIO~1`) |
| 2026-10-21 | The private CLI homes stay on the machine on Windows | `agent_panel` `agents_work_in_the_local_folder_of_studios_roaming_data` (now also: `private_homes()` and the working directories share one `agent` folder) | The homes were under `studio_settings::data_dir()`, the roaming `%APPDATA%` on Windows; they now come from the same `%LOCALAPPDATA%` counterpart as the working directories (`sessions::agent_dir`). Run on macOS, where both are the data folder; the Windows mapping is `local_counterpart`'s, checked by the same test |
| 2026-10-21 | A real CLI that reads the home reaches the gateway, run | `agent_panel` `gateway` `a_live_cli_reaches_the_gateway_with_what_its_home_holds` with `OMNIGRAPH_STUDIO_LIVE_CLI=~/.local/bin/omnigraph`, `0.14.0-edge+7fff9df` (commit `7fff9dfd`, Omnigraph main with `ff542202`), on macOS, with the gateway changes above | Passed. `choose_cli` took it (`version --json`: contract 0.14, target protocol 1; its `config validate` accepted the private home) with the wrapper ready. `branch list --json` through `omnigraph_run` (`--target studio`) and through the wrapper (the default target) each reached the fake server once, over TLS, with the person's token and the contract, and the CLI read the gateway's answer (it reported the fake answer's missing `branches`, `http_status: 200`). The wrapper's first run took 7-23 s across runs, a fresh two-line script written by the test binary the same, the CLI on that home 24 ms and every later run about 20 ms: the first exec of a new script, which Gatekeeper scans (the wrapper's known first-run cost, above), not the CLI or the gateway. Supersedes the row above's "Not run"; the server end-to-end test is still not run |
| 2026-10-21 | The wrapper's first run waits for Gatekeeper's scan | An unsigned C probe (not checked in) that writes a two-line script, makes it `0700` and runs it twice; `/usr/bin/log show` for `syspolicyd` and `XprotectService` over those seconds; Apple M5 Pro, macOS 26.5.2 | `syspolicyd` logged `GK performScan` for the script (`-67062`: unsigned), opened XProtect's analysis service, which loaded the XProtect YARA rules, then `Putting executable into provenance`; the second run logged no scan. First run 0.17 s and 0.26 s in two tries, second 6 ms and 12 ms. Not reproduced: the 7-23 s the live test saw while builds were running |
| 2026-10-22 | The gateway keeps a slot for a connection that gives the token | `agent_panel` `socket::a_full_listener_lets_go_of_the_oldest_unproved_connection` (a listener of 64: the CLI's connection, then 63 held open by another process, then connections with and without the token until all 64 have it, then one more), `gateway::only_a_request_with_the_token_has_its_body_read`, extended (only the token proves a connection, before its body is read); the MCP socket's connections prove themselves by reaching its private folder | Full, each next connection closed the oldest without the token, never the CLI's; with all 64 holding it, the next was closed unread. Before, a full listener closed the next whatever it was, the CLI's among them, for as long as 64 others were held |
| 2026-10-22 | `.studio` is refused by an NTFS stream's name | `agent_panel` `the_studio_folder_is_refused_by_any_name_that_opens_it`, extended: `.studio::$INDEX_ALLOCATION`, `.Studio:$I30:$INDEX_ALLOCATION` and `.studio. :stream` | Refused on every platform, the name read up to its first `:`. Before, each passed, and NTFS opens the folder itself by the first two. Not covered still: an 8.3 short name (`STUDIO~1`) |
| 2026-10-24 | The agent's CLI is Omnigraph `v0.14.0` or later, chosen by its probe alone | `agent_panel` `cli_home` `the_first_cli_studio_drives_is_chosen_without_a_server`, `a_cli_is_probed_once_for_every_conversation` and the refusal tests with stand-ins for `0.14.0-edge+50283ce` (no access protocol) and `0.14.0-edge+888223b` (protocol 1); `~/.local/bin/omnigraph` `version --json` | The first CLI reporting contract 0.14 and protocols 2, 2 and 2 goes in the wrapper, probed once for every conversation, though the stand-ins answer nothing but `version --json` (no `--help`, no `config validate`); the edge stand-ins are passed over and the notice ends with Omnigraph's installer command. The installed `0.14.1-edge+7711e4a` reports 2, 2 and 2 |

## Corrections

- 2026-10-22 · The agent's CLI is now found by Studio's own finder (`graph_store::cli`), and this
  plan described the agent panel's own. "Choosing the CLI" named `agent.omnigraph_cli` under
  Settings → Agent; the one setting is `omnigraph_cli`, under Connection, which an older
  `agent.omnigraph_cli` no longer sets. It said each CLI reported its contract and target
  protocol within 5 s and was validated within 5 s; it takes the shared probe (the credential
  and access protocols and the four commands too, kept in `cli_probes.json`), and both bounds
  are 10 s. It gave "predates the version-1 configuration (it has no `config validate`)" as a
  reason of its own; a CLI without `config validate` is refused with its parser's words, and
  `0.14.0-edge+50283ce` is refused first for reporting no access protocol. It had no case for a
  choice Studio's other commands held back past its 120 s wait. The variables were matched by
  `agent_panel::scrub` (`scrub::PREFIX`, removed), in any case on Windows alone; they are matched
  by `graph_store::cli`, in any case on every system, the shell wrapper included. A home no longer
  removes a `credentials` file an older Studio left. "What exists today" gave the CLI's operator
  file and token rules from before #1090 and #1115, and the installed CLI as `50283ce` and
  0.13.0, as current.

- 2026-10-21 · The gateway answered the health probe without its token but read the probe's body
  first, up to 64 MiB, though the plan said a body is read only after the token check: any local
  process that finished a TLS handshake (it need not trust the certificate) could make Studio
  hold 64 MiB per connection, about 4 GiB over the 64 connections a listener serves, and take
  every one of them from the agent's CLI. The probe now carries no body (one that declares one
  is refused unread), a request without the token is refused before its body as before, and the
  handshake and the head share one 10-second bound from the connection's opening instead of
  10 seconds each, so such a connection holds a place no longer than that.
- 2026-10-21 · `a_run_starts_the_chosen_cli_with_its_private_home`, cited for `omnigraph_run`
  removing Studio's `OMNIGRAPH_*` variables, ran in an environment that held none; it now runs in
  a process of its own that holds one. The rules test named for health had no health case; it is
  `reads_go_anywhere`, and the gateway's tests own the probe. The `.studio` refusal compared the
  name exactly, so `.Studio/…` reached the folder on a case-insensitive file system; it now
  matches the name in any case and without Windows' trailing dots and spaces. The private CLI
  homes, which hold the session token and certificate, were under the roaming `%APPDATA%` on
  Windows; they are now under `%LOCALAPPDATA%` with the working directories.

- 2026-10-21 · `--page-token` was refused by `omnigraph_run` with the flags named for a token. It
  is a page's cursor, not a credential, and now passes where the CLI takes it.

- 2026-10-21 · A stored read on the agent's branch (`omnigraph query <name> --branch agent/…`,
  `expect_mutation: false`) was taken for a write, and its answer's `graph_commit_id`, the commit
  it read at, for a commit it made: the card said Committed and the session logged it. It is now a
  read, and only a write's receipt names a commit. A commit of a kind Studio does not know, taken
  as committed unless said held, now leaves the outcome unknown.
- 2026-10-21 · The MCP instructions were made at launch, before the CLI was chosen, and told the
  agent the wrapper was its CLI when none fit or its shell could not run the one chosen. They are
  now made when the agent asks, from the choice.
- 2026-10-21 · `omnigraph_run` refused only `--target` and `--as` and passed every other flag as
  given. Omnigraph main added `--operator-config FILE` (#1115), whose file can give the target
  `studio` a credential command the CLI runs before any request, so an agent could have run
  its own program outside its sandbox, with Studio's environment. Each command now passes only
  the flags Studio lists for it. The wrapper removed a fixed list of Omnigraph variables; it now
  removes every `OMNIGRAPH_*` variable by `scrub`'s prefix, as `omnigraph_run` and the agent's
  launch do. `version` and `graphs list`,
  which the CLI refused with the target or graph Studio added, now run.

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
  Windows any Omnigraph variable in Studio's environment kept the agent from starting. Windows
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
  Omnigraph tokens, or pointed the CLI at another address. The run now starts the chosen CLI
  from a private home, without Studio's Omnigraph variables. Ending a conversation also removed
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
  environment. An agent may need them for its model provider, so only Omnigraph's variables are
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
