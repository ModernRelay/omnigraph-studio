# 032: Agents drive Studio's window

Status: steps 1 and 2 built · 2026-11-16 · checked end to end with Claude Code against the dev graph (Validation log)

## Problem

Ragnor asked (2026-11-16) for Studio's actions to be exposed "to internal agent but also
external as MCP to drive the studio". Today an agent can work the graph but not the window. The
agent in Studio's panel reaches the graph only through `./omnigraph` and Studio's gateway
(spec 013), so it can answer "which projects have no owner?" but cannot open those projects'
table, show one on the canvas, or put a query in front of the person. An agent outside Studio
(Claude Code in a terminal, an IDE agent) cannot reach Studio at all, except by opening an
`omnigraph-studio://` link, which shows one graph, node or query and returns nothing.

The person wants to say "show me the projects with no owner, on a canvas from Acme" and see
Studio do it. They also want an outside agent to put its findings on screen in Studio, without
copying ids by hand.

## Principles

- I: the tool server makes no request to an Omnigraph server. A tool acts on the window, and the
  views it opens read through `graph_client` as they do when the person opens them.
- III and IV: no tool writes to a graph, and no tool answers a write card or the agent's own
  permission requests. Writes stay on the CLI and its cards (spec 013), so one rule set and one
  card list remain.
- V: a tool opens a tab or moves the selection in the same way the person's command does. It
  never moves a tab the person is reading without the person seeing it happen (see Focus).
- VI and VIII: every tool is an existing Studio action with a doc comment and a binding or a
  palette entry. The server adds no behavior that the person cannot invoke themselves.
- IX: every answer is capped in size, every call has a timeout, and the server keeps a fixed
  number of connections.
- XII: the claims below were checked in source at the pins. What still needs a live agent is
  listed as open in the Validation log.
- XIII: a tool names a thing and a view, as a sidebar row and the view strip do.

## What exists today (checked)

Studio (main at `26d293af`):

- Every action is a GPUI action in `crates/studio_actions/src/studio_actions.rs`. Its doc
  comment becomes its palette description. 34 actions take arguments and derive
  `serde::Deserialize` and `schemars::JsonSchema`. Among them: `graph::OpenType { type_name,
  at_commit, … }`, `graph::OpenNode`, `graph::OpenNodePage`, `graph::ShowNodeOnCanvas`,
  `graph::ShowNodeInTree`, `graph::ShowSetInTree`, `graph::ShowResultsOnCanvas`,
  `graph::OpenMap`, `graph::OpenSource { type_name }`, `graph::OpenBranch`,
  `graph::OpenBranchHistory`, `graph::OpenCommit`, `graph::NewQueryWithText { text }` ("It is not
  run"), `workspace::ActivatePanel` and `workspace::OpenLink { url }`.
- `studio_actions::offered::is_offered(action, cx)` says whether an app-wide action is offered
  now (`studio_actions.rs:995`).
- `studio_actions::Thing` (`Schema`, `Type`, `Node { type_name, id }`, `Branch`) and `ViewKind`
  (`Table`, `Page`, `Tree`, `Canvas`, `Map`, `Source`, `Review`, `History`) name what a tab shows
  and how it shows it (spec 020). Every workspace item answers `thing(cx)`
  (`crates/studio_workspace/src/item.rs:185`).
- The snapshot runner already builds an action from a name and JSON and dispatches it into a
  window, after it gives the workspace focus: `cx.build_action(name, data)` and then
  `window.dispatch_action` (`crates/studio/examples/snapshot.rs:428-448`).
- `omnigraph-studio://` links open a graph, a node or a query to read, and never run a query or
  add a server (`crates/studio/src/deep_link.rs:63-81`, `crates/studio/src/lib.rs:682`).
- Studio has served no MCP server since 2026-10-28 (spec 013, "No MCP server"). The removed
  server was `crates/agent_panel/src/mcp.rs` (258 lines), `socket.rs` (784) and `tools.rs` (230),
  deleted in `0591de9c`. It ran as Studio's own executable, started again as
  `omnigraph-studio mcp-proxy <socket>` once per conversation. The debug executable is 490 MB.
- The agent's gateway serves the CLI over HTTPS on `127.0.0.1`, with a per-session token and a
  certificate made in memory when the gateway is bound (spec 013, "The gateway's TLS"). A
  listener holds 64 connections, and a connection that does not show the token is the first to
  be closed (spec 013, 2026-10-22).
- Layers (`docs/architecture.md`): `studio_workspace` is layer 4, `agent_panel` is a layer-5
  feature crate, and `studio` (layer 6) initializes everything. Feature crates never depend on
  each other.

Zed (`v1.27.0`, `c84af5b99cbc`, `crates/gpui/src`):

- `App::all_action_names`, `App::action_schemas(generator)`,
  `App::action_schema_by_name(name, generator)` and `App::action_documentation()` list every
  registered action with its JSON schema and doc comment (`app.rs:2763-2805`).
  `App::build_action(name, data)` builds one from JSON (`app.rs:2753`).
- `Window::dispatch_action(Box<dyn Action>, cx)` dispatches on the focused element and is
  deferred (`cx.defer`), and it returns nothing (`window.rs:2595`).
  `Window::is_action_available` and `Window::available_actions` say what the focused element
  handles (`window.rs:3390`, `:6813`).

Agents, run live on 2026-11-16 against a stub Streamable HTTP server that logged every request
(Validation log):

- **In the panel.** A minimal ACP client passed
  `{"type": "http", "name": "studio", "url": …, "headers": [{"name": "Authorization", …}]}` in
  `session/new`, and each adapter called the tool and finished the turn:
  - claude-agent-acp 0.107.3 advertises `mcpCapabilities: {http: true, sse: true}`. Its Claude
    Code (2.1.325) asked the client for no permission, and titled the call
    `mcp__studio__studio_windows`.
  - codex-acp 2.7.0 advertises `{http: true, sse: false, acp: false}` at run time. The `false`
    in its source (`dist/index.js:24613`) is only a fallback. It turns an HTTP entry into Codex's
    `url` plus `http_headers` config (`dist/index.js:41190`, `:41210`). With
    `INITIAL_AGENT_MODE=agent-full-access`, as Studio starts it (spec 013), it asked no
    permission, and titled the call `mcp.studio.studio_windows`.
- **Outside Studio.** Claude Code 2.1.267 (`claude -p --mcp-config`, `"type": "http"` with a
  `headers` entry) and Codex 0.167.2 (`mcp_servers.<name>.url` with `bearer_token_env_var`) both
  connected over plain HTTP to `127.0.0.1` and called the tool. Codex asks before every MCP call
  unless the tool has `approval_mode = "approve"` (`mcp_servers.<name>.tools.<tool>`); with
  `approval_policy = "never"` and no such entry, the call is refused.
- **What they send.** Claude Code first POSTs `server/discover` (`MCP-Protocol-Version:
  2026-07-28`). When that is answered "method not found", it falls back to `initialize` with
  protocol `2025-11-25`, then opens a `GET /mcp` for a server stream and carries on when that is
  answered 405. Codex sends `initialize` with `2025-06-18` and no GET. Neither client sends an
  `Origin` header. Every POST carries the bearer token.

## Design

### Pieces

| Piece | Where | Role |
|---|---|---|
| `UiTool` and its registry | `studio_workspace` (layer 4), module `ui_tools` | A tool is a name, an action to build, a schema and a result. Features register theirs at init, as they register actions |
| The state reader | `studio_workspace::ui_tools` | Lists windows, tabs, panels and the selection as identifiers, never as row values |
| The server | new crate `ui_tool_server` (layer 5) | MCP over Streamable HTTP on `127.0.0.1`. It dispatches into the window and waits for the result |
| The internal agent's entry | `agent_panel` | Gives each conversation the server as an MCP server, through a global that `studio` sets, so `agent_panel` does not depend on `ui_tool_server` |
| Setting and copy | `studio` and `studio_settings` | The outside-agent switch, its address and the token file |

### What a tool is

A tool is a Studio action plus four things the action alone lacks:

1. **An allow-list entry.** Only actions in `ui_tools::ALLOWED` are tools. The set is opt-in, so
   a new action is not a tool until someone adds it here.
2. **A target.** Every call names a `window` (from `studio_windows`), and the server gives that
   window's workspace focus before it dispatches, as the snapshot runner does. A call to a
   window that has closed is refused with `window_closed`.
3. **A precondition.** Being available does not mean an action will do anything. With no graph
   session, `NewQueryWithText` is available on the workspace, and its handler returns without
   doing anything (`query_editor.rs:371-374`; Validation log). So each tool states what it
   needs (a session, a type the schema has, a branch that exists), and the server checks that
   before it dispatches. If the check fails, the answer is `not_available` with the reason in
   words ("No graph is open in this window"). `is_action_available` is also checked, for
   actions registered with `register_action_when`.
4. **A result.** `dispatch_action` returns nothing and runs in `cx.defer`, so the action takes
   effect when the effects of the update that dispatched it are flushed, not inside it. No
   frame needs to be drawn (Validation log). The server dispatches in one `update_window` and
   reads the window in the next one. It answers with what changed: the tab that opened or came
   to the front (`thing`, `view`, `title`), or the panel that was shown. If nothing changed, it
   answers `no_change`. A handler that starts a read answers as soon as its tab exists. The tab
   loads its data afterwards, as it does when the person opens it. No timeout is needed, because
   nothing waits on a frame. If the window closed between the two updates, the answer is
   `window_closed`.

The tool's name is the action's name with `::` replaced by `_` and the `studio`
namespaces shortened (`studio_graph::OpenType` becomes `graph_open_type`). Its description is the
action's doc comment, and its input schema is `action_schema_by_name` plus a `window` field.

### The first tools

The first set is small and only navigates and shows things.

| Tool | Action | What it does |
|---|---|---|
| `graph_open_type` | `graph::OpenType` | Open a type's table, optionally filtered to a node or to the nodes related to it |
| `graph_open_node` | `graph::OpenNode` | Show one node in the inspector and the Explore panel |
| `graph_open_node_page` | `graph::OpenNodePage` | Open a node's page |
| `graph_show_node_on_canvas` | `graph::ShowNodeOnCanvas` | Open a canvas walk from a node |
| `graph_show_node_in_tree` | `graph::ShowNodeInTree` | Open the relationship tree from a node |
| `graph_open_map` | `graph::OpenMap` | Open the schema's map |
| `graph_open_source` | `graph::OpenSource` | Open the schema's source, at a type |
| `graph_open_branch` | `graph::OpenBranch` | Open a branch's review tab |
| `graph_open_branch_history` | `graph::OpenBranchHistory` | Open a branch's history |
| `graph_open_commit` | `graph::OpenCommit` | Open one commit |
| `graph_new_query_with_text` | `graph::NewQueryWithText` | Open a query tab with text in it. It does not run |
| `workspace_activate_panel` | `workspace::ActivatePanel` | Show a dock panel |

Plus three tools that only read the window:

| Tool | Answers |
|---|---|
| `studio_windows` | Each window: an id, its server's name, its graph, its branch, and whether it is in front |
| `studio_tabs` | A window's panes: each tab's `thing`, `view`, title, whether it is pinned to a commit and which one, and which tab is active |
| `studio_selection` | The active tab's selection as identifiers: type, node ids (at most 200), and the walk's root on a canvas |

The read tools return identifiers and names only: type names, node ids, branch names, commit
ids and tab titles. They never return property values, row contents or query results. An agent
that needs the data reads it with its own credential (the panel's agent through `./omnigraph`
and the gateway; an outside agent through its own CLI login), so Cedar still decides what each
agent may read.

### Never a tool

These are left out of the allow-list on purpose, and a test fails if any of them is added:

- Anything that writes: `table` edit, delete and undo actions, `graph::CreateBranchAt`,
  `graph::UndoDelete`, merge, the query editor's `Run`, and every write card.
- The agent panel's `Allow`, `AllowAlways`, `Reject`, `Stop`, `Restart`, `Ask` and
  `OpenConversation`. If the agent could call these, it could approve its own writes or start
  more agents.
- `workspace::OpenLink`: a link can name a server, and opening it would pass the tool's input to
  the link parser unchecked.
- Settings, sign-in, sign-out, updates and quitting.
- Focus moves and editor keystrokes: they only make sense as keys.

`query::Run` stays out until read-only runs exist. Studio cannot tell a read from a mutation
before it sends the query, except through `gq_language`, and a mutation must go to a card.

### Focus

A tool call must not take a tab away from the person while they read it. A tool opens its tab as
the pane's preview tab where the action supports `preview`, so the person's pinned tabs stay. A
tool never closes a tab or changes a filter on an open tab. If the window is not in front, the
tool opens its tab there and does not raise the window. The window shows one chip in the status
bar, "Opened by <agent>", for 4 seconds; the chip uses the motion rules in VII.

### The panel's agent

When a conversation starts, `agent_panel` asks the `UiToolEndpoint` global (set by `studio` when
the server is listening) for an entry for the conversation, and passes it in `session/new`:

- If the agent advertises `mcpCapabilities.http` in `initialize`, as both adapters Studio
  offers do, the entry is `McpServer::Http { name: "studio", url:
  "http://127.0.0.1:<port>/mcp", headers: [Authorization: Bearer <token>] }`.
- Otherwise there is no entry. That agent then has no UI tools, and nothing else changes. No
  stdio relay is needed for the agents Studio offers.

Each conversation has its own token, and that token is bound to the conversation's window, so
`window` is fixed and the agent cannot drive another graph's window. `AGENTS.md` in the
conversation's folder gets one paragraph: the UI tools exist, they only show things, and writes
still go through `./omnigraph`.

Every UI tool is pre-allowed: Studio answers a permission request for a call to its own server
with Allow, without asking the person. No tool writes, and each call already shows in the
thread. Neither adapter asked in the live runs, but Codex in a sandboxed mode may ask, and so
may Claude in a stricter permission mode. The adapters choose the tool's title
(`mcp__studio__graph_open_type`, `mcp.studio.graph_open_type`). The thread recognizes either
form for the `studio` server, and shows the result's words instead: "Opened Project · Table".

### Outside agents

Off by default. The switch is in Settings, under Agents:

- **Let agents outside Studio show things in Studio.** When on, Studio writes
  `<data dir>/ui-tools/endpoint.json` (mode 0600, with `studio_settings::write_atomic`) with
  `url` and `token`. The token is 32 random bytes, encoded as base64url, and it changes each
  time Studio starts. The row below the switch shows the command that adds Studio to Claude Code,
  `claude mcp add --transport http omnigraph-studio <url> --header "Authorization: Bearer $(…)"`,
  with a Copy button that reads the token from the file instead of showing it. For Codex it
  shows the `config.toml` entry: `url`, `bearer_token_env_var`, and an `approval_mode =
  "approve"` line per tool, since Codex otherwise asks before each call.
- When off, the server is not listening and the file is deleted.

An outside agent can call every tool in any window. The first call from a token in a Studio run
shows a toast in the window it targets: "An agent outside Studio is showing things here", with one
action, Turn off, which opens the setting. The toast follows VII and appears once per run.

### The server

- MCP over Streamable HTTP, `POST /mcp` with plain JSON answers: `initialize`,
  `notifications/initialized` (answered 202), `tools/list`, `tools/call` and `ping`. No SSE
  stream, no resources, no prompts.
- Protocol versions `2025-06-18` (Codex) and `2025-11-25` (Claude Code). `initialize` echoes the
  client's version when it is one of these, and otherwise offers `2025-11-25`. Any other method,
  `server/discover` included, gets JSON-RPC error -32601, which is what makes Claude Code fall
  back to `initialize`.
- `GET /mcp` and `DELETE /mcp` get 405 with `Allow: POST`. The server keeps no session, so it
  sends no `Mcp-Session-Id`.
- It binds to `127.0.0.1` on a port the operating system chooses. Plain HTTP: the token is the
  only proof, as for every loopback MCP server that Claude Code and Codex connect to, and the
  certificate the gateway uses would not be trusted by an outside client.
- A request without `Authorization: Bearer <token>` gets 401 before its body is read. A request
  whose `Origin` header is anything other than absent or `null` is refused with 403, so a web
  page cannot reach the server through the browser (DNS rebinding).
- At most 16 connections, 64 KiB per request body, and 64 KiB per answer. A `studio_selection`
  answer larger than that is cut, with `truncated: true`.
- At most 4 calls in flight per token, so one agent cannot queue a burst of tabs. A fifth call is
  refused with `busy`.
- The protocol code is the removed `mcp.rs` (`0591de9c^`), moved to the new crate and changed
  from a socket to HTTP. No MCP crate is added.

### What is never logged

The token, the tool's input text (a query's text is input), node ids and tab titles. The log
records the tool's name, the window id and the outcome.

### Steps

1. The registry, the read tools and the server, with the outside-agent switch. Tests:
   `#[gpui::test]` with `add_window_view` for each tool's result, `not_available`, `no_change`
   and `window_closed`; the allow-list test; the server's 401, 403, 405, -32601, size and
   `busy` refusals; and `initialize` answering both protocol versions.
2. The panel's agent: an HTTP entry, the permission auto-answer, the thread's titles, and
   `AGENTS.md`.
3. More tools as people ask for them, each with its own row in this plan.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| UI tools only; the graph stays on the CLI | Graph tools in the same server (query, mutate) | Spec 013 removed MCP graph tools so that writes would have one way and one card list. UI tools do not write, so serving them brings that problem back for none |
| An allow-list of actions | Every registered action, minus a deny-list | A new action would become a tool without review. Allow and Reject would be one mistake away from an agent approving its own writes |
| Read tools return identifiers, never values | Return the table's rows as the person sees them | An agent would read the graph with the person's token, without Cedar seeing the agent. With identifiers alone, the data is read under the agent's own credential |
| Check each tool's precondition, then report what changed | Rely on `is_action_available`; return as soon as the action is dispatched; wait for a frame | An available action can still do nothing (no session), and a dispatch says nothing. What changed is known as soon as the deferred dispatch has run, with no frame, so a frame wait would only add a timeout to handle |
| Streamable HTTP on loopback with a bearer token, for both kinds of agent | A Unix socket; stdio through `omnigraph-studio mcp-proxy`; the gateway's HTTPS | Both adapters and both CLIs connected over plain HTTP in live runs. A socket needs a relay. A stdio relay starts the 490 MB executable once per client. The gateway's self-signed certificate would not be trusted |
| Plain JSON answers, no server stream | SSE answers and a GET stream | Both clients take JSON, and Claude Code goes on when GET gets 405. A stream would only matter for progress on long calls, and no tool is long |
| Off by default for outside agents, with a token per run | Always on; a token that never changes | Any local process that can read the data dir could drive the window. The switch makes this the person's choice, and a token per run limits how long a copied token works |
| A new layer-5 crate, wired by a global | The server in `agent_panel`; the server in `studio` | Outside agents do not need the agent panel. `studio` is the binary and should stay thin. A global keeps feature crates independent |
| Preview tabs, never closing or refiltering | Tools that close tabs or change open filters | V: nothing moves under the person. An agent may add to the window, but it may not take anything away |

## Deferred

- Read-only query runs as a tool, once Studio can tell a read from a mutation with `gq_language`
  before sending it, and the run is capped as IX requires.
- Tools that change an open tab (filters, sort, a canvas layout). Each needs an Undo, as IV asks
  of what Studio can put back.
- No engine change is needed. An agent with its own scoped credential (E20) would let an outside
  agent read the graph without a login of its own. That is a convenience; this plan does not wait
  on it.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-16 | GPUI lists every action with its schema and doc comment, builds one from JSON, and dispatches it without a result | Read `crates/gpui/src/app.rs:2753-2805`, `window.rs:2595`, `:3390`, `:6813` at `c84af5b99cbc` | Confirmed. `dispatch_action` defers and returns `()` |
| 2026-11-16 | Building and dispatching an action by name already works in Studio | Read `crates/studio/examples/snapshot.rs:428-448` | Confirmed, with the workspace given focus first |
| 2026-11-16 | The candidate actions take their arguments as JSON with schemas | Read `crates/studio_actions/src/studio_actions.rs` | 34 actions derive `Deserialize` and `JsonSchema`. `NewQueryWithText` says "It is not run" |
| 2026-11-16 | Studio's earlier MCP code can be reused | `git show 0591de9c --stat` | `mcp.rs` 258 lines, `socket.rs` 784, `tools.rs` 230 |
| 2026-11-16 | Feature crates cannot reach each other, so the agent panel needs a global | `docs/architecture.md`, "Crate layers" | Confirmed |
| 2026-11-16 | Both adapters take an HTTP MCP server in `session/new` and call its tool | A Node ACP client (initialize, `session/new` with one HTTP entry and a bearer header, one prompt; it answers any permission request with the first allow option) driving `npx -y @agentclientprotocol/claude-agent-acp@0.107.3` and `@agentclientprotocol/codex-acp@2.7.0` (with `INITIAL_AGENT_MODE=agent-full-access`), against a Python stub that logged each request without the token | Both advertised `http: true` and called the tool, and the turn ended `end_turn`. Neither asked for permission. Titles: `mcp__studio__studio_windows` and `mcp.studio.studio_windows` |
| 2026-11-16 | codex-acp's source says `http: false` | `grep mcpCapabilities` in the 2.7.0 package | Source fallback `false` (`dist/index.js:24613`); HTTP converted to `url` plus `http_headers` (`:41190`, `:41210`); it reports `true` at run time |
| 2026-11-16 | Claude Code and Codex connect from outside to plain HTTP on loopback with a bearer token | `claude -p --strict-mcp-config --mcp-config <file>` (2.1.267); `codex exec -c mcp_servers.studio.url=… -c mcp_servers.studio.bearer_token_env_var=…` (0.167.2) | Both called the tool. Codex refused the call until `mcp_servers.studio.tools.studio_windows.approval_mode="approve"` was set |
| 2026-11-16 | What the clients send | The stub's log | Claude Code: `server/discover` first (`2026-07-28`); after -32601, `initialize` `2025-11-25`; then `GET /mcp`, and goes on after a 405. Codex: `initialize` `2025-06-18`, no GET. No `Origin` from either. Every POST carried the token |
| 2026-11-16 | An action built by name takes effect when the dispatching update's effects flush, with no frame | A temporary `#[gpui::test]` in `query_editor` (removed after the run): a `Workspace` window, `cx.build_action("studio_graph::NewQueryWithText", {text})`, `window.dispatch_action`, the active tab read inside the same update and right after it | Inside the update: no tab. After it: "Query 1". A second dispatch with no draw in between opened "Query 2" |
| 2026-11-16 | `is_action_available` says whether the action will do anything | The same test, before `set_session` | Available `true`, and no tab opened: the handler returns when there is no session (`query_editor.rs:371-374`). So the design checks a precondition per tool |
| 2026-11-16 | The first set's handlers are on the workspace, and most can open a preview tab | `grep register_action` per action; `preview` fields in `studio_actions.rs` | All use `workspace.register_action`, except `ActivatePanel`, an `on_action` on the workspace's root element (`studio_workspace.rs:5447`). Every one but `NewQueryWithText` and `ActivatePanel` has `preview` |
| 2026-11-16 | The server's refusals and both protocol versions | `cargo test -p ui_tool_server` (11 tests): raw HTTP against a bound listener | No token: 401 with `www-authenticate: Bearer`, answered while a declared 40,000-byte body never came. `Origin: http://rebound.example`: 403 even with the token; `Origin: null` and none: 200. GET and DELETE: 405, `allow: POST`, no `mcp-session-id`. `server/discover`, `resources/list`: -32601. A notification: 202, empty. A body over 64 KiB: 413; an answer over 64 KiB: a tool error with the call's id. A fifth call while four wait: `busy`; after the four are answered, the next goes through. `initialize` echoes `2025-06-18` and `2025-11-25`, offers `2025-11-25` for `2026-07-28` |
| 2026-11-16 | The registry: never-a-tool, the listed schemas, and each outcome | `cargo test -p studio_workspace ui_tools` (6 tests), a stand-in tab behind a registered `OpenMap` | `never_a_tool` passes (60 named actions and 8 namespaces). Schemas carry `window` (required unless the token is a conversation's), no `preview`, no `$ref`; the list is under 32 KiB. No graph: `not_available`, "No graph is open in this window". Opened as the preview, "Opened Map", chip "Opened by Claude Code" gone after 4 s. Same call: `no_change`. Closed window and unknown id: `window_closed`. Another conversation's window: `not_available`. 250 selected ids: 200 sent, `truncated: true`. An outside agent's first call: the toast "An agent outside Studio is showing things here" |
| 2026-11-16 | A tool's result is ready in the update after the dispatch, for every tool in the first set | `cargo test -p studio --lib ui_tools_tests` (10 tests): every feature's real handler, a `followed_session` on `dev/cluster/knowledge.pg` | `graph_open_type` Person: opened, type Person, Table, "Opened Person · Table"; again: `no_change`; with `filter_id` while it is open: `not_available`; `Planet`: "Planet is not a node or edge type of knowledge". `graph_open_node_page`, `graph_open_node`: Page of `person-000000`. `graph_show_node_on_canvas`: Canvas, `studio_selection` root `person-000000`. `graph_show_node_in_tree`: Tree. `graph_open_map`: schema, Map; `graph_open_source`: Source. `graph_open_branch` main: branch main, History; `graph_open_branch_history` main then: `no_change`; `nope`: `not_available`. `graph_open_commit` c1: opened. `graph_new_query_with_text`: opened, no read sent. `workspace_activate_panel` branches: `panel_shown` |
| 2026-11-16 | The panel passes the server, allows its calls and titles them by their result | `cargo test -p agent_panel` (`the_ui_tool_server_goes_to_an_agent_that_takes_http`, `studio_ui_tool_calls_are_allowed_and_titled_by_their_result`, `agents_md_names_the_cli_the_branch_and_the_rules`) | `session/new` gets `{"type":"http","name":"studio","url":…,"headers":[{"name":"Authorization","value":"Bearer …"}]}` only with `http: true`; the launch's `Debug` never shows the token. With the tools: `mcp__studio__graph_open_type` and `mcp.studio.studio_windows` answered `AllowOnce` with no card; `mcp__studio__run_query` and any call without the tools ask. Titles: "Studio · graph_open_type" until the result, then "Opened Project · Table" (content) and "Opened Map" (raw output) |
| 2026-11-16 | End to end: an outside Claude Code drives a headless Studio | `script/dev-server` (`omnigraph-server` 0.15.1, contract 0.14, `knowledge`); `target/debug/examples/snapshot` with `OMNIGRAPH_STUDIO_DATA` holding `{"agent":{"outside_agents":true}}`, its `run:` step starting `claude-accounts exec auto -- claude -p --strict-mcp-config --mcp-config <file>` (2.1.316) with the url and token from `ui-tools/endpoint.json`, then `wait:120000` | Claude Code: `studio` connected. `studio_windows`: "1 window", `{"window":4294967297,"server":"environment","graph":"knowledge","branch":"main","front":true}`. `graph_open_type` `{"type_name":"Person","window":4294967297}`: "Opened Person · Table". `studio_tabs`: one preview tab, Person, Table; panels `types`, `inspector`. The screenshot shows the Person table as the preview tab. `ui-tools/` was `0700`, and empty after the run ended |
| 2026-11-17 | The outside notice shows in the first window a call names, and the endpoint files are written off the main thread | `cargo test -p studio_workspace ui_tools`, `-p ui_tool_server`; `held_at` added to the query editor (`at_commit`) and a held Blob tab | `studio_windows` with the notice owed: no toast, `announced: false`; the next call: the toast, `announced: true`; four waiting calls all owe it, and the next after one showed it does not. `endpoint.json` and `token` are written and deleted on Studio's write thread, which quit waits for |
| open | The adapters pass the result's words to the thread as the panel reads them (Claude in `content`, Codex in `rawOutput`) | A conversation in Studio with claude-agent-acp 0.107.3 and codex-acp 2.7.0 | Not yet run; the panel reads both places and falls back to "Studio · <tool>" |

## Corrections

| Date | What the plan said | What was built | Why |
|---|---|---|---|
| 2026-11-16 | The switch is "under Agents" | Under Agent, the settings section that holds the agent setting | Studio's section is named Agent |
| 2026-11-16 | "When off, the server is not listening" | The server listens while Studio runs; the switch decides whether the outside token is accepted and the endpoint file exists. Off, an outside token gets 401 | The panel's conversations use the same server, so it cannot stop listening when outside agents are turned off |
| 2026-11-16 | `endpoint.json` alone holds the token | `endpoint.json` holds `url` and `token`; `ui-tools/token` beside it holds the token alone, also 0600 | The copied commands read it with `$(cat …)`, so they never show it and need no JSON reader. Both go when the switch is turned off and at quit |
| 2026-11-16 | Tool names shorten the `studio` namespaces | `studio_<x>::Name` is `<x>_name`; the workspace's own actions (namespace `studio`) are `workspace_<name>` | `ActivatePanel` is `studio::ActivatePanel`; the rule needs a word for the bare namespace |
| 2026-11-16 | Features register their tools at init | One allow-list, `ui_tools::ALLOWED`, each row with what it needs | Every need of the first set reads only the workspace and its session, so a registration step would add nothing a row does not |
| 2026-11-16 | The server gives the window's workspace focus before it dispatches | Only when nothing in the window has focus, as the snapshot runner does | Taking focus would take the keyboard from a field the person types in (V) |
| 2026-11-16 | `graph::OpenNode` shows a node in the inspector and the Explore panel | It opens the node's page, as it has since spec 020; its doc comment, which is the tool's description, now says so | The handler (`type_table.rs`) opens a page; the comment was stale |
| 2026-11-16 | The first set never changes a filter on an open tab | `graph_open_type` with `filter_id` while that type's table is open is `not_available`, saying how to show it | The type table's handler filters an open table to the node (`show_only`) |
| 2026-11-16 | Read tools use what tabs already answer | `Item` gains `held_at`, `selected_ids` and `view_kind`, answered by the table, page, tree and canvas | Tabs named neither their commit nor a selection of ids, and an edge table has no view strip to name its view |
| 2026-11-16 | Turn off "opens the setting" | A new action, `studio::OpenAgentSettings` (⌘⌥, ; Ctrl+Alt+, elsewhere), opens Settings at Agent; the toast offers it | A toast's follow-up is an action, and `OpenSettings` opens no section |
| 2026-11-16 | Answers are `no_change`, `not_available`, `window_closed` | Also `opened`, `shown`, `panel_shown`, `read`, `invalid_input`, `unknown_tool`, `busy` and `too_large`; each answer is two texts, the words then the JSON | The agent and the thread need to tell an opened tab from one brought forward, and the thread shows the words |
| 2026-11-16 | A conversation's token is bound to its window | Also `studio_windows` lists only that window for it | Naming other windows' graphs to a conversation that cannot act in them says more than it needs |
| 2026-11-16 | The address and token are copied once | The row says they change each time Studio starts, so the person copies the command again then | The port is the system's choice and the token is per run, so a copied config stops working at the next start |
| 2026-11-17 | "The first call from a token in a Studio run shows a toast in the window it targets" | The first call that names an open window shows it, whatever the call did (a read, a refusal, an action); the main thread, which runs calls one at a time, asks whether it is still owed just before each call and marks it given once a window showed it | An outside agent's first call is `studio_windows`, which targets no window: counted then, the notice never showed; and calls sent together each owed it when read, so each showed it (review of #126) |

