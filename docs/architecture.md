# Architecture

Studio is one GPUI application on Zed's crates. It reaches Omnigraph only over HTTP
(constitution I). This page is the current design; decisions and their evidence live in
`specs/`.

## Crate layers

A crate may depend only on crates in lower layers. Feature crates (layer 5) never depend on each
other: what two features share moves down a layer.

| Layer | Crates | Role | Allowed dependencies |
|---|---|---|---|
| 0 | `studio_actions` | Every action type, with doc comments that become palette descriptions | `gpui` |
| 1 | `studio_settings`, `studio_theme` | Typed layered settings; graph tokens over Zed's themes, set up through Zed's `theme_settings`, the themes Studio bundles, and Studio's icons in place of Zed's | `gpui`, `settings`, `theme`, `theme_settings`, `assets`, serde |
| 2 | `graph_client` | Every request to an Omnigraph server: contract header, DTOs for the 0.14 API, write outcomes, change-feed paging. **No `gpui`** | Zed `http_client` (Apache-2.0), serde |
| 2 | `gq_language` | Schema and query intelligence on `omnigraph-compiler`: parse, diagnostics, completion items, hover text, result-column types. **No `gpui`** | `omnigraph-compiler` |
| 2 | `studio_ui` | Studio components on Zed's `ui`: the design system (`ds`: buttons, modals, the context menu forked from Zed's), state chips, write-outcome banners, empty and error states, text read as markdown | `ui`, `menu`, `theme`, `studio_theme`, `pulldown-cmark` |
| 3 | `graph_store` | GPUI entities: connections, graph sessions, schema model, snapshot pins, exploration, drafts | `gpui`, `graph_client`, `gq_language`, Zed's `reqwest_client` and the `reqwest` it pins |
| 3 | `studio_update` | The updater (spec 017): checks Studio's own releases, downloads, verifies and installs an update. The only requests not to an Omnigraph server | `gpui`, `studio_settings`, `cargo-packager-updater` |
| 4 | `studio_workspace` | The window frame: panes, tabs, docks, panels, status bar, modal and toast layers | `gpui`, `ui`, `studio_ui`, `studio_actions`, `studio_settings` |
| 4 | `change_cards` | The entity cards a review and a commit's view share: an entity's card, its relationship lines, a run's header, how many lines a card shows | `gpui`, `ui`, `studio_ui`, `graph_store`, `gq_language` |
| 5 | `command_palette`, `type_table`, `relationship_tree`, `graph_canvas`, `query_editor`, `review`, `history`, `inspectors`, `agent_panel` | One surface each | layers 0–4 |
| 6 | `studio` | The binary: initialises every crate in order, menus, windows, single instance, URL scheme | everything |

`studio_workspace` is forked and trimmed from Zed's `workspace`, without projects,
collaboration or Zed's database (spec 002). `command_palette` is forked from Zed's `picker` and
`command_palette` the same way.

## Data flow

```
view (layer 5) ──reads──▶ entity in graph_store ──calls──▶ graph_client ──HTTP──▶ Omnigraph server
      ▲                         │
      └──── cx.notify() ◀───────┘   (results applied on the main thread)
```

- Views never call `graph_client` directly. They read entities and dispatch actions. The
  session's client is private to `graph_store`, so every read goes through a session method
  that counts it while it runs and notes what it says about the server (the history's and the
  review's change pages, the storage inspector's server state among them). The one client
  another crate holds is the agent gateway's (`GraphSession::gateway_client`), which forwards
  the agent CLI's own requests inside its sandbox.
- `graph_client` sends through Zed's `HttpClient` (the `reqwest_client` that `graph_store::init`
  installs, or one of its own for a CLI target whose `transport` names a proxy or a certificate
  authority); `graph_store` awaits it in a task it keeps in a field, so dropping or replacing the
  task cancels the request.
- Results are applied in one `update` on the main thread, followed by one `notify`.

## State model

| Entity | Holds | Notes |
|---|---|---|
| `Connection` | server URL and name (the CLI target it is reached as, or `environment`), the token (in memory only: the one the CLI exports for that target, or one given), contract and version check, health and latency, the graph list, state (`connecting`, `ready`, `contract-mismatch`, `unauthorized`, `unreachable`) | One per server address; the status-bar chip renders it. Health is checked while a window shows one of its graphs or the Open dialog lists it (`Connection::watch`). A graph's first reads wait until its token is ready (`Connection::token_ready`) |
| `GraphSession` | graph id, branch, schema, branch list, type counts and the commit they count (`counts_commit`), the heads it watches, the read memo, what the credential may do, drafts and the outcome log | One per open graph in a window; its views share it. Each view holds its own snapshot pin. What Studio saves of it is under one key, its CLI target and its id (`overrides_key`, `<target>/<graph>`). A new token on its connection reads what it may do again, forgets every answer the read memo kept with the old one (reads at a commit, Blob cells, relationship counts, branch heads, commits' changes), and reads the type counts, the stored-query catalog (once a view has wanted it) and the branch list again, showing none of the old token's meanwhile; a parked session forgets at once and reads when shown again. It keeps the schema, which the server answers alike to every credential that may read the graph, and reads it again only when none is held, since a 401 may have refused it; it keeps the heads and change logs, which describe commits and only carry forward answers read since. A 403 on its graph reads what the credential may do again and keeps the answers; one on another graph of the server changes nothing here |
| `SchemaModel` | the `.pg` source from `GET /schema`, compiled with `omnigraph-compiler` into node, edge and property shapes | One per session, rebuilt when the schema is read again (Refresh, or another writer's schema seen at a head move) |
| `Exploration` | the set of expanded paths (node → relationship group → node …) | Shared by the relationship tree and the canvas |
| Drafts (`GraphSession::drafts`) | unsaved property values by branch, each with the value it was read at (spec 014) | A branch's wait while another is shown; a save is one mutation bound to the values read |
| Outcomes (`GraphSession::outcomes`) | each write and its outcome (committed, refused, unknown), the newest 1,000 | Rendered in the activity panel |

### Snapshot pins

A view's first read returns `graph_commit_id`. Later reads in that view pass it as `snapshot`.
The `GraphSession` watches branch heads for every view: every 30 s while a Studio window is
active and shows the graph, and at once when a window becomes active after a missed poll (or
shows the graph again after parking it), it reads each watched branch's head with a query that
returns no rows (`limit 0`; its answer names the head commit), and it reads the branch list. Studio's own commits set the head without asking. A view whose
pin is not the head shows "Newer data · Refresh" and changes nothing until the person
refreshes (constitution II, V). Writes use `POST /mutate/if-graph-commit` with the pinned id;
a 412 reopens the draft.

## The window frame

```
┌───────────────┬──────────────────────────────────────────┬───────────────────┐
│ ●●●  Graph   ⌕│ ← → │ Person │ Company │ query.gq        │ Inspector       × │
│ Types   12 480│                                          │  fields, edges    │
│ Relationships │            active item                   │                   │
│               ├──────────────────────────────────────────┤                   │
│               │ History                               ×  │                   │
├───────────────┴──────────────────────────────────────────┴───────────────────┤
│ ◧ │ ▦ ≡ ⎘ ⑂ ✦ │ ● env · 0.14 · 31 ms │ graph / ⑂ main │ facts …    ⌨ 41 MB │ ▦ ◷ ⚡ │ ⓘ │
└──────────────────────────────────────────────────────────────────────────────┘
```

- Each column's header is its panel's own (title and buttons); the first holds the traffic
  lights. There is no title bar.
- Left dock: Graph (types and relationships with counts), Explore, Queries, Branches,
  Conversations. Right dock: inspector. Bottom dock: Nodes, History, Activity.
- Center: panes of items (type table, canvas, query editor, review, commit); each pane's tab
  strip starts with its own Back and Forward.
- Status bar, after Zed's: the sidebar toggle, every panel's button (the left dock's at the
  left, the bottom and right docks' at the right), the connection chip, graph and branch, the
  tab's few facts, running work, a keyboard button listing the tab's keys, memory and CPU. A button's right-click menu moves its panel to another dock
  (`panels.<id>.dock` in the settings); `studio.bottom_dock_layout` says where the bottom dock
  spans; ⇧⎋ zooms the active pane.

## Actions, keymaps, settings, theme

- Actions are declared in `studio_actions` and bound in `assets/keymaps/default-{macos,linux,
  windows}.json` with key contexts (`TypeTable`, `Canvas`, `QueryEditor`, `Review`, `Tree`).
  The person's `keymap.json` loads over them and is read again when it changes; no crate binds
  keys in code, since a reload rebinds from the files alone.
- Settings are one typed document: built-in defaults, then the user file, then per-graph
  overrides. The settings window is generated from the same schema (spec 002). The user file
  is the person's text: comments allowed, a value that does not fit costs only its setting,
  and writes edit only the keys that changed (`studio_settings::document`).
- `studio_theme` sets up Zed's `theme_settings` with Zed's themes and the ones Studio bundles
  (Studio Light and Dark, the defaults, written by `script/studio-theme.py` from Vitesse Refined
  with interfaces.dev's greys; Vitesse Refined itself), and adds one graph token: a color per node
  or edge type (`type_color`). Its asset source serves Nucleo drawings for the Zed icon paths
  `studio_theme::icons` lists, so every `IconName` Studio or Zed's components draw is Tower's
  icon (spec 002, "Icons").

## Testing

- `#[gpui::test]` for views and entities, against `graph_client` driven by Zed's
  `FakeHttpClient` with recorded responses.
- `graph_client` and `gq_language` have plain unit tests; they do not depend on GPUI.
- `crates/graph_client/openapi/omnigraph-0.14.0.json` (Omnigraph `v0.14.0`) is the contract:
  `graph_client`'s contract tests check every DTO's fields and every enum's variants against its
  schemas, and `every_route_the_client_calls_is_in_the_contract` makes
  each of the client's calls once and checks that its method and path are one of the
  document's routes.
- macOS screenshot tests for the main views, once the views exist.

## Connecting to graphs today

Until the `omnigraph studio` launcher exists (deferred, `docs/deferred-engine-changes.md`),
Studio connects to running servers:

1. The server targets the Omnigraph CLI lists (`omnigraph target list --json`), each token
   exported by the CLI through a pipe (`omnigraph credentials export --target NAME`) and kept in
   memory: Studio parses neither the operator file nor any credential source (spec 003). The
   CLI is the first Studio can drive of the one the `omnigraph_cli` setting names, those on
   `PATH` and those in the install folders (`graph_store::cli`, which finds the agent's CLI too).
2. A server added by URL and token in Studio becomes a CLI target, through the CLI (`omnigraph
   target add`, and `omnigraph credentials set` for its token); Studio keeps no server list.
3. A local graph is opened by serving it first (`omnigraph-server --cluster <dir>`); Studio
   does not open storage itself.
