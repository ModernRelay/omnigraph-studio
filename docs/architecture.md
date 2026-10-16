# Architecture

Studio is one GPUI application on Zed's crates. It reaches OmniGraph only over HTTP
(constitution I). This page is the current design; decisions and their evidence live in
`specs/`.

## Crate layers

A crate may depend only on crates in lower layers. Feature crates (layer 5) never depend on each
other: what two features share moves down a layer.

| Layer | Crates | Role | Allowed dependencies |
|---|---|---|---|
| 0 | `studio_actions` | Every action type, with doc comments that become palette descriptions | `gpui` |
| 1 | `studio_settings`, `studio_theme` | Typed layered settings; theme tokens and the `ThemeSettingsProvider` Zed's `theme` crate needs | `gpui`, `theme`, `refineable`, serde |
| 2 | `graph_client` | Every HTTP request: contract header, DTOs for the 0.13 API, write outcomes, change-feed paging. **No `gpui`** | Zed `http_client` (Apache-2.0), serde |
| 2 | `gq_language` | Schema and query intelligence on `omnigraph-compiler`: parse, diagnostics, completion items, hover text, result-column types. **No `gpui`** | `omnigraph-compiler` |
| 2 | `studio_ui` | Studio components on Zed's `ui`: state chips, write-outcome banners, empty and error states, key hints | `ui`, `theme`, `studio_theme`, `studio_actions` |
| 3 | `graph_store` | GPUI entities: connections, graph sessions, schema model, snapshot pins, exploration, drafts | `gpui`, `graph_client`, `gq_language`, `gpui_tokio` |
| 4 | `studio_workspace` | The window frame: panes, tabs, docks, panels, status bar, modal and toast layers | `gpui`, `ui`, `studio_ui`, `studio_actions`, `studio_settings` |
| 5 | `command_palette`, `type_table`, `relationship_tree`, `graph_canvas`, `query_editor`, `review`, `history`, `inspectors`, `agent_panel` | One surface each | layers 0–4 |
| 6 | `studio` | The binary: initialises every crate in order, menus, windows, single instance, URL scheme | everything |

`studio_workspace` is forked and trimmed from Zed's `workspace`, without projects,
collaboration or Zed's database (spec 002). `command_palette` is forked from Zed's `picker` and
`command_palette` the same way.

## Data flow

```
view (layer 5) ──reads──▶ entity in graph_store ──calls──▶ graph_client ──HTTP──▶ OmniGraph server
      ▲                         │
      └──── cx.notify() ◀───────┘   (results applied on the main thread)
```

- Views never call `graph_client` directly. They read entities and dispatch actions.
- `graph_client` runs on Tokio through `gpui_tokio`; `graph_store` awaits it in a task it keeps
  in a field, so dropping or replacing the task cancels the request.
- Results are applied in one `update` on the main thread, followed by one `notify`.

## State model

| Entity | Holds | Notes |
|---|---|---|
| `Connection` | server URL, token source, contract check, capability set from the served OpenAPI document, state (`connecting`, `ready`, `contract-mismatch`, `unauthorized`, `unreachable`) | One per server; the status-bar chip renders it |
| `GraphSession` | graph id, branch, **snapshot pin** (a `graph_commit_id`), actor | One per open graph tab; views share it |
| `SchemaModel` | the `.pg` source from `GET /schema`, compiled with `omnigraph-compiler` into node, edge and property shapes | Rebuilt when the pinned commit changes |
| `Exploration` | the set of expanded paths (node → relationship group → node …) | Shared by the relationship tree and the canvas |
| `Draft` | staged cell edits, inserts and deletes for one branch, with the commit they were read at | Published as one mutation; kept until the outcome is known |
| `OutcomeLog` | every write and its outcome (committed, refused, unknown) | Rendered in the activity panel |

### Snapshot pins

A view's first read returns `graph_commit_id`. Later reads in that view pass it as `snapshot`.
The `GraphSession` watches branch heads for every view: every 30 s while a Studio window is
active, and at once when a window becomes active after a missed poll, it reads each watched
branch's head with a query that returns no rows (`limit 0`; its answer names the head commit),
and it reads the branch list. Studio's own commits set the head without asking. A view whose
pin is not the head shows "Newer data · Refresh" and changes nothing until the person
refreshes (constitution II, V). Writes use `POST /mutate/if-graph-commit` with the pinned id;
a 412 reopens the draft.

## The window frame

```
┌───────────────┬──────────────────────────────────────────┬───────────────────┐
│ ●●● ◧ Graph  ⌕│ ← → │ Person │ Company │ query.gq        │ Inspector       × │
│ Types   12 480│                                          │  fields, edges    │
│ Relationships │            active item                   │                   │
│               ├──────────────────────────────────────────┤                   │
│               │ History                               ×  │                   │
├───────────────┴──────────────────────────────────────────┴───────────────────┤
│ ▦ ≡ ⎘ ⑂ ✦ │ ● env · 0.13 · 31 ms │ graph / ⑂ main │ tab facts … 41 MB │ ▦ ⌥ ◷ │ ⓘ │
└──────────────────────────────────────────────────────────────────────────────┘
```

- Each column's header is its panel's own (title and buttons); the first holds the traffic
  lights. There is no title bar.
- Left dock: Graph (types and relationships with counts), Explore, Queries, Branches,
  Conversations. Right dock: inspector. Bottom dock: Nodes, History, Activity.
- Center: panes of items (type table, canvas, query editor, review, commit); each pane's tab
  strip starts with its own Back and Forward.
- Status bar, after Zed's: every panel's button (the left dock's at the left, the bottom and
  right docks' at the right), the connection chip, graph and branch, the tab's facts, running
  work, memory and CPU. A button's right-click menu moves its panel to another dock
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
- `studio_theme` registers Studio's `ThemeSettingsProvider` and adds graph tokens: one color per
  node type, edge colors, diff colors. It starts from Zed's base themes.

## Testing

- `#[gpui::test]` for views and entities, against `graph_client` driven by Zed's
  `FakeHttpClient` with recorded 0.13 responses.
- `graph_client` and `gq_language` have plain unit tests; they do not depend on GPUI.
- `crates/graph_client/openapi/omnigraph-0.13.0.json` is the contract; a test checks every DTO
  and route against it.
- macOS screenshot tests for the main views, once the views exist.

## Connecting to graphs today

Until the `omnigraph studio` launcher exists (deferred, `docs/deferred-engine-changes.md`),
Studio connects to running servers:

1. Servers from `~/.omnigraph/config.yaml` (or `$OMNIGRAPH_HOME`), with tokens resolved as the
   CLI resolves them: `OMNIGRAPH_TOKEN_<NAME>`, then `[<name>] token = …` in
   `$OMNIGRAPH_HOME/credentials`.
2. A server added by URL and token in Studio, with the token kept in the OS keychain.
3. A local graph is opened by serving it first (`omnigraph-server --cluster <dir>`); Studio
   does not open storage itself.
