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
(⌘Y), **Always for this branch** (⌘⌥Y), **Reject** (⌘⌥N), each showing its binding; see
Actions for why ⌘↩ (Send) and ⎋ are not used. On Allow, Studio creates the branch if
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

Rendered in order, as Delta's thread reads: the person's messages with their mark; the agent's
replies as markdown, with the agent's mark once per turn rather than on every chunk; each run of
tool calls and thoughts folded into one muted line that says what happened ("Thought 1 time,
read the schema, ran 2 queries", then "· 1 failed" in the error colour when a call failed), with
a turning arrow while a step runs and a chevron that opens the steps under a hairline: each
call by name, with no mark when it went well, a turning arrow while it runs and "… failed" in
the error colour, and each thought's text. A query the agent runs is not folded: as Delta
shows an edit, a line "Query people" (the declared name in the code font, "Running" while it
runs, "· failed" when it failed) stands over the query in a code card with the query editor's
colours (`TokenKind::syntax_name`, shared with spec 007's editor), with its parameters on one
muted line under it, taken from the call's `rawInput`; the line folds the card; the plan as a list with each entry's status. Steps
of a turn sit 12 px apart and a new question opens 16 px more. Other updates are ignored. A
banner above the composer shows "`agent/<stamp>` · N changes · **Review**" after the first
committed change; Review switches the session to the agent branch and opens spec 008's review.

### Actions

| Action | macOS | Linux and Windows |
|---|---|---|
| `studio_agent::OpenAgent` | ⌘⇧A | Ctrl+Shift+A |
| `studio_agent::Send` | ↩ in the composer (⇧↩ for a new line), or ⌘↩ | Enter in the composer (Shift+Enter for a new line), or Ctrl+Enter |
| `studio_agent::Allow` | ⌘Y | Ctrl+Y |
| `studio_agent::AllowAlways` | ⌘⌥Y | Ctrl+Alt+Y |
| `studio_agent::Reject` | ⌘⌥N | Ctrl+Alt+N |
| `studio_agent::Stop` | ⌘⇧. | Ctrl+Shift+. |
| `studio_agent::Restart` | — | — |
| `studio_agent::Ask { text }` | the palette's Ask (spec 002) | |

Enter sends from the composer, as in Zed's agent panel (`AcpThread > Editor`, `enter` →
`agent::Chat` unless `use_modifier_to_send`); Studio's binding comes after Zed's editor
bindings, so it wins over `editor::Newline`. While the agent works, Enter leaves the message in
the composer with one note rather than dropping it. Studio loads Zed's keymap first. Zed binds ⌘. to `editor::ToggleCodeActions` in every editor,
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

## Conversations

Checked against Zed's thread sidebar at `v1.23.2` (`sidebar`, `agent_ui/src/thread_metadata_store.rs`)
and Delta's sidebar.

- **Studio's own index.** Like Zed's metadata store, Studio keeps the list itself
  (`agent/sessions.json` in the data directory, owner-only, format 2): per conversation, the
  agent's session id, the graph, the agent command, its title, when it started, the last
  message the person sent, the agent branch its changes go to, and whether it is pinned or
  archived. Ids and titles only; the conversation stays with the agent. The agent's own
  `session/list` is not the source: every graph's agent works in the same directory, so it
  cannot tell graphs apart. The first format (one session per graph and agent) is read as
  records of their own.
- **Titles.** The agent's `session_info_update` title wins; until it sends one, the first line
  of the first message (cut at 80 characters); "New conversation" before that.
- **One tab per conversation.** Each conversation is its own Agent tab, titled after it, and
  keeps running while another is shown, so two can sit side by side. Opening one that has a
  tab shows that tab; otherwise the agent starts and replays it with `session/load` (a
  conversation it no longer knows starts anew in that tab). A tab comes back after a restart
  with its conversation. ⌘⇧A shows an open agent tab, else the graph's last conversation with
  the agent, else a new one. New conversation (sidebar, model menu, `studio_agent::NewConversation`)
  opens a new tab and leaves the earlier conversation where it is. ↻ appears only after the
  agent stopped and starts it again on the same conversation. Each tab runs its own agent
  process; one process serving every conversation (ACP allows several sessions per connection)
  is deferred.
- **The Conversations panel** is a tab of the left sidebar (the agent mark;
  `studio_agent::ShowConversations`), after Delta's threads list: "New conversation", then
  Pinned and Recent (labels only when something is pinned), each row the title and, at the
  end, what the conversation is doing (a turning circle while it runs, a warning mark while a
  change or tool waits for the person, a red cross when the agent stopped with an error, an
  accent dot when it finished while its tab was not shown, cleared when the tab gains focus)
  or else when the person last wrote ("now", "5m", "3h", "2d", "12 Oct"). Rows sort by the
  person's last message, as Zed's sort by the last interaction, so a conversation finishing in
  the background does not move. The shown tab's row is selected. Hovering a row swaps the time
  for Pin and Archive; archiving only hides it here (the agent keeps the session, as Zed's
  archive sends nothing), and Archived folds away at the end with a count, where Undo brings a
  row back. The tooltip has the title, the agent and the last activity. What a conversation is
  doing and whether it was seen live in memory only.
- A loaded conversation keeps writing to its branch and keeps its own options; a new one starts
  from the options chosen last for the agent.

## Session options and context usage

As in Zed's agent panel (checked in `agent_ui` at `v1.23.2`), and only through stable ACP v1:

- **Options.** The options an agent returns from `session/new` and `session/load`, and later in
  `config_option_update`, are its model, mode, reasoning level and the like. Studio advertises
  `session.configOptions.boolean`, so an on/off option comes as a switch rather than an On/Off
  menu. The footer follows Delta's: borderless, an icon and the value, no chevrons. The mode
  (a person-check mark) and the reasoning level (a thinking mark, "High") show in the agent's
  order; every other option (switches such as Fast mode, agent profiles) sits behind one ⋯
  "More options" menu whose tooltip lists their values, so an agent with many options does not
  crowd the composer; the model comes last, with its provider's mark (Anthropic, OpenAI,
  Gemini, by name), and its menu also holds the agent: New conversation and Choose another
  agent. Before an agent offers a model, the agent's name and state dot stand there. A
  picker's tooltip is "Name: value" over the agent's description. Choosing sends
  `session/set_config_option`; the picker shows the chosen value and is disabled until the
  agent answers, and its answer replaces every option, since one choice can change another (a
  model's reasoning levels). A refusal leaves the agent's values and adds an error note to the
  thread. Options exist only while the session runs. Send fills in only when the composer has
  text.
- **Remembered choices.** Studio remembers the last value chosen for each option, per agent
  command (`agent/preferences.json`, owner-only, value ids and booleans only), and sets them on
  the agent's next new session where it offers that value and starts on another. A resumed
  session keeps its own.
- **Context.** Each `usage_update` replaces the usage: a 14 px ring on a visible track, so it
  reads as a gauge rather than a spinner, muted until 85 % and then the warning colour with the
  percentage beside it. Hover
  shows the share of the window, the tokens as people read them (999, 1.2k, 84k, 1.2M) and the
  cost so far (four decimals under a cent). Nothing shows until the agent reports usage; ↻
  clears it.

Zed's searchable picker with favourites (from five values) and its option keybindings are not
built: agents offer few enough values for a menu. Claude's ACP agent (0.89) offers Mode, Model,
Effort, Fast mode and Agent; the test agent offers the same five.

## Deferred

- Windows: the proxy uses Unix sockets. Windows support waits for a named-pipe transport.
- Agent-attributed commits: commits are recorded under the person's actor. Recording "on
  behalf of an agent" needs an engine change, E20 in `docs/deferred-engine-changes.md`.
- One agent process for every conversation of an agent: today each tab starts its own.
- Renaming a conversation, deleting it at the agent (`session/delete`), importing the agent's
  sessions (`session/list`), and searching the list; Zed has all four.
- Queuing messages while the agent works, and sending one at once by cancelling the turn, as
  Zed does on the client side; ACP itself has neither, nor steering.

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
| 2026-10-12 | Agent replies render as markdown | the test agent answering "summarize people as markdown" with bold, a list, inline code and a code block; an offscreen snapshot | Zed's `markdown` crate (`Markdown::new`, not `new_text`, which parses links only) with `MarkdownStyle::themed(Agent)`; a streaming reply appends its new tail to the parsed text |
| 2026-10-12 | A conversation comes back after a restart | `agent_panel` `a_session_is_kept_per_graph_and_agent`; offscreen snapshots with the test agent (now with `loadSession` and a transcript per session): ask, close, reopen the Agent tab; then ↻ | The question and the markdown reply replayed with "Resumed the last conversation"; after ↻ the panel cleared and `sessions.json` held a new session id |
| 2026-10-12 | Zed's agent offers the model through ACP session options and shows context from `usage_update` | Read `agent_ui/src/config_options.rs`, `agent_servers/src/acp.rs` (`set_config_option`) and `thread_view.rs` (`render_token_usage`) at `v1.23.2`, and `SessionConfigOption`, `UsageUpdate` in `agent-client-protocol-schema` 1.9.1 | One picker per option in the agent's order, the current value's name with a chevron; the response to `session/set_config_option` replaces every option; a 16 px ring, warning colour from 85 %, used, size and cost on hover |
| 2026-10-12 | The footer shows the agent's options and how full its context is | Offscreen snapshots with the test agent (now offering a mode and a grouped model, taking `session/set_config_option`, reporting usage after each turn): ask, open the model menu, pick Claude Opus, ↻, hover the ring | "Ask before changes ▾", "Claude Sonnet ▾", then a ring with "21%"; the menu showed the Claude and Local groups with Sonnet checked; after ↻ the new session showed Claude Opus and no ring; the tooltip read "21% of the context window · 42k of 200k tokens · 0.14 USD so far"; `preferences.json` held `"model": "opus"`, mode `0600` |
| 2026-10-12 | The footer and thread read like Delta's | Offscreen snapshots with the test agent offering Claude's five options: two questions, open a run, open ⋯ | Footer: ring, "Ask before changes", "High", ⋯, "Claude Opus" with the Anthropic mark, ↻, send (filled with text in the composer); ⋯ held Fast mode and Agent (Default checked); each turn showed one agent mark and "Read the schema ›", "Ran 1 query ›", the first opening to "Read the schema" |
| 2026-10-12 | A query the agent runs shows as code | Offscreen snapshot with the test agent reporting `rawInput` for its count query | "Query people ⌄" over the four-line query in a card, keywords, the query name and `Person` in the theme's syntax colours; the schema read stayed folded as "Read the schema ›" |
| 2026-10-12 | Conversations are listed per graph and run side by side | `agent_panel` tests `conversations_are_kept_per_graph_newest_first`, `the_first_format_becomes_records`, `times_read_short`; offscreen snapshots with the test agent (now with a "slow" turn): two and three conversations in tabs, a hovered row, a pin, a slow turn finishing behind another tab, a restart | Tabs titled "How many people are there?" and "List the companies by size"; the shown one selected in the panel with "now"; hover showed pin and archive; the pinned row moved under Pinned; the slow conversation showed a turning circle while running and an accent dot after finishing unseen; after a restart both tabs came back and the shown one replayed |

## Corrections

- The socket first lived in `<data dir>/agent/`. With a long data directory the path exceeded
  macOS's 104-byte limit and binding failed ("path must be shorter than SUN_LEN"). It now lives
  in the per-user runtime directory described above, with a length check that fails with a
  clear message.
- Stop was planned as ⌘.; Zed's editor binding takes it inside the composer, so it is ⌘⇧.
- 2026-10-12 · Approval said Allow was ⌘↩ and Reject ⎋. ⌘↩ is Send in the composer and ⎋
  belongs to the editor, so the card uses the bound ⌘Y, ⌘⌥Y and ⌘⌥N, as in Actions.
