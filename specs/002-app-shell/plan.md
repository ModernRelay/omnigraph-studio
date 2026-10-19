# 002: App shell

Status: in progress · 2026-10-13. Built: the window frame with panes, tabs and docks, menus,
settings, several windows restored on launch, and `omnigraph-studio://` links. The command
palette landed 2026-10-12 (`crates/command_palette`), with scope tabs and the Ask hand-off.

## Problem

Every surface needs the same frame: a window with docks, panes and tabs, a status bar, a command
palette, menus, settings, a theme, notifications, and keyboard bindings. Zed has
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
shares that drag to resize). Each pane has its own tab strip, which starts with that pane's Back
and Forward buttons; the top-left one makes room for the traffic lights. ⌘\ and ⌘⇧\ split right
and down with a copy of the active tab (reopened from its saved form), ⌘⌥→/← move between
panes, a pane that loses its last tab closes. Pinned tabs come first with a pin instead of a
close button and survive "Close others" and "Close all"; a tab's right-click menu has the
close variants, Pin and the splits, as Zed's: each entry acts on the clicked tab and shows the
key of the action it stands for, read in that tab's context, and a close variant with no tab to
close is disabled. A preview tab (italic title) is what a single click in the
Types list opens; the next preview replaces it in place, and a double click, opening it again
without preview, or work in it keeps it. Tabs that hold unsaved work stay open on the bulk
closes. The workspace file saves each pane's tabs, active tab and pinned count, the split tree
and the active pane; a file from before splits restores as one pane.

Built (several graphs in one window): opening another graph parks the shown one with its panes,
tabs and their back/forward trails, saved first; the status bar's server chip is the switcher ("Open in this
window", the shown graph checked, Open Another Graph…, Close Graph) and reads "+N graphs" when
more are open; ⌃⌘] and ⌃⌘[ step through them. Closing a graph saves its tabs (opening it
again brings them back) and shows the one opened before; a graph with a write still running
stays open, so the write's outcome is never dropped. The same holds for its window and for
Studio: closing a window or quitting while one of its graphs has a write running is refused
with a toast in that window, which comes forward. Toasts follow the shown graph; a parked
graph's writes still land in its activity log. Each window's graphs (server URL, graph,
branch, and the one shown) and bounds are written to the owner-only `session.json` as they
change, in the order the windows opened. The next start reopens every window that has a graph
on a server still known, where it was, showing the graph it showed; with none it opens one
window asking for a graph. Closing a window forgets it, unless it was the last one open: that
one comes back at the next start, or when the Dock icon is clicked with no window open, until
another window opens. Quitting keeps every window. A new window (⌘⇧N) asks for a graph; it
opens where the last moved window was. A `session.json` from before held one window's graphs
and is read as one window.

Built (docks and status bar, after Zed's, 2026-10-16): the window keeps its column headers (no
title bar; Ragnor's choice over a Zed-style title bar) over one status bar across the window.
- Each column's header is its panel's own, as a Zed panel draws its header: the active panel's
  title and its buttons (search, new). The sidebar's header also holds the traffic lights, the
  hide button and search; the inspector's and the bottom panel's end with a hide button.
- Panels are switched from the status bar, as Zed's `PanelButtons`: one icon per panel, the left
  dock's at the far left, the bottom dock's then the right dock's at the far right. The active
  panel of an open dock is lit (accent icon); pressing it hides the dock, which keeps that panel
  for the next time; pressing any other shows its panel and opens its dock. The tooltip names
  the panel, or "Hide sidebar" (inspector, bottom panel) with the key. A hidden sidebar comes
  back from its icons; before, only ⌘B did.
- The status bar's first button is the sidebar toggle, as Zed's: its icon drawn as the sidebar
  is (shown or hidden), then a divider. The sidebar's header has no toggle of its own: the
  traffic lights' room, the panel's title and search.
- The status bar (30 px, `ds::size::STATUS_BAR`: Zed's 22 px buttons with 4 px around them)
  reads, after the sidebar toggle and the left dock's buttons: the
  server chip (state and round trip; the graph switcher), the graph chip and the branch menu,
  then the few facts the open tab adds (`Item::footer`: a query's rows and time, a Blob's size;
  not what the tab shows itself, such as a table's row count or a schema's counts); at the
  right, what is running, a keyboard button whose menu lists the tab's keys (`ItemFooter::keys`,
  Zed's context menu: each entry runs its action, its binding read in the tab's context),
  Studio's memory and CPU, then the bottom and right docks' buttons. Key hints are not spelled
  out in the bar, and a tab's controls belong in the tab. Groups are split
  by Zed's divider (`ds::divider_v`). As in Zed, the panel buttons and the left side's chips are
  4 px apart; the right side's bare text keeps 8 px. On glass it sits on the wash under the
  sheets.
- Every panel has its own icon: Graph `Blocks`, Explore `ListTree`, Queries `FileCode`, Branches
  `GitBranch`, Conversations `ZedAssistant`; Nodes `Table`, History `HistoryRerun` (its tab and
  header too), Activity `BoltOutlined`. Zed draws `GitGraph` and `GitBranch` nearly alike, so
  `GitGraph` is left to the ontology map.
- Back and Forward are kept per pane, as Zed's `NavHistory`: a pane records the tab it leaves,
  its arrows (and ⌘[ / ⌘] for the active pane) step through the tabs it showed, skipping closed
  ones, and a new pane starts with none. An arrow with nowhere to go is disabled.
- ⌘J is the bottom panel (View → Bottom Panel), which holds Nodes, History and the activity
  log; it was labeled "Activity".
- A panel moves to another dock from its button's right-click menu (Dock Left, Dock Bottom,
  Dock Right, the current one checked), as in Zed. The choice is a setting,
  `panels.<id>.dock`, written into `settings.json` in place; moving a panel back to its own
  dock removes it. Panels are placed when they are added and moved when the setting changes, a
  shown panel staying shown where it lands. A panel is drawn as its feature made it wherever it
  sits: a side panel in its scrolling host, a bottom panel cached at the dock's size.
- `studio.bottom_dock_layout` ("Bottom panel" on the settings tab) is Zed's: `contained` (under
  the document, the default), `full` (across the window), `left_aligned` (under the sidebar and
  the document) and `right_aligned` (under the document and the inspector). On glass a bottom
  panel outside the document is a sheet of its own.
- ⇧⎋ (`studio::ToggleZoom`) zooms the active pane over the document area, with Zed's 8 px
  inset, a border and a float shadow; the right and bottom docks' headers have a zoom button
  too. Pressing it again, showing a tab of another pane, or hiding the zoomed dock ends the
  zoom. A zoomed header keeps room for the traffic lights.
- Each graph keeps its own docks: showing a graph again from the switcher brings back its docks
  as it left them (open, active panel, size), not the shown graph's; before, only a graph
  opened fresh read its saved docks.
- Tabs, after Zed's tab bar. A strip's tabs scroll when they do not fit, and the active tab is
  brought into view when it changes. The active pane's strip ends with three buttons: "+" (New
  Query, Open Query File…, Go to Type or Node…, History), a split menu (Split Right, Left, Up,
  Down; `studio::SplitLeft` and `SplitUp` are new) and zoom. A middle click closes a tab that is
  not pinned. A tab drags: dropped on a tab it takes that place within its group (pinned or
  not); dropped after a strip's tabs it goes to the end of that pane, moving between panes; and
  dropped on a pane's content within a fifth of the shorter side from an edge it splits a new
  pane off on that side with the tab, the half it will take shaded while dragging. A pane a drop
  leaves empty closes, and a pane's only tab dropped at its own edge stays. A press on a tab
  starts the tab's drag, never a window move. Not built: Zed's per-item position in the
  history (an entry restores the tab, not a scroll position inside it), and Zed's new tab on a
  double click at the strip's empty end (here that zooms the window, as a title bar's does).

### Drawing cost

GPUI rebuilds the whole window on every frame that draws, except views marked cached (Zed caches
each pane and dock). Studio caches the same places: each tab's view, and each dock panel inside
a scrolling host of its own (a cached view needs a definite size, which a panel measured by its
content lacks; the host has one and scrolls the panel). A cached view is drawn again only when it
or a view inside it notifies, or when a setting changes (which redraws every window); otherwise
the frame replays it. A hover, a toast or an agent's streamed reply therefore redraws only what
changed. A dock panel's title is in its column's header, above its scrolling content. The snapshot example's
`measure:<n>[,refresh]` step times frames after notifying the workspace, or redrawing all. The
workspace itself renders whenever anything inside it notifies, so it compares its state with
what was last saved at most every 250 ms rather than serializing every tab on every frame.

### Window chrome

- Transparent title bar with no title area: the traffic lights sit in the first column's
  header (the sidebar's, else the top-left pane's), and every header strip moves the window.
  As in Zed's title bar, that room is left on macOS only, and not in full screen, where the
  lights hide with the menu bar; there the strip starts at its own edge.
- Status bar left: the docks' panel buttons, then the connection chip (`ready`,
  `connecting…`, `contract mismatch`, `unauthorized`, `unreachable`, with the contract and the
  round trip), the graph and the branch menu, which switches branch for the window. Right: what
  is running ("2 queries running", "Writing (1)…"), memory and CPU, the other docks' buttons.
- Not built: the actor in the status bar; a `write · <branch>` chip whenever the active item can
  write, and a read-only chip when the actor cannot write or the item is pinned to an older
  commit.

### Actions and keys

- Actions in `studio_actions`, one module per surface. Doc comments are the palette text.
- Default keymaps in `assets/keymaps/default-{macos,linux,windows}.json`. Contexts:
  `Workspace`, `TypeTable`, `Tree`, `Canvas`, `QueryEditor`, `Review`, `History`, `Inspector`.
  Every action without arguments has a key on every platform except, as in Zed, the rare and
  the destructive (reveal the log, the design system gallery, directional splits left and up,
  pin and keep tab, close graph, branch create, delete, merge and review, history, the storage
  inspector, the agent's queue, restart and conversation list), which the palette and menus
  reach. `crates/studio/tests/keymaps.rs` holds that list and fails on an action outside it with
  no key. Workspace keys from Zed: ⌥⌘O open a graph (Zed's open recent), ⌥⌘T close others, ⌘R
  refresh the graph (a table's own ⌘R refreshes the table); ⌥⌘W closes every tab, since ⌘K is
  the palette and cannot start Zed's ⌘K W chord. ⌘N in the agent panel starts a conversation.
- Global: ⌘K palette, ⌘P go to type or node, ⌘⇧B switch branch, ⌘, settings, ⌘1–⌘9 tabs,
  ⌘[ / ⌘] back and forward, `?` key sheet (outside text fields).
- In lists and tables: J/K or ↑/↓, ↵ open, Space peek, Esc steps back one level (close editor,
  then peek, then clear selection).
- The person's `keymap.json` in the data directory loads after Zed's keymap and Studio's, in
  Zed's format, so its bindings win; `null` removes a key and comments are allowed. Studio reads
  it again when it changes (checked every two seconds) and rebinds from the defaults up, then
  sets the menus again, since a menu's shortcuts are fixed when it is set. A binding that does
  not load is listed on the settings tab and in a toast ("`cmd-k` in the `Workspace` section:
  didn't find an action named …") while the rest apply; a file that does not parse keeps the
  bindings in effect. The settings tab's keyboard button shows the file, writing it first when
  there is none: an empty `Workspace` section and Studio's bindings as a commented reference.
  Every binding comes from these three files; no crate binds keys in code.

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
  until the new answer replaces them. From the keystroke until every type has answered, a
  turning circle (`ds::spinner`) sits at the end of the search field in All and Nodes, an
  empty list reads "Searching nodes…" instead of "No node matches", and the footer leaves
  out its count until something has come back.
- Opens in one frame with no animation (constitution VII).
- Landed 2026-10-12: ⌘K and ⌘⇧P (Ctrl elsewhere), bound by the default keymaps, so a person's
  keymap can unbind them. Rows are humanized action names (`studio_graph::OpenType` → "Graph:
  open type"), fuzzy matched with word-start and run bonuses, each with the binding that wins
  where the person was and the action's doc comment in the footer. Recently run rows come first
  (`command-palette.json` in Studio's data directory, read off the main thread and written on
  it, so two runs close together save in order);
  then Studio's actions, node and edge types, and the rest. A type row dispatches
  `studio_graph::OpenType` and appears only when that action is available where the person is.
  ↑/↓ move, ↵ runs after the palette closes and focus returns, Esc closes.
- Since 2026-10-18 the palette is Tower's: a 580 px glass frame (`ds::surface::floating`,
  `radius::MODAL`, 4 px inset) holding the search and scope tabs on the frame, one solid card
  of results rounded within it (`radius::PALETTE_LIST`), and the description and count under
  it. Its rows are menu rows (`ds::MenuRow`, 32 px). It sits a tenth of the window down over a
  light veil (`ModalPlacement::Palette`), not a dialog's dim.

### Tooltips and disabled states

Every button's tooltip shows the action name and binding (Zed's `Tooltip::for_action`). A
disabled control's tooltip gives the first blocking reason ("Read-only: pinned to commit
01JA…", "Your token cannot write to main"). Where a CLI command does the same, the tooltip's
second line shows it (`omnigraph branch create feature-x --from main`).

### Settings (`studio_settings`)

- One typed document with serde defaults in `assets/settings/default.json`, the user file
  `settings.json` in the app config directory, and per-graph overrides.
- Watched on disk; parse errors become a notification that names the line.
  - Built: the file is the person's text (`studio_settings::document`). It may hold comments
    and trailing commas. A value that does not fit costs only its own setting, which takes its
    default; a key no setting reads is reported. Every problem is listed with its path and
    line on the settings tab ("`studio.canvas_max_nodes` (line 5) was not applied: invalid
    value: integer `-1`, expected a whole number, 0 or more") and summed up in a toast. Inside
    a list, an element with a bad part (one saved view) is set aside alone; a list with a
    wrong element (a number in the agent's command) is set aside whole. A change from the tab
    reads the file again, then rewrites only the settings it touches, in place, with Zed's
    `settings_json` editor: comments, order, unknown keys and values that did not fit stay. A
    new key goes in first, so a comment above the object's first key stays with it; a setting
    put back to its default is removed, and so is an object that leaves empty, so a later
    change to a default reaches the person. An edit from another program that the watch has
    not read yet survives the change and applies with it.
- A settings window (⌘,) generated from the same schema: search box, left nav, rows of title,
  one-line description and control, "Reset to default" per row. Forked in spirit from Zed's
  `settings_ui`; rows are generated, not hand-written.
  - Built: `studio_settings::fields` walks the `schemars` schema of `StudioSettings`. A field's
    doc comment is its description; `title`, `range` and the extensions `x-section`,
    `x-control` (`theme`, `font`, `agent`), `x-step` and `x-hidden` give the rest. The tab
    draws a control per kind (segmented choice up to four, a menu above that, a stepper in
    range, a switch, a text field, the theme and font menus, the agent presets) and writes by
    path, so a new setting is a field and its attributes. A text field writes when Enter is
    pressed or it loses focus, not on every key; empty unsets a setting that may be unset.
    Typing not yet written stays when the values are drawn again (a switch pressed before the
    field hears it lost focus), and the scope switch writes it to the scope it was typed in
    before showing the other. A
    doc comment's lines read as one paragraph. Only `agent.env` (a map) and `graphs` stay in
    `settings.json` alone. The nav counts changed settings per section;
    search spans every section; the search box has focus when the tab opens.
  - Per-graph overrides: `graphs."<server name>/<graph id>"` holds a graph's own values for
    the settings marked `x-per-graph` (table and relationship page sizes, canvas node limit).
    With a graph open, the tab's scope switch ("All graphs" | the graph) shows only those,
    writes the graph's own value, and Reset removes it; an own value's description names the
    shared one. Views read the values in effect through `GraphSession::options`.
- `studio_theme` implements `theme::ThemeSettingsProvider` from these settings: UI font,
  buffer font, sizes, density.
- The default light theme is Vitesse Refined Light (2026-10-16; it was One Light), the theme
  Ragnor uses in Zed. Studio bundles its family unchanged from the Zed extension
  `vitesse-theme-refined` 0.5.0 (MIT, Colin Lienard, after Anthony Fu's MIT Vitesse) as
  `assets/themes/vitesse-refined/`; `studio_theme::Assets` lists Studio's themes beside Zed's, so
  Zed's loader registers them before the first window, and all five appear in the theme menus.
  The dark default stays One Dark.

### Modals

A modal is a `ModalView`. As in Zed's modal layer, the workspace asks its
`on_before_dismiss` before every close: ⎋, a click outside, the same command again, and the
modal's own dismissal. A dialog with a write running (insert or delete a row or an edge,
create or delete a branch) says no, so the outcome it shows is not lost and the task that
waits for it is not dropped.

Since 2026-10-18 every dialog is one card, after Tower's dialogs (`studio_ui::modal`, through
`ds`): `ds::modal` (480 px, `size::MODAL_WIDE` 720 for Open a graph; 16 px corners, the
floating fill, an 8 % hairline and the sheet shadow), then `ds::ModalHeader` (the title,
semibold; beside it what the dialog writes to, `ds::write_target`; a round outline close
button, faded while a write runs), `ds::modal_body` and `ds::modal_footer` (a strip under a
hairline, the actions on the right: Cancel an outline pill, `ds::cancel_button`; the action
solid, `ds::action_button`, red for a delete). Explanation is one muted paragraph, the
header's description; the body holds only what that paragraph names (a list, fields) and a
write's outcome, and is left out when empty. The workspace places a dialog over a dim
(`surface::backdrop`) a quarter of the way down the space it leaves free, Tower's `1fr auto
3fr`; a click on the dim closes it. Modals are opaque: GPUI cannot blur what is behind them,
and a 97 % fill let the text behind show through.

### Files, logs and the data directory

- Every file Studio keeps (the workspace and session files, window bounds, settings, saved
  servers, recent commands, the agent's conversation index, saved queries) is written through
  `studio_settings::write_atomic`: a temporary file beside the target, synced, then renamed over
  it. A crash or a full disk leaves the old file or the new one, never an empty one. Studio's
  own files are owner-only (`0600`); a query file the person chose keeps its mode, and a link
  is written through to its target.
- A file Studio wrote but cannot parse is set aside as `<name>.unreadable-<seconds>` before
  anything replaces it (workspace, session and conversation files). A `settings.json` that
  does not parse keeps the settings in effect and refuses changes from the settings tab until
  it reads again, so the person's text is never replaced by defaults. A change is written
  into the file as it is on disk at that moment, never over it.
- The data directory is the person's unless the binary is Cargo's build output (`cargo run`,
  an example): that uses "OmniGraph Studio Dev" (`omnigraph-studio-dev` on Linux). The data
  directory is `%APPDATA%\OmniGraph Studio` on Windows (`OmniGraph Studio Dev` for a development
  run). A bundled
  app is the person's whichever profile built it. `OMNIGRAPH_STUDIO_DATA` overrides both; the
  snapshot tool sets a fresh temporary directory when it is unset. With restoring off, Studio
  saves no tabs or window bounds.
- The log (`logs/Studio.log` in the data directory, about 1 MB, with `Studio.old.log`) holds
  warnings, errors and panics from Studio and the Zed crates, through Zed's `zlog`; a run from a
  terminal logs to standard output, and `RUST_LOG` changes the level. A panic is logged with its
  thread and location, its message stripped of quoted text. Help → Reveal Log File shows it.
- A transport failure reads as its cause ("Could not reach the server: the connection was
  refused", "… the request timed out"), never with the request URL, which can hold a Blob
  row's key.

### Notifications

- Silent success for routine work. A toast only after a durable publish, with one follow-up
  ("Committed to feature-x · View commit"); 10 s, paused on hover.
- Errors as notifications that say what happened and what to do; they stay until dismissed.
- The activity log (bottom dock) keeps every write with its outcome.

### Visual language

Delta's window, measured from the mock on delta.dev (its CSS uses Zed's One Light tokens and
IBM Plex Sans, as Studio does), kept quiet: few colours, hairlines, space over lines. Tokens and components live in
`studio_ui::ds` (spacing, radii, sizes, surfaces, elevation, text roles, and the rows, cards,
strips and states built from them); the "Design system" tab (`studio::OpenDesignSystem`) renders
every specimen in the current theme and window style. Features compose `ds` components rather
than styling one-off elements.

- **Column headers, one status bar.** No title bar. Each column has its own 32 px header strip
  (`size::HEADER_STRIP`, Zed's tab bar), so the rules line up across the window, over one 30 px
  status bar across it (`size::STATUS_BAR`, after Zed's). Header strips are window drag areas; as in Zed, a
  move starts on the first mouse move after a press, so clicks on strip buttons land, and a
  press on a tab drags the tab. The window leaves that band to the app
  (`WindowOptions::app_owns_titlebar_drag`, as Zed's windows): otherwise AppKit drags the window
  from anywhere in the transparent title bar's height, tabs included. A tab's double click keeps
  it and stops there; the strip's own zooms the window.
- **Panels in docks** (`studio_workspace::panels`, after Zed's `Panel`/`Dock` and Obsidian's
  side docks). A panel is a view with an id, a title and an icon; features add one with
  `Workspace::add_panel(position, …)` and show it with `activate_panel`. A dock (left, right,
  bottom) shows one panel at a time, opens and closes as a whole, and draws its active panel
  under a header strip with that panel's title and buttons. Panels are switched from their
  icons in the status bar (Zed's `PanelButtons`); the left dock is 264 px, the right (the
  inspector) 340 px, and the right and bottom docks' headers end with a hide button. Selection
  fills the inspector without reopening a dock the user closed. Each dock's open state and
  active panel are remembered per graph. Documents are tabs in the center, not panels. A panel
  may put small buttons in its header, on the title's line (`Workspace::set_panel_actions`), as
  Delta and Zed put a list's search and "new" there rather than as rows in the list; the
  Conversations panel does (spec 011).
- **Status bar, after Zed's** (until 2026-10-16 two footers, after Delta's). One bar across the
  window: the left dock's panel buttons, the connection (a status dot, the server and its
  state, opening the graph switcher), where the open tab reads and writes (graph / branch with
  the branch menu), then the tab's own facts; at the end what is running, the tab's controls,
  memory and CPU, and the bottom and right docks' panel buttons. A tab supplies its part through
  `Item::footer` (`ItemFooter { start, end }`): the table its loaded count, read commit and key
  hints; the agent its restart, agent pill and send. A pane's header holds its Back and Forward
  and its tabs.
  - The connection's state reads with the server's round trip, from the last health check
    (`Connection::latency`, no extra request): "contract 0.13 · 42 ms". It is timed on the
    background executor, so a busy main thread does not lengthen it.
  - With `studio.show_performance` on (the default since 2026-10-14), the status bar ends with
    Studio's memory and CPU, muted, in the warning colour past 2 GB or half a core:
    "412 MB · 3% CPU". Memory is macOS's physical footprint (`proc_pid_rusage`, what
    Activity Monitor shows) or Linux's resident set. CPU is Studio's own process, every
    thread, user and system time (`getrusage(RUSAGE_SELF)`), over the 2 s between readings,
    as a share of one core: 100% is one core busy throughout, more when several threads are,
    as Activity Monitor counts. Agents, their MCP proxies and the CLIs they run are separate
    processes and not counted. It is read every 2 s while the window is in front, and a
    reading never spans time in the background: the window leaving marks the last reading,
    so the next one, whenever it comes, starts over. The tooltip adds the memory peak and says
    what CPU counts. Other Unix systems show CPU only, and Windows shows neither.
  - Frame times are not in the footer: an FPS counter reads 0 when idle and draws frames
    itself. A build with the `perf` feature (GPUI's `profiler`) has its frame-time overlay,
    `studio::ToggleFrameOverlay` (⌘⌥⇧F); other builds say how to get one.
- **Window styles.** `studio.window_style`: `flat` splits columns with hairlines; `glass`
  floats the three columns as sheets over a soft wash, with one gap around and between them,
  and moves the traffic lights into the sidebar sheet. They sit centered in the first header
  strip (`studio_workspace::traffic_light_position`), where a window also opens with them.
- **Hairlines.** `ds::surface::rule`. A rule between the rows or the sections of a document
  (a table's rows, query results, settings rows, a queue, storage's sections, a feed's date
  rule) is `ds::dotted`: dotted on glass, as Tower's feeds, solid when flat. A region's edge
  (a toolbar's, a strip's, a pane's, the composer's) is solid in both styles.
- **One document fill** (since 2026-10-18). Every tab sits on `surface::document`, which the
  pane paints, not the view: before, the table, schema, agent and canvas were white while a
  query, History, Storage and Settings showed the glass sheet through.
- **Sheet corners.** GPUI clips a glass sheet's children to its rectangle, not its rounded
  corners, so a child with a fill flush in a corner takes the sheet's radius
  (`Radii::sheet`): the first tab of the top-left pane, when the sidebar is shown, and the
  document fill of every pane along the sheet's bottom. Such a pane also keeps its tab a
  corner's radius above the edge (`SheetFloor`), since a view's own fill (an editor's) would
  square the corner again.
- **Quiet things.** A row a filter dims, a card for a side not written yet and a merged
  branch's lane recede by one opacity, `ds::opacity::DIMMED`; what lies away from the
  canvas's focus, its chips and their lines' labels alike, by `ds::opacity::FAR` (they were
  0.3 and 0.25). Colours a view used to derive
  itself are surface roles: the canvas's here-ring and its halo, a pointed chip's hairline,
  the dot grid, the marquee, a highlighted span of query text, rows picked beside the cursor's,
  a card's hairline (also its footer's rule).
- **Notices, chips and checkboxes** (since 2026-10-19). Zed's `Banner`, `Callout`, `Chip`
  and `Checkbox` draw their own corners (4 px, 4 px, 2 px), tones and spacing, which the
  corner setting never reached. Views take `ds::banner(severity)` (a box of the scale's small
  corner), `ds::callout(severity, title)` (a strip under a rule, its description scrolling
  past 128 px), `ds::chip` (a code-like value: a `ds::tag` in the buffer font; a "new"
  marker is a tag) and `ds::checkbox` (a box of the extra-small corner, ink and a check when
  on); a notice's fill and hairline by severity are `surface::notice`.
- **Floating bars and lists.** Everything floating over a document is a `ds::float_bar` (the
  canvas's controls, the table's drafts bar) with bar buttons; a list of rows in a form is a
  `ds::row_list` with its own fill and hairline, and one floating under a field a
  `ds::popover_list`.
- **Corners: one setting, one scale** (since 2026-10-18). `studio.corner_radius` (Appearance,
  0–20 px, 10 by default) is the base every corner derives from, through `ds::radii(cx)`; no
  view picks its own (`rounded_sm()`-style utilities and literal radii are gone). The scale is
  additive, as Tower's (`--radius-sm` is `--radius` less 4 px), because the gaps between nested
  surfaces are fixed: `md` (cards, menus, popovers) is the base; `sm` (rows, fields, segments,
  tabs, icon buttons, status-bar items, tiles) is it less 4; `xs` (key caps, tags, chips, a
  write target, a row list's rows) less 6; `lg` (glass sheets) plus 4; `modal` (modals, the
  palette) plus 6. 0 squares every corner. Nested radii follow by `radius::within`/`around`,
  and the gap counts a hairline border (`size::HAIRLINE`), which GPUI draws inside the box: a
  menu row is the menu's radius less its hairline and 4 px padding, the palette's results card
  and rows likewise, a floating bar is `sm` around its controls 3 px in (`Radii::bar`). A fill
  flush with a bordered edge (a card's header, a modal's footer) rounds its own corners at the
  container's radius less the hairline (`radius::flush`): GPUI clips children to a rectangle,
  not to rounded corners. The `ds` test `nested_radii_are_concentric` holds every pair at every
  base the setting allows.
- **Round ends only for buttons with a label.** A text button, a labelled menu trigger (a
  select, the composer's agent picker) and a filter chip are pills (`Shape::Pill`); everything
  else takes the scale's corner: icon buttons (`Shape::Rounded`), fields, segments and tabs,
  the status bar's pickers and panel buttons, floating bars and the buttons set into them
  (`ds::bar_button`), chips, drag previews. Dots and count badges stay round.
- **Control sizes, in code.** `size::CONTROL` (28 px: a medium button, a field, a floating
  bar, a tile), `size::PILL` (26 px: a filter, a composer's picker, a dock's title tab),
  `size::BAR_BUTTON` (22 px: a segment, a bar's button, the status bar's items), `size::TAG`
  (18 px: tags, count badges, a write target), `size::DOT` and `size::DISCLOSURE`. Every
  single-line field is `ds::field_frame` (the scale's `sm` corner at a control's height, a
  10 % hairline and the control shadow, the error border when invalid)
  around `ds::field_input`, and every search box is `ds::search_field`, so fields line up
  with the buttons beside them: the settings search and text fields, the history and review
  searches, the dialogs' fields, the review editor's fields and the query's parameters. A
  filter among a sidebar's rows (the conversations') is `ds::sidebar_search` instead: a
  sunken row as tall as the rows, its text on their edge, with no border.
- **Tabs** fill the 32 px strip (an explicit height: the right-click wrapper lays a tab out at
  its content's height, so `h_full` would shrink it to its label); the active tab joins the
  document, inactive ones sit on the strip's rule. A tab is laid out as Zed's `Tab`: 4 px
  padding and gaps, a 12 px slot before its icon and title for its state dot (unsaved, newer
  data) and a 14 px slot after them for its close or pin button, both kept when empty; the
  close button shows only under the pointer, the active tab's too (Zed's
  `show_close_button: hover`), so a title sits evenly between its tab's edges. The
  strip's Back/Forward and its end buttons sit in Zed's regions: 6 px padding, 4 px gaps, 22 px
  buttons with 14 px icons, as do a dock header's zoom and hide; a dock header's title starts
  16 px in, the edge its panel's toolbar and rows start at.
- **Buttons, after Tower's** (since 2026-10-18; `studio_ui::button`, `ds::Button`,
  `ds::IconButton`). A text button is a pill and an icon button takes the scale's corner
  (above), medium weight, in six variants: primary (solid ink; a surface's one action), secondary (a faint ink
  fill), outline (the surface's own fill, a 10 % hairline and a contact shadow; Cancel),
  ghost (nothing until hovered; toolbars, rows), destructive (solid red) and glass (a floating
  pill). Zed's `ButtonStyle` picks the variant, so call sites keep `.style(…)`: Filled is
  primary, Outlined outline, Subtle ghost, Tinted(Error) destructive. Sizes keep Zed's
  heights, and a view names a button by where it sits, never by its size (since 2026-10-19,
  when the same row action was an 18, 22 or 28 px square from one view to the next): a
  control's, `ds::button` and `ds::icon_button`, 28 px (`size::CONTROL`, 10 px in), beside a
  field or among a dialog's actions; a strip's, `ds::toolbar_button`, `ds::bar_button` and
  their icon forms, 22 px (`size::BAR_BUTTON`: the header strips, floating bars and the
  status bar); a row's or a card's, `ds::row_button` and `ds::row_icon_button`, 18 px
  (`size::ROW_BUTTON`, a tag's height: a tree row's toggle, an inline Fix, Show more, a
  setting's reset, a value's copy); and a tab's pin and close, `ds::inline_icon_button`, no
  square beyond its icon. An icon button's icon follows its size: 14 px at 28 and 22 px,
  12 px in a row and a tab; a sheet's close, `ds::sheet_close_button`, is the one role that
  sets its own, an outlined 22 px square with the 12 px icon. A plain `IconButton::new` is the 22 px one, as Zed's, so its
  hover fill keeps 4 px clear in the 30 px status bar and 5 px in a 32 px strip (until
  2026-10-18 every icon button defaulted to 28 px, which nearly filled the strips it sat
  in). A label is the body size at 28 px and up (13 px at the default 15 px
  interface size, Tower's `text-sm`) and the small size below; a leading icon is 16 px at
  28 px and 12 px below, a trailing one (a chevron) 12 px, both at 80 % of the label's ink.
  A toggled button sits on the pressed fill; a disabled one fades to 64 %; a key hint on a
  solid button takes its ink. Primary and destructive carry a 16 % white top edge inside and
  a 24 % tinted contact shadow; outline buttons and fields the control shadow
  (`elevation::control`: a 5 % contact shadow and a 4 % 1 px edge under them). A toolbar's
  buttons sit in `ds::toolbar_actions`, with no gap beyond their own padding. Zed's `ButtonLike` keeps its corners at 4 px and
  its colours private, so Studio draws its own and keeps Zed's builder traits; no view
  constructs Zed's `Button` or `IconButton`.
- **Menus** (`studio_ui::menu`, a fork of Zed's `ContextMenu` at v1.23.2, GPL-3.0-or-later,
  credited in the file). Zed's behaviour (keys, submenus, asides, actions, accessibility) with
  Tower's drawing: the modal's fill, hairline and shadow at the scale's `md` corner (`Radii::menu`), a 4 px
  inset, 26 px rows (`ds::MenuRow`, `size::MENU_ROW`) rounded within it, the hovered or
  highlighted row on a faint ink (`surface::highlight`), group headers small and muted,
  separators edge to edge. Every view's menus are this one.
- **Icons** (since 2026-10-18): Nucleo's `ui` family, outline-duo at 18 px, as Tower's.
  Views keep naming Zed's `IconName`; `studio_theme::Assets` serves a Nucleo drawing for each
  path `studio_theme::icons::NUCLEO` lists, so Zed's own components change with them. Brand
  marks and key-cap glyphs keep Zed's. Nucleo's licence allows an open-source project 100
  icons with its notice: `script/vendor-icons.py` copies only the listed drawings from the
  licensed bundle, and a test keeps the count under the limit and every file to a row.
- **Spacing.** Every gap and inset is a `ds::space` step (2, 4, 6, 8, 12, 16, 24, 32), never
  GPUI's rem utilities (`gap_2()`): since 2026-10-18 no view uses them.
- **Everything from the design system, checked** (since 2026-10-18). A view takes every size,
  space, corner, offset, colour, shadow and weight from `studio_ui::ds`: `ds::size`,
  `ds::space` and `ds::radii` by role, `ds::surface` colour roles (document, chrome,
  status bar, tabs, header strip, solid, paper, floating, highlight, pressed, rule, focus,
  accent, warning, drop target, on tint, control border, error border) or a `ui` `Color`,
  `ds::elevation`, and `ds::text::weight::{BODY, HEADING}`. Only `ds` reads the theme's
  palette. `script/check-design-tokens.py`, run in CI beside the layer check, refuses
  anything else outside the design system; an exception carries a `// ds-exempt: <reason>`.
  It also refuses any label or icon given a size (its builder chain is read whole), a label
  given only the muted colour, a button given a sized or tinted icon, a button's size or an icon
  button's icon size named directly (`ButtonSize::…`, `.icon_size`), any icon, label or type
  size named in a view (`IconSize`, `LabelSize`, `TextSize`), GPUI's text sizes, a border wider than a hairline written as a GPUI utility
  (`.border_l_2()`), an opacity written as a number, and Zed's `Divider`, `Banner`,
  `Callout`, `Chip` and `Checkbox`; the forked Zed menu keeps upstream's calls.
  Data column widths and algorithm constants (zoom steps, layout forces, text-width
  estimates) are named once where they are used; the canvas's chip text is world geometry
  the zoom scales.
- **Weights.** Two: the body's (regular) for everything read, buttons, segments and a key
  column included, and the heading's (medium) for what names a surface: a toolbar's, a
  page's or a dialog's title, a tree's root, a card's subject. Inter's heavier cuts read
  heavy at these sizes; semibold is not used.
- **Text sizes.** The window's rem size is `ui_font_size`, as in Zed, so every `ui` size
  scales with it. Its default is 15 px since 2026-10-18 (16 before): Tower's root size
  (93.75 %), at which Zed's `Default`, `Small` and `XSmall` labels are 13, 11 and 9 px, Tower's
  `text-sm` and `text-xs`. Body text is `Small`; labels, counts and metadata `XSmall`; buttons
  `Default`. Views take text and quiet icons by role (`ds::text::body`, `muted`, `meta`,
  `state` for an empty or waiting state's line, `error`, `error_note`; `ds::icon::lead`,
  `meta`; a page's one title is `text::page_title`) rather than setting a label's or an icon's
  size and colour themselves: a view adds at most a semantic `ui` `Color`. Text a view does
  not set itself (an agent's reply, the composer's editor) takes `ds::Prose` (`prose`,
  `quiet_prose`), not GPUI's `text_sm()`; text that is not a label (a markdown value, a grid
  cell, an editor's font) takes the size of its role, `ds::text::plain_size`, `body_size` or
  `meta_size`, never a `TextSize` named in the view. A button sizes and inks its own icons (above), so
  it is given a plain `Icon::new`. The canvas's label fill and a far hub's dot are surface
  roles too (`canvas_label`, `canvas_hub`). The interface font is Inter by default (Tower's face; Inter 4.1, OFL-1.1, the
  regular, italic, medium, semibold and bold static files in `assets/fonts/inter/`, loaded
  with Zed's fonts); IBM Plex Sans and every installed font stay selectable.
- **Toolbars.** A document's toolbar and every header or notice strip (a table's, a query's,
  history's and its commit pane's, the review's, storage's, the inspector's header, the text
  editor's, a newer-data notice) holds `ds::toolbar_button`s and `ds::toolbar_icon_button`s;
  a button inside a dense row (an inline Fix or Run, Show more, a crumb) is a
  `ds::row_button`. Toolbar buttons are Zed's 22 px
  size, Tower's `xs`, with small type and 12 px icons, set with no gap beyond their padding
  (`ds::toolbar_actions`), so the chrome reads smaller than the document it serves.
- **Shadows, Tower's values** (`ds::elevation`, light and dark): `raised` for a solid item on
  glass (4 %, 1/2 px; 30 % dark); `control` for outline buttons and fields; `sheet` for glass
  sheets and everything that floats (an inner white rim at 70 %, 6 % dark; 4 % at 1/2 px and
  11 % at 12/28 px, spread −9; 30 % and 60 % dark); `float` for floating bars and chips
  (12 % at 6/14 px, spread −6; 50 % dark). Menus, popovers, modals and the palette use
  `sheet` (`floating`), as Tower gives every popup the glass shadow.
- **Sidebar rows.** `ds::SidebarRow`: section labels muted, not bold; the selected row a grey
  pill; counts muted and right-aligned. Footnotes become tooltips. What leads a label (an icon
  through `SidebarRow::icon`, at 14 px, or a type mark) sits in one 16 px slot
  (`size::ROW_ICON`), so every list's labels start on one edge, 36 px in.
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
  space as outlined buttons with their key bindings (`ds::empty_state`, `ds::state_action`); an
  empty panel is one dim row (`ds::sidebar_placeholder`). A line the words and actions cannot
  carry, such as a reference's address or why a value is not shown, goes under the title
  (`ds::state_detail`). While content is read, a muted line holds its place
  (`ds::loading_state`); content that could not be read shows an error icon, what failed and
  the reason as it came, then its recovery (`ds::error_state`). These are the only three: no
  view centers a sentence of its own.

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
| 2026-10-19 | Buttons sized by role; every icon, label and type size from the design system | `script/check-design-tokens.py` with its new rules (`ButtonSize::`, `.icon_size`, `IconSize::`, `LabelSize::`, `TextSize::` and `.text_ui*` outside the system); `studio_ui` `controls_draw_at_their_sizes` (a toolbar icon button 22 px, a row button and a row icon button 18 px squares, a tab's inline icon under 18 px, the icon size each button size gives); workspace clippy `-D warnings` and the workspace suite (43 targets) on a copy of the parent commit with only these paths; offscreen glass snapshots on a scratch dev server: the design gallery, Person's table with its peek and a pinned tab, a hovered relationship group | Before: views chose 25 button sizes and 47 icon sizes by name, so one kind of row action was an 18, 22 or 28 px square (the session list, saved queries, the relationship tree, the peek, the review, a toast); 15 calls restated the default icon and four named `TextSize`. After: none, and the guard refuses them. Row actions are 18 px squares with 12 px icons, their hover fill 4 to 10 px smaller; labelled row buttons stay 18 px; a tab's pin and close and every toolbar's buttons are unchanged |
| 2026-10-18 | One document fill with the sheet's corners; rules, dimming and floating bars by role | `script/check-design-tokens.py`; `cargo clippy` `-D warnings` on the ten touched crates; the `studio_ui`, `studio_workspace`, `type_table`, `graph_canvas`, `query_editor`, `history`, `review` and `inspectors` suites; offscreen glass snapshots of a query with results, Person's table, History, Storage, the schema, Settings and the ontology, their bottom corners cropped at 2×, sampled by pixel | Before: the table, schema, agent and ontology were white (255) with square bottom corners on the rounded sheet; a query, History and Storage were the glass sheet (245) with round ones. After: all seven are white with round corners. Settings rows, query results, storage sections and the agent's queue drew solid rules where the table's rows were dotted; the agent's composer edge was dotted beside a solid banner edge. Storage built its own 36 px header with a labelled Refresh; it takes the 48 px toolbar and the icon refresh History and the table use, as the review's header now does. The drafts bar was a solid card with `lg` corners beside the canvas's glass bars. Dimmed things used 0.45, 0.5, 0.55 and 0.6 |
| 2026-10-18 | One corner setting; round ends only for labelled buttons; Tower's type scale, shadows and button type; spacing and alignment by token | `ds` `nested_radii_are_concentric` (the default scale, every pair at bases 1–20, 0 square); `studio_settings` row order with `studio.corner_radius`; `button` tests; workspace clippy `-D warnings`; offscreen snapshots in glass: a table, its peek, the canvas, settings, history, a query, the delete dialog, ⌘K | The canvas's mode switch was a bare 22 px segmented control beside two 28 px glass bars (the segmented track had been removed); it is a bar now, and all three share height, corner and shadow. The table's header text started 4 px right of its cells' (Zed pads a header cell); the inspector's rows 4 px right of their section labels (their hover inset); a peek group's children at 26 and 28 px under an arrow at 22. The branch picker was a 28 px pill with 13 px text beside a 22 px graph picker with 12 px text. Buttons were 12 px text with 14 px icons, 24–32 px apart in the table toolbar; they are 13 px with 16 px icons at 80 %, with their padding as their gap. Fourteen hover fills used Zed's theme hover; they use `surface::highlight` |
| 2026-10-18 | Nested radii count the hairline; flush fills follow their container's corners; no raw radius utilities | `ds` test `nested_radii_are_concentric` (menu row, palette list and row, a row list, `radius::flush` for a card header and a modal footer); offscreen snapshots cropped at 2×: the gallery's card header, the delete dialog's footer, ⌘K's highlighted row, a tab's menu | Before, the card header's fill showed a square corner outside the card's curve; the menu row, palette card and palette row were 1 px rounder than their insets allowed, since the hairline was not counted. Fourteen `rounded_sm`/`rounded_md` calls became tokens by role: the inspector's rows SM, its image frame MD, a table cell's edit notice a menu surface, a dragged column, the agent picker and the tree's filter row pills |
| 2026-10-18 | Tower's buttons, menus, modals and icons; spacing on the ds scale | `studio_ui` `button` tests (`controls_draw_at_their_sizes`: 28 px pills 10 px in, circles, pill fields; `zed_styles_pick_the_variants`), `menu` tests (Zed's six, on the fork), `studio_theme` `icons` tests (≤ 100 Nucleo drawings, each file named once, each replacing a Zed icon); the workspace suite; `cargo clippy --workspace --all-targets -D warnings`; offscreen snapshots in glass on a scratch cluster: the delete dialog with two dependents, ⌘K, a tab's context menu, the design gallery, a table with the new icons | The dialog read one muted paragraph under "Delete Acme", the dependents list, then Cancel and a red Delete with a light ↵; the palette a frame with the results card inside; the menu 26 px rows and edge-to-edge rules. A 97 % modal fill let the rows behind show through above the title; the fill is opaque now. The settings reset, a 28 px circle, made its row taller than the others; it is compact. A spacing conversion made both arms of one `if` equal (clippy); collapsed |
| 2026-10-17 | Views read through the session; growing lists draw only their visible rows | `graph_store` `feature_reads_are_counted_while_they_run` (six held reads counted, then none once dropped); `studio_ui` `rows` `only_the_rows_between_a_shared_start_and_end_change`; the compiler, with the session's client private to `graph_store`; offscreen snapshots of History, Storage, Conversations, Queries and Activity against the dev server | History listed its 10 commits with their counts and the head's change; Storage showed the server, both graphs and main's snapshot; Queries and Activity kept their layout as lists. Conversations' empty note now wraps at the sidebar's width, where it was clipped, and its filter shows a funnel beside the sidebar's search |
| 2026-10-17 | A tab's menu shows its keys; every action has a key or is palette-only; the lights' room only where they show; one loading and one error state | `studio` `keymaps` `every_action_has_a_key_or_is_palette_only` (red on `studio_graph::OpenSchema` while optional-field actions counted; then on none); `studio_workspace` `strips_make_room_for_traffic_lights_only_when_they_show`; offscreen snapshots: the right-click menu on the first and on the last of two tabs, and the design gallery's state row | The menu read Close ⌘W, Close Others ⌥⌘T, Close All ⌥⌘W, Split Right `⌘\`, Split Down `⌘⇧\`; on the last tab "Close to the Right" was disabled. Five actions gained keys from Zed's (⌥⌘O, ⌘R, ⌥⌘T, ⌥⌘W, ⌘N in the agent panel), sixteen stay palette-only. The old centred-sentence helper is gone; its callers read the schema, history, changes, review, Blob and table states through `ds::loading_state`, `ds::error_state` and `ds::empty_state` |
| 2026-10-16 | The status bar lists keys behind one button | Offscreen snapshots of Person's table: the bar, then the keyboard button pressed | The bar reads the panels, the server, knowledge / main, ⌨, the memory and the docks; the menu lists Edit ↵, Peek ␣, Open page ⌘↵, Explore G, Copy ⌘C, Filter by this value ⌥⌘F, New row ⌘N, Delete row ⌘⌫, Save drafts ⌘S, Refresh ⌘R. A Studio view built as the menu closed itself at once; Zed's context menu does not |
| 2026-10-16 | Buttons at Zed's insets; a tab's title centred | `studio_ui` `controls_draw_at_their_sizes` (8 px each side, red at 6 before the change); offscreen snapshots: two tabs with the pointer on the inactive one, the table toolbar, the insert dialog, the bottom dock | The active tab had 21 px before its icon and 8 after its ×; with × under the pointer only, 21 and 23, as Zed's. Toolbar and dialog buttons read with Zed's 8 px; a dock header's title now starts on its toolbar's 16 px edge (it was 12) |
| 2026-10-16 | The sidebar toggle at the status bar's start; tabs drag in the app | `studio` `windows_leave_the_titlebar_drag_to_the_app` (red before the option was set); offscreen snapshots with the sidebar shown and hidden, the ontology browsing Company into Nodes, Activity empty | In the app, dragging a tab moved the window: AppKit owned the title bar's band, where Studio's tabs sit. The toggle reads filled with the sidebar shown and empty with it hidden; the sidebar's list starts 6 px under its header; Nodes and Activity did what their specs say, their empty states now naming where a type is clicked and showing Activity's icon |
| 2026-10-16 | Strips at Zed's heights; tabs, regions and sidebar rows on Zed's spacing | `studio_workspace` `tabs_fill_the_header_strip` (tab and status bar at the new heights) and the workspace suite; offscreen snapshots of two tables, then one with the bottom panel, compared with Zed's and the Claude app's measurements | Strips 32 px, status bar 30. The sidebar's labels now start on one edge whatever leads them (they were 4 px apart between the view rows' icons and the type marks); the Claude app's sidebar rows are 26 px apart, as Studio's already were. No two panel buttons share an icon (Graph and Nodes were both `Table`, Branches and History near twins); "Delete" and the memory readout, 4 px apart, read as one phrase until a rule split them |
| 2026-10-16 | Vitesse Refined Light is bundled and the default | `studio_theme` test `studio_themes_are_listed_and_read_as_families`; `studio` `settings_apply` (the light mode resolves "Vitesse Refined Light" through Studio's startup); the bundled file's SHA-256 equals the copy Zed installed (`9731fc19…2998`); offscreen snapshots of a table with the inspector and History, and a query | Green accents and lit panel buttons, Vitesse's syntax colours in the query; One Light before |
| 2026-10-16 | Tabs scroll, drag and drop as Zed's | `studio_workspace` tests `dropped_tabs_move_within_and_between_panes_and_split_at_an_edge`, `a_drop_near_an_edge_splits_toward_it`, `pane` `dragged_tabs_move_within_their_group_and_arrive_unpinned`; offscreen snapshots: nine tabs; Person and Company with a `drag:` from the Company tab to the document's right edge | Nine tabs in the strip with "+", split and zoom at its end; the drag split the document: Person on the left, Company on the right with the end buttons, the left pane without them |
| 2026-10-16 | Panels move between docks; the bottom panel spans as its layout says; zoom | `studio_workspace` tests `a_panel_goes_where_the_settings_place_it` (placed at its add, home again when the setting goes, the menu's move), `the_bottom_panel_spans_as_its_layout_says` (contained: the window less the inspector; full: the window), `zoom_covers_the_document_area_until_toggled_or_hidden`, `a_graphs_docks_come_back_with_it`, `panels` `a_taken_panel_leaves_the_others_as_they_were`; offscreen snapshots: `full` with History and the inspector, Explore placed in the right dock with the menu on the Graph button, a zoomed pane | History ran under the sidebar and the inspector; Explore showed in the right dock with its icon at the bar's right end; the menu read "✓ Dock Left, Dock Bottom, Dock Right" with no tooltip over it; the zoomed table covered the document area inside an 8 px inset, its arrows after the traffic lights' room |
| 2026-10-16 | Docks are switched from a status bar, and each pane keeps its own back and forward | `studio_workspace` tests `panel_buttons_show_their_panel_and_hide_the_dock_it_shows`, `back_and_forward_walk_the_active_panes_own_tabs`, `tabs_fill_the_header_strip` (the status bar's height), `pane` `a_pane_steps_back_and_forward_through_the_tabs_it_showed`; offscreen snapshots on the dev graph, flat and glass, with the bottom panel, with the sidebar hidden and a row peeked | One status bar across the window: the five sidebar panels' icons at the left (Graph lit), the server, graph and branch, the tab's facts, memory and CPU, then Nodes, History and Activity and, with a row peeked, the inspector (lit). Column headers show their panel's title; each pane's strip starts with ← → (← enabled after a second tab, → disabled). With the sidebar hidden its icons stay in the bar |
| 2026-10-14 | `settings.json` keeps the person's text, and `keymap.json` rebinds keys | `studio_settings` `document` tests (comments and trailing commas, a syntax error's line, each value that does not fit costing only its setting, a saved view set aside alone, edits in place, a new key above a comment's key, defaults removed, a bad value kept until its setting changes, graph entries); `studio` `settings_apply` (`a_change_keeps_the_persons_text`: a hand edit with a comment written before the change and not yet read stays and applies; only chosen keys are written; a broken file refuses the change unchanged) and `keymaps` (`the_persons_keymap_wins_and_is_read_again`: a key added, `null` taking ⌘K from the palette and from the shortcut text, an unknown action reported, a broken save keeping the bindings, a second save restoring Studio's); offscreen snapshot of the settings tab in a scratch data folder with three bad values and a bad binding | All pass. The tab listed "3 problems in settings.json" (the font size, a misspelled `stuido`, a negative node limit) and "A problem in keymap.json" with each path and line; Interface font size showed 16. Before, a comment or one wrong value read as a file that could not be applied, the first change wrote every default into the file, and a change within two seconds of a hand edit wrote over it |
| 2026-10-14 | ⌘K and ⌘P say when they are still looking for nodes | `command_palette` test `the_palette_shows_it_is_searching_nodes` (a fake server; the circle and "Searching nodes…" from the keystroke, no count of none, then the node rows and no circle once it answers; fails without the flag); offscreen snapshots on the dev graph through a proxy that held each node search for 3 s: ⌘K, ⌘3 then "ad"; ⌘K then "ad" | Nodes: the circle at the field's end, "Searching nodes…", no "0 nodes". All: the matching commands at once, with the circle until the nodes came. Before, both showed nothing, and Nodes said "No node matches." while it waited |
| 2026-10-13 | The palette's selected row is never clipped | offscreen snapshots: ⌘P, ⌘2, "Person" (one row), and ⌘K "open" with ↓↓ | Before, the list's height was its rows alone while its padding sat inside it, so the rows overflowed by 8 px and scrolling to the selection hid the top of the single row under the tabs. The height now includes the padding; the row sits whole with even space, and longer lists keep theirs |
| 2026-10-12 | Links open a graph, a node or a query without running anything | `studio` `deep_link` tests (each route, missing parts, other schemes, unknown routes, text over 64 KB); offscreen snapshots through `studio::OpenLink`: a query link to `notes`, a node link with a scheme-less server, a link to an unknown server | `notes` opened beside `knowledge` with a Query 1 tab holding the text and no rows; Donald Dijkstra in Explore and the inspector; "Studio does not know the server https://elsewhere.example…" and nothing opened |
| 2026-10-11 | Fork sources exist at the tag | `git ls-tree v1.23.2 crates/workspace/src crates/picker/src crates/command_palette/src` | Present, as listed above |
| 2026-10-11 | `ui` components needed by the frame exist | `git ls-tree -r v1.23.2 crates/ui/src/components` | Present, as listed above |
| 2026-10-12 | The palette's rows come from where the person was | `gpui/src/window.rs` `available_actions` (the focused node in the rendered frame, plus global listeners) and `dispatch_action` (captures focus when called) at `v1.23.2` | The list is built before the modal takes focus; on ↵ focus is restored, the modal dismissed, then the action dispatched |
| 2026-10-12 | Zed's global handlers appear as rows | palette snapshot | `editor::init` registers `workspace::NewFile` and `workspace::NewWindow` globally; their handlers read Zed's `AppState` with `cx.global`, which panics in Studio. The palette hides `workspace::`, `zed::` and `menu::`; ⌘N is rebound to `studio_graph::NewQuery` at the root (spec 007), so the key no longer reaches `NewFile` |
| 2026-10-12 | With nothing focused, an action reaches only global handlers | offscreen snapshots: `studio::ToggleSidebar` and `studio::OpenGraph` did nothing until the workspace had focus | A window opens with no focus, so the workspace's handlers and key bindings are unreachable until something is clicked. The snapshot example now focuses the workspace first; the app should focus it when a window opens |
| 2026-10-12 | Settings rows are generated in declaration order | `studio_settings` field tests (row order, sections, kinds, path writes and resets); `serde_json`'s `preserve_order` is on in the workspace graph; offscreen snapshots of the Appearance section and of searches for "page" and "agent" | Fourteen rows in four sections; `agent.env` hidden; changed rows show Reset and the nav counts them |
| 2026-10-14 | Every setting with a control is on the page | `studio_settings` field tests (row list, kinds, one-paragraph descriptions); offscreen snapshots in a scratch data folder: a search for "agent", a path typed into OmniGraph CLI then ↵, a second run with Reset, the Ask before writes switch | `agent.ask_before_writes` and `agent.omnigraph_cli`, added for spec 013, had been hidden; text settings had no control at all (the page showed the value read-only), and no switch had been drawn. Now sixteen rows: ↵ wrote `"omnigraph_cli": "/opt/omnigraph/bin/omnigraph"` (mode 0600) and showed Reset, Reset wrote `null` and emptied the field, the switch wrote `true`. Descriptions had kept their doc comments' line breaks, and GPUI breaks lines at `/`, so paths left the descriptions |
| 2026-10-14 | The footers show the server's round trip and, when asked, Studio's memory and CPU | `studio_workspace` test `the_process_reads_its_memory_and_cpu`; `studio_settings` tests (row list, `default.json`); `cargo clippy -p studio --features perf`; an offscreen snapshot with `show_performance` on, a table open, 4.5 s waited, then ⌘⌥⇧F | The sidebar read "environment contract 0.13 · 2 ms", the document footer ended "42 MB · 1% CPU", and the overlay key toasted the `perf` build command in a default build. A `release-fast` build with `--features perf` then drew the overlay after ⌘⌥⇧F (frame time, 1% and 10% slowest, maximum, frames). On the running app (debug bundle), the reading Studio uses, `proc_pid_rusage`'s physical footprint, gave 363.7 MiB where macOS's `footprint` reported 364 MB and `top` 361 MB: the figure Activity Monitor shows |
| 2026-10-14 | Glass corners, button padding and the tab strip | Offscreen snapshots in glass with two tabs and the pointer on the first, the edge dialog, and a query in flat; corners cropped at 2× from every sheet | The first tab's shade had a square corner outside the document sheet's curve, faint at rest and plain on hover; now it follows the curve. Every other corner of the sidebar, document and inspector sheets, including the footer switcher under the pointer, was already clean. Buttons read with 6 px insets at 28 px; header strips are 36 px, footers 40 |
| 2026-10-14 | Nested radii are concentric | Audit of all 69 rounded surfaces; `ds` test `nested_radii_are_concentric`; offscreen snapshots of the filter pill being written with × under the pointer, the edge dialog's offers and the canvas's floating controls, cropped at 2× | Wrong before: offer rows (4 px) in a 6 px list 4 px in, the context chip's 6 px tile 6 px inside 10 px, the canvas bars sized for 6 px children around Zed's 4 px buttons, square 4 px buttons in the pills' round ends. Now the lists are 8 px, the tile 4 px, the bars 7 px, and the pill buttons round with an even 4 px gap. Dialogs, the peek and the palette pad more than their radius, so their children keep their own |
| 2026-10-12 | A graph's own value reaches its views | offscreen snapshots with the new `click:x,y` step: the scope switch, then + on Table page size in the graph scope; `settings.json` after; the Person table after | `graphs."environment/knowledge".type_table_page_size` 125, `studio.type_table_page_size` still 100; the table footer reads "125 of 200" |
| 2026-10-12 | Panes, pins and previews | `studio_workspace` pane tests (preview replaced in place and kept, pin order, close others, removal, split nesting and collapse, JSON round trip); offscreen snapshots: preview Person then Company, pin a query, split right and down, restart with restore, drag the boundary with the new `drag:` step | Company replaced Person in its place in italics; the query stayed first with a pin; both splits showed a copy of the Project table; the layout, pins and shares came back after the restart |
| 2026-10-12 | Two graphs in one window | a second dev graph `notes` (`dev/cluster/notes.pg`, `dev/notes-seed.jsonl`); offscreen snapshots: Person and a query on `knowledge`, Open Graph, `click:` on `notes`, a Note table split in two, the switcher; then ⌃⌘] | The switcher listed both with `notes` checked and "+1 graph" in the footer; stepping back showed `knowledge` with its Person tab and the query draft |
| 2026-10-12 | Palette scopes and Ask | offscreen snapshots with the new `key:` step: All for "pro", Nodes for "ada", ⌘4 with two query tabs, then Tab on "who knows ada" | Types, nodes and commands under All; five Adas under Nodes; both query tabs under Queries; Tab opened the agent tab with the question waiting. A Tab bound under `CommandPalette` alone lost to a binding at the editor level |
| 2026-10-12 | Every tab kind comes back after a restart | offscreen snapshots: one run opened a type table, a query with a draft, history, canvas and storage; a second run with `OMNIGRAPH_STUDIO_NO_RESTORE=0` | All five tabs restored in order, the draft intact; the workspace file is `-rw-------` |
| 2026-10-12 | Cached tabs and docks: a frame redraws only what changed | snapshot example `measure:60` (debug build) on Sammet Systems → WorksAt → Ada open on the canvas, the Explore tree and the inspector showing | Before: 9.6 ms a frame after a workspace notify, the same as a full redraw (nothing was reused). Tabs cached: 8.7 ms. Docks hosted and cached too: 3.2 ms. A full redraw (`refresh`, for a settings change) is 11.9 ms. The docks alone had cost about 7 ms a frame (the inspector 3.4, the tree 1.7) |
| 2026-10-12 | Every window comes back | `persistence` tests (the older one-window file, opening order, a closed window forgotten, the last one kept until another opens; a GPUI test removing one of two windows); the debug binary with a separate data directory and a `session.json` of three windows, one on an unknown server | Two windows opened at their saved bounds, the third dropped, and the unknown server's graph left out of the other; an older file was rewritten in the new shape on the first start |
| 2026-10-13 | ⌘P reads only what typing could have changed | `command_palette` test `a_complete_answer_narrows_without_a_read_and_a_full_one_asks_again`, `gq_language` test `longer_search_text_matches_among_what_shorter_text_matched` (a final sigma refuses to narrow); offscreen snapshots through a request-counting proxy on the dev graph's three node types: "ad"; "ad", "a", " l"; "ad", "a", backspace | 3, 5 and 4 `POST /query` (before: 3, 9 and 9); "ada l" showed Ada Lovelace alone. The same run with "ad", "a", " l" had crashed before: typing again cut the node rows after the matches were ranked over them |

| 2026-10-13 | Files are replaced in one step and unreadable ones kept | `studio_settings` `files` tests: the whole file replaced with no temporary file left, owner-only and kept modes, a link written through, a failed rename keeping the old file, an unreadable file set aside | All pass |
| 2026-10-13 | A development run uses its own data directory | `studio_settings` test `a_build_directory_binary_is_a_development_run_and_a_bundle_is_not`; the snapshot tool run without `OMNIGRAPH_STUDIO_DATA` | `target/debug`, `target/release-fast`, `examples` and `deps` binaries are development runs; an `.app` (also under `target/bundle`) and installed paths are not. The snapshot printed `snapshot data: …/omnigraph-studio-snapshot-<pid>` |
| 2026-10-13 | A refused connection reads as its cause | `graph_client` test `a_transport_error_names_its_cause_and_never_the_url`; the snapshot tool against `http://127.0.0.1:9` | "Studio could not read the schema. Could not reach the server: the connection was refused"; before, the message was the client's "error sending request for url (…)" with the full URL |
| 2026-10-13 | A busy modal stays open until it agrees | `studio_workspace` GPUI test `a_busy_modal_stays_open_until_it_agrees` (a real `Workspace` in a test window, Zed's base theme) | `hide_modal` and the same command again leave it open while busy; it closes once not. Before the change, every close went straight through |
| 2026-10-14 | Review findings on the footer, settings fields and controls | `studio` `typing_not_yet_written_outlives_the_values_drawn_again` (fails on the earlier show logic); `studio_workspace` `leaving_the_window_pauses_the_cpu_reading` (fails without the mark on leaving); `studio_ui` `a_pill_button_keeps_the_focus_and_the_click` (fails without it: the click reached the surface around it) and `controls_draw_at_their_sizes` (a float bar's buttons 22 px, inside its border) | All pass. The settings test runs on `studio_settings::init_test`, settings in memory that refuse every write, so no test reaches `settings.json` |
| 2026-10-13 | A panic message loses quoted text | `studio` test `a_panic_message_loses_the_text_it_quotes` | ``'é'`` and `` `Ada Lovelace's notes` `` become `'…'` and `` `…` ``; messages over 300 characters are cut |

## Corrections

- 2026-10-16 · Switching graphs in a window kept the docks of the graph shown before: a graph's
  saved docks were read only when it opened fresh. Showing a parked graph now applies its own.

- 2026-10-16 · A hidden sidebar had no button to bring it back (only ⌘B, the View menu or the
  palette did); ⌘J and its button were labeled "Activity" but toggled the whole bottom panel,
  which opened on Nodes; and the docs drew a title bar with server, graph and branch that was
  never built. The status bar now holds every panel's button, the bottom panel is named so, and
  the frame is described as it is.

- 2026-10-14 · `settings.json` was Studio's file in all but name. Each change serialized every
  setting over it, so the first one wrote all the defaults (a later default never reached the
  person), and a change made before the two-second watch had read a hand edit wrote over that
  edit. One value of the wrong type made the whole file unreadable, and a misspelled key did
  nothing without a word. Changes now edit the text in place, each value stands alone and every
  problem is listed. There was also no way to change a shortcut: `keymap.json` now loads over
  Studio's keymap.

- 2026-10-14 · A review of the footer and control changes found: a settings text field lost
  typing when a switch was pressed before the field heard it lost focus, and a scope switch
  then wrote it to the other scope; a CPU reading could span a stretch in the background that
  fell between two ticks; the canvas's text buttons were medium buttons in a 26 px space; a
  pill button let the click reach the surface around it and took the focus from its field;
  the round trip counted time the main thread was busy; the window opened with the traffic
  lights 2 px lower than the workspace puts them; and three fields were framed by hand. Each
  is fixed as described above. A reported fifth, that a table culls its columns with the last
  frame's width, does not occur: the header tracks the same scroll handle and is laid out
  before the rows are picked; `rows_draw_only_the_columns_in_view` now widens the window and
  checks the far column in that frame.

- 2026-10-14 · `?` opened the key sheet inside every text field (the query, filters, the agent's
  composer), so it could not be typed. Its binding excluded `TextInput`, a context nothing sets;
  it excludes `Editor` now, the context of every text field, and the keymap test refuses any
  binding of a typing key that acts while an editor has focus, matched as dispatch matches
  (`depth_of`). Text that names a shortcut takes it from the keymap (`studio_ui::shortcut_text`:
  "⌘⇧A" on macOS, "Ctrl+Shift+A" elsewhere) instead of a `⌘` in a string; the Explore empty
  state also said ↵, which edits a cell now, where exploring a row is G.

- 2026-10-13 · Studio wrote its files by truncating them in place, so a crash or a full disk
  could leave an empty `session.json`, workspace file or conversation index, and the next save
  replaced it with nothing. A `settings.json` with a comment or a trailing comma loaded as
  defaults, and the next change wrote the defaults over it. The documented snapshot command
  used the person's data directory and overwrote that graph's tabs. Studio had no log, so a
  panic after a Finder launch left no trace. All four are fixed (see "Files, logs and the data
  directory").
- 2026-10-13 · A click outside a modal, or its command again, closed it without asking, even
  while its write ran: "New branch" then dropped the task that switches to the branch it
  created. Quitting and closing a window did not check for running writes, although closing a
  graph did. Modals now say whether they may close, and quitting and window closes check every
  graph of the window.

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
