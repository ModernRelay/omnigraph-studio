# 011: Agent panel

Status: implemented · 2026-10-12

## Problem

A person wants to ask an agent about the graph ("which projects have no owner?") and let it
propose changes, without handing the agent their token or letting it write to `main`. Every
change the agent makes must be visible and approved before it lands, and must arrive in review
(spec 008) like anyone else's branch.

## Principles

- III: an agent write has one of three outcomes, shown on its card; nothing is retried.
- IV: agent writes go only to a branch Studio creates for the agent, never to the branch the
  person is on, and never without an approval in Studio.
- VII: the token stays in Studio. The agent reaches the graph only through tools Studio serves
  and executes itself.

## What exists today (checked)

- Studio: `GraphSession::read_request` and `write_request` take a request with an explicit
  `branch` (`crates/graph_store/src/session.rs`), so tools can read and write a branch other
  than the one the person is on, through the same outcome log.
- Zed (`v1.23.2`): Zed's agent panel hosts external agents over the Agent Client Protocol with
  `agent-client-protocol = "=2.2.0"` (`Cargo.toml` at the pin); `crates/agent_servers/src/acp.rs`
  is 5,644 lines and depends on Zed's project, terminal and file-system layers, so Studio
  depends on the protocol crate, not on Zed's agent crates.
- ACP crate 2.2.0: runtime-agnostic (`futures`, `async-process`; no Tokio).
  `AcpAgent::spawn_process` starts the agent in its own process group, and dropping the
  connection kills the group (`src/acp_agent.rs`). Request handlers hold the connection's
  dispatch loop until they return (`src/concepts/ordering.rs`), so a permission request that
  waits for a person must be answered from a task spawned with `cx.spawn`.
  `AcpAgent::claude_agent()` runs `npx -y @agentclientprotocol/claude-agent-acp@latest`;
  `AcpAgent::codex()` runs `npx -y @agentclientprotocol/codex-acp@latest`.
- ACP v1 schema 1.9.1: `session/new` takes `McpServer::Stdio { name, command, args, env }`,
  which every agent supports (HTTP and SSE are capability-gated). Session updates:
  `UserMessageChunk`, `AgentMessageChunk`, `AgentThoughtChunk`, `ToolCall`, `ToolCallUpdate`,
  `Plan`, and others Studio ignores. Permission options have kinds `AllowOnce`, `AllowAlways`,
  `RejectOnce`, `RejectAlways`.
- OmniGraph (`v0.13.0`): the server's `/mcp` endpoint (`crates/omnigraph-server/src/mcp.rs`)
  is read-only and serves stored queries only ("Mutation tools are unavailable"), so it cannot
  carry proposals, and using it would hand the agent a bearer token.
- OmniGraph (`v0.13.0`): branch names may contain `/`. Checked on the dev server: creating
  `agent/probe-1` from `main`, listing it, and deleting it as `agent%2Fprobe-1` all succeeded.

## Design

### Pieces

| Piece | Where | Role |
|---|---|---|
| Panel | `crates/agent_panel` (layer 5) | The thread view, the composer, approval cards |
| ACP host | `crates/agent_panel/src/host.rs` | Spawns the agent, speaks ACP v1, turns traffic into thread events |
| Tool server | `crates/agent_panel/src/tools.rs`, `mcp.rs` | MCP over a Unix socket; executes tools on the person's `GraphSession` |
| Proxy | `omnigraph-studio mcp-proxy <socket>` in `crates/studio/src/main.rs` (`agent_panel::proxy`) | The stdio MCP server the agent starts; copies bytes between its stdio and Studio's socket |
| Test agent | `crates/agent_panel/examples/test_agent.rs` | A scripted ACP agent for end-to-end checks without a model or network |

The proxy holds no token and no logic. The agent never learns the server URL or token.

### Choosing an agent

Setting `agent.command` (a list: program then arguments) and `agent.env` (a map). Empty by
default: the panel then offers **Claude Agent**, **Codex** and **Custom command…**. A preset
writes the crate's preset command to settings, and the panel always shows the command it runs.
Nothing is downloaded until the person picks one and sends a prompt.

### Lifecycle

`Setup` (no command) → `Starting` (spawn, `initialize`, `session/new`) → `Ready` →
`Prompting` (Stop sends `session/cancel`) → `Ready`. If the process exits, `Exited` shows the
exit status and the last 20 lines (at most 4 KB) of its stderr, and **Restart**. Changing the
graph or closing the window ends the agent (the process group is killed) and removes the
socket.

`session/new` gets `cwd` = `<data dir>/agent/workspace` (created empty) and one MCP server:
`{ name: "omnigraph", command: <Studio's executable>, args: ["mcp-proxy", <socket>] }`. The
socket is `<runtime dir>/<pid>-<n>.sock`, where the runtime directory is
`$TMPDIR/omnigraph-studio` on macOS (the per-user temporary directory), and
`$XDG_RUNTIME_DIR/omnigraph-studio`, else `/tmp/omnigraph-studio-$USER`, on Linux. Studio
creates it with mode `0700`, refuses an existing one that is not a real directory owned by the
user with mode `0700`, and refuses a socket path longer than 100 bytes. Unix socket paths are
limited to 104 bytes on macOS.

### Tools

| Tool | Arguments | Does |
|---|---|---|
| `omnigraph_schema` | — | The `.pg` source and a one-line summary per type |
| `omnigraph_branches` | — | Branch names, and which are the person's branch and the agent's |
| `omnigraph_query` | `query`, `name?`, `params?`, `branch?` | Checks the text with `gq_language`; refuses a mutation ("use omnigraph_propose_change"); runs it on `branch`, else the agent's branch once it exists, else the person's branch. At most 200 rows and 100 KB of JSON, with `truncated: true` when cut |
| `omnigraph_propose_change` | `query`, `name?`, `params?`, `summary` | Must check as a mutation. Waits for approval in Studio, then runs on the agent's branch |

Diagnostics from `gq_language` come back as the tool error, with line and column, so the agent
can fix its query.

### Approval

A proposal shows a card: the summary, the query text, the parameters, and the target branch,
with "creates `agent/<stamp>` from `<base>`" before the first write. Buttons: **Allow**
(⌘↩), **Always for this branch**, **Reject** (⎋). On Allow, Studio creates the branch if
needed, then sends the mutation to it. The card and the tool result show the outcome:

- Committed: the commit id and the affected counts.
- Refused: the server's message.
- Unknown: "The server did not confirm this change. It was not retried; check the branch
  history before proposing it again."

Reject returns "The person rejected this change." "Always for this branch" runs later
proposals without asking, still as cards, until the agent session ends.

The agent branch is `agent/<YYYYMMDD-HHMMSS>` (local time when the session starts), created
from the branch the person was on when the session started.

### The agent's own permission requests

`session/request_permission` for one of Studio's tools (title or tool name containing
`omnigraph_`) is answered with the agent's allow-once option at once: reads are read-only and
writes have Studio's own approval, so asking twice would only add noise. Any other request
(file edits, shell commands) shows a card with the agent's own options; closing the panel
answers `Cancelled`.

### Thread

Rendered in order: the person's messages; agent text (plain text, wrapped); thought chunks
folded under "Thinking"; tool calls as cards with title, kind and status (pending, running,
done, failed); the plan as a list with each entry's status. Other updates are ignored. A
banner above the composer shows "`agent/<stamp>` · N changes · **Review**" after the first
committed change; Review switches the session to the agent branch and opens spec 008's review.

### Actions

| Action | macOS | Linux and Windows |
|---|---|---|
| `studio_agent::OpenAgent` | ⌘⇧A | Ctrl+Shift+A |
| `studio_agent::Send` | ⌘↩ | Ctrl+Enter |
| `studio_agent::Allow` | ⌘Y | Ctrl+Y |
| `studio_agent::AllowAlways` | ⌘⌥Y | Ctrl+Alt+Y |
| `studio_agent::Reject` | ⌘⌥N | Ctrl+Alt+N |
| `studio_agent::Stop` | ⌘⇧. | Ctrl+Shift+. |
| `studio_agent::Restart` | — | — |
| `studio_agent::Ask { text }` | the palette's Ask (spec 002) | |

Studio loads Zed's keymap first. Zed binds ⌘. to `editor::ToggleCodeActions` in every editor,
which would take Stop from the composer, so Stop is ⌘⇧.; Zed's ⌘Y and ⌘⌥Y editor bindings
exclude a context named `AgentPanel`, and ⌘↩ is bound only for full-mode editors, so they reach
the panel. The composer shows the bindings with `ui::KeyBinding`, so each platform sees its own.

The agent is also a row in Settings, with the presets and Reset.

### What is never logged

Query text, parameters, rows, tokens and prompt text. Agent stderr is shown only in the
`Exited` state and never written anywhere.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Studio serves the tools and executes them on its own session | Point the agent at OmniGraph's `/mcp`; give the agent the CLI and a token | The token would leave Studio, and 0.13's MCP cannot write. Executing in Studio lets one place enforce the agent branch and the approval |
| A stdio proxy to a Unix socket | MCP over ACP (`unstable_mcp_over_acp`); an HTTP MCP server on loopback | Stdio is the one transport every ACP agent supports. A loopback port is reachable by any local process; a socket in a `0700` directory is not |
| Depend on `agent-client-protocol` 2.2.0, default features | Fork Zed's `agent_servers` and `agent_ui`; Zed's unstable feature set | Zed's crates need its project and terminal layers. Stable v1 is what released agents speak |
| No default agent | Default to the Claude preset | Running `npx …@latest` downloads code; the person should choose it |
| Approval is Studio's, not the agent's permission prompt | Rely on the agent asking | Agents differ in whether and how they ask; the guarantee has to hold for every agent |
| One agent branch per agent session | Write to the person's branch; one branch per proposal | Review works per branch (spec 008), and one branch keeps a session's changes together |

## Deferred

- Windows: the proxy uses Unix sockets. Windows support waits for a named-pipe transport.
- Markdown rendering of agent text (Zed's `markdown` crate is a fork candidate, spec 002).
- Agent-attributed commits: commits are recorded under the person's actor. Recording "on
  behalf of an agent" needs an engine change, E20 in `docs/deferred-engine-changes.md`.
- Resuming an earlier agent session (`session/load`).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-12 | 0.13's MCP endpoint cannot write | Read `crates/omnigraph-server/src/mcp.rs` at `v0.13.0` | One tool, `query`, for stored reads; instructions say mutations are unavailable |
| 2026-10-12 | Branch names may contain `/` | `POST /graphs/knowledge/branches {"name":"agent/probe-1"}`, list, `DELETE …/agent%2Fprobe-1` on the dev server | All three succeeded |
| 2026-10-12 | Handlers block the ACP dispatch loop | Read `src/concepts/ordering.rs` in `agent-client-protocol` 2.2.0 | "the dispatch loop waits for each handler to complete" |
| 2026-10-12 | The ACP crate needs no Tokio | Its `Cargo.toml` dependencies | `futures`, `async-process`, no `tokio` |
| 2026-10-12 | Studio's tools reach the graph through the proxy; the agent never holds a token | `examples/snapshot` with the test agent as `agent.command`, actions `OpenAgent`, `Ask {"text":"please make a change"}` | Schema and query tools answered (6 node types, 200 people); the proposal card waited for approval |
| 2026-10-12 | An approved change lands only on the agent's branch | Same, then `studio_agent::Allow`; `GET /branches`; read `person-000000`'s title on both branches | `agent/20261012-084228` created from `main`; committed there; `main` still reads "Chief Scientist", the agent branch "Retitled by the test agent" |
| 2026-10-12 | A rejected change creates nothing | Same, then `studio_agent::Reject`; `GET /branches` | Card "Rejected.", the agent got "The person rejected this change.", no new branch |
| 2026-10-12 | Key bindings reach the panel | Read every ⌘↩, ⌘Y, ⌘⌥Y, ⌘⌥N, ⌘. and ⌘⇧. binding in Zed's `default-macos.json` at `v1.23.2` | As in Actions above; ⌘. collides, ⌘⇧. is free |

## Corrections

- The socket first lived in `<data dir>/agent/`. With a long data directory the path exceeded
  macOS's 104-byte limit and binding failed ("path must be shorter than SUN_LEN"). It now lives
  in the per-user runtime directory described above, with a length check that fails with a
  clear message.
- Stop was planned as ⌘.; Zed's editor binding takes it inside the composer, so it is ⌘⇧.
