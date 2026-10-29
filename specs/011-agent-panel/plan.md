# 011: Agent panel

Status: implemented · 2026-10-12. Since 2026-10-13 the agent reaches the graph only through the
`omnigraph` CLI and a Studio gateway ([spec 013](../013-agent-cli/plan.md)); the tools and
proposal cards below are replaced by spec 013's write cards. Since 2026-10-28 Studio serves no
MCP server: each conversation works in a folder of its own, where the agent runs `./omnigraph`
as `AGENTS.md` there says.

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
- Omnigraph (`v0.13.0`): the server's `/mcp` endpoint (`crates/omnigraph-server/src/mcp.rs`)
  is read-only and serves stored queries only ("Mutation tools are unavailable"), so it cannot
  carry proposals, and using it would hand the agent a bearer token.
- Omnigraph main (2026-10-20; #992, #994, #995, #999): the server's MCP has read tools (`access`,
  paged `commits_list`, changes, `snapshot`) and opt-in write tools (`mutate`, `load`, branch
  create, delete and merge), with the skill's references as resources. Its writes go wherever
  the token the agent holds may write, and no credential narrows that to the agent's branch
  yet (E20), so the agent still reaches the graph only through Studio's gateway (spec 013).
- Omnigraph (`v0.13.0`): branch names may contain `/`. Checked on the dev server: creating
  `agent/probe-1` from `main`, listing it, and deleting it as `agent%2Fprobe-1` all succeeded.

## Design

### Pieces

| Piece | Where | Role |
|---|---|---|
| Panel | `crates/agent_panel` (layer 5) | The thread view, the composer, approval cards |
| ACP host | `crates/agent_panel/src/host.rs` | Spawns the agent, speaks ACP v1, turns traffic into thread events |
| CLI home and gateway | `crates/agent_panel/src/cli_home.rs`, `gateway.rs` (spec 013) | The conversation's `./omnigraph`, `AGENTS.md`, and the loopback gateway it reaches |
| Test agent | `crates/agent_panel/examples/test_agent.rs` | A scripted ACP agent for end-to-end checks without a model; it runs `./omnigraph` in the session's folder as a shell tool would, each command a tool call titled with it |

The agent is never given the server URL or the person's token.

### Choosing an agent

Setting `agent.command` (a list: program then arguments) and `agent.env` (a map). Empty by
default: the panel then offers **Claude Agent**, **Codex** and **Custom command…**. A preset
writes the crate's preset command to settings, and the panel always shows the command it runs.
Nothing is downloaded until the person picks one and sends a prompt.

### Lifecycle

`Setup` (no command) → `Starting` (spawn, `initialize`, `session/new`) → `Ready` →
`Prompting` (Stop sends `session/cancel`) → `Ready`. If the process exits, `Exited` shows the
exit status and the last 20 lines (at most 4 KB) of its stderr, and **Restart**. Changing the
graph or closing the window ends the agent (the process group is killed; on Windows, the job
Studio's launcher holds it in, spec 013).

The agent starts when the first message is sent, so that message waits for the start (a first
`npx` fetch and the agent's own MCP servers take seconds). It is not queued: it shows in the
thread at once as the person's, with "Starting the agent…" under it, and is sent as the first
turn when the agent is ready (`AgentPanel::opening`); messages sent after it wait in the queue
behind it. A start that has not answered in 20 s says "The agent has not answered yet" with
Start again, and only then does the composer offer the restart button. A start that fails keeps
the message ("Not sent yet: it goes when the agent starts again") under the agent's error, which
reads as its reason without the protocol library's debugging fields. A message sent before an
agent is chosen goes once one is.

`session/new` gets the conversation's own folder as `cwd`, `<agent dir>/graphs/<graph>/<branch>-<random>`
(`sessions::working_dir`; a saved conversation keeps the folder it ran in), and no MCP server.
Studio writes the CLI, its home and `AGENTS.md` there first (spec 013).

### Tools

Replaced on 2026-10-13 by spec 013's `omnigraph_cli` and `omnigraph_run` MCP tools, which were
removed on 2026-10-28 for `./omnigraph` in the conversation's folder. As first built:

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
(⌘Y), **Always on this branch** (⌘⌥Y), **Reject** (⌘⌥N), each showing its binding; see
Actions for why ⌘↩ (Send) and ⎋ are not used. On Allow, Studio creates the branch if
needed, then sends the mutation to it. The card and the tool result show the outcome:

- Committed: the commit id and the affected counts.
- Refused: the server's message.
- Unknown: "The server did not confirm this change. It was not retried; check the branch
  history before proposing it again."

Reject returns "The person rejected this change." "Always on this branch" runs later
proposals without asking, still as cards, until the agent session ends.

The agent branch is `agent/<YYYYMMDD-HHMMSS>` (local time when the session starts), created
from the branch the person was on when the session started.

### The agent's own permission requests

Every `session/request_permission` shows a card with the agent's own options: Studio never
picks one for the person. Since 2026-10-28 Studio serves no tools of its own, so no request is
answered without asking; before, a request for `omnigraph_cli` or `omnigraph_run` was allowed
once at once, by what the agent's adapter said the call was.
Closing the panel answers `Cancelled`. Only its buttons answer it (since 2026-10-21): ⌘Y, ⌘⌥Y and
⌘⌥N answer the write card that waits longest and shows them, never an agent's request, whose
"always" option is a lasting rule of the agent's own that ⌘⌥Y ("Always on this branch" for a
write) must not pick unseen.

### Thread

Rendered in order, as Delta's thread reads: the person's messages with their mark; the agent's
replies as markdown, with the agent's mark once per turn rather than on every chunk; each run of
tool calls and thoughts folded into one muted line that says what happened ("Thought 1 time,
read the schema, ran 2 queries", then "· 1 failed" in the error colour when a call failed), with
a turning arrow while a step runs and a chevron that opens the steps under a hairline: each
call by its title, with no mark when it went well, a turning arrow while it runs and "… failed"
in the error colour, and each thought's text. The query cards and record cards Studio drew for
`omnigraph_run` calls went with that tool on 2026-10-28; a write's query and outcome show on
its write card (spec 013). A finished turn ends in its actions under its
last reply: Copy (the turn's reply text) and how many tools it called, muted; the agent offers
no retry or rating, so there is none. The plan as a list with each entry's status. Steps
of a turn sit 12 px apart and a new question opens 16 px more. Other updates are ignored. The
thread is a list that lays out only the blocks in view, as Zed's agent thread is: a block is
measured again only when what it shows changes (or its markdown finishes parsing), so a long
conversation costs what its visible part costs. The blocks are made again from the turn of the
first entry that changed (`thread_blocks`), so a streamed token costs its own turn, not every
turn before it; a block never spans a message of the person's, and a turn's blocks read only its
own entries and whether a message follows it. It follows the newest reply while it is scrolled
to its end; a person reading further up stays where they are, and sending a message returns to
the end. A tool call keeps only its title and status: Claude's Read and Write calls carry whole
files.

The composer, after the reference chat's (since 2026-10-21): a card (`ds::composer_card`)
holding the message and, on one line at its end, the controls that send it, under the thread's
text column with the card's inset outside it; under it, centred and muted, what the agent
reaches ("Reads knowledge at main · writes only to a branch of its own", which was the
composer's tooltip). A notice strip attached to the card's top (`ds::notice_strip`) holds what
the conversation's state asks of the person, a line each, and is not drawn otherwise: the
context window at 85 % or more (the gauge, "88% of the context window used", the token counts),
and, once the agent's branch exists, the branch ("`agent/<stamp>` · ✎ N · **Review**"), also
when it was made before the conversation came back (the record's `branch_created`), with ✎ N
the changes committed on it since the conversation opened in the tab, shown when there are any;
Review switches the session to the agent branch and opens spec 008's review. The composer's
gutter mark and the banner across the panel are gone. Queued messages sit above the card.

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
| Studio serves the tools and executes them on its own session (until 2026-10-13; spec 013 replaced them with the CLI and a gateway) | Point the agent at Omnigraph's `/mcp`; give the agent the CLI and a token | The token would leave Studio, and 0.13's MCP cannot write. Executing in Studio lets one place enforce the agent branch and the approval |
| No MCP server (since 2026-10-28; before, a stdio proxy to a Unix socket) | Keep the proxy for the CLI's path and `omnigraph_run` | A folder per conversation gives the agent its CLI by a fixed name, and both agents run unsandboxed, so their shells reach the gateway (spec 013) |
| Depend on `agent-client-protocol` 2.2.0, default features | Fork Zed's `agent_servers` and `agent_ui`; Zed's unstable feature set | Zed's crates need its project and terminal layers. Stable v1 is what released agents speak |
| No default agent | Default to the Claude preset | Running `npx …@latest` downloads code; the person should choose it |
| Approval is Studio's, not the agent's permission prompt | Rely on the agent asking | Agents differ in whether and how they ask; the guarantee has to hold for every agent |
| One agent branch per agent session | Write to the person's branch; one branch per proposal | Review works per branch (spec 008), and one branch keeps a session's changes together |

## Conversations

Checked against Zed's thread sidebar at `v1.23.2` (`sidebar`, `agent_ui/src/thread_metadata_store.rs`)
and Delta's sidebar.

- **Studio's own index.** Like Zed's metadata store, Studio keeps the list itself
  (`agent/sessions.json` in the data directory, owner-only, format 2): per conversation, the
  agent's session id, the graph (its one key, `<target>/<graph id>`, its CLI target's name and
  its id, since 2026-10-22; a record filed under `<server url>/<graph id>`, as builds from
  2026-10-20 did, stays under it, and no graph's list shows it; one filed under
  `<server name>/<graph id>`, as builds before then did, is the graph's again where that name is
  its target's), the agent command, its title, when it started, the last message the person
  sent, the agent branch its changes go to, and whether it is pinned or archived. Ids and titles
  only; the conversation stays with the agent. The agent's own `session/list` is not the source:
  every graph's agent works in the same directory, so it cannot tell graphs apart. The file is
  read whole: one that does not parse as this build writes it (a record Studio cannot read, a
  field missing or of another type, makes the whole file so), one of another format, or one
  Studio cannot read at all (not text, or held by another program) is set aside whole, renamed
  beside itself, and none of its records is read, so the next save loses nothing (since
  2026-10-22; before, each record was read on its own and one Studio could not read was
  skipped). One it cannot set aside (on Windows, another program holds it open without letting
  it be renamed) is never saved over: the list lasts until Studio quits, and the Conversations
  panel says so above its rows, naming the file. (The first format, one session per graph and agent,
  predates v0.1.0 and is not read.)
- **The launcher.** The Conversations panel ends with "Ask the agent", an outlined pill with
  its icon and `studio_agent::OpenAgent`'s keys as caps (⌘⇧A), as the reference sidebar ends with
  its "Ask" launcher; an empty list says only "No conversations yet."
- **Titles.** The agent's `session_info_update` title wins; until it sends one, the first line
  of the first message (cut at 80 characters); "New conversation" before that.
- **One tab per conversation.** Each conversation is its own Agent tab, titled after it, and
  keeps running while another is shown, so two can sit side by side. Opening one that has a
  tab shows that tab; otherwise the agent starts and replays it with `session/load` (a
  conversation it no longer knows starts anew in that tab). A tab comes back after a restart
  with its conversation. ⌘⇧A shows an open agent tab, else the graph's last conversation with
  the agent, else a new one. New conversation (sidebar, model menu, `studio_agent::NewConversation`)
  opens a new tab and leaves the earlier conversation where it is. ↻ appears only after the
  agent stopped and starts it again on the same conversation.
- **One process per agent.** As Zed keeps one connection per agent server, every conversation
  with an agent (same command and environment) opens a session on one process; the host routes
  each session's updates and permission requests by session id, holds updates that arrive
  before a session is known (up to 256), and asks the agent to close a session (`session/close`,
  when it can) when its tab closes. The process stops when the last conversation and the panel
  let go of it, and is started again on the next use. A conversation the agent cannot load but
  can resume (`session/resume`) comes back without its messages, with a note.
- **Each graph's own directory.** A new conversation runs in `agent/graphs/<graph>-<hash>` in the
  data directory (the key made safe plus a stable FNV-1a hash), so an agent's `session/list` in
  it is the graph's conversations. On Windows it is under the local application folder
  instead (`%LOCALAPPDATA%\Omnigraph Studio\agent\graphs\…`): the data directory is the roaming
  one, which folder redirection can put on a network share, and cmd cannot work in a network
  folder (spec 013). A conversation keeps the directory it started in (recorded with it;
  `sessions::working_dir`), so one started under a graph's older key still resumes where the
  agent keeps it; only a new one gets the directory of the key it has now. Import lists the
  key's directory alone. Earlier conversations ran in the shared `agent/workspace`.
- **The Conversations panel** is a tab of the left sidebar (the agent mark;
  `studio_agent::ShowConversations`), after Delta's threads list, every title on one edge. Its
  title line holds Filter, More (Import from the agent) and New conversation, before the
  sidebar's own search, which searches everything; a filter's funnel keeps the two apart. Then
  Pinned and Recent (labels only when something is pinned), each row the title alone and, at
  the end,
  only what the conversation is doing: a turning circle while it runs, a warning mark while a
  change or tool waits for the person, a red cross when the agent stopped with an error, an
  accent dot when it finished while its tab was not shown (cleared when the tab gains focus).
  When the person last wrote ("just now", "5m ago", "12 Oct") is in the tooltip with the title
  and the agent, not on the row, as Delta's rows carry no times. Rows sort by the person's last
  message, as Zed's sort by the last interaction, so a conversation finishing in the
  background does not move. The shown tab's row is selected. Hovering a row shows Pin and
  Archive; archiving only hides it here (the agent keeps the session, as Zed's archive sends
  nothing), and "Archived 3 ›" folds them away at the end, where Undo brings a row back. What
  a conversation is doing and whether it was seen live in memory only. How an import or a
  delete went is a toast, not a line in the list.
- A right click on a row offers Rename, Pin or Unpin, Archive or Restore, and Delete…. Rename
  edits the title in place (↩ or leaving the field keeps it, ⎋ drops it); the person's title
  wins over the agent's, and an empty one gives the agent's back. Delete… asks first, in a
  dialog confirmed by typing the conversation's title as the list shows it, since neither Studio
  nor the agent can bring it back (constitution IV; since 2026-10-21, before which ↵ confirmed
  it): Studio forgets the conversation and its open tab closes, and the agent deletes it with
  `session/delete` when it can (otherwise the note says why); the graph and the agent's branch
  are not touched. Filter in the title line opens a field on the rows' edge (⎋ closes it); it
  matches titles and agent names and looks through the archive too. "Import from the agent"
  (More) asks the agent for its sessions in the graph's directory (`session/list`, every page up
  to 20) and adds the ones the list lacks, titled and dated as the agent reports; a toast says
  how many.
- A loaded conversation keeps writing to its branch and keeps its own options; a new one starts
  from the options chosen last for the agent.

## Queued messages

ACP runs one turn at a time and has no queue; Zed queues on the client
(`agent_ui/src/conversation_view/message_queue.rs` at `v1.23.2`), and so does Studio. A message
sent while the agent works (or after the first one while it starts, see Lifecycle) waits in a
card above the composer ("2 queued messages", Clear all),
one line each with the next marked by an accent dot; Remove and Edit (back into the composer,
ahead of a draft) on hover, Send now on the next one always and on the rest on hover. When a turn
ends the next message is sent. Stop pauses the queue ("paused until you send") until the person
sends or queues again. Send now (a row's button, or ⌘⇧↩ / Ctrl+Shift+Enter in the composer)
cancels the running turn and sends that message as soon as it ends, ahead of the queue, without
a "Stopped." note. The queue is per tab and in memory, as Zed's.

## Session options and context usage

As in Zed's agent panel (checked in `agent_ui` at `v1.23.2`), and only through stable ACP v1:

- **Options.** The options an agent returns from `session/new` and `session/load`, and later in
  `config_option_update`, are its model, mode, reasoning level and the like. Studio advertises
  `session.configOptions.boolean`, so an on/off option comes as a switch rather than an On/Off
  menu. The controls sit in the composer, on a row under the prompt (as Zed's message editor
  keeps them with the text they send; the window's status bar holds only facts about the open
  tab), and read as Delta's composer footer: borderless, an icon and the value, no chevrons,
  ending with send or stop. The mode
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
  session keeps its own. A file Studio cannot read is set aside as the conversations' is, and
  one it cannot set aside is never written over.
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

- Agent-attributed commits: commits are recorded under the person's actor. Recording "on
  behalf of an agent" needs an engine change, E20 in `docs/deferred-engine-changes.md`.
- Steering (ending a turn at the next tool boundary for a queued message): Zed does it only for
  its own agent; ACP cannot express it.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-28 | The panel serves no MCP server, and each conversation has a folder of its own | `agent_panel` `sessions::tests::each_conversation_has_a_folder_of_its_own`, `cli_home::tests::agents_md_names_the_cli_the_branch_and_the_rules`, `a_conversation_home_holds_its_config_token_and_wrapper`, `panel::tests::a_run_of_steps_reads_as_one_sentence`, `streaming_into_the_last_turn_keeps_the_earlier_turns_blocks`; `cargo test -p agent_panel -p studio`, clippy `-D warnings` | See spec 013's log of the same date. `socket.rs`, `mcp.rs`, `tools.rs` and the `mcp-proxy` command are gone; every permission request is the person's; the thread shows each tool call by its title |
| 2026-10-29 | A conversation restored behind another tab starts no agent until it is shown | `agent_panel::panel::open_session` and `AgentPanel::start_when_shown` by reading: the start is owed to the tab's next draw, as the other views' reads are (`type_table` `a_page_not_drawn_reads_nothing` and siblings test that mechanism); `cargo test -p agent_panel`, clippy `-D warnings` | Pass. Before, every conversation tab the workspace restored started its agent at launch, a process and a replay each, shown or not; one opened from the list starts as its tab draws |
| 2026-10-28 | A rename or a delete begun before a conversation was rebound finishes on it | `agent_panel` `sessions::tests::a_rebound_conversation_is_found_by_its_old_id` (rebound twice; an unknown id left alone), `SessionsPanel::commit_rename` and `delete` by reading | Pass. Before, a rename typed, or a delete confirmed, while the agent opened a new conversation in place of one it could not load went to the old id: the name was lost, and the agent was asked to delete a conversation it never had |
| 2026-10-26 | The first message waiting for the agent can be taken back, and an unseen merge keeps Open history | `studio` `activity::tests::open_history_is_offered_for_an_unsettled_outcome_on_a_listed_branch` (a merge the history has not shown keeps it); the agent panel's Edit compiles and passes clippy; CI on PR #11 (run 40281608899), Check on macOS, Linux and Windows: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` | All pass: 1014 passed on macOS, 1009 on Linux, 979 on Windows; 0 failed. The Edit button itself is not drawn by a test |
| 2026-10-26 | A first message shows in the thread while the agent starts, and the whole flow reads right | `agent_panel` `a_first_message_opens_the_conversation_and_later_ones_queue`, `streaming_into_the_last_turn_keeps_the_earlier_turns_blocks` (the waiting block ends the thread, before the stop's callout), `a_run_of_steps_reads_as_one_sentence`, `host::tests::an_agents_error_reads_without_the_librarys_internals`; offscreen snapshots on the dev graph with a stand-in ACP agent whose start takes 6 to 90 s: a first send, a second send while starting, a start past 20 s, an agent that exits at once, a send while a turn runs, Stop; then Codex (`@agentclientprotocol/codex-acp` 2.2.2) asked to reply "ok" and to count Person nodes, and the conversation opened again | Before: Ragnor's first message on `mr-ops` sat in "1 queued message" with Send now while the agent started, under the empty "Ask about the graph", with only an orange dot to say why, and the restart button showed from the first second. After: the message is in the thread with "Starting the agent…", a second waits in the queue behind it, both go in order once the agent is ready; past 20 s the thread says the agent has not answered, with Start again; an agent that exits shows "Process exited with exit status: 1" (before, the library's JSON with a path into `~/.cargo/registry`) above the message, kept to send. Codex answered "ok" and "200 Person nodes" (the sidebar's count); its run's line now says "used 3 other tools" beside Studio's own, so the parts add up to the turn's tool calls (before, "used 6 tools" read as the total beside "9 tool calls"); the context ring shows its percentage, so a nearly empty one no longer reads as a spinner. The conversation came back with its history. Outside Studio, `codex-acp` answered `initialize` in 3 s and `session/new` in 1 to 4 s with and without Studio's MCP proxy; a `session/load` of a conversation another Codex client holds is refused (`thread_active_writer`), which Studio answers by opening a new one. The ten minutes Ragnor's live message waited were not reproduced |
| 2026-10-24 | A conversations file set aside is told, naming it and where it went | `agent_panel` `a_file_set_aside_is_told_once` (a version-2 file with a `cwd: null` record, as v0.2.1 wrote; the file written again; one that cannot be set aside), `a_file_that_does_not_parse_is_set_aside_whole` (extended: each file set aside yields a notice naming it and the file it went to; one of another version says so) | One record of an older form still sets the whole file aside, with no record read on its own (Studio keeps no backwards compatibility). The index now holds a notice, “The agent conversations list … could not be read (…), so it starts empty; the file is kept as ….unreadable-…”, shown once as an error toast in the active window when the index is first looked at with a window open (`AgentSessions::tell_pending`), as the removed `servers.json` was. A file that could not be set aside holds no notice: the Conversations panel's note says it. Before, the list emptied and nothing said so unless the rename failed |
| 2026-10-21 | With no CLI that reads the home Studio writes, the thread says which CLI it found, why it was refused, and what to install | `agent_panel` `the_cli_that_reads_the_home_studio_writes_is_chosen_without_a_server` (the notice is `no_cli_text`, spec 013) | The notice names the graph's HTTP API and Omnigraph's version-1 operator configuration, each CLI found with its release and reason ("0.14.0-edge+50283ce, which predates that configuration: it has no `config validate`" for the CLI installed here), and ends with the edge build to install, by commit (main `ff542202` or later; `edge-20261021-888223b` is older), the edge installer's command, or `agent.omnigraph_cli`. A configured CLI passed over is named as not speaking the API "with Omnigraph's version-1 operator configuration". Until such a build is installed the agent has no CLI here |
| 2026-10-21 | The approval keys answer only the write card that shows them | `agent_panel` `the_keys_answer_only_the_write_card_that_shows_them` | Before, ⌘Y, ⌘⌥Y and ⌘⌥N answered the oldest waiting card of any kind (`pending_index`), which could be a permission card that showed no keys: with a Bash request waiting above a write card, ⌘Y allowed the shell command and ⌘⌥Y picked the agent's AllowAlways, a lasting rule. Now they answer the oldest waiting write card (`keyed_write`), which alone shows them; a permission card with no write card under it takes no key, and is answered by its buttons |
| 2026-10-21 | A write sent before the agent stopped shows its real outcome, and the branch it made is kept | `agent_panel` `a_write_sent_before_the_agent_stopped_still_shows_its_outcome`, `sessions` `conversations_are_kept_per_graph_newest_first` (extended with `set_branch_created`) | Before, stopping the agent marked a running write's card "Outcome unknown" ("the server did not confirm this change") though the gateway ran the write on and the session logged how it ended, and the panel stopped listening, so a branch that write made was never saved and Review was gone after a resume. Now the card keeps running until the outcome comes through what records the write (the gateway's `GatewayEvent::Finished` is gone), finds its card by its own id after the thread changed, and the branch learns whether it exists and is saved; with the tab closed, the conversation's record still learns it. A card still waiting for the person says "Not sent: the agent stopped before you decided", not "Rejected" |
| 2026-10-21 | A conversation that comes back offers Review of a branch it made before | `agent_panel` `a_resumed_conversation_offers_review_of_a_branch_it_made_before` | Before, the strip needed a change committed in this tab (`changes > 0`), and a resumed conversation starts at 0. Now it shows once the branch exists; ✎ N shows the changes made since the conversation opened, when there are any |
| 2026-10-21 | A streamed token makes only its own turn's blocks again | `agent_panel` `streaming_into_the_last_turn_keeps_the_earlier_turns_blocks` (the blocks equal a rebuild of the whole thread after a token, a turn's end and a change in an earlier turn), `a_write_shows_its_query_once_on_its_card`, `a_changed_entry_changes_its_hash_and_no_other` | Before, every chunk built every block (with `ends.contains` over a list of turn ends, so entries × turns) and its fingerprint, and each query block's `query_on_card` copied the later queries' texts, twice a frame (fingerprint and render). Now the turns before the changed entry's keep their blocks and fingerprints, the turn ends are a set, a query block carries whether its text is on a card, and `query_on_card` reads the texts without copying. Not measured on a long conversation here |
| 2026-10-21 | Another tool's file is not kept for the conversation's life | `agent_panel` `only_studios_tools_keep_what_they_were_given` | Before, every tool call kept its `rawInput` and result, though only Studio's `omnigraph_run` reads them; a Read or Write call kept a whole file, and each rehash serialised it. Now another tool keeps neither its result nor an input past 4 KiB |
| 2026-10-21 | A conversation is deleted only with its title typed | `agent_panel` `a_conversation_is_deleted_only_with_its_title_typed` | Before, ↵ deleted it, though the dialog said it cannot be undone (constitution IV). Now the dialog holds a field, Delete is disabled until the title is typed whole, and ↵ with other words marks the field. Not seen in the running app |
| 2026-10-21 | An agent's write held `as commit` or `as unattached branch` is logged as held, as Studio's own are | `agent_panel` `an_agents_held_write_is_logged_as_held` (the panel's `outcome_kind` through `OutcomeKind::written`) and `a_writes_success_answer_says_what_it_did`, extended (the gateway's `Seen::Unpublished` carries the commit's kind: `unpublished`, `unattached`, a kind this build does not know, and no kind with `published: false`; a `published` kind with `published: false`, and an unknown kind not said to be held, are on the branch) | Before, the gateway kept no kind and the panel logged a held write as `Committed { commit: None }`, so the activity log said "committed to agent/…" for a proposal or a snapshot. Now a proposal is logged held as its commit, to publish to the agent's branch, a snapshot as saved, and an unknown kind leaves whether a branch moved unknown, as for Studio's own writes. The card's tooltip says a snapshot never publishes |
| 2026-10-21 | A write's query shows once, on its card | `agent_panel` `a_write_shows_its_query_once_on_its_card`; offscreen snapshots with the test agent, light and dark, before and after ("list people, then make a change") | Before, the retitle query showed highlighted under "Mutate retitle" with its `--branch` line, then again on the Mutation card. Now a call whose text a write card in its turn carries, or a `mutate` still running (its card on the way or waiting for Allow, which shows the text), is its line alone, with no fold; one that ended with no card shows its text. A waiting approval could not be rendered offscreen: the headless dispatcher never parks while the CLI's command waits, so the snapshot does not finish |
| 2026-10-21 | A query's rows show as a record card, a finished turn ends in its actions, and the composer is a card with its notices on a strip | Offscreen snapshots with the test agent (now in the Claude adapter's wire format) in a scratch data folder against a scratch dev server: "list people, summarize as markdown" and "list people, then make a change", light and dark; `agent_panel` `a_querys_rows_are_read_from_what_its_call_returned`, `a_tool_call_keeps_the_result_its_agent_reported`, `a_turn_ends_at_its_last_reply` | "3 rows · main" over p.name and p.title for three people, and "1 row" for the count; Copy and "5 tool calls" under the last reply; after the change the strip read "agent/20261021-122659 · ✎ 1 · Review". Before, the test agent's calls read as "Used 1 tool" (its adapter was not recognised) and its count as -1 (it parsed Studio's answer with the CLI's stderr in it) |
| 2026-10-21 | Conversations filed under a server's name are not moved (decided: no conversion code) | `AgentSessions::move_named_graphs` and its test `conversations_filed_by_server_name_move_to_the_graphs_key` deleted with `graph_store::named_keys`; `AgentSessions::global` loads the index as it is | Supersedes the 2026-10-20 row on conversations following the graph's key: a record under `<server name>/<graph id>` keeps that graph field, so no graph's Conversations list shows it; one under the key resumes in its own `cwd` as before |
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
| 2026-10-12 | Messages sent while the agent works are queued and sent in order | `agent_panel` test `the_queue_sends_in_order_pauses_on_stop_and_lets_send_now_jump_it`; offscreen snapshots with the test agent's slow turn: two messages queued behind it, then the transcript the agent kept; then one queued and one sent with ⌘⇧↩ | The card read "2 queued messages", the next one dotted with Send now; the agent received the three in order; with ⌘⇧↩ it received the urgent one second and the queued one third |
| 2026-10-12 | One agent process serves every conversation; the list renames, searches, deletes and imports | `agent_panel` tests `updates_go_to_their_session_and_early_ones_wait`, `conversations_are_kept_per_graph_newest_first` (rename, remove, import), `each_graph_has_a_directory_of_its_own`; offscreen snapshots with the test agent (one process for every session, `session/list`, `session/delete`, `session/close`), counting its processes | Two conversations ran on one test-agent process, each with its own tools, in `agent/graphs/environment-knowledge-2998657d`; with two records dropped, Import brought both back ("Imported 2 conversations."); Rename showed "Best-connected people"; "people" left two rows; Delete… asked, removed the row, and the agent deleted its transcript (6 to 5); a session the agent did not know left "Removed here; the agent did not delete it" |
| 2026-10-12 | The Conversations panel reads clean | Offscreen snapshots after "looks messy": a pinned, three recent and an archived conversation; Search open with "comp"; a hovered row | Search, More and + on the title line; one text edge for every row; no times on rows (the tooltip read "you last wrote 12m ago"); "Archived 1 ›" at the end; the search field framed on the rows' edge |
| 2026-10-13 | The thread lays out only what shows and follows the newest reply | Read `gpui/src/elements/list.rs` (`ListState::splice`, `remeasure_items`, `FollowMode::Tail`, `scroll_to_end`) at `v1.23.2`; offscreen snapshots with the test agent: three questions in a 1300×800 window, then an empty conversation | The view stood at the end of the third reply, its markdown at full height (lists, inline code and the code block), the earlier turns above it; the empty conversation still showed "Ask about the graph" |
| 2026-10-14 | Typing in the composer does not grow with the conversation | Snapshot tool on `release-fast` with the test agent in a scratch data folder: one and three markdown replies, a click into the composer, then 200 keys | 0.85 ms of main-thread CPU a key beside one reply, 0.93 ms beside three: the composer and the thread are one view, but the thread's `list` lays out only the replies that show. No change needed. `studio_agent::Ask` leaves focus off the composer, so the measurement clicks into it first |
| 2026-10-16 | Send and the options sit under the prompt | Offscreen snapshot of a conversation tab with text in the composer | Once the status bar became one bar across the window, the composer's controls had landed at the window's bottom edge; they now end the composer, right-aligned under the prompt, and the status bar shows the graph and branch alone |
| 2026-10-20 | The agent panel and the Conversations panel run agents on Windows | Both refusals removed (`panel.rs` `start`, `sessions_panel.rs` `agent`); spec 013's Windows rows | Not run on Windows here: the Windows-only tests run in CI on `windows-2025` |
| 2026-10-20 | An overdue close is not cancelled, and does not free a later conversation's close | `agent_panel` `an_overdue_close_frees_the_session_and_still_runs_to_its_answer` | Past the bound the waiting conversation was woken and the close still ran to its answer; its late answer left a later conversation's close of the session in place. Dropping the unanswered request had sent `$/cancel_request` (`SentRequestCancellation::drop`, agent-client-protocol 2.2.0); the bound is now `FREED_WITHIN` (60 s), not 15 s |
| 2026-10-20 | Sessions files of another build are set aside, not lost | `agent_panel` `a_record_that_cannot_be_read_leaves_the_others` | A record without `cwd`, with `cwd: null`, or without its marks sets the file aside (renamed beside itself, byte for byte); a file of version 3, 1 or none is set aside with nothing read |
| 2026-10-20 | A conversation that asked for the session before the close was sent is freed by the close, not told to try again | `agent_panel` `a_conversation_waiting_before_the_close_is_sent_is_freed_by_the_close` (new), `a_conversation_waits_for_a_start_given_up_only_so_long` (its close step now waits); run on macOS | Before the fix the new test failed with `Err(Late { closing: true })`: the waiter's own bound, started at its claim and shorter than the close's, ran out while the close was under way. After it the waiter waited past its own bound (1 s) and was routed once the close was overdue (2 s after sending, no sooner); a waiter for an open still hears "try again" at its bound |
| 2026-10-20 | A sessions or preferences file Studio could not read is never written over | `agent_panel` `a_file_that_could_not_be_read_is_never_written_over`, `a_preferences_file_that_could_not_be_read_is_never_written_over` (run on macOS; a folder at each name the file would be set aside as stands in for a program holding it); `a_file_another_program_holds_is_never_written_over` (Windows only: a handle without delete sharing, then without any; runs in CI on `windows-2025`, not run here) | Before the fix: a sessions file that was not UTF-8 was not set aside, and both files were written over when the rename failed. After it: a file that is not text is set aside like one that does not parse; one that cannot be set aside stays byte for byte through a save, the changes kept in memory, and `not_saved` names it for the Conversations panel's note |
| 2026-10-20 | On Windows agents work in the local application folder | `agent_panel` `agents_work_in_the_local_folder_of_studios_roaming_data` (run on macOS) | A data folder under `%APPDATA%`, local or on a `\\server\share`, maps to the same name under `%LOCALAPPDATA%`; a data folder set elsewhere keeps its agents |
| 2026-10-20 | Conversations follow the graph's key, and an old one resumes where it ran | `agent_panel` `conversations_filed_by_server_name_move_to_the_graphs_key` (records written as a build before wrote them, loaded with the server list `dev` → `http://127.0.0.1:8787`); `graph_store` `a_name_resolves_only_to_a_known_server`; the panel's working directory now `sessions::working_dir` (`panel.rs`, `start`), the record's own `cwd` when resuming | `dev/crm` became `http://127.0.0.1:8787/crm`, its `cwd` still `graph_cwd("dev/crm")`, and `latest` found it under the key; a new conversation gets `graph_cwd` of the key, another directory; `gone/crm`, a name no server has, stayed; the next load read the record under the key. Import lists only the key's directory, so a conversation the agent has in the old directory and the list lacks is not offered. Not yet seen in the running app |

## Corrections

- 2026-10-22 · "Conversations" gave a conversation's graph key as `<server url>/<graph id>`; it is
  `<target>/<graph id>` since `1b5a110`. It said each record of the index is read on its own and
  one Studio cannot read is skipped; the file is read whole, and one that does not parse is set
  aside whole with none of its records read.

- 2026-10-21 · The approval section named the second button "Always for this branch"; the card
  says "Always on this branch", as the permission section and spec 013 do, and now so does this
  plan.
- 2026-10-12 · A tool call the agent never closed kept its spinner turning after the turn
  ended, which redrew the window at the display's rate while idle. When a turn ends, tools still
  shown pending or running take Completed (the turn finished) or Failed (it was stopped, failed
  or the agent left); when the agent exits, Failed.
- The socket first lived in `<data dir>/agent/`. With a long data directory the path exceeded
  macOS's 104-byte limit and binding failed ("path must be shorter than SUN_LEN"). It now lives
  in the per-user runtime directory described above, with a length check that fails with a
  clear message.
- Stop was planned as ⌘.; Zed's editor binding takes it inside the composer, so it is ⌘⇧.
- 2026-10-12 · Approval said Allow was ⌘↩ and Reject ⎋. ⌘↩ is Send in the composer and ⎋
  belongs to the editor, so the card uses the bound ⌘Y, ⌘⌥Y and ⌘⌥N, as in Actions.
- 2026-10-20 · Raising a close's bound to 60 s made a conversation that asked for its session
  before the close was sent (a restart, or one during a long load) time out on its own 60 s
  bound first, counted from its claim, and hear "try again". A waiter's own bound now covers
  only the agent's open; a close frees it within the close's bound, counted from sending.
- 2026-10-20 · Setting the sessions file aside was not checked: when the rename failed, or when
  the file could not be read at all (not UTF-8, or held by another program), the next save
  wrote over the file Studio promised to keep. Such a file is now never saved over.
- 2026-10-21 · The approval keys answered the oldest waiting card of any kind, so with an agent's
  permission request waiting above a write card ⌘Y allowed the request and ⌘⌥Y granted the
  agent's lasting rule, on a card that showed no keys. They now answer write cards alone.
- 2026-10-21 · Stopping the agent called a write already sent "Outcome unknown", though it ran on
  and its outcome was logged; a branch it made was not saved, and a resumed conversation lost its
  Review. The card now hears the outcome, the branch is saved, and Review shows once the branch
  exists.
- 2026-10-21 · Deleting a conversation, which cannot be undone, was confirmed with ↵. It is now
  confirmed by typing its title.
