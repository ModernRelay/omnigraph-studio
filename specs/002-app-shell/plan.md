# 002: App shell

Status: in progress · 2026-10-13. Built: the window frame with panes, tabs and docks, menus,
settings, several windows restored on launch, and `omnigraph-studio://` links. The command
palette landed 2026-10-12 (`crates/command_palette`), with scope tabs and the Ask hand-off.

## Problem

Every surface needs the same frame: a window with docks, panes and tabs, a status bar, a command
palette, menus, settings, a theme, notifications, and keyboard bindings for everything. Zed has
all of it, but its `workspace` assumes a code project, collaboration and Zed's database.

## Principles

V (stable layout), VI (controls explain themselves), VII (loudness, motion), VIII (keyboard
first), XI (fork with notices).

## What exists today (checked)

- Zed `v1.23.2` `crates/workspace/src/`: `workspace.rs`, `pane.rs`, `pane_group.rs`, `item.rs`,
  `dock.rs`, `status_bar.rs`, `toolbar.rs`, `modal_layer.rs`, `toast_layer.rs`,
  `notifications.rs`, `persistence.rs`, `multi_workspace.rs`.
- `crates/ui/src/components/` at the tag: `button`, `context_menu`, `popover_menu`,
  `right_click_menu`, `dropdown_menu`, `tooltip`, `keybinding`, `keybinding_hint`, `list`
  (`list_item`, `list_header`), `tree_view_item`, `indent_guides`, `sticky_items`, `tab`,
  `tab_bar`, `modal`, `notification`, `banner`, `callout`, `chip`, `count_badge`, `indicator`,
  `scrollbar`, `data_table`, `redistributable_columns`, `progress`, `diff_stat`, `disclosure`.
- `crates/picker/src/` (`picker.rs`, `head.rs`, `footer.rs`, `preview.rs`, `persistence.rs`)
  and `crates/command_palette/src/` (`command_palette.rs`, `persistence.rs`).
- GPUI actions: `actions!` and `#[derive(Action)]`, JSON keymaps with context predicates,
  `window.available_actions(cx)` for the palette (Zed's palette uses it).

## Design

### Frame (`studio_workspace`, forked from Zed's `workspace`)

Keep: `PaneGroup` splits, `Pane` with tabs (preview tab, pinned tabs, dirty and stale dots),
the `Item` trait (tab content, tooltip, events, save/close), `Dock` left, bottom and right with
`Panel`s, `StatusBar` with left and right items, `Toolbar` per pane, `ModalLayer`, `ToastLayer`,
`Notifications`.

Remove: projects and worktrees, collaboration and following, remote and SSH, Zed's database and
session restore (replaced in step 2 below), tasks, shared screens, security modal, welcome page.

Steps:

1. Fork the files above at `v1.23.2` with a header naming the source commit. Strip until it
   builds against Studio's crates only.
2. Persistence: window bounds, dock sizes and open tabs saved as JSON under the app's data
   directory, keyed by server and graph. No SQLite.
3. One window can hold several graph sessions as tabs; a second window is "New Window" (⌘⇧N).

Built (`studio_workspace::pane`): the document area is a tree of panes (`Member`, splits with
shares that drag to resize). Each pane has its own tab strip; the top-right pane carries the
dock toggles and the top-left one makes room for the traffic lights. ⌘\ and ⌘⇧\ split right
and down with a copy of the active tab (reopened from its saved form), ⌘⌥→/← move between
panes, a pane that loses its last tab closes. Pinned tabs come first with a pin instead of a
close button and survive "Close others" and "Close all"; a tab's right-click menu has the
close variants, Pin and the splits. A preview tab (italic title) is what a single click in the
Types list opens; the next preview replaces it in place, and a double click, opening it again
without preview, or work in it keeps it. Tabs that hold unsaved work stay open on the bulk
closes. The workspace file saves each pane's tabs, active tab and pinned count, the split tree
and the active pane; a file from before splits restores as one pane.

Built (several graphs in one window): opening another graph parks the shown one with its panes,
tabs and back/forward trail, saved first; the sidebar footer is the switcher ("Open in this
window", the shown graph checked, Open Another Graph…, Close Graph) and reads "+N graphs" when
more are open; ⌃⌘] and ⌃⌘[ step through them. Closing a graph saves its tabs (opening it
again brings them back) and shows the one opened before; a graph with a write still running
stays open, so the write's outcome is never dropped. Toasts follow the shown graph; a parked
graph's writes still land in its activity log. Each window's graphs (server URL, graph,
branch, and the one shown) and bounds are written to the owner-only `session.json` as they
change, in the order the windows opened. The next start reopens every window that has a graph
on a server still known, where it was, showing the graph it showed; with none it opens one
window asking for a graph. Closing a window forgets it, unless it was the last one open: that
one comes back at the next start, or when the Dock icon is clicked with no window open, until
another window opens. Quitting keeps every window. A new window (⌘⇧N) asks for a graph; it
opens where the last moved window was. A `session.json` from before held one window's graphs
and is read as one window.

### Drawing cost

GPUI rebuilds the whole window on every frame that draws, except views marked cached (Zed caches
each pane and dock). Studio caches the same places: each tab's view, and each dock panel inside
a scrolling host of its own (a cached view needs a definite size, which a panel measured by its
content lacks; the host has one and scrolls the panel). A cached view is drawn again only when it
or a view inside it notifies, or when a setting changes (which redraws every window); otherwise
the frame replays it. A hover, a toast or an agent's streamed reply therefore redraws only what
changed. The left panel's title stays put above its scrolling content. The snapshot example's
`measure:<n>[,refresh]` step times frames after notifying the workspace, or redrawing all. The
workspace itself renders whenever anything inside it notifies, so it compares its state with
what was last saved at most every 250 ms rather than serializing every tab on every frame.

### Window chrome

- Transparent title bar with traffic lights; the title area shows
  `server ▸ graph ▸ branch ▾ ▸ snapshot`. The branch menu switches branch for the active tab.
- Right side of the title: `write · <branch>` whenever the active item can write; a read-only
  chip when the actor cannot write or the item is pinned to an older commit.
- Status bar left: connection chip (`ready`, `connecting…`, `contract mismatch`,
  `unauthorized`, `unreachable`), contract version, actor. Right: one activity slot ("2 queries
  running", "Publishing…"), which opens the activity log.

### Actions and keys

- Actions in `studio_actions`, one module per surface. Doc comments are the palette text.
- Default keymaps in `assets/keymaps/default-{macos,linux,windows}.json`. Contexts:
  `Workspace`, `TypeTable`, `Tree`, `Canvas`, `QueryEditor`, `Review`, `History`, `Inspector`.
- Global: ⌘K palette, ⌘P go to type or node, ⌘⇧B switch branch, ⌘, settings, ⌘1–⌘9 tabs,
  ⌘[ / ⌘] back and forward, `?` key sheet (outside text fields).
- In lists and tables: J/K or ↑/↓, ↵ open, Space peek, Esc steps back one level (close editor,
  then peek, then clear selection).

### Command palette (`command_palette`, forked from Zed's `picker` + `command_palette`)

- Lists only `window.available_actions` from the focused element, ranked by recent use
  (persisted), with the winning binding shown. Focus returns to the previous element before the
  action runs.
- Scope tabs: All ⌘1, Types ⌘2, Nodes ⌘3, Queries ⌘4, Branches ⌘5, Commands ⌘6. Tab hands the
  text to the agent ("Ask") once spec 011 lands.
  Built: a segmented control under the search field; ⌘1–⌘6 (Alt elsewhere) inside the
  palette. ⌘K opens All, ⌘P Nodes, ⌘⇧B Branches; the key sheet (`?`) has no tabs. Queries
  lists the open query tabs with their first line, then the graph's saved query files that no
  tab has open (spec 007). Tab
  (bound under `CommandPalette > Editor`, so it outranks the editor's) dismisses the palette
  and sends `studio_agent::Ask`; with no agent chosen, the agent tab says the question waits.
  Node search tries the text as typed, lowercase and capitalized, because `contains` is
  exact on 0.13 ("ada" finds "Ada Turing"). GQ has no node alternatives, so a search is one
  read per node type, five rows each. Typing on is mostly free: when a type's answer came
  back under five rows and the new text only narrows the old (each spelling of the new text
  starts with one of the old, `gq_language::builder::search_narrows`), its rows are filtered
  in the palette by the query's own predicate (`search_matches`); text typed back to after a
  backspace shows its answer again. The earlier node rows stay, ranked against the new text,
  until the new answer replaces them.
- Opens in one frame with no animation (constitution VII).
- Landed 2026-10-12: ⌘K and ⌘⇧P (Ctrl elsewhere; each bound in `init` only when the keymap
  does not already bind it). Rows are humanized action names (`studio_graph::OpenType` → "Graph:
  open type"), fuzzy matched with word-start and run bonuses, each with the binding that wins
  where the person was and the action's doc comment in the footer. Recently run rows come first
  (`command-palette.json` in Studio's data directory, read and written off the main thread);
  then Studio's actions, node and edge types, and the rest. A type row dispatches
  `studio_graph::OpenType` and appears only when that action is available where the person is.
  ↑/↓ move, ↵ runs after the palette closes and focus returns, Esc closes.

### Tooltips and disabled states

Every button's tooltip shows the action name and binding (Zed's `Tooltip::for_action`). A
disabled control's tooltip gives the first blocking reason ("Read-only: pinned to commit
01JA…", "Your token cannot write to main"). Where a CLI command does the same, the tooltip's
second line shows it (`omnigraph branch create feature-x --from main`).

### Settings (`studio_settings`)

- One typed document with serde defaults in `assets/settings/default.json`, the user file
  `settings.json` in the app config directory, and per-graph overrides.
- Watched on disk; parse errors become a notification that names the line.
- A settings window (⌘,) generated from the same schema: search box, left nav, rows of title,
  one-line description and control, "Reset to default" per row. Forked in spirit from Zed's
  `settings_ui`; rows are generated, not hand-written.
  - Built: `studio_settings::fields` walks the `schemars` schema of `StudioSettings`. A field's
    doc comment is its description; `title`, `range` and the extensions `x-section`,
    `x-control` (`theme`, `font`, `agent`), `x-step` and `x-hidden` give the rest. The tab
    draws a control per kind (segmented choice up to four, a menu above that, a stepper in
    range, a switch, the theme and font menus, the agent presets) and writes by path, so a new
    setting is a field and its attributes. The nav counts changed settings per section;
    search spans every section; the search box has focus when the tab opens.
  - Per-graph overrides: `graphs."<server name>/<graph id>"` holds a graph's own values for
    the settings marked `x-per-graph` (table and relationship page sizes, canvas node limit).
    With a graph open, the tab's scope switch ("All graphs" | the graph) shows only those,
    writes the graph's own value, and Reset removes it; an own value's description names the
    shared one. Views read the values in effect through `GraphSession::options`.
- `studio_theme` implements `theme::ThemeSettingsProvider` from these settings: UI font,
  buffer font, sizes, density.

### Notifications

- Silent success for routine work. A toast only after a durable publish, with one follow-up
  ("Committed to feature-x · View commit"); 10 s, paused on hover.
- Errors as notifications that say what happened and what to do; they stay until dismissed.
- The activity log (bottom dock) keeps every write with its outcome.

### Visual language

Delta's window, measured from the mock on delta.dev (its CSS uses Zed's One Light tokens and
IBM Plex Sans, as Studio does), with Tower's quiet rules. Tokens and components live in
`studio_ui::ds` (spacing, radii, sizes, surfaces, elevation, text roles, and the rows, cards,
strips and states built from them); the "Design system" tab (`studio::OpenDesignSystem`) renders
every specimen in the current theme and window style. Features compose `ds` components rather
than styling one-off elements.

- **Columns, not bars.** No global title bar or status bar. Each column has its own 40 px
  header strip and the sidebar and document their own footer strip, so the rules line up across
  the window. Strips are window drag areas; as in Zed, a move starts on the first mouse move
  after a press, so clicks on strip buttons land.
- **Panels in docks** (`studio_workspace::panels`, after Zed's `Panel`/`Dock` and Obsidian's
  side docks). A panel is a view with an id, a title and an icon; features add one with
  `Workspace::add_panel(position, …)` and show it with `activate_panel`. A dock (left, right,
  bottom) shows one panel at a time, opens and closes as a whole, and draws the same chrome for
  every panel: a header strip with its tabs, then the active panel. The left dock (264 px)
  switches panels with icon tabs (Types, Explore, Branches); the right (340 px, the inspector)
  and bottom (the activity log) docks use title tabs and a close button. Selection fills the
  inspector without reopening a dock the user closed. Each dock's open state and active panel
  are remembered per graph. Documents are tabs in the center, not panels. A left panel may put
  small buttons on its title line, on the label's baseline (`Workspace::set_panel_actions`,
  drawn by `ds::section_header`), as Delta and Zed put a list's search and "new" there rather
  than as rows in the list; the Conversations panel does (spec 011).
- **Footers, after Delta.** The sidebar footer holds the connection: a status dot, the server
  and its state, opening "Open a graph" (Delta's account). The document footer holds where the
  open tab reads and writes, graph / branch with the branch menu (Delta's repo / branch), then
  the tab's own facts; at the end what is running and the tab's controls. A tab supplies its
  part through `Item::footer` (`ItemFooter { start, end }`): the table its loaded count, read
  commit and key hints; the agent its restart, agent pill and send. The document header holds
  only the tabs and the dock toggles.
- **Window styles.** `studio.window_style`: `flat` splits columns with hairlines; `glass`
  floats the three columns as sheets over a soft wash, with one gap around and between them,
  and moves the traffic lights into the sidebar sheet.
- **Hairlines.** `ds::surface::rule`, never a solid border; dashed between rows on glass.
- **Tabs** fill the 40 px strip (an explicit height: the right-click wrapper lays a tab out at
  its content's height, so `h_full` would shrink it to its label); the active tab joins the
  document, inactive ones sit on the strip's rule.
- **Buttons.** `ds::button` and `ds::icon_button` (Zed's medium size: 28 px, an 8 px inset)
  for every button in a toolbar, strip, panel, card footer or dialog, so a hover or pressed
  fill has room around its label. Buttons inside a dense row (a filter pill's segments, a
  tab's close, a tree row's toggle, an inline Fix or Run, the sidebar header's icons) keep
  Zed's compact default.
- **Text sizes.** The window's rem size is `ui_font_size` (16 px by default), as in Zed, so
  every `ui` size scales with it. Body text is `Small`; labels, counts and metadata `XSmall`.
- **Sidebar rows.** `ds::SidebarRow`: section labels muted, not bold; the selected row a grey
  pill; counts muted and right-aligned. Footnotes become tooltips.
- **Context once.** The branch appears once, in the document footer's branch menu.
- **Tables.** Like Linear: a 48 px toolbar, a one-line header (name, then the type muted, 30
  px), 28 px rows split by hairlines, no column rules, the cursor as a ring and the selected row
  as a pill. Dense enough to scan a page at a glance; an 18 px borderless tag or chip still sits
  inside a row. Sidebar rows are 26 px, feed rows 36 px; every list takes its height from
  `ds::size`, never its own constant.
- **Layout, not sentences.** Information sits in structure, as in Delta's thread and Zed's
  panels: metadata as an icon and a short value (`ds::meta`, `meta_code`: a count, a commit, a
  branch); keys as key caps (`ds::key_hint`, `KeyBinding` on buttons), never "↵ edit · …"
  strings; activity as one-line rows with a gutter icon and a verb; approvals and outcomes in
  cards with a header and a footer of icon buttons (✓ allow, ✓✓ always, ✗ reject) or an outcome
  chip; code and commands in bordered blocks. The agent's prompt sits under its thread as the
  next message's gutter row. Explanations go in tooltips. Sentences appear only in errors
  (`Callout`) and in what an agent writes.
- **Empty states.** An icon, a title of two or three words, and the actions that fill the
  space as outlined buttons with their key bindings (`ds::empty_state`); an empty panel is one
  dim row (`ds::sidebar_placeholder`).

### Motion

Palette, J/K, cell cursor and tab switches never animate. Dialogs and peeks enter with a
120–180 ms ease-out and leave faster. Reduced motion keeps fades only.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Fork and trim `workspace`, `picker`, `command_palette` | Depend on them | They require `project`, `client`, `db`, `language`; Delta also wrote its own panes and picker |
| JSON files for layout persistence | Zed's SQLite `db` | Small state; no migrations to carry |
| Palette lists only available actions | A global list of every command | Zed's model: a listed action always works where you are |
| Every tab kind restores; a query tab keeps its draft, in an owner-only (0600) workspace file | Restore query tabs empty; keep drafts only in memory | Spec 007: a draft is never cleared. The rule against logging query text is about logs; the workspace file is the person's own document state |
| Panels in docks, one active panel per dock | Stacked sidebar sections | Zed's and Obsidian's model; every panel gets the same bounded chrome and the sidebar stays short |
| Settings rows from a JSON schema (`schemars`) of the settings types | A hand-written row list; Zed's `settings_ui` page macros | One source: the field, its doc comment and attributes. The same schema can later validate `settings.json` in an editor |

### Links

Built: `omnigraph-studio://` links (`studio::deep_link`), registered with macOS in the bundle's
`Info.plist`. `graph?server=…&graph=…[&branch=…]` opens a graph; `node?…&type=…&id=…` opens it
and shows the node in Explore and the inspector; `query?…&text=…` opens it with a new query tab
holding the text, which is not run (at most 64 KB). A link opens in the window that already has
that graph on that branch, else in the active window, else in a new one. It never runs a query,
writes, adds a server or carries a token: the server must already be known (compared after
`graph_client::server_root`, so `127.0.0.1:8787` matches `http://127.0.0.1:8787`), and an unknown
one is refused with the reason. `studio::OpenLink { url }` opens a link from inside Studio.

## Deferred

- `omnigraph-studio://connect?code=…`, the launcher's single-use code (engine change E1): a link
  of that kind is refused with a note until OmniGraph has `omnigraph studio`.
- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E9 branch details: the branch menu derives head and fork from `GET /commits?branch=` until then.
  - E15 who am I: every action stays visible; a 403 is shown as a denial and the draft stays.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-13 | The palette's selected row is never clipped | offscreen snapshots: ⌘P, ⌘2, "Person" (one row), and ⌘K "open" with ↓↓ | Before, the list's height was its rows alone while its padding sat inside it, so the rows overflowed by 8 px and scrolling to the selection hid the top of the single row under the tabs. The height now includes the padding; the row sits whole with even space, and longer lists keep theirs |
| 2026-10-12 | Links open a graph, a node or a query without running anything | `studio` `deep_link` tests (each route, missing parts, other schemes, unknown routes, text over 64 KB); offscreen snapshots through `studio::OpenLink`: a query link to `notes`, a node link with a scheme-less server, a link to an unknown server | `notes` opened beside `knowledge` with a Query 1 tab holding the text and no rows; Donald Dijkstra in Explore and the inspector; "Studio does not know the server https://elsewhere.example…" and nothing opened |
| 2026-10-11 | Fork sources exist at the tag | `git ls-tree v1.23.2 crates/workspace/src crates/picker/src crates/command_palette/src` | Present, as listed above |
| 2026-10-11 | `ui` components needed by the frame exist | `git ls-tree -r v1.23.2 crates/ui/src/components` | Present, as listed above |
| 2026-10-12 | The palette's rows come from where the person was | `gpui/src/window.rs` `available_actions` (the focused node in the rendered frame, plus global listeners) and `dispatch_action` (captures focus when called) at `v1.23.2` | The list is built before the modal takes focus; on ↵ focus is restored, the modal dismissed, then the action dispatched |
| 2026-10-12 | Zed's global handlers appear as rows | palette snapshot | `editor::init` registers `workspace::NewFile` and `workspace::NewWindow` globally; their handlers read Zed's `AppState` with `cx.global`, which panics in Studio. The palette hides `workspace::`, `zed::` and `menu::`; ⌘N is rebound to `studio_graph::NewQuery` at the root (spec 007), so the key no longer reaches `NewFile` |
| 2026-10-12 | With nothing focused, an action reaches only global handlers | offscreen snapshots: `studio::ToggleSidebar` and `studio::OpenGraph` did nothing until the workspace had focus | A window opens with no focus, so the workspace's handlers and key bindings are unreachable until something is clicked. The snapshot example now focuses the workspace first; the app should focus it when a window opens |
| 2026-10-12 | Settings rows are generated in declaration order | `studio_settings` field tests (row order, sections, kinds, path writes and resets); `serde_json`'s `preserve_order` is on in the workspace graph; offscreen snapshots of the Appearance section and of searches for "page" and "agent" | Fourteen rows in four sections; `agent.env` hidden; changed rows show Reset and the nav counts them |
| 2026-10-12 | A graph's own value reaches its views | offscreen snapshots with the new `click:x,y` step: the scope switch, then + on Table page size in the graph scope; `settings.json` after; the Person table after | `graphs."environment/knowledge".type_table_page_size` 125, `studio.type_table_page_size` still 100; the table footer reads "125 of 200" |
| 2026-10-12 | Panes, pins and previews | `studio_workspace` pane tests (preview replaced in place and kept, pin order, close others, removal, split nesting and collapse, JSON round trip); offscreen snapshots: preview Person then Company, pin a query, split right and down, restart with restore, drag the boundary with the new `drag:` step | Company replaced Person in its place in italics; the query stayed first with a pin; both splits showed a copy of the Project table; the layout, pins and shares came back after the restart |
| 2026-10-12 | Two graphs in one window | a second dev graph `notes` (`dev/cluster/notes.pg`, `dev/notes-seed.jsonl`); offscreen snapshots: Person and a query on `knowledge`, Open Graph, `click:` on `notes`, a Note table split in two, the switcher; then ⌃⌘] | The switcher listed both with `notes` checked and "+1 graph" in the footer; stepping back showed `knowledge` with its Person tab and the query draft |
| 2026-10-12 | Palette scopes and Ask | offscreen snapshots with the new `key:` step: All for "pro", Nodes for "ada", ⌘4 with two query tabs, then Tab on "who knows ada" | Types, nodes and commands under All; five Adas under Nodes; both query tabs under Queries; Tab opened the agent tab with the question waiting. A Tab bound under `CommandPalette` alone lost to a binding at the editor level |
| 2026-10-12 | Every tab kind comes back after a restart | offscreen snapshots: one run opened a type table, a query with a draft, history, canvas and storage; a second run with `OMNIGRAPH_STUDIO_NO_RESTORE=0` | All five tabs restored in order, the draft intact; the workspace file is `-rw-------` |
| 2026-10-12 | Cached tabs and docks: a frame redraws only what changed | snapshot example `measure:60` (debug build) on Sammet Systems → WorksAt → Ada open on the canvas, the Explore tree and the inspector showing | Before: 9.6 ms a frame after a workspace notify, the same as a full redraw (nothing was reused). Tabs cached: 8.7 ms. Docks hosted and cached too: 3.2 ms. A full redraw (`refresh`, for a settings change) is 11.9 ms. The docks alone had cost about 7 ms a frame (the inspector 3.4, the tree 1.7) |
| 2026-10-12 | Every window comes back | `persistence` tests (the older one-window file, opening order, a closed window forgotten, the last one kept until another opens; a GPUI test removing one of two windows); the debug binary with a separate data directory and a `session.json` of three windows, one on an unknown server | Two windows opened at their saved bounds, the third dropped, and the unknown server's graph left out of the other; an older file was rewritten in the new shape on the first start |
| 2026-10-13 | ⌘P reads only what typing could have changed | `command_palette` test `a_complete_answer_narrows_without_a_read_and_a_full_one_asks_again`, `gq_language` test `longer_search_text_matches_among_what_shorter_text_matched` (a final sigma refuses to narrow); offscreen snapshots through a request-counting proxy on the dev graph's three node types: "ad"; "ad", "a", " l"; "ad", "a", backspace | 3, 5 and 4 `POST /query` (before: 3, 9 and 9); "ada l" showed Ada Lovelace alone. The same run with "ad", "a", " l" had crashed before: typing again cut the node rows after the matches were ranked over them |

## Corrections

- 2026-10-12 · After a graph switch the next action went nowhere: the Open Graph modal handed
  the focus back to a tab of the graph just parked, which is no longer drawn. Parking clears the
  modal's saved focus; a modal with none focuses the shown graph's active tab.
- 2026-10-12 · ⌘W, the dock toggles (⌘B, ⌘⌥B, ⌘J) and next and previous tab had no key:
  Zed's bindings for them name Zed's actions, which Studio's keymap loader drops. Studio binds
  its own in the `Workspace` context.
- 2026-10-12 · The table page size, relationship page size and canvas node limit were settings
  rows that changed nothing: the views used constants of the same values. They now read the
  settings through `GraphSession::options`.
- 2026-10-12 · The palette is not forked from Zed's `picker`, which depends on `project`,
  `workspace` and `db`. It is a small list of its own (a uniform list, Studio's matcher and
  recency file), following `command_palette.rs` at `v1.23.2` for the focus and dispatch order.
