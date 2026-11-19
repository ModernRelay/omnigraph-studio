# 032: Agents drive Studio's window

Status: steps 1 and 2 built · step 3 (the canvas and the tree, the settings, the port) built 2026-11-17 · amended 2026-11-18 (the tools in their own crate, a tab's hooks named for no caller, nothing started until an agent is served; see "Where each part lives")

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

As amended 2026-11-18 (the first plan put the tool list and the state reader in
`studio_workspace`; see "Where each part lives" and Corrections):

| Piece | Where | Role |
|---|---|---|
| `UiTool`, the allow-list, the adapter and the state reader | `ui_tool_server` (layer 5), module `tools` | A tool is a name, an action to build, a schema and a result. The adapter checks, dispatches and says what changed; the reader lists windows, tabs, panels and the selection as identifiers, never as row values |
| The server | `ui_tool_server`, modules `http` and `mcp` | MCP over Streamable HTTP on `127.0.0.1`, started when an agent is first to be served |
| What a window and a tab answer any reader | `studio_workspace` (layer 4) | `Workspace::pane_tabs`; `Item::outline`, `settling`, `catch_up`, `refuses`, with `selected_ids`, `held_at` and `view_kind`. Named for no caller, and unused they cost nothing |
| The internal agent's entry | `agent_panel` (`UiToolEndpoint`, `UiToolEntry`) | Gives each conversation the server as an MCP server, through a global that `studio` sets, so `agent_panel` does not depend on `ui_tool_server` |
| The binary's wiring | `studio`, module `ui_tools` | The one module that names `ui_tool_server`: the start, the entries, New token and the settings' rows |
| Setting | `studio_settings` | The outside-agent switch and the rest of `agent.*` |

### Where each part lives

Amended 2026-11-18, at Andrew's request: "make system modular and for people who do not want to
use mcp not being affected by bloat at all". A review of #126 and #127 that day found the server
isolated but what the tools mean spread through the window frame, the tab trait, both graph
views and the store, and found that every Studio paid for the server at launch.

**One command surface.** A person's key, the palette and a tool reach the same action handlers.
The tool crate is an adapter over them and adds no second way to do anything.

| Layer | Crate | What it holds of this plan |
|---|---|---|
| 0 | `studio_actions` | The vocabulary: the action types and their schemas, as before |
| 3 | `graph_store` | What an expansion opens and costs (`plan_expand`, `opening`), for every caller |
| 4 | `studio_workspace` | The seams below, and the status bar's "Opened by" chip. No tool, no tool name, no MCP |
| 5 | the views | They implement the seams. They know no tool and no agent |
| 5 | `agent_panel` | The conversation's entry and how a call to Studio's server is known by its title |
| 5 | `ui_tool_server` | Everything else: the list, the adapter, the words an agent reads, MCP, HTTP, tokens |
| 6 | `studio` | One module of wiring |

The adapter sits beside the server at layer 5 because it reaches a view only through the tab
trait. It was first a module of `studio_workspace` because the layer check forbids one layer-5
crate from depending on another and the agent panel needed three of its names; those moved to
the panel, with which names are tools carried by the entry `studio` hands it.

**The seams.** What the workspace and a tab answer, and why each is there:

| Seam | What it is | Before |
|---|---|---|
| `Item::refuses(action, on)` | Why the tab would not do what the action asks, as a fact (`Refusal`): a node not shown, nothing named, a result picture that is not a walk, a relationship not open, more than is opened without asking | `agent_check(tool: &str, input: &Value)`: the view matched the tool's name as a string and parsed the call's JSON again. A tool renamed in the list would have left its check off without a word |
| `Item::outline` | What a view that walks the graph shows, as identifiers and the names drawn (`ViewOutline`) | `agent_view` |
| `Item::settling` | The tab still reads or lays out what the last action on it asked for | `agent_busy` |
| `Item::catch_up` | Does now what the tab leaves for its next draw (`docs/drawing.md`), for a reader in a window the system is not drawing | None: a call waited the whole 15 s on a minimized or covered window |
| `Workspace::pane_tabs` | The window's panes and tabs in order | The adapter read the workspace's private fields |

A refusal states the fact and the tool crate words it for an agent, tool names included
(`tools::refused`). So `graph_canvas` and `relationship_tree` name no tool, and the words of
this plan ("Not on this canvas; expand a neighbour or use graph_show_node_on_canvas") are
written once, where the tool names live. The action is built before the view is asked, so
arguments that do not fit answer `invalid_input` with what does not fit.

**Nothing runs until an agent is served.** `ui_tool_server::init` only follows the settings.
The server starts when a conversation asks for its entry or while agents outside Studio are let
in. Until then Studio binds no port, makes no token, asks the keychain nothing and writes no
file; the one thing done is removing an endpoint file an earlier run may have left, once, on
the write thread. The kept token is read from the keychain the
first time agents outside Studio are let in, never for a conversation. With outside agents off
by default (below), a person who starts no conversation runs none of this plan's code but that
one removal and a settings observer.

**What keeps it so.** `script/check-layers.py`, which CI runs, fails when a crate other than
`studio` depends on `ui_tool_server`; when code outside `crates/studio/src/ui_tools.rs` and its
two test modules names the crate; or when a crate other than the tool crate, the binary and the
agent panel names a UI tool (the names are read from the list). A build-time switch was
rejected (Decisions).

### What a tool is

A tool is a Studio action plus four things the action alone lacks:

1. **An allow-list entry.** Only actions in `tools::ALLOWED` are tools. The set is opt-in, so
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
| `graph_open_branch` | `graph::OpenBranch` | Open a branch's review tab; a branch the window has not listed yet is looked for in the branch list read once first (spec 043) |
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
tool never closes a tab or changes the filters of a tab the person has open: a filtered table
(`graph_open_type` with `filter_id`) opens as a tab of its own, which the person can close, and
the type's open table is left as it is. If the window is not in front, the tool opens its tab
there and does not raise the window, unless the person turned on "Bring Studio to the front
when an agent shows something". The window shows one chip in the status bar, "Opened by
<agent>", for 4 seconds; the chip uses the motion rules in VII.

The canvas and the tree are the exception, at the person's request (2026-11-16): an agent walks
them as the person does. Their tools change the open tab they act on, the canvas or tree in
front, and only it: they expand and collapse, show more, hide, pin, select and move the camera.
None writes, every one is an action the person invokes too, and the walk keeps the person's own
Back and Forward ([`WalkBack`], `]`) and Reset layout.

A call never takes the keyboard (amended 2026-11-18). An action that gives what it opens or
shows the keyboard, as opening a tab and showing a panel by its key do, has it taken back to
where the person had it. The exception is a place the call made disappear, a panel it hid or a
tab it put another in front of: then what took the keyboard keeps it, since keys must not go on
driving something nobody sees. The bare window holding the keyboard, as after the last tab
closed, is the keyboard nowhere in particular, and what a call opens takes it, as a tab the
person opens does. A view tool's action is dispatched on its tab
(`FocusHandle::dispatch_action`), which need not hold the keyboard for it, so a canvas or a tree
is walked while the person types elsewhere.

### The canvas and the tree

Every action of `studio_actions::canvas`, `studio_actions::tree` and the map
(`studio_ontology`) is a tool, and so are those a person reached only by pointer before, now
actions with a palette entry:

| Tool | Action | What it does |
|---|---|---|
| `canvas_select_node` | `canvas::SelectNode` | Select a node by type and id, as a click does |
| `canvas_expand` | `canvas::ExpandNeighbourhood` | Expand nodes' neighbourhoods: `nodes`, `relationships` (`out:WorksAt`, `in:Knows`), `depth` 1 or 2, `limit_per_relationship`, `confirm` |
| `canvas_collapse` | `canvas::CollapseNodes` | Collapse nodes (the chip menu's Collapse all) |
| `canvas_show_more` | `canvas::ShowMore` | "+K more" on a relationship: `node` and `relationship`, or the one selected |
| `canvas_hide_nodes`, `canvas_show_hidden` | `canvas::HideNodes`, `canvas::ShowHidden` | Hide nodes with what hangs off them; draw them again |
| `canvas_pin_nodes`, `canvas_unpin_nodes` | `canvas::PinNodes`, `canvas::UnpinNodes` | Keep where they are drawn; let the layout place them |
| `canvas_reroot_here` | `canvas::RerootHere` | A walk from the node in a tab of its own (the chip menu's "Canvas from here") |
| `canvas_expand_node`, `canvas_collapse_node` | `ExpandNode`, `CollapseNode` | Expand or collapse one node |
| `canvas_move_left`/`right`/`up`/`down` | `MoveLeft` … | Move the selection to the nearest chip that way |
| `canvas_open_selected_page`, `canvas_focus_selected`, `canvas_follow_relationship` | `OpenSelectedPage`, `FocusSelected`, `FollowRelationship` | A node's page; centre on it; follow a single-target relationship to its end |
| `canvas_walk_back`, `canvas_walk_forward` | `WalkBack`, `WalkForward` | The walk's history |
| `canvas_select_all`, `canvas_clear_selection` | `SelectAll`, `ClearSelection` | Mark every chip; unmark |
| `canvas_fit_to_window`, `canvas_zoom_in`, `canvas_zoom_out`, `canvas_reset_layout` | `FitToWindow` … | The camera and the layout |
| `graph_show_results_on_canvas` | `graph::ShowResultsOnCanvas` | A query's result picture |
| `table_show_row_on_canvas` | `table::ShowRowOnCanvas` | Walk from the row under the table's cursor |
| `map_back`, `map_show_domains`, `map_select_next`, `map_select_previous`, `map_open` | `studio_ontology::…` | The schema's map |
| `tree_select_node`, `tree_expand`, `tree_collapse` | `tree::SelectNode`, `tree::ExpandNeighbourhood`, `tree::CollapseNodes` | As the canvas's |
| `tree_select_next`, `tree_select_previous`, `tree_expand_row`, `tree_collapse_row`, `tree_expand_all`, `tree_collapse_all`, `tree_open_selected`, `tree_reroot_here`, `tree_show_on_canvas`, `tree_open_table`, `tree_open_page`, `tree_clear_filter`, `tree_toggle_empty_groups` | every `studio_tree` action | As its key does |

Plus two reads: `studio_canvas` (the root; the nodes drawn as `[type, id, name]`, at most 500
with `truncated`; edges as `[from, to, relationship]` by index; relationships as `[node,
relationship, other type, count, open, shown]`; the selection, zoom, node cap, history, marked,
pinned and hidden) and `studio_tree` (the root; the relationships shown and the nodes under each
open one). Names are what the person already sees on the chip or row, never another property.
`studio_selection` on a canvas or tree also counts what is drawn, so an agent knows there is
more to read.

**Targeting.** A per-node tool takes an optional `node: {type_name, id}`, which the view selects
first by `SelectNode`, as a click does. A node not drawn answers `not_available`: "Not on this
canvas; expand a neighbour or use graph_show_node_on_canvas" (the tree: "Not in this tree;
expand a relationship that holds it or use graph_show_node_in_tree").

**Results.** A view tool waits, at most 15 seconds of the executor's clock, for its view to have
read and laid out what it opened, then answers with what changed: the nodes and edges drawn, the
nodes added and removed and the edges added (at most 200 each, with `truncated`), the
relationships left closed with their counts, the selection, the zoom, and `cap_reached`. An
expansion answers `expanded`, `needs_confirmation` or `cap_reached`.

**Expansion.** `canvas_expand` and `tree_expand` plan through the exploration
(`Exploration::plan_expand`): each node's path and its relationships' paths, opened together by
`set_open`, so the reads go out in the same batches as Expand all (spec 005, 006). Its cost is
`Exploration::opening`, the same count the person's question shows. Past it
(`Opening::asks`), the tool answers `needs_confirmation` with the nodes and reads, opens nothing,
and opens it when called again with `confirm: true`; the person's own invocation asks
(`confirm_expand`). Depth 2 opens the first `limit_per_relationship` neighbours of each
relationship the first level read, once it has read them; a second level past the question
stops, and the answer says what it would open. The node cap stays absolute: the walk is drawn
capped, and the answer is `cap_reached` with what was drawn.

**Left out**, with the reason:

- `canvas::ActualSize`: the canvas has no handler; it is an image viewer's.
- The chip menu's "Show as" (the property a type's nodes are named by): it writes the person's
  settings for the graph, not the walk.
- "Group by" on a relationship: it changes how a relationship reads its neighbours, with a
  property to choose; deferred until an agent needs it.
- "Copy key": the agent has the id already.
- Retry on a chip whose read failed: Refresh reads the walk again; an agent sees the failure as a
  relationship with no count.
- Dragging a chip and panning: `canvas_pin_nodes`, `canvas_focus_selected` and the zoom tools
  cover what an agent needs; positions in pixels mean nothing to it.
- The tree's sentence filter on a relationship (`ask`): it sends the sentence to a model
  (spec 019), a call the person makes with their own key.
- The tree's per-relationship sort, filter and grouping controls: they change how a relationship
  reads; deferred with "Group by".

None of the canvas's or tree's interactions writes to the graph (survey of 2026-11-17).

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

On by default (2026-11-16, at Ragnor's request; kept on 2026-11-18). Since 2026-11-18 the
server starts only when an agent is let in, so with this switch off, and no conversation
started, no server runs, no keychain item is read and no token file is written. The switches are
in Settings, under Agent. Their names say what they are (renamed 2026-11-18, at Ragnor's request: "Let agents outside Studio show things in Studio" did not say it was an MCP server, which is the word a person setting up Claude Code or Codex looks for). Their keys in `settings.json` are unchanged:

- **Run Studio's MCP server for external agents.** When on, Studio writes
  `<data dir>/ui-tools/endpoint.json` (mode 0600, with `studio_settings::write_atomic`) with
  `url` and `token`. The token is 32 random bytes, encoded as base64url, and it changes each
  time Studio starts. The row below the switch shows the command that adds Studio to Claude Code,
  `claude mcp add --transport http omnigraph-studio <url> --header "Authorization: Bearer $(…)"`,
  with a Copy button that reads the token from the file instead of showing it. For Codex it
  shows the `config.toml` entry: `url`, `bearer_token_env_var`, and an `approval_mode =
  "approve"` line per tool, since Codex otherwise asks before each call.
- When off, an outside token is refused and the file is deleted.
- **Keep the server's address and token between starts**, on by default. Studio asks for the port it used
  last, then `agent.ui_tools_port` (24187, below) and the nine after it, passing over any another
  process holds, and only then takes one the system chooses; the row says which port is in use
  when it is not the one asked for, and that the command must be copied again. It never takes a
  port from another process. The token is kept in the system keychain (item
  `omnigraph-studio://ui-tools`, through `graph_store::sign_in::PlatformKeychain`), so a copied
  command keeps working across restarts; `endpoint.json` and `token` are still written for the
  copied commands. **New token** replaces it, confirmed with ↵ since nothing is lost: every agent
  holding the old one gets 401 from then on. Off, the address and token are made per run.
- **Give the agent panel Studio's UI tools**, on by default. The server starts with
  the first conversation that asks for its entry. Off, a conversation names no UI tool server,
  passes none in `session/new`, asks for no entry, and `AGENTS.md` says nothing of one.
- **Bring Studio to the front when an agent shows something**, off by default.

### The port

24187 (`studio_settings::DEFAULT_UI_TOOLS_PORT`). It is in the registered range 10000-49151 and
below 32768, so no system hands it out: macOS and Windows take ephemeral ports from 49152-65535
and Linux from 32768-60999; it is also below Kubernetes' NodePorts (30000-32767). IANA lists
24007-24241 as unassigned (the registry of 2026-11-10; Validation log), so it and the nine
fallbacks are free there. No common development tool or database defaults to it (3000-3999, 4000,
5000-5999, 8000-8999, 9000-9999, 5432, 6379, 11211, 27017, the MCP inspector's 6274 and 6277),
and Studio uses none near it: the dev server's 8787, the sign-in bed's 8791, and the OAuth
callbacks and the agent gateway, which take ports the system chooses.

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
3. The canvas and the tree (2026-11-17): every canvas, tree and map action as a tool, the
   pointer's interactions as actions, `node` targeting, `studio_canvas` and `studio_tree`,
   `canvas_expand` and `tree_expand`; the settings for outside agents, a kept address on port
   24187, the panel's agent and bringing the window forward. Tests: a `#[gpui::test]` per tool
   (`crates/studio/src/ui_tools_canvas_tests.rs`), and the expansion's two nodes, relationship
   filter, `needs_confirmation` round trip, `cap_reached` and depth 2.
4. More tools as people ask for them, each with its own row in this plan.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| UI tools only; the graph stays on the CLI | Graph tools in the same server (query, mutate) | Spec 013 removed MCP graph tools so that writes would have one way and one card list. UI tools do not write, so serving them brings that problem back for none |
| An allow-list of actions | Every registered action, minus a deny-list | A new action would become a tool without review. Allow and Reject would be one mistake away from an agent approving its own writes |
| Read tools return identifiers, never values | Return the table's rows as the person sees them | An agent would read the graph with the person's token, without Cedar seeing the agent. With identifiers alone, the data is read under the agent's own credential |
| Check each tool's precondition, then report what changed | Rely on `is_action_available`; return as soon as the action is dispatched; wait for a frame | An available action can still do nothing (no session), and a dispatch says nothing. What changed is known as soon as the deferred dispatch has run, with no frame, so a frame wait would only add a timeout to handle |
| Streamable HTTP on loopback with a bearer token, for both kinds of agent | A Unix socket; stdio through `omnigraph-studio mcp-proxy`; the gateway's HTTPS | Both adapters and both CLIs connected over plain HTTP in live runs. A socket needs a relay. A stdio relay starts the 490 MB executable once per client. The gateway's self-signed certificate would not be trusted |
| Plain JSON answers, no server stream | SSE answers and a GET stream | Both clients take JSON, and Claude Code goes on when GET gets 405. A stream would only matter for progress on long calls, and no tool is long |
| On by default for outside agents, with a kept address and token (2026-11-16, at Ragnor's request; 0.4.8 had it off). On 2026-11-18 Andrew proposed off by default, since on, every Studio listened on 24187, kept a token in the keychain and wrote it to a file at each start; Ragnor kept it on | Off by default | A command copied once keeps working across restarts, and an agent the person points at Studio connects without a trip to Settings. A person who wants none of it turns it off once: then there is no listener, no keychain item and no token file. What protects a person while it is on: the token file is 0600 in a 0700 folder; the server listens on loopback only and refuses a non-null `Origin`; no tool writes; the first call of a run shows the notice with Turn off; New token refuses every copy of the old one |
| The tools in one crate at layer 5; the workspace and the views with seams named for no caller (2026-11-18) | The list and the adapter in `studio_workspace`; hooks named `agent_*` that take a tool's name | The window frame is under every feature, so what sits in it is everyone's. A view that matches tool names holds a second copy of the list, which nothing keeps in step |
| A view's refusal is a fact, worded by the tool crate | The view writes the agent's words | The words name tools ("use graph_show_node_on_canvas"), and a tool's name is the tool crate's alone |
| A view's action dispatched on its tab | Give the view the keyboard for the dispatch and put it back after | The view held the keyboard for the call's wait, up to 15 s, and the person's keys drove it |
| The server started at first need | Started at launch | Launch paid for a port, a keychain read and a file whether or not an agent was ever used; on a debug build the keychain asked for the login password at each rebuild |
| A check in `script/check-layers.py` keeps the tools in their crate | A Cargo feature `ui-tools` on `studio`, on by default, with a CI build without it | `script/bundle-mac` builds a release with `--no-default-features` when Xcode's Metal compiler is present, so a default feature would be dropped from the macOS bundle unless that script named it. The check costs no build and guards the same line |
| The canvas and the tree change the open tab they act on | No tool changes an open tab | The person asked (2026-11-16) for an agent to walk a canvas and expand neighbourhoods as they do. Each change is the person's own action on the view in front, keeps their Back, Reset layout and Show hidden, and writes nothing; other tabs are still never changed |
| A new layer-5 crate, wired by a global | The server in `agent_panel`; the server in `studio` | Outside agents do not need the agent panel. `studio` is the binary and should stay thin. A global keeps feature crates independent |
| Preview tabs, never closing or refiltering | Tools that close tabs or change open filters | V: nothing moves under the person. An agent may add to the window, but it may not take anything away |

## Deferred

- Read-only query runs as a tool, once Studio can tell a read from a mutation with `gq_language`
  before sending it, and the run is capped as IX requires.
- Tools that change an open table's filters or sort, and a relationship's grouping or filters on
  the canvas or in the tree. The canvas and the tree are walked by tools now (above): their
  changes are the person's own actions, with Back, Collapse, Show hidden and Reset layout to put
  them back.
- No engine change is needed. An agent with its own scoped credential (E20) would let an outside
  agent read the graph without a login of its own. That is a convenience; this plan does not wait
  on it.

Left open by the amendment of 2026-11-18, each found by the review of #126 and #127 and none
fixed here:

- **An action that answers.** A handler still returns nothing: the adapter asks `refuses`
  first, then dispatches, then compares the outline before and after. A person's own invocation
  that would be refused still does nothing without a word (`ExpandNeighbourhood` with nothing
  selected). The end of this line is one `perform(action) -> outcome` per view, reached by the
  key and the tool alike; it needs the views' `on_action` lists and a direct call to share one
  table, so that no second list has to be kept in step.
- **A view tool needs its tab drawn once.** The action is dispatched through the frame drawn
  last, so a canvas opened by one call in a window that is never drawn answers `not_available`
  to the next. `catch_up` covers the wait, not this.
- **The outline's open JSON.** `ViewOutline::selected` and `facts` are JSON, so "never a
  property value" holds there by convention, not by type.
- `limit_per_relationship` has no upper bound, and depth 2 reads each first-level relationship
  as far as it before anything is priced (IX).
- `tree_open_table` on a relationship's row filters the type's open table, as the person's own
  click does: the rule that a tool never changes an open tab's filters is kept only for
  `graph_open_type` (`UiTool::apart_when_filtered`).
- "Bring Studio to the front" calls `Window::activate_window` only, which does not pass
  another application's window on macOS.
- The port used last is tried before `agent.ui_tools_port`, so a port set after the first start
  is not asked for.
- The 30 s a request may take covers its run as well as its read, while calls run one at a
  time: a call queued behind two that each wait 15 s loses its connection and still runs.
- The agent's name is kept per token, and every outside agent shares one token.
- Hiding a node leaves the chips that hang off it marked; nothing on the canvas says nodes are
  hidden.
- `http.rs` holds a second copy of the agent gateway's loopback HTTP pieces: two layer-5 crates
  cannot share them, and no lower crate holds them yet.
- Four of step 3's actions are palette-only and ten that take arguments have no binding
  (VIII).

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
| 2026-11-17 | Review of #126 (head `c60873e`): Studio allows only its own server's calls unasked | `cargo test -p agent_panel studio_ui_tool_calls_are_allowed_and_titled_by_their_result`, `-p studio_workspace a_studio_call_is_known_by_either_title_and_its_own_name_only` | Before: `mcp__studio__graph_open_type` was allowed with no card while the conversation held an entry, whatever server answered to `studio`, and an `AllowAlways` option was chosen when no allow-once was offered. After: the server is `studio_` plus six random letters or digits per conversation; with the entry but no `Ready`, and after a `Ready` saying no server was passed (no HTTP), every call asks; after it, only `mcp__studio_k3x9q2__…` and `mcp.studio_k3x9q2.…` UI tools are allowed once; `mcp__studio__…`, `mcp.studio.…` and a request offering only Allow always each show a card |
| 2026-11-17 | Showing a panel leaves the keyboard where it was | `cargo test -p studio --lib ui_tools_tests` (11 tests) | With the fix turned off, `workspace_activate_panel` `queries` moved focus from the query editor to the panel (FocusId 4 to 11); with it, the editor kept it. `ActivatePanel`'s doc comment named `explore`, `nodes` and `history`; it now names exactly the window's panels: `activity`, `agent-conversations`, `branches`, `commits`, `inspector`, `queries`, `types` |
| 2026-11-17 | An outside agent's first call of any kind shows the notice, once | `cargo test -p studio_workspace ui_tools` | `studio_windows`, then `graph_open_map`, then `studio_tabs`, with the notice owed until an answer says it was shown: the toast once, from `studio_windows`, in the window in front |
| 2026-11-17 | The log, a dropped answer, a token turned on again, the copied command | `cargo test -p ui_tool_server` (14 tests) | A name that is not a tool is logged as `unknown`. A call whose reply was let go answers `cancelled` with the window it named (was `window_closed` for window 0). Off then on: the same token, its state new, the notice owed again. The settings say Claude Code keeps the token in `~/.claude.json` once its command runs |
| 2026-11-17 | Keys in a panel the tool hides do not stay there | `cargo test -p studio --lib ui_tools_tests` (12 tests): the queries panel holds the keys, then `workspace_activate_panel` `types` in the same dock | Before: the keys went back to the hidden queries panel. After: the active tab, Person, has them; a panel that takes the keys when shown keeps them |
| 2026-11-17 | 24187 and the nine after it are unassigned, and below every system's ephemeral range | IANA's `service-names-port-numbers.csv` and `.xml` (`<updated>2026-11-10</updated>`), fetched and searched for 10000-32767; the systems' documented ephemeral ranges | `,24007-24241,,Unassigned` (CSV line 13576): 24187-24196 inside it. macOS and Windows 49152-65535, Linux 32768-60999, Kubernetes NodePorts 30000-32767: none overlaps |
| 2026-11-17 | The address is kept, a taken port passed over, and the token kept | `cargo test -p ui_tool_server` (16 tests): `a_taken_port_is_passed_over_never_taken`, `the_kept_token_lives_in_the_keychain` (a `MemoryKeychain`) | A free port taken; held by another listener: the next; the one used last first; all ten held: one the system chose outside them; not kept: the system's. The token read from the keychain; New token refuses the old one and is kept in its place; turned off, the kept token is deleted and the run takes its own |
| 2026-11-17 | The four agent settings and their defaults | `cargo test -p studio_settings` (`the_agents_window_settings_default_as_the_person_asked`, `the_documented_defaults_are_the_defaults`), `-p agent_panel with_the_setting_off_the_agent_gets_no_ui_tools`, `-p studio_workspace the_window_comes_to_the_front_only_when_the_person_asked` | Outside agents, a kept address and the panel's tools on, bringing forward off, in `default.json` too. Off, the panel's agent gets no server and no `AGENTS.md` paragraph. A window behind stays behind unless bringing forward is on |
| 2026-11-17 | Every canvas, tree and map tool, against the real views | `cargo test -p studio --lib ui_tools` (68 tests: 56 in `ui_tools_canvas_tests`), a fake server of 60 people at one company, the first six knowing the next two | `canvas_expand` on p1 and p2 (depth 1): `expanded`, p3 and p4 added with their names and edges. `relationships: ["out:WorksAt"]`: c0 alone; `out:Owns`: `not_available`. 60 nodes at once: `needs_confirmation` (60 nodes), nothing opened; with `confirm`: opened. Cap 12: `cap_reached`, at most 12 drawn. Depth 2 on `out:Knows`: p3, p4 added. A node not drawn: the words of the plan. Every other canvas and tree tool answers as its key does; `studio_canvas`, `studio_tree` and `studio_selection`'s counts read what is drawn; the canvas tab is "Canvas: Person 0" |
| 2026-11-17 | A filtered table opens beside the type's own | `graph_open_type_opens_the_table` | With Person open, `filter_id` opened a second tab; the same call again: `no_change` |
| 2026-11-18 | The list and the adapter move out of the workspace with no change in behaviour | `cargo test -p ui_tool_server -p agent_panel -p studio_workspace -p graph_canvas -p relationship_tree` and `cargo test -p studio --lib ui_tools`, on the commit that only moves them | 285 and 69 tests pass, the tests that passed before the move. `studio_workspace` no longer depends on `schemars` |
| 2026-11-18 | A view is asked about the action as built, and every refusal reads as it did | `cargo test -p studio --lib ui_tools` (70 tests; those that assert a refusal's words are unchanged); `arguments_that_do_not_fit_say_which` | `relationships: "out:Knows"`, a string: `invalid_input`, naming the value and "expected a sequence"; before, `not_available`, "`depth` is 1 or 2". `depth: 3`: `not_available`, "`depth` is 1 or 2", the view's own |
| 2026-11-18 | A view that is not drawn settles when asked to catch up | `a_canvas_not_drawn_catches_up_when_asked` (`graph_canvas`), `a_tree_not_drawn_catches_up_when_asked` (`relationship_tree`): the view made and never drawn, `catch_up` called as the adapter's wait calls it | Before: a layout (the tree: its rows) owed, and settling. After ten calls 40 ms apart: laid out, with more chips than before (the rows made), and settled. A window minimized or covered on a real screen was not tried |
| 2026-11-18 | Nothing runs until an agent is served | `nothing_starts_until_an_agent_is_served` (`ui_tool_server`), a `MemoryKeychain` that counts what is asked of it | After `init` with outside agents off, and through three setting changes that let no agent in: no server, no outside address, 0 keychain reads and writes. A conversation's entry: the server runs with one token, the conversation's, and still 0. Outside agents turned on: 1 read; a token made and kept |
| 2026-11-18 | GPUI dispatches an action on a focus handle without moving focus | Read `crates/gpui/src/window.rs:637` (`FocusHandle::dispatch_action`), `:3399` (`Window::is_action_available_in`) and `:2654` (`Window::defer`) at `c84af5b99cbc` | Confirmed. The dispatch goes through the frame drawn last, so the tab must have been drawn (Deferred) |
| 2026-11-18 | A call never takes the keyboard | `opening_a_tab_leaves_the_keyboard_where_it_was`, `a_tab_put_in_front_of_the_persons_takes_the_keyboard`, `a_view_tool_leaves_the_keyboard_where_it_was` (`studio`); `a_view_tool_acts_on_its_tab_without_the_keyboard`, `a_tab_opened_with_the_keyboard_nowhere_takes_it` (`ui_tool_server`) | With the dispatch as it was: `graph_open_type` moved focus from the queries panel to the table (FocusId 4 to 18), and a stand-in canvas's zoom ran while the canvas held the keyboard. Now: the panel keeps it through `graph_open_type`, `graph_open_node_page` and `graph_open_map`; the zoom ran once with the keyboard off the canvas; a tab put in front of the one the person was in takes it, and so does a tab opened with the keyboard on the bare window, which without that rule answered no key |
| 2026-11-18 | Only the expansion that stopped says its second level asks | `only_the_expansion_says_its_second_level_asks` (`studio`): `canvas_expand` on c0, depth 2, `limit_per_relationship` 60 | The expansion: `needs_confirmation`, 60 nodes. `canvas_zoom_in` and `canvas_select_node` after it: neither the words nor the object. Before: "canvas_zoom_in changed the canvas; the second level would open 60 nodes with 480 reads: call again with confirm: true to open it" |
| 2026-11-18 | A call is Studio's only by its whole title | `a_studio_call_is_known_by_either_title_and_its_own_name_only` (`agent_panel`) | Five titles that hold more than a tool's name, a command line after it among them: none is a call to Studio's tools. Whether an adapter can be made to title another call so was not tried with a live agent |
| 2026-11-18 | The tools stay in their crate | `python3 script/check-layers.py` here, and its three rules run on `main` (`b2b47fa2`) | Here: "layers ok: 24 crates; 64 UI tools in their crate". On `main`: 16 lines in four files besides the list itself name a tool (`graph_canvas/src/agent.rs`, `relationship_tree/src/agent.rs`, `graph_store/src/exploration.rs`, `studio_workspace/src/item.rs`), and ten lines of `lib.rs` and `settings_view.rs` name the crate |
| 2026-11-18 | Studio builds without the tool crate | A throwaway checkout of the amendment's code: `crates/studio/src/ui_tools.rs` and the two test modules deleted, 13 lines changed in `lib.rs`, `settings_view.rs` and `Cargo.toml`; `cargo check -p studio --all-targets`; `cargo tree -p studio` | It builds, with one warning for a variable the stub left unused, and `ui_tool_server` is not in its tree |
| 2026-11-18 | The whole workspace after the amendment | `cargo test --workspace --no-fail-fast`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`, `check-layers.py`, `check-brand.py`, `check-design-tokens.py`, `test_release_files.py`, `test_measure_expand_all.py`, `cargo deny check`, on macOS (Apple silicon) | 1,552 tests pass, none fails, 9 are ignored; ten of them are new with the amendment. Clippy and the format check are clean; every script passes ("layers ok: 24 crates; 64 UI tools in their crate"); `cargo deny check`: advisories, bans, licenses and sources ok |
| open | The amendment with a live agent, in a window on a screen, and on Linux and Windows | Not run: no agent was started and Studio was not opened; every check of 2026-11-18 is a headless test on macOS | |
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
| 2026-11-17 | Calls to the server named `studio` are allowed unasked | Each conversation names Studio's server `studio_` and six random lowercase letters or digits; only calls to that name are allowed unasked, once the host reports `session/new` passed it, and only with an allow-once option, never Allow always | Agents run with the person's own home and config, where a server may be named `studio`; the entry existed before `initialize` said whether HTTP MCP was taken; and Allow always would outlast the call (review of #126) |
| 2026-11-17 | A tool's action runs as the person's key does | `workspace_activate_panel` puts focus back where the person had it, in the update that reads what changed, unless it was in a panel the tool hid: then the panel shown keeps it, or the active tab takes it | `ActivatePanel` shows a panel through `show_panel`, which gives it the keyboard; a person typing in a query or the composer lost their keys to it (V; review of #126) |
| 2026-11-17 | The notice is shown by the first call that names an open window | It is shown by an outside agent's first call of any kind: in the window it named, else the window in front | The person should know an outside agent is there as soon as it connects, and its first call, `studio_windows`, names no window (review of #126) |
| 2026-11-17 | A call Studio could not answer is `window_closed` | It is `cancelled`, naming the window the call meant | `window_closed` for window 0 told the agent a window it never named had closed (review of #126) |
| 2026-11-17 | The log names the tool | It names the tool only when it is one of Studio's, else "unknown" | The name is the agent's text, up to 64 KiB, which may carry a query or ids (review of #126) |
| 2026-11-17 | Copy "never shows the token" | Copy shows none, and a line under the Claude Code command says Claude Code keeps it in `~/.claude.json` once the command runs, until Studio starts again or the switch is turned off | `$(cat …)` expands when the person runs `claude mcp add`, so the token is written to Claude Code's config in plain text (review of #126) |
| 2026-11-17 | `graph_open_type` with `filter_id` while that type's table is open is `not_available` | It opens the filtered rows as a tab of its own (`OpenType { separate }`, a key of its own, not a preview), which the person can close; the type's open table is left as it is | Refusing stopped exploration ("the nodes related to X through R") whenever a table of the type was open, which was usually; the rule that matters, never changing the filters of a tab the person has open, holds (2026-11-16 live run) |
| 2026-11-17 | An answer names a node's tab by its title | It adds `label`, the name the session already holds (the canvas's or tree's root, or the spec-031 search index), and a walk is named by it from the start (`Exploration::for_walk`) | The answer was read before the node was, and named "Canvas: company-00000" where `studio_tabs` later said "Canvas: Sammet Systems" |
| 2026-11-17 | `studio_selection` names the selection | On a canvas or a tree it also counts the nodes, edges and relationships drawn | A fresh walk reported only its root, as if empty |
| 2026-11-17 | Tool names follow their action's | `canvas_expand`, `canvas_collapse`, `tree_expand`, `tree_collapse`, `tree_expand_row`, `tree_collapse_row` and `map_*` are named for what an agent asks (`ui_tools::RENAMED`) | `tree_expand` from `tree::Expand` would have taken the expansion's name, and `studio_ontology` is the map |
| 2026-11-17 | The server binds a port the system chooses | It asks for 24187 and the nine after it while the address is kept | A port that changes each start broke every copied command; the person asked for a non-standard fixed port |
| 2026-11-17 | Depth 2 opens the neighbours the first level read | It reads each relationship of the first level as far as `limit_per_relationship` first (a page at a time), waits through a Refresh for the reads again, and a canvas draws each expanded relationship's limit, at both levels (`Exploration::expansion_limit`) | A limit past one page stopped at the page; a Refresh dropped the second level unsaid; the second level drew the default eight (review of #127) |
| 2026-11-17 | The kept token is read at start | A token made, or replaced with New token, while the keychain is read or the address is kept is the one kept; turning "Keep the same address" on records the port in use | The first token made after turning it on was not kept, a New token pressed during the read was overwritten by the old one, and the next start asked for 24187 instead of the port in use (review of #127) |
| 2026-11-17 | A view tool's answer compares what the view says | The canvas says where it is centred; the map says its focus, Back and what is pointed at; the lists are cut to fit the answer; hiding is refused on a query's result picture, which draws its result whole; a filtered table saved as a view is found by the view's key | A camera move or a map move answered `no_change`; a large change could lose its whole answer; hiding a result node did nothing; a saved filtered view opened twice (review of #127) |
| 2026-11-18 | `UiTool`, its list and the state reader are a module of `studio_workspace` (layer 4) | They are `ui_tool_server::tools` (layer 5), with their tests. The workspace keeps `pane_tabs`, `panel_with_focus` and `focus_active_tab` for any reader, and the "Opened by" chip | The window frame is under every feature crate; 1,780 lines of one feature's policy sat in it (review of #126 and #127) |
| 2026-11-18 | `UiToolEndpoint` is a global of `studio_workspace` | `agent_panel::UiToolEndpoint`, with `UiToolEntry`, the server's name and the title of a call; the entry carries which names are tools | The panel is their only reader, and the tool list it needed for one check stays in the tool crate |
| 2026-11-18 | Tabs answer `agent_view`, `agent_busy` and `agent_check(tool, input)` | `Item::outline`, `settling`, `catch_up` and `refuses(action, on)`; `AgentView` and its parts are `ViewOutline`, `OutlineNode`, `OutlineEdge`, `OutlineGroup`; a refusal is the enum `Refusal`, worded by `tools::refused`. The views' `agent.rs` are `by_id.rs` | A view matched tool names as strings and parsed the call's JSON a second time, which answered "`depth` is 1 or 2" for any argument that did not fit |
| 2026-11-18 | "The server listens while Studio runs" (2026-11-16) | It listens from the first conversation that asks for its entry, or while outside agents are let in | A person who uses no agent had a listener, a keychain read and, with the switch on, a token file at every launch |
| 2026-11-18 | "The kept token is read at start" (2026-11-17) | It is read the first time agents outside Studio are let in while the address is kept | A conversation's token is its own, so starting for a conversation need not ask the keychain |
| 2026-11-18 | Outside agents are on by default (2026-11-17) | Off, as in 0.4.8 | At Andrew's request: a person who wants none of this is left alone. Once on, the kept address and token are as 2026-11-17 made them |
| 2026-11-18 | "A view tool gives the canvas or tree the keyboard for its dispatch, as a click would, and gives it back where the person had it after" | Its action is dispatched on the tab, which never takes the keyboard; and every tool, not `workspace_activate_panel` alone, leaves the keyboard where the person had it. `UiTool::keeps_focus` is gone | A tab-opening tool left the keyboard in the tab it opened, where ↵ twice saves a cell; a view held it for the call's wait (review of #126 and #127) |
| 2026-11-18 | A second level past the question stops, "and the answer says what it would open" | The answer of that expansion says it, and no later call's | The walk holds what stopped until the next expansion, so every call after it repeated "call again with confirm: true" (review of #127) |
| 2026-11-18 | "The thread recognizes either form" of a call's title | Either form, and only when the whole title is the name | Only the title's first word was read, so a title that went on after a tool's name was allowed as that tool (review of #126) |
| 2026-11-18 | A tool is matched by name where it differs (`graph_open_type`; a name ending `_expand`) | `UiTool::apart_when_filtered` and `UiTool::expands`, fields of the tool's row | No tool is named outside the list |
