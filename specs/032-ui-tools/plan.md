# 032: Agents drive Studio's window

Status: draft · 2026-11-16

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

Agents (spec 013's checks, 2026-10-13):

- ACP `session/new` takes `McpServer::Stdio { name, command, args, env }`, which every agent
  supports. HTTP and SSE servers are capability-gated (`agent-client-protocol` 2.2.0, schema
  1.9.1).
- claude-agent-acp drops an `mcpServers` entry written with `"type": "stdio"`. It passes on
  server instructions, cut at 2,048 characters. Every MCP tool call asks the client for
  permission unless the tool is pre-allowed.
- Codex runs stdio MCP servers outside its seatbelt sandbox, where they can reach loopback, and
  its MCP tool timeout is 300 seconds.

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

A tool is a Studio action plus three things the action alone lacks:

1. **An allow-list entry.** Only actions in `ui_tools::ALLOWED` are tools. The set is opt-in, so
   a new action is not a tool until someone adds it here.
2. **A target.** Every call names a `window` (from `studio_windows`), and the server gives that
   window's workspace focus before it dispatches, as the snapshot runner does. A call to a
   window that has closed is refused with `window_closed`.
3. **A result.** `dispatch_action` returns nothing, so each tool also defines how to tell what
   happened. After the dispatch, on the next frame, the server reads the window's state and
   answers with the active tab's `thing`, `view` and `title`. If the active tab did not change
   and the action was not available on the focused element (`is_action_available` was false
   before the dispatch), it answers `not_available` and says which action and window it was. If no frame comes within 2 seconds, it
   answers `timeout`. A timeout means the effect is unknown; the call is not sent again.

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

- If the agent advertises MCP over HTTP in `initialize`, the entry is
  `McpServer::Http { url: "http://127.0.0.1:<port>/mcp", headers: [Authorization: Bearer
  <token>] }`.
- Otherwise there is no entry. The panel's agent then has no UI tools; nothing else changes.
  (Open: whether claude-agent-acp and codex-acp advertise HTTP. If neither does, step 2 brings
  back a stdio proxy. See Deferred.)

Each conversation has its own token, and that token is bound to the conversation's window, so
`window` is fixed and the agent cannot drive another graph's window. `AGENTS.md` in the
conversation's folder gets one paragraph: the UI tools exist, they only show things, and writes
still go through `./omnigraph`.

Every UI tool is pre-allowed in the agent's permission request handling: each call already shows
in the thread by its title, and none of them writes. A call shows as "Opened Project · Table",
the result's title.

### Outside agents

Off by default. The switch is in Settings, under Agents:

- **Let agents outside Studio show things in Studio.** When on, Studio writes
  `<data dir>/ui-tools/endpoint.json` (mode 0600, with `studio_settings::write_atomic`) with
  `url` and `token`. The token is 32 random bytes, encoded as base64url, and it changes each
  time Studio starts. The row below the switch shows the command that adds Studio to Claude Code,
  `claude mcp add --transport http omnigraph-studio <url> --header "Authorization: Bearer $(…)"`,
  with a Copy button that reads the token from the file instead of showing it.
- When off, the server is not listening and the file is deleted.

An outside agent can call every tool in any window. The first call from a token in a Studio run
shows a toast in the window it targets: "An agent outside Studio is showing things here", with one
action, Turn off, which opens the setting. The toast follows VII and appears once per run.

### The server

- MCP 2025-06-18 over Streamable HTTP, `POST /mcp` only: `initialize`, `tools/list`,
  `tools/call`, `ping`. No SSE stream, no resources, no prompts.
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
   `#[gpui::test]` with `add_window_view` for each tool's result, `not_available` and
   `window_closed`; the allow-list test; the server's 401, 403, size and `busy` refusals.
2. The panel's agent: an HTTP entry where the agent advertises it, and `AGENTS.md`. If neither
   agent advertises HTTP, bring back the `mcp-proxy` stdio relay to the same server.
3. More tools as people ask for them, each with its own row in this plan.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| UI tools only; the graph stays on the CLI | Graph tools in the same server (query, mutate) | Spec 013 removed MCP graph tools so that writes would have one way and one card list. UI tools do not write, so serving them brings that problem back for none |
| An allow-list of actions | Every registered action, minus a deny-list | A new action would become a tool without review. Allow and Reject would be one mistake away from an agent approving its own writes |
| Read tools return identifiers, never values | Return the table's rows as the person sees them | An agent would read the graph with the person's token, without Cedar seeing the agent. With identifiers alone, the data is read under the agent's own credential |
| A result read on the next frame | Return as soon as the action is dispatched | `dispatch_action` is deferred and says nothing. Without a result, the agent cannot tell an opened tab from an action that was not available |
| Streamable HTTP on loopback with a bearer token | A Unix socket; stdio through `omnigraph-studio mcp-proxy`; the gateway's HTTPS | Claude Code and Codex connect to HTTP MCP servers directly. A socket needs a relay. A stdio relay starts the 490 MB executable once per client. The gateway's self-signed certificate would not be trusted |
| Off by default for outside agents, with a token per run | Always on; a token that never changes | Any local process that can read the data dir could drive the window. The switch makes this the person's choice, and a token per run limits how long a copied token works |
| A new layer-5 crate, wired by a global | The server in `agent_panel`; the server in `studio` | Outside agents do not need the agent panel. `studio` is the binary and should stay thin. A global keeps feature crates independent |
| Preview tabs, never closing or refiltering | Tools that close tabs or change open filters | V: nothing moves under the person. An agent may add to the window, but it may not take anything away |

## Deferred

- A stdio relay for agents that cannot reach HTTP MCP (step 2, only if needed).
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
| open | claude-agent-acp and codex-acp advertise MCP over HTTP in `initialize` | Run both with a stub ACP client and read `agentCapabilities.mcpCapabilities` | Not yet run |
| open | Claude Code and Codex, as outside agents, connect to a plain-HTTP loopback MCP server with a bearer header | `claude mcp add --transport http`; Codex's `mcp_servers` config with an HTTP URL | Not yet run |
| open | A tool's result is ready on the next frame for every tool in the first set | One `#[gpui::test]` per tool | Not yet written |

## Corrections
