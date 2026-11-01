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
   directory, keyed by the graph's CLI target and id. No SQLite.
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
with a toast in that window, which comes forward. A write's outcome is toasted in its window
whichever of the window's graphs is shown: a parked graph's toast starts with the graph's name
("people: Delete Person ada · committed to main") and its follow-up acts on that graph, and
its writes land in its activity log too. Other notices about a graph (its branch gone) show
only while it is shown. A parked graph's tab that asks to close (a
conversation deleted in another window) closes there, with a pane it leaves empty unless it is
the graph's only one, so the graph shows without it when shown again (since 2026-10-21; before,
the request was ignored, since only the shown graph's panes were searched). A parked graph asks after no branch head and no
branch list, and its server's health is checked only while something else shows that server
(a graph of it shown in a window, or the Open dialog) (since 2026-10-21); shown again, it asks
at once when a poll came meanwhile. A graph a start reopens behind the shown one is opened
parked and sends nothing, its first reads included (schema, branch list, counts, access, and
the stored queries once a view wants them), until first shown. A 503's recheck due while it is
parked waits for the show, and refreshes what is held rather than blanking it (since
2026-10-21). Each window's graphs (their CLI target
by name, graph, branch, and the one shown) and bounds are written to the owner-only
`session.json` as they change, in the order the windows opened; a graph's tabs and drafts are
kept by its target's name and its id too (since 2026-10-22; before, by its server's address,
which a window saved then names instead, so it is not reopened). The next start reopens every
window that has a graph on a target the CLI still lists, where it was, showing the graph it
showed; with none it opens one window asking for a graph. Closing a window forgets it, unless it was the last one open: that
one comes back at the next start, or when the Dock icon is clicked with no window open, until
another window opens. Quitting keeps every window. A new window (⌘⇧N) asks for a graph; it
opens where the last moved window was. A `session.json` from before held one window's graphs
and is read as one window.

Superseded 2026-11-01 by [spec 020](../020-ui-unification/plan.md): a top bar holds the scope and
the traffic lights, the sidebar's navigation is labeled rows, and History is a tab. What follows is
the record of the frame before it.

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
  graph switcher (the connection's dot, server / graph; its state beside it only when not
  ready, and in its tooltip with the round trip) and the branch menu,
  then the few facts the open tab adds (`Item::status`: a query's rows and time, a Blob's size;
  not what the tab shows itself, such as a table's row count or a schema's counts); at the
  right, what is running, a keyboard button whose menu lists the tab's keys (`ItemStatus::keys`,
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
- ⌘J is the bottom panel (View → Bottom Panel), which holds History and the activity
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
  zoom. A zoomed header keeps room for the traffic lights. ⇧⎋ and every zoom button take the
  keyboard from what the zoom covers: a zoomed pane's tab takes it from a dock's panel or
  another pane, a zoomed dock's panel from a tab or another dock.
- Each graph keeps its own docks: showing a graph again from the switcher brings back its docks
  as it left them (open, active panel, size), not the shown graph's; before, only a graph
  opened fresh read its saved docks.
- Tabs, after Zed's tab bar. A strip's tabs scroll when they do not fit, and the active tab is
  brought into view when it changes. The active pane's strip ends with three buttons: "+" (New
  Query, Open Query File…, Go to Node…, History), a split menu (Split Right, Left, Up,
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
- Status bar left: the docks' panel buttons, then one breadcrumb (since 2026-10-29, when a
  server chip and a graph chip side by side both switched graphs): the graph switcher, the
  connection's dot with server / graph (its state, `connecting…`, `contract mismatch`,
  `unauthorized`, `unreachable`, spelled out beside it only when not ready; the contract and
  the round trip in its tooltip; a ready server's dot beats, `ds::live_dot`: every 3 s while
  the window is in front and motion is allowed, one ring goes out from it and fades in 0.9 s,
  at most 24 redraws a second, and nothing is drawn between rings), then the branch menu, which switches branch for the window, marks the window's
  branch, and says when the list is being read or could not be (it is read again when the menu
  opens). Right: what is running ("2 queries running", "Writing (1)…"), memory and CPU, the
  other docks' buttons.
- What the credential may do (E15, below): "read only" follows the branch's name when the
  access route refuses `change` there, its tooltip naming what the credential lacks; the actor
  the credential resolves to is in the connection chip's tooltip ("… · as act-ada · Switch
  graph"), not a chip of its own, so the bar stays as it was.
- Not built: a `write · <branch>` chip whenever the active item can write.

### About

- About Omnigraph Studio, first in the app menu (`studio::About`, palette-only like Zed's),
  opens one small window, 300 × 240, that cannot be resized or minimized, and brings it
  forward when it is open. It shows, centred, the icon the Linux bundle installs (64 px), the
  name as a title, "Version <workspace version>", then "Copyright © 2026 Modern Relay" and
  "Free software under the GNU GPL, version 3 or later." in metadata type: a Mac app's About
  panel, without "All rights reserved", which the GPL contradicts.
- Escape closes it (its own keymap context, `AboutWindow`), and so do Cmd-W and Ctrl-W:
  Cmd-W is File → Close Tab's key, whose action macOS sends before the keymap sees the key,
  so the window closes on `CloseActiveItem` too.
- Its sizes are design-system tokens (`ds::size::ABOUT_WIDTH`, `ABOUT_HEIGHT`, `ABOUT_ICON`).

### Actions and keys

- Actions in `studio_actions`, one module per surface. Doc comments are the palette text.
- Default keymaps in `assets/keymaps/default-{macos,linux,windows}.json`. Contexts:
  `Workspace`, `TypeTable`, `Tree`, `Canvas`, `QueryEditor`, `Review`, `History`, `Inspector`.
  Every action without arguments has a key on every platform except, as in Zed, the rare and
  the destructive (reveal the log, the design system gallery, directional splits left and up,
  pin and keep tab, close graph, branch create, delete, merge and review, history, the agent's
  queue, restart and conversation list), which the palette and menus
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
  One that cannot be written is said in a toast with its path and why (since 2026-10-21;
  before, only the log said so). Every binding comes from these three files; no crate binds
  keys in code.

### Command palette (`command_palette`, forked from Zed's `picker` + `command_palette`)

- Lists only `window.available_actions` from the focused element, ranked by recent use
  (persisted), with the winning binding shown. Focus returns to the previous element before the
  action runs.
- Scope tabs: All ⌘1, Types ⌘2, Nodes ⌘3, Queries ⌘4, Branches ⌘5, Commands ⌘6. Tab hands the
  text to the agent ("Ask") once spec 011 lands.
  Built: text tabs under the search field (`ds::segmented`); ⌘1–⌘6 (Alt elsewhere) inside the
  palette. ⌘K opens All, ⌘P Nodes, ⌘⇧B Branches; Branches starts on the window's branch and
  every other scope on its first row, so ↵ straight after ⌘K never switches to the branch
  shown. In an open palette ⌘P and ⌘⇧B change the scope and keep the text, rather than
  closing it (since 2026-10-21); from the key sheet (`?`), which has no tabs, they open that
  picker in its place. Queries
  lists the open query tabs with their first line, then the graph's saved query files that no
  tab has open, and the stored queries the graph serves (spec 007); ↵ opens one as the Queries
  panel does (`studio_graph::OpenStoredQuery`). Tab
  (bound under `CommandPalette > Editor`, so it outranks the editor's) dismisses the palette
  and sends `studio_agent::Ask`; with no agent chosen, the agent tab says the question waits.
  Node search tries the text as typed, lowercase and capitalized, because `contains` is
  exact on 0.13 ("ada" finds "Ada Turing"). GQ has no node alternatives, so a search is one
  read per node type, five rows each. A pause reads at most eight types
  (`TYPES_PER_SEARCH`, as many as a connection has on the wire at once, so the reads go out
  together rather than 120 of them queueing behind every other view's on a graph of 120
  types): those a property names first, since a name is what a person types, then those
  found by id alone, types the counts say are empty last, each in schema order. When types
  are left, a last row "Search N more types" reads the next eight and keeps the palette open.
  Each type's rows show as it answers, below the rows already shown, which keep their place
  whichever type answers first (constitution V); in All they follow the ranked rows instead
  of being ranked among them, so a node that lands never pushes down the row under the
  cursor. New text stops the reads for the old. Typing on is mostly free: when a type's
  answer came back under five rows and the new text only narrows the old (each spelling of
  the new text starts with one of the old, `gq_language::builder::search_narrows`), its rows
  are filtered in the palette by the query's own predicate (`search_matches`); text typed
  back to after a backspace shows its answer again. Earlier node rows the new text's
  predicate refuses go at once; the others stay until their type answers. From the keystroke
  until every type read has answered, a turning circle (`ds::spinner`) sits at the end of the
  search field in All and Nodes, an empty list reads "Searching nodes…" instead of "No node
  matches", and the footer leaves out its count until something has come back.
- Opens in one frame with no animation (constitution VII).
- Landed 2026-10-12: ⌘K and ⌘⇧P (Ctrl elsewhere), bound by the default keymaps, so a person's
  keymap can unbind them. Rows are humanized action names (`studio_graph::OpenType` → "Graph:
  open type"), fuzzy matched with word-start and run bonuses, each with the binding that wins
  where the person was and the action's doc comment in the footer. Recently run rows come first
  (`command-palette.json` in Studio's data directory, read off the main thread and written on
  it, so two runs close together save in order; it keeps 50. A found node and an open tab are
  not kept: a node is listed in the order its type answered, never by recent use, and before
  2026-10-21 opening 50 of them emptied the Recent group);
  then Studio's actions, node and edge types, and the rest. A type row dispatches
  `studio_graph::OpenType` and appears only when that action is available where the person is.
  ↑/↓ move, ↵ runs after the palette closes and focus returns, Esc closes.
- Since 2026-10-18 the palette is Tower's: a 580 px glass frame (`ds::surface::floating`,
  `radius::MODAL`, 4 px inset) holding the search and scope tabs on the frame, one solid card
  of results rounded within it (`radius::PALETTE_LIST`), and the description and count under
  it. Its rows are menu rows (`ds::MenuRow`, 32 px). It sits a tenth of the window down over a
  light veil (`ModalPlacement::Palette`), not a dialog's dim.
- Since 2026-10-21 it follows the reference palette on the design system's tokens: a grey tray
  (`surface::tray`, `radius::MODAL`, 4 px inset) holding white inset panels (`ds::inset_panel`),
  the search in one and the results in others; the scope tabs between them as text tabs, the
  chosen one a raised segment. With nothing typed, the rows fall in groups as `rank` orders
  them, each its own panel under its name ("Recent", "Commands", "Types", "Queries", "Nodes",
  "Branches", "Other commands"), named only when there are two groups or more; with text typed
  they are one ranked panel under their count ("12 results", left out while nodes are still to
  come, as the footer's count was). The list is virtualized, so each row draws its slice of its
  panel and the first and last draw the panel's ends (`ds::inset_slice`, `ds::inset_label`, a
  row high). A row is led by a neutral tile (`ds::icon_tile`: a command by its area's icon, a
  branch, a query, a stored query; a node type or a node by the type's dot in the
  tile, since the row names the type) and ends in its keys as caps (`ds::KeyCaps`, one box per
  key) or what it is, muted. The footer keeps the highlighted row's description and adds the
  keys that work as raised caps: ↑ ↓ Navigate, ↵ Select, Esc Close, and Tab Ask once text is
  typed. The reference's `?` key in the search field is left out: `?` types itself there.

### Tooltips and disabled states

Every button's tooltip shows the action name and binding (Zed's `Tooltip::for_action`). A
disabled control's tooltip gives the first blocking reason ("Read-only: pinned to commit
01JA…", "Your credential may not change main"). Where a CLI command does the same, the tooltip's
second line shows it (`omnigraph branch create feature-x --from main`).

### What the credential may do (E15)

Built 2026-10-20 on `GET /graphs/{id}/access` (Omnigraph #1073, E15). The session reads it for
its branch when the graph opens and when the branch changes, and again after a 403 on the graph
or a new token, which the client counts apart; a branch's menu reads it for that branch as it
opens, and a review for its branch and target (`AccessScope::branch_into`) as it opens. Each is
read once until the credential may have changed, and views draw again when an answer lands
(`SessionEvent::AccessChanged`). A new token (typed in, or one the CLI gives on Refresh or for a
changed target, spec 003) reads it at once, with no write to bring it about, and forgets every
answer the session kept with the old token (spec 003 says which, and what is read again); a 403 on the session's
graph reads it again and keeps them. An answer that lands after either was read for the
credential before, and lands nowhere. The client counts 403s per graph, by the id in the
request's path (since 2026-10-21): one connection serves every window on an address, and a 403
on one graph says nothing of another's policy, so a session of another graph keeps its answers,
and one in flight still lands. A 403 from the server's own routes (listing its graphs, which
most tokens may not) is not counted. Studio offers every action not known to be refused
(`Allowed::offered`): before an answer lands, or when none can be read, everything is offered.
The server stays the authority: a 403 still shows as a denial, and the draft stays.

An action the answer refuses stays in view, disabled, and says what the credential lacks
("Your credential may not change fix", `GraphSession::refusal`): in its tooltip, or in its
label for a menu entry, which has no tooltip ("Delete (your credential may not delete fix)").
Its action is not registered, so the palette and the keyboard button's list leave it out
(constitution VIII); a workspace action is registered with a condition
(`Workspace::register_action_when`).

- `change` on the branch a view shows: a table's Edit (a click on a value peeks it instead),
  New row, Delete row, the bulk set, Save drafts, and a Blob cell's Choose File, Clear and
  drop, with "Read only" in its toolbar; a node's peek and page (fields do not open, an enum's
  menu and a switch show as values, a relationship's + and × and Save are disabled, Blob slots
  take no file); a text editor's Save; a Blob tab's Choose File and Clear. A cell edit still
  open when a refusal lands is kept as the cell's draft by ↵ or Tab, and nothing is sent. ↵
  stays registered in a table, since it also confirms the filter being written.
- `branch_delete`: a branch's Delete in the Branches panel, the palette's Delete branch for
  the window's branch, and a review's "Delete <branch>", which is then unchecked and the merge
  sent without it.
- `branch_merge`: a review's Merge (the button and ⌘⇧↵); its dry run is not asked for. The
  branch menu's "Merge into main…" opens the review, and is disabled only once a review has
  read the answer for that branch and target.
- Not gated: a branch's creation (New branch…, History's "New branch here…"). The answer judges
  a create only with its target, the new branch's name, typed in the dialog, so it stays
  offered and the server decides. Studio has no control that publishes or drops a commit (R2).
  A mutation run from the query editor and a review's Revert and Edit are not gated yet; the
  server refuses them, shown as a denial.

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
    change to a default reaches the person. A list is edited entry by entry (since 2026-10-21;
    before, a changed list was written whole, which dropped an element set aside and every
    comment inside it): an entry that changed is written in its place, one gone is removed, new
    ones go at its end (Zed's editor inserts nowhere else, so entries added before unchanged
    ones write those again in their places), and an element set aside stays where it is, as
    does what was set aside inside an object removed (a graph's entry whose saved views all
    go). An edit from another program that the watch has not read yet survives the change and
    applies with it.
- A settings window (⌘,) generated from the same schema: search box, left nav, rows of title,
  one-line description and control, "Reset to default" per row. Forked in spirit from Zed's
  `settings_ui`; rows are generated, not hand-written.
  - Built: `studio_settings::fields` walks the `schemars` schema of `StudioSettings`. A field's
    doc comment is its description; `title`, `range` and the extensions `x-section`,
    `x-control` (`theme`, `font`, `agent`), `x-step` and `x-hidden` give the rest. The tab
    draws a control per kind (segmented choice up to four, a menu above that, a stepper in
    range, a switch, a text field, the theme and font menus, the agent presets) and writes by
    path, so a new setting is a field and its attributes. The font menus list the system's
    fonts, read off the main thread once when the first Settings tab opens (the platform
    enumerates every face it has); until they are in, a font field shows the font chosen. A text field writes when Enter is
    pressed or it loses focus, not on every key; empty unsets a setting that may be unset.
    Typing not yet written stays when the values are drawn again (a switch pressed before the
    field hears it lost focus), and the scope switch writes it to the scope it was typed in
    before showing the other. A
    doc comment's lines read as one paragraph. Only `agent.env` (a map) and `graphs` stay in
    `settings.json` alone. The Omnigraph CLI (`omnigraph_cli`), the CLI Studio and the agent
    look for first (specs 003 and 013), is under Connection (since 2026-10-22; before,
    `agent.omnigraph_cli` under Agent, which no longer sets anything). The nav counts changed settings per section;
    search spans every section; the search box has focus when the tab opens.
  - Per-graph overrides: `graphs."<target>/<graph id>"`, the graph's one key (its CLI target's
    name and its id), which also files its saved queries (spec 007), agent conversations (spec
    011), tabs and drafts, as saved windows name the graph (since 2026-10-22; from 2026-10-20,
    `<server url>/<graph id>`, which Studio no longer reads, and no conversion moves it; before
    then, `<server name>/<graph id>`, the graph's again where that name is its target's), holds
    a graph's own values for the settings marked `x-per-graph` (table and relationship page
    sizes, canvas node limit).
    With a graph open, the tab's scope switch ("All graphs" | the graph) shows only those,
    writes the graph's own value, and Reset removes it; an own value's description names the
    shared one. Views read the values in effect through `GraphSession::options`.
  - **Node names** (since 2026-10-19). A node is named by one property of its type everywhere
    Studio shows it: a canvas chip, a tree row, the inspector's title, a table's relationship
    cell, the palette. By default that is `name`, `title`, `display_name` or `label`, else the
    key (`NodeType::default_label_property`); a graph may choose another of the type's single
    text properties, kept in `graphs."<target>/<graph>".labels` (`{"Person": "title"}`). The
    choice lives on the schema model (`NodeType::label`, applied by
    `SchemaModel::with_labels`), so every caller of `NodeType::label_property` follows it:
    the session applies the graph's choices when it takes a schema and again when the
    settings change, then announces `SchemaChanged`; a view that keeps its data across a
    schema read of the same text compares `SchemaModel::same_as` (text and names), not the
    text alone. It is chosen from a canvas chip's menu ("Show as"), a table's Views menu
    ("Name rows by") and the settings tab's "Node names" section, which the tab offers with a
    graph open: a row per type, its automatic choice described, a dropdown of the properties
    that can name it, and Reset.
- `studio_theme` implements `theme::ThemeSettingsProvider` from these settings: UI font,
  buffer font, sizes, density.
- **Gray tones** (since 2026-10-20). The default themes are Studio Light and Studio Dark
  (`assets/themes/studio/studio-theme.json`): Vitesse Refined Light and Dark with every interface
  grey replaced by interfaces.dev's scale, written by `script/studio-theme.py`, the one place the
  greys are named. Text is `--color-gray-1200` (`#202020`; dark `#eee`), muted text and icons
  `-1100` (`#646464`; `#b5b5b5`), a placeholder and a line number `-800` (`#bbb`; `#606060`),
  a border `-400` (`#e8e8e8`; `#252525`) and its quieter variant `-300`; the window's chrome
  (sidebar, tab bar, status bar) is the page (`#fcfcfc`; `#101010`), a document and an active tab
  the card (`#fff`; `#0b0b0b`), and a floating surface white (dark `#191919`). A hover, a
  selection, a guide or a scrollbar is the text grey at a low alpha, so it holds on any fill.
  Vitesse keeps its syntax, terminal and status colours. Vitesse Refined Light was the default
  from 2026-10-16 (One Light before), the dark default One Dark; Studio still bundles Vitesse's
  family unchanged from the Zed extension `vitesse-theme-refined` 0.5.0 (MIT, Colin Lienard,
  after Anthony Fu's MIT Vitesse) as `assets/themes/vitesse-refined/`, and `studio_theme::Assets`
  lists Studio's themes beside Zed's, so Zed's loader registers them before the first window and
  all of them appear in the theme menus.
- **Switches** (since 2026-10-20) are Studio's own (`ds::switch`): a pill track, ink when on and
  the divider's grey when off, and a knob in the solid surface with `shadow-custom`. Zed's
  `Switch` drew its knob in the theme's text colour, the ink of an on track, so an on switch
  showed no knob, and framed it in the theme's `border.transparent`, which Vitesse paints grey.

### Modals

A modal is a `ModalView`. As in Zed's modal layer, the workspace asks its
`on_before_dismiss` before every close: ⎋, a click outside, the same command again, and the
modal's own dismissal. A dialog with a write running (insert or delete a row or an edge,
create or delete a branch) says no, so the outcome it shows is not lost and the task that
waits for it is not dropped.

Since 2026-10-18 every dialog is one card, after Tower's dialogs (`studio_ui::modal`, through
`ds`): `ds::modal` (480 px, `size::MODAL_WIDE` 720 for Open a graph; 16 px corners, the
floating fill, the 6 % outline hairline and the sheet shadow), then `ds::ModalHeader` (the title,
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

- Every file Studio keeps (the workspace and session files, window bounds, settings, the graph
  ids remembered per CLI target, recent commands, the agent's conversation index, saved
  queries) is written through
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
  an example): that uses "Omnigraph Studio Dev" (`omnigraph-studio-dev` on Linux). The data
  directory is `%APPDATA%\Omnigraph Studio` on Windows (`Omnigraph Studio Dev` for a development
  run). A bundled
  app is the person's whichever profile built it. `OMNIGRAPH_STUDIO_DATA` overrides both; the
  snapshot tool sets a fresh temporary directory when it is unset. With restoring off, Studio
  saves no tabs or window bounds.
- The log (`logs/Studio.log` in the data directory, about 1 MB, with `Studio.old.log`) holds
  warnings, errors and panics from Studio and the Zed crates, through Zed's `zlog`; a run from a
  terminal logs to standard output, and `RUST_LOG` changes the level. A panic is logged with its
  thread and location, its message stripped of quoted text, and that line is all it prints:
  Studio's panic hook replaces Rust's, which printed the message whole to standard error (and
  a backtrace when one was asked for), and writes the line there itself when the log does not
  go to the terminal (since 2026-10-21). Help → Reveal Log File shows it, or for a run that
  logs to a terminal its folder, made first; a folder that cannot be made is said in a toast
  with its path and why (since 2026-10-21; before, nothing happened).
- A transport failure reads as its cause ("Could not reach the server: the connection was
  refused", "… the request timed out"), never with the request URL, which can hold a Blob
  row's key.

### Notifications

- Silent success for routine work. A toast only after a durable publish, with one follow-up
  ("Committed to feature-x · View commit"); 10 s, paused on hover. Four show at once. A fifth
  takes away the oldest toast with no follow-up, else the oldest with one, but never one that
  offers Undo, which only it offers, nor an error, which stays until dismissed (since
  2026-10-21; before, the oldest went whatever it was, so an agent's four commits within a
  delete's ten seconds took its Undo away). While every toast shown is one of those, errors
  fold instead: the two oldest become one toast in the older's place, "2 earlier errors" with
  Open activity, and each later fold adds the oldest error still shown to its count. Open
  activity shows the activity log, which lists each write's refusal and unknown outcome with
  its reason; another error is only counted. An Undo is never folded, so the stack passes four
  only while Undos fill it, each gone after its 10 s (since 2026-10-21; before, such a stack
  grew by every error, so an agent's dozen refused writes pushed what came after them, a
  delete's Undo among it, below the window's edge).
- A write's follow-up (View commit, Undo, Open history) names the graph written to, by its
  server's address and id as tabs identify it, and the branch, and acts there whatever the
  window shows when it is pressed: the window shows that graph on that branch first, bringing
  it back if it is parked. A graph closed since is named, and nothing runs (since 2026-10-20;
  before, it acted on the graph and branch shown, so "Open history" for an unknown outcome on
  `fix` could show another graph's or main's history).
- Every write's outcome is toasted once, by its place in the graph's outcome log, never by its
  text: two deletes described alike are two toasts, each undoing its own commit (since
  2026-10-21; before, a toast whose text one on screen already had was dropped, so the first
  toast's Undo put back the older delete). Every commit that published is toasted, whatever
  else of its graph is still out: a commit's toast used to wait until no write of its graph was
  out, the agent's and a cell's save included, so a delete's Undo, which only its toast offers,
  could be passed over. Nothing is folded: the session logs each outcome in an update of its
  own, which the window hears before the next, and an outcome names no run of writes it
  belongs to (a review's fixes go one at a time, so they were toasted each already). An
  outcome nobody knows names its branch ("… · outcome unknown on fix. Check its history before
  trying again."), whose history its Open history shows.
- Errors as notifications that say what happened and what to do; they stay until dismissed.
- The activity log (bottom dock) keeps every write with its outcome. It draws again when an
  outcome is logged, and once a minute while it lists writes, for the times they read; a read
  starting or ending, which raises the same session event, leaves it as it was (since
  2026-10-21; before, every session event drew it). Only a commit that moved its branch reads
  "committed to ⑂ main"; a write held off every branch (`OutcomeKind::Held`) names its commit
  and where it is: a proposal "was held as commit OPOSALAA to publish to ⑂ main", a snapshot
  "was saved as snapshot APSHOTAA", the full id and the `commit publish`/`commit drop` lines
  in the tooltip; neither toasts, since neither is a publish (since 2026-10-21).

### Visual language

Delta's window, measured from the mock on delta.dev (its CSS uses Zed's One Light tokens and
IBM Plex Sans, as Studio does), kept quiet: few colours, hairlines, space over lines. Tokens and components live in
`studio_ui::ds` (spacing, radii, sizes, surfaces, elevation, text roles, and the rows, cards,
strips and states built from them); the "Design system" tab (`studio::OpenDesignSystem`, in debug builds since 2026-10-31) renders
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
  window: the left dock's panel buttons, where the open tab reads and writes as one breadcrumb
  (the connection's dot and server / graph, opening the graph switcher; then / branch with the
  branch menu), then the tab's own facts; at the end what is running, the tab's controls,
  memory and CPU, and the bottom and right docks' panel buttons. A tab supplies its part through
  `Item::status` (`ItemStatus { start, end, keys }`): the table its loaded count, read commit and key
  hints; the agent its restart, agent pill and send. A pane's header holds its Back and Forward
  and its tabs.
  - The connection's state reads with the server's round trip, from the last health check
    (`Connection::latency`, no extra request): "contract 0.14 · 42 ms". It is timed on the
    background executor, so a busy main thread does not lengthen it.
  - With `studio.show_performance` on (the default from 2026-10-14 to 2026-10-31; off since), the status bar ends with
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
    `studio::ToggleFrameOverlay` (⌘⌥⇧F); other builds offer no such action (until 2026-10-26
    they listed it, and said how to get one).
- **Window styles.** `studio.window_style`: `flat` splits columns with hairlines; `glass`
  floats the three columns as sheets over a soft wash, with one gap around and between them,
  and moves the traffic lights into the sidebar sheet. They sit centered in the first header
  strip (`studio_workspace::traffic_light_position`), where a window also opens with them.
- **Hairlines.** `ds::surface::rule`, which when flat is `surface::divider`: interfaces.dev's
  `--color-gray-400`, `#e8e8e8` on a white page (9 % black), 8 % white in a dark theme; an
  outlined chip's edge is the same line (`surface::control_border`). A rule between the rows or the sections of a document
  (a table's rows, query results, settings rows, a queue, storage's sections, a feed's date
  rule) is `ds::dotted`: dotted on glass, as Tower's feeds, solid when flat. A region's edge
  (a toolbar's, a strip's, a pane's, the composer's) is solid in both styles.
- **Shadows, not borders** (since 2026-10-29, after interfaces.dev's "Use shadows instead of
  borders"). A border that only raised a surface is `shadow-custom` instead (`ds::lifted`: its
  ring and two drops over a clear border, so the layout keeps the hairline's room, a darker
  ring under the pointer, the ring alone in a dark theme): cards (a change's, a conflict's,
  an image preview's), a form's row list, an unchecked checkbox, the composer's outlined
  control and the ontology's labels, as buttons, fields, pills, chips and floating bars
  already were. A border stays where it is structure or state: a rule between rows or
  regions, an inset panel drawn row by row, a banner's severity, a focused, selected or
  invalid edge, the canvas's marquee, a sheet's rim. A Blob tab's image carries a 1 px
  outline on its edge at 10 % black, white in a dark theme (`surface::image_outline`).
- **Ink on glass** (since 2026-10-20). What glass lays over a sheet (the rule, a well, the
  selected and hovered rows, an inactive tab) is black in a light theme and white in a dark
  one, at the same strengths (`surface::ink`): a dark theme's sheet is its own dark surface
  (`surface::glass`), where black ink moved the sheet by less than a level, so selection,
  hover and rules did not show. A selected sidebar row's drops (`elevation::raised`) stay
  none in a dark theme, as interfaces.dev's dark `--shadow-custom` is its ring alone.
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
  branch's lane recede by one opacity, `ds::opacity::DIMMED` (a second, `FAR`, for what lay
  away from the canvas's focus, went on 2026-10-21: nothing used it once the canvas painted
  its own fades). Colours a view used to derive
  itself are surface roles: the canvas's here-ring and its halo (a pointed chip is lifted by
  its shadow, `elevation::Lift::Hover`, not a hairline), the dot grid, the marquee, a highlighted span of query text, rows picked beside the cursor's,
  a card's hairline (also its footer's rule).
- **Hover and selection by kind** (since 2026-10-19, when the inspector's and the query
  problems' rows hovered with the control fill, half again darker than a table's or the
  tree's). A row in a list, tree, table or the inspector hovers on `surface::hover`; a
  control, a tab, a pill and a menu's or picker's highlighted row on `surface::highlight`.
  A selected row is `surface::selected_document` in a document and `selected_chrome` in a
  sidebar. Work under way turns `ds::spinner()` (muted) wherever it shows, a running session
  or proposal included.
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
  menu row is the menu's radius less its hairline and 4 px padding, an inset panel and its
  rows likewise, a floating bar is `sm` around its controls 3 px in (`Radii::bar`). A fill
  flush with a bordered edge (a card's header, a modal's footer) rounds its own corners at the
  container's radius less the hairline (`radius::flush`): GPUI clips children to a rectangle,
  not to rounded corners. The `ds` test `nested_radii_are_concentric` holds every pair at every
  base the setting allows.
- **Round ends only for buttons with a label.** A text button, a labelled menu trigger (a
  select, the composer's agent picker), a filter chip and a property's value, a button whose
  click edits, copies or opens it (an endpoint's node), are pills (`Shape::Pill`); everything
  else takes the scale's corner: icon buttons (`Shape::Rounded`), fields, segments and tabs,
  the status bar's pickers and panel buttons, floating bars and the buttons set into them
  (`ds::bar_button`), chips, key caps and icon tiles (`xs`), inset panels
  (`Radii::inset_panel`, concentric in their tray) and their rows (`Radii::inset_row`), cards
  and grid cards (`md`), drag previews. Dots and count badges stay round.
- **Trays, inset panels, tiles and key caps** (since 2026-10-21, after the reference command
  palette). A tray (`surface::tray`: white less 4 % in a light theme, the elevated surface in a
  dark one) holds white inset panels (`ds::inset_panel`: the document's fill inside the ring's
  hairline and 4 px of padding; they sit in the tray, so the ring and no drops). A group's name
  sits above its panel, muted at the metadata size on its rows' tile edge (`ds::inset_label`). A
  virtualized list draws its panels row by row (`ds::inset_slice` with `ds::InsetEdge`): the
  first and last rows draw the panel's rounded ends just outside themselves, so every row keeps
  the list's height. A pick-list row is led by a neutral icon tile (`ds::icon_tile`, 20 px,
  `surface::tile` grey with a dark glyph, 6 px in from a 32 px row's fill on every side,
  `MenuRow::inset`); colour stays for what is data, as a type's dot in the tile
  (`ds::tile`). Keys show as caps, one box per key (`ds::KeyCaps`, `size::KEY_CAP`): the
  binding that wins in a focus's context, Zed's glyphs on macOS (⌘ ⇧ ⌥ ⌃ ↵ ⇥ ⌫, arrows; ⌦ for
  the forward delete, which Zed draws as ⌫), words elsewhere, "Esc" on both; a faint ink on a
  white panel or a document, raised (the solid fill and `shadow-custom`) on a tray. Text that
  names a shortcut (`studio_ui::shortcut_text`) reads the same table (`ds::key_names`), so it
  writes Escape as "Esc" too, where it wrote ⎋ until 2026-10-21. A key hint is caps and a word (`ds::key_hint`, `.and` for a
  second action, `.raised`), a row of them `ds::key_hints`; a full-width button with caps
  (`Button::key_caps`) reads as a launcher, its label at the start and its keys at the end.
- **Text tabs and segments.** A choice (the palette's scope tabs, a setting's options, a
  floating bar's modes) is `ds::segmented` of `ds::segment`s: no track, muted labels, the
  chosen one raised (the solid fill and `shadow-custom`), every segment keeping a clear
  hairline so choosing one moves no label.
- **Grids** (since 2026-10-21, after the reference table). A grid's header row
  (`ds::header_row`, the `ds::header_fill` faint ink, `#fafafa` on white, which lightens a dark
  document) holds header cells (`ds::header_cell`: the property kind's icon, then its name,
  muted, in ink while it orders the rows, 30 px); data cells (`ds::data_cell`) are 28 px; both
  are 12 px in from either side and ruled on their right (`ds::cell_rule`), but for the last
  column, whose edge is the grid's; rows are `ds::grid_row`s under a hairline. A cell's trailing action (`ds::cell_action`,
  a row's icon button) shows while its header or row is hovered: in a wide cell in its own
  place, over a header's end on the header's opaque fill (`ds::hover_slot`,
  `ds::header_fill_opaque`), so a narrow column's name and type keep their room. It never
  takes room on hover only: GPUI lays a frame out before it paints the hover, and an element
  shown by hover alone panics ("must call prepaint before paint"). A record card is a
  `ds::card` with a title line (`ds::card_title`: a tile, the record's name, then muted facts
  after a middle dot) over a grid. The type table and the agent's record cards share these: a
  record card in its card, the type table flush in its document (since 2026-10-21).
- **Composers and notice strips** (since 2026-10-21, after the reference agent chat). A
  composer is a card (`ds::composer_card`: the card's corner, fill and `shadow-custom`) holding
  the message, then its controls on one line at its end (`ds::composer_controls`). A notice
  strip (`ds::notice_strip`, a `ds::notice_line` each) is a tab of the card it is attached to
  (`ds::attach_notice`): the tray's grey under the card's corner and ring, its foot under the
  card, which is drawn over it.
- **Property rows** (since 2026-10-21, after the reference meeting card's body). An object's
  properties (an inspector's node) are `ds::property_row`s: the property's kind as an icon
  (the type table's header icon) and its name, muted, in a label column a row tall
  (`size::PROPERTY_ROW`, 30 px, the top of a row's 28–30), then its value in a
  `ds::property_value`, which centres the value's first line in that height, so a wrapping
  list keeps its first line level with the label. A value is a pill (`ds::value_box`,
  `ds::value_text`), a button since a click edits, copies or opens it, on an outline button's
  surface (the document's fill and `shadow-custom`), so it and an enum's `ds::value_select`
  beside it look alike, `size::VALUE` (22 px, a bar button's) tall, its text on one line and cut at the pill's end
  rather than broken mid-word. A list's items sit side by side in a `ds::value_list`; a node
  as a value (an edge's endpoint) is `ds::node_value`, its type's mark and its name. A long
  text sits under a `ds::fold` (its name, muted, and a chevron). A boolean is the
  `ds::switch`, an enum its menu's trigger, `ds::value_select` (a select's pill at a value's
  height, its chevron at its end).
- **Relationships are rows, not values.** A relationship's neighbours are nodes to go to, and
  a group may hold many, so they are listed rather than boxed: a `ds::fold_row` (the fold's
  title and chevron as a sidebar row, the rest of the row for its count and its "+") over a
  `ds::SidebarRow` per neighbour, `flush` on the panel's margin so its type mark sits on the
  property icons' edge. A neighbour's `ds::value_remove` (a quiet ×) is the row's action,
  over its end under the pointer; the group's "+" sits in its header.
- **Control chips** (since 2026-10-21, after the reference toolbar). A toolbar's controls are
  `ds::control_chip`s, the filter pill's family: one raised pill (`shadow-custom`, whose ring
  is its edge, a button's 22 px) filled by a quiet `ds::chip_button`, its label 8 px in as
  every button's (since 2026-10-29; the chip was 26 px with its button 2 px inside, its label
  10 px in, a size apart from the buttons beside it) (its icon, its label and, while set, what it is set to in the
  accent, `Button::value`: "Sort name", "Filters 2") and, while set and clearable, a hairline
  and a round clear button (`ControlChip::on_clear`). The pill is the same set or not, so
  setting a control adds to its chip rather than changing what it is. `ds::filter_pill` and
  the chip share their frame, hairline and clear end.
- **Control sizes, in code.** `size::CONTROL` (28 px: a field, a floating bar, a tile),
  `size::PILL` (`BAR_BUTTON`'s 22 px: a filter's pill, a toolbar's chips, a shared type's
  pill; the round button at a pill's end is a row button's 18 px, 2 px inside it),
  `size::BAR_BUTTON` (22 px: every button, a segment, the status bar's items), `size::TAG`
  (18 px: tags, count badges, a write target), `size::DOT` (7 px: every status dot,
  `ds::dot`, a type's and a live one; Zed's 6 px `Indicator` is not used) and
  `size::DISCLOSURE`. Every single-line field is `ds::field_frame` (the scale's `sm` corner at a control's height, a
  `shadow-custom` (below; its border clear), the error border with the drops alone when
  invalid)
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
  fill), outline (interfaces.dev's secondary button: the surface's own fill and
  `shadow-custom`, a fill a shade off it under the pointer, `#fcfcfc` on white; Cancel),
  ghost (nothing until hovered; toolbars, rows), destructive (solid red) and glass (a floating
  pill). Zed's `ButtonStyle` picks the variant, so call sites keep `.style(…)`: Filled is
  primary, Outlined outline, Subtle ghost, Tinted(Error) destructive. Sizes keep Zed's
  heights, and a view names a button by where it sits, never by its size (since 2026-10-19,
  when the same row action was an 18, 22 or 28 px square from one view to the next): a
  button, `ds::button`, `ds::bar_button` (the scale's corner, in a floating bar) and
  `ds::icon_button`, 22 px (`size::BAR_BUTTON`, 8 px in) wherever it sits, beside a field,
  among a dialog's actions, in a strip, a floating bar or the status bar, a pill and a
  rounded button alike (since 2026-10-29, when a pill was 28 px with 10 px in beside 22 px
  rounded buttons with 8 px); a row's or a card's, `ds::row_button` and `ds::row_icon_button`, 18 px
  (`size::ROW_BUTTON`, a tag's height: a tree row's toggle, an inline Fix, Show more, a
  setting's reset, a value's copy); and a tab's pin and close, `ds::inline_icon_button`, no
  square beyond its icon. An icon button's icon follows its size: 14 px at 22 px,
  12 px in a row and a tab; a sheet's close, `ds::sheet_close_button`, is the one role that
  sets its own, an outlined 22 px square with the 12 px icon. A plain `IconButton::new` is the 22 px one, as Zed's, so its
  hover fill keeps 4 px clear in the 30 px status bar and 5 px in a 32 px strip (until
  2026-10-18 every icon button defaulted to 28 px, which nearly filled the strips it sat
  in). A label is the small size, a leading icon 12 px and a trailing one (a chevron)
  12 px, both at 80 % of the label's ink (Zed's medium and large sizes, which no button takes
  since 2026-10-29, give the body size and a 16 px leading icon).
  A toggled button sits on the pressed fill; a disabled one fades to 64 %; a key hint on a
  solid button takes its ink. Primary and destructive carry interfaces.dev's primary shadow
  (`elevation::filled`: a 6 % white haze inside, a 10 % drop 1.5 px down, and a 1 px ring
  outside in their own fill); outline buttons and fields `elevation::control`, which is
  `shadow-custom`. A toolbar's
  buttons sit in `ds::toolbar_actions`, with no gap beyond their own padding; control chips,
  each a pill of its own, sit `space::SM` apart there. Zed's `ButtonLike` keeps its corners at 4 px and
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
  (`.border_l_2()`), a turning icon outside `ds::spinner`, an opacity written as a number, and Zed's `Divider`, `Banner`,
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
  editor's, a newer-data notice) holds `ds::button`s and `ds::icon_button`s;
  a button inside a dense row (an inline Fix or Run, Show more, a crumb) is a
  `ds::row_button`. Toolbar buttons are Zed's 22 px
  size, Tower's `xs`, with small type and 12 px icons, set with no gap beyond their padding
  (`ds::toolbar_actions`), so the chrome reads smaller than the document it serves. Control
  chips (a type table's Views to Row) are pills of their own and sit `space::SM` apart.
- **Shadows and borders, interfaces.dev's** (since 2026-10-20; its CSS is
  `--shadow-custom`, `-hover`, `-active` and `--shadow-border`). Every button, field, card,
  pill, floating bar and canvas chip carries `shadow-custom`: a 1 px ring (`#0000000f`) and two
  soft drops (`0 1px 2px -1px #0000000f`, `0 2px 4px #0000000a`) in a light theme, the ring
  alone (`#ffffff14`) in a dark one; hovered `#00000014`/`#00000014`/`#0000000f` (`#ffffff21`),
  pressed `#0000001f`/`#0000001a`/`#00000014` (`#fff3`). One table holds them
  (`elevation::layers`, by `elevation::Lift`). GPUI takes a shadow's blur as the Gaussian's
  sigma where CSS's blur is twice the sigma, so every layer's blur is halved on the way in
  (`elevation::Layer::shadow`); Tower's values, copied as written, had drawn twice as soft.
  The ring is drawn outside the box as a spread shadow, as CSS draws it, so the drops darken
  it where they cross it (a ring drawn as the border hid them under the fill, and an edge
  read 240 where the page reads 224–233); the element's border is clear and keeps its room
  (`ds::lifted`). GPUI keeps the element's corner for every layer where CSS grows it with the
  spread, so a ring is a little heavier at a corner; the canvas, which paints its own chips,
  gives each layer CSS's corner (`elevation::paint_layers`). `raised` is the drops alone, for
  an element that draws its own hairline (a document page, a selected sidebar row on glass).
  `surface::outline` is the ring's colour, the one hairline of a floating surface
  (`floating_border`, `card_border`). Menus, popovers, modals and glass sheets keep Tower's
  `sheet` (`floating`) and `float` shadows (an inner white rim at 70 %, 6 % dark; 4 % at
  1/2 px and 11 % at 12/28 px, spread −9; 30 % and 60 % dark), now drawn at CSS's blur.
- **Sidebar rows.** `ds::SidebarRow`: section labels muted, not bold; the selected row a grey
  pill; counts muted and right-aligned. Footnotes become tooltips. What leads a label (an icon
  through `SidebarRow::icon`, at 14 px, or a type mark) sits in one 16 px slot
  (`size::ROW_ICON`), so every list's labels start on one edge, 36 px in.
- **Context once.** The branch appears once, in the document footer's branch menu.
- **Tables.** A 48 px toolbar, then the grid flush in the document under it, with no card:
  from the document's left edge, the key column's content at the page's 16 px gutter
  (`space::EDGE`, as the toolbar's title), the other cells 12 px in (since 2026-10-21; that day
  the grid was briefly a rounded card inset 16 px either side and 12 above and below, which read
  as a second border inside the page). The grid ends at its last column: its header and rows
  are no wider than the columns, so no fill or hairline runs past it, and the last column has no
  rule on its right. A one-line header row of header cells (the kind's icon, the name, the type
  muted, 30 px), drawn by Studio rather than Zed's table, whose header padded it to 42 px; the
  key's header and the others scroll sideways with the rows. Each header ends in its column menu (⋯, the right-click menu) while hovered; the key
  cell ends in the row's Peek while the row is hovered. 28 px rows split by hairlines, every
  cell but the last column's ruled on its right, as every header is, cells 12 px in either
  side, the cursor as a ring and the selected row
  on its fill. Dense enough to scan a page at a glance; an 18 px borderless tag or chip still
  sits inside a row. Sidebar rows are 26 px, feed rows 36 px; every list takes its height from
  `ds::size`, never its own constant. Query results keep the grid header of
  `studio_ui::resizable_grid_header` for now.
- **Layout, not sentences.** Information sits in structure, as in Delta's thread and Zed's
  panels: metadata as an icon and a short value (`ds::meta`, `meta_code`: a count, a commit, a
  branch); keys as key caps (`ds::KeyCaps`, `ds::key_hint`; `KeyBinding` on buttons), never
  "↵ edit · …" strings; activity as one-line rows with a gutter icon and a verb; approvals and
  outcomes in cards with a header and a footer of icon buttons (✓ allow, ✓✓ always, ✗ reject)
  or an outcome chip; code and commands in bordered blocks; a query's rows in a record card.
  The agent's prompt is a composer card under its thread (spec 011). Explanations go in
  tooltips. Sentences appear only in errors
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
that graph open (a graph is open in one window at a time), else in the active window, else in a
new one. A graph open there on another branch moves to the branch the link names, and the window
says so in a toast, since every tab of it reads and writes that branch from then on. It never
runs a query, writes, adds a server or carries a token: the server must already be known
(compared after `graph_client::server_root`, so `127.0.0.1:8787` matches
`http://127.0.0.1:8787`), and an unknown one is refused with the reason. A link names a server by
its address alone: of two CLI targets at one address, each with its own credential, it opens on
the one a window has the graph open on, and with neither it is refused, naming both (since
2026-10-28; before, it took whichever the CLI listed first). A link longer than its longest query
text percent-encoded, about 196 KB, is refused before it is read. `studio::OpenLink { url }`
opens a link from inside Studio.

## Deferred

- `omnigraph-studio://connect?code=…`, the launcher's single-use code (engine change E1): a link
  of that kind is refused with a note until Omnigraph has `omnigraph studio`.
- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E9 branch details: the branch menu derives head and fork from `GET /commits?branch=` until then.
  - E15 what may I do (`GET /graphs/{id}/access`, Omnigraph #1073): built 2026-10-20 (Design,
    What the credential may do). Branch creation stays offered: the answer judges it only with
    the new branch's name.

## Validation log

| Date | Claim | Check | Result |
| 2026-10-29 | A border that only raises a surface is the shadow; images take the outline | `studio_ui` `controls_draw_at_their_sizes` and the workspace's tests; every all-round border in the crates' code read and sorted (structure or state kept: rules, inset panels, banners, focus, selection, errors, the marquee, sheets' rims) | Change cards, conflict cards, image preview frames, row lists, unchecked checkboxes, the composer's outlined control and the ontology's labels take `ds::lifted`; a Blob tab's image its 10 % outline (`surface::image_outline`) on its edge |
| 2026-10-29 | A pill and a rounded button are one size | `studio_ui` `controls_draw_at_their_sizes` (extended: `ds::button` and a `ds::bar_button` with the same label draw the same box, 22 px with the label 8 px in; `ds::icon_button` a 22 px square) | Passes. Before: a text pill was 28 px with its label 10 px in and the body size, beside 22 px rounded buttons with 8 px and the small size; `toolbar_button`, `toolbar_icon_button` and `bar_icon_button`, which only named the 22 px size, are gone |
| 2026-10-29 | A frame costs the whole window, so the dot draws only while its ring moves | `studio_workspace` `the_connection_dot_beats_only_while_ready_and_seen` (one beat a pulse while ready and in front; none behind another window, none once the server stops answering), `motion` `a_ring_goes_out_as_it_fades`; the workspace suite; `sample` of a release build with the ring drawn at every frame over a 126-row table; `top` of the app, the frontmost app read every 2 s, before, at every frame and with the beat | At every frame Studio took a steady 31.5 % CPU in front (about 2.6 ms a frame), against 0.5 % at rest. Of the frame, about 85 % of the samples were GPUI building the scene (`BoundsTree::insert`, 819 of about 1,100 busy samples): a cached view skips its render and layout but `Scene::replay` puts each of its quads and lines back in the draw order, so a frame costs about the window whatever changed, and a dot in a view of its own would have saved only the render. A repeating ring, even capped, asked for frames through its rest. The beat is a one-shot ring keyed by its count, counted every 3 s only while the server is ready, the window in front and motion allowed: in front 10.0 % median while in use (8.2 % before, also in use), behind other windows 0.5 % (0.5 % before). GPUI draws a view again when an entity it read while drawing notifies, so the dot follows the connection without an observer |
| 2026-10-29 | A toolbar's chips are a button's size; a ready server's dot beats; ambient motion is capped | `studio_ui` `property_and_control_parts_draw_at_their_sizes` (a chip 22 px, its button filling it from its start, its clear button 18 px and 2 px inside its end), `motion` `a_pulse_goes_out_fades_and_rests`; the workspace suite; the relaunched app's table toolbar | The chips were 26 px with their labels 10 px in, a size apart from the 22 px buttons; now Views, Columns, Sort, Group, Filters and Row read as buttons. Every animation frame renders the window's root view again (the panes are cached), and Zed's `with_rotate_animation` asks for every frame, so a spinner redrew the chrome up to 120 times a second for as long as work ran; the spinner and the dot take `motion::AMBIENT_FPS` (24) |
|---|---|---|---|
| 2026-11-01 | A top bar holds the scope; the sidebar's navigation is in words; each tab keeps its inspector | `studio_workspace` `each_tab_keeps_its_inspector`, `opening_a_view_keeps_its_source`, `a_view_opens_beside_its_source`, `a_disabled_view_does_not_open`; `studio` keymaps `every_destination_has_a_key`; offscreen snapshots | Spec 020 reverses this plan's "no title bar". Linux and Windows not checked in a real build |
| 2026-10-26 | Update actions are offered only where they apply, from any window, and a notice about no graph stays up while graphs' errors fold | `studio_actions` `an_action_is_offered_unless_it_says_when`; `studio` `update_status::tests::work_in_the_background_shows_its_progress_without_a_spinner`; `studio_workspace` `restarting_to_update_with_unsaved_work_asks_first` (the button says Restart), `errors_of_four_sources_fold_across_graphs_within_the_stack` (the notice stays); CI on PR #11 (run 40281608899), Check on macOS, Linux and Windows: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` | All pass: 1014 passed on macOS, 1009 on Linux, 979 on Windows; 0 failed |
| 2026-10-28 | A link picks no credential, is read within a bound, and a window opens with every feature | `studio` `a_link_to_two_servers_at_one_address_picks_no_credential` (two targets at `http://shared.test`: refused naming both, then opened on the one holding the graph, the other left unconnected), `deep_link` `a_link_missing_a_part_or_of_another_kind_is_refused_with_the_reason` (extended: the longest text, every byte encoded, reads; past the bound refused), `a_window_opens_with_every_feature` (every feature's init and a window; fails with the Conversations panel's read of its workspace put back), `keymaps`' `user_keymap_file` on `studio_settings::write_new` (`a_new_file_never_replaces_one`), Reveal Log File's look and folder off the main thread | All pass. Before: a link took the first target listed at its address; a long link was parsed and decoded whole before its text's bound; main at 08e4a91 aborted at launch, before a window showed (fixed in 9913d77); opening the keymap could replace a file made between its look and its write |
| 2026-10-24 | A link refused at a start leaves its window asking for a graph | `studio` `a_link_refused_at_a_start_leaves_the_open_dialog` (a link from `omnigraph studio`) and `a_link_to_an_unknown_server_at_a_start_leaves_the_open_dialog` (a window asking for a graph and the link, both before the CLI's targets are listed); `a_link_at_a_start_connects_its_server_alone` unchanged | Once listed, the window waited for the link, the link was refused with a toast, and the dialog was then shown, connecting every server as it does. A window waiting to ask is recorded with the links waiting (`LinksWaiting::asking`) and asks once the last of them is opened (`link_opened`), unless it then has a graph or a dialog. Before, the window saw a link waiting and skipped the dialog for good, so a refused link left it empty, its toast pointing at an “Open a graph” that was not there |
| 2026-10-24 | A start that reopens no window waits for a link before asking | `studio` `a_link_at_a_reopened_start_keeps_the_dialog_away` (a saved window whose target the CLI no longer lists, filled after the listing with a link still waiting), failing with the dialog shown and every server connected before the link's graph opened; `a_reopened_window_connects_its_own_servers`, its dialog now opened by `show_open_dialog` | Only the link's server was connected and no dialog shown over its graph. `ask_for_graph` checks the window and the links waiting on both paths, listed or not, so `fill_reopened`'s doc and its code agree. Before, the listed path showed the dialog at once, over a window that might have a graph, connecting every server for the link to hide it |
| 2026-10-24 | Errors from four sources fold across graphs and stay within the stack | `studio_workspace` `errors_of_four_sources_fold_across_graphs_within_the_stack` (an Undo, then refusals on three graphs and a notice about the servers, then two more refusals); the fold tests of 2026-10-22 unchanged | The fifth toast folded the two oldest errors, of two graphs, into “2 earlier errors”; each after joined it (“4 earlier errors”), the stack held at four with the Undo up, and its Open activity showed the activity log with the graph shown kept. Folding by graph still comes first (`fold_oldest_error`), and a fold across graphs (`Toast::across`) takes no by-graph join. Before, with no two errors of one graph up nothing folded, and every new error ran the stack past `TOASTS_SHOWN` |
| 2026-10-24 | Open history is offered for an unsettled outcome on a branch the session lists | `studio` `open_history_is_offered_for_an_unsettled_outcome_on_a_listed_branch` (`history_to_check` and `history_refusal` over a listed, a missing and an unread branch, and a merge settled either way), `an_unknown_outcomes_row_follows_its_branch_and_settlement` (an unknown merge into main and an unknown write to fix; fix deleted and the list read again; the merge settled) | The fix row's button was drawn disabled once fix left the list, said why in its tooltip, and did nothing when clicked, the window staying on main; the merge's row lost its button once settled. The log draws for a new branch list while a row offers Open history, and when `GraphSession::merges_settled` moves, as well as for a new outcome. Before, every unknown outcome offered Open history for its row's life and moved the window to a branch that might be gone |
| 2026-10-24 | What Studio saves of a graph is keyed by its CLI target's name and address, fixed when the graph is opened | `graph_store` `a_graphs_saved_state_is_filed_by_its_targets_name_and_address` (two servers on one target name; a graph override in the settings; a rename in place); `studio` `a_graph_two_windows_listed_reopens_in_the_first` (extended: a window saved on the same name at another address); `studio_workspace` `saved_windows_read_back_and_a_file_without_them_is_none` (extended: a graph naming its target and no address); the CLI's identities in spec 003's row of this date | The key is `<target>@<address>/<graph>` (`ServerEntry::saved_key`, kept on the session as `server_key`), so settings, saved queries' folders, conversations and tabs of a name reused for another server are another key, and a saved window reopens only on its target at its address (`SavedGraph::url`). A rename while the graph is open keeps its key and its settings; opened again under the new name it starts with nothing saved, as a target moved to another address does. What was saved under `<target>/<graph>`, and a `session.json` without addresses, stay unread (no conversion). Supersedes the 2026-10-22 key row. Before, the key was the name alone, read again at each call |
| 2026-10-22 | An unknown outcome's row in the activity log opens History on the branch it went to | `studio` `an_unknown_outcome_opens_its_branchs_history` (a workspace on a fake graph shown on main, with the activity log as its panel; a refused write and then an unknown one, both to fix; the panel shown and the unknown row's button clicked), failing with the button left off the row | The unknown outcome's row, listed first, ends with Open history after its time (a ghost `row_button` inside the 36 px feed row and its rule, `ds::ActivityRow::action`); the refusal's row has none. Clicked, the window moved to fix and `OpenHistory` ran with knowledge on fix shown. The graph is the window's, since the log lists the writes of the graph shown. Left off, there was no button to click. Before, the row only said "Check History before trying again." in its tooltip, while folded error toasts send people to the log (since 1b5a110) |
| 2026-10-22 | What Studio saves of a graph has one key, its CLI target's name and its id | `studio_settings` `a_graph_takes_its_own_limits_then_the_shared_ones`, `a_graph_scope_writes_and_clears_its_own_override`, `query_folders_are_safe_names_per_graph`; `studio_workspace` `persistence::tests::keys_are_stable_and_distinct`; `GraphSession::overrides_key` and `options`, which `type_table`, `query_editor`, `command_palette` and `agent_panel` read | Settings (`graphs."<target>/<graph>"`), saved queries' folders, agent conversations and the workspace files are under `<target>/<graph>`. Before, settings, saved queries and conversations were under `<server url>/<graph id>` and tabs and drafts under the target, so a graph had two keys. What was saved under an address stays there, unread (no conversion). In memory an open graph is still found by its server's address, as a link and the Open dialog name it (`address_of`) |
| 2026-10-22 | A `session.json` from before windows named their graphs' targets is set aside | `studio_workspace` `saved_windows_read_back_and_a_file_without_them_is_none` (a window whose graph names `server_url` and no `target`); `studio` `a_graph_two_windows_listed_reopens_in_the_first`, its `server_url` window removed | The file does not parse and is set aside; `SavedGraph.target` has no default. Supersedes the earlier 2026-10-22 row in its `server_url` window |
| 2026-10-22 | Errors fold by graph | `studio_workspace` `errors_fold_by_graph_and_open_their_graphs_activity` (two graphs' errors on a full stack, one an outcome nobody knows; then five notices about no graph), `a_dozen_errors_fold_and_leave_an_undo_within_the_stack`, `an_undo_and_an_error_stay_when_toasts_overflow` | "notes: 2 earlier errors" and "people: 3 earlier errors" apart; the notes fold's Open activity showed notes on its branch, whose log held the refusal and the outcome nobody knows it counts, the latter's row saying to check History; the notices folded into "2 earlier errors" with no button, being in no log. Before, one fold took every graph's errors and its Open activity showed the graph shown, so a folded outcome nobody knows had no way to its own graph's log |
| 2026-10-22 | A start shows its windows at once and fills them when the CLI's targets are listed | `studio` `a_link_at_a_start_connects_its_server_alone` (a window asking for a graph and a link both waiting for the listing); `studio::open_windows` and `fill_reopened` by reading (a saved window left with no graph closes, unless none has one: the first then asks) | The link's graph opened in the window once listed, and no dialog was shown over it. Before, the start waited on the main thread for the listing (spec 003) before opening any window |
| 2026-10-22 | Saved windows reopen by CLI target | `studio` `a_graph_two_windows_listed_reopens_in_the_first`, extended: a window with a graph on a target no longer listed and one saved by its server's address (`server_url`); `studio_workspace` `persistence` tests with `target` | The listed target's graphs reopen as before; the other window is not reopened. `SavedGraph` holds `target`, and the workspace files are keyed by target and graph |
| 2026-10-21 | A log folder or `keymap.json` that cannot be made is said | `studio` `a_log_folder_that_cannot_be_made_is_said` (a log file present, absent, and under a file where its folder would be); the keymap button's error path by reading `settings_view.rs` `show_keymap_file` | The blocked folder reads "The log folder could not be made at …/blocker/logs: " and the system's reason, and goes to a toast, deferred past the window's dispatch as a link's notice is; a `keymap.json` write that fails is logged and toasted with its path. Before, the first did nothing and the second only logged. The keymap toast is not driven by a test |
| 2026-10-21 | The saved recent list is read as it was written | `command_palette` `saved_keys_follow_those_run` (it replaces `saved_keys_follow_those_run_and_drop_found_nodes`: a list saved after a key was run, then 55 more keys) | The saved keys followed `type:Person` in their own order, each once, up to the 50 kept. No key is dropped on reading: Studio records no `node:` key, and the filter for the ones an earlier build saved is gone with `found::is_node_key` |
| 2026-10-21 | A full toast stack keeps an Undo and an error | `studio_workspace` `an_undo_and_an_error_stay_when_toasts_overflow` (an error, a delete with Undo, an info toast, then four commits; then two more errors and two commits); run again with the oldest toast taken away, as before | The info went first, then the oldest commits: the error, the Undo and the two newest commits showed. With four kept toasts the stack grew to five, the newest commit taking the place of the one before it. Taking the oldest away left only the four commits, the error and the Undo gone |
| 2026-10-21 | A full toast stack folds its errors, so an Undo stays within it | `studio_workspace` `a_dozen_errors_fold_and_leave_an_undo_within_the_stack` (six refused writes, a delete with Undo, six more refusals; then Open activity pressed) and `an_undo_and_an_error_stay_when_toasts_overflow`, its second half updated (an error, the Undo and two more errors up, then two commits); both run again with the fold taken out | Four toasts: "10 earlier errors" with Open activity, the Undo, and the two newest refusals; Open activity showed the bottom dock on the activity log, and the folded toast went. In the other test the two oldest errors became "2 earlier errors" and the newest commit took the place of the one before it. Without the fold the first test showed thirteen toasts, the Undo seventh, and the other five |
| 2026-10-21 | Spec text matches the code | `studio_workspace` `watch_graph` (every open graph's outcomes toasted, a parked one's named; a missing branch's notice only while shown); `type_table` `table.rs` (the header's `ruled`, `index < last`, and the data cell's `border_r_1`, `col + 1 < columns.len()`) | "Toasts follow the shown graph" and "every cell ruled on its right" were not what the code does; both read as it does now |
| 2026-10-21 | A graph reopened behind sends nothing until first shown | `graph_store` at `43ad4f3`: `a_graph_opened_behind_asks_nothing_until_first_shown`, `a_503_recheck_waits_for_the_graph_to_show_and_keeps_what_it_holds`; `studio` `lib.rs` opens a window's other graphs with `open_session_behind` before `Workspace::add_session_behind` | Written here from that commit and its tests, read against the code |
| 2026-10-21 | A commit's toast, Undo among them, is offered while other writes of its graph are out | `studio_workspace` `a_commit_is_toasted_while_another_write_of_its_graph_is_out` (a write held out with `writes_in_flight`, then a delete that can be undone and a cell's save logged); run again with `toast_outcomes`' earlier condition put back; a read of `graph_store` `GraphSession::finish_write`, the one caller of `log_outcome`, and of review's `queue_fix`/`send_next_fix` (one fix out at a time) | Both toasted, "Delete Person ada · committed to main" with Undo and the save with View commit. With the earlier condition, neither: a commit waited until no write of its graph was out. The folding never folded a review's run of fixes, which lands one fix at a time with none out, only other views' and the agent's writes |
| 2026-10-21 | Toasts are told apart by the outcome, not by their text | `studio_workspace` `outcomes_described_alike_are_toasted_each_with_its_own_follow_up` (two deletes "Delete Person ada" at c2 and c3, then two unknown outcomes of "Edit Person ada", on fix and on main); run again with the text check put back | Four toasts: each Undo bound to its own commit, each unknown one naming its branch and its Open history bound to people on that branch. With the text check, the second delete was dropped (3 toasts), so the first toast's Undo would have put back c2 for both |
| 2026-10-21 | A parked graph's tab closes when it asks | `studio_workspace` `a_parked_graphs_tab_closes_when_it_asks` (notes with a tab in each of two panes, parked behind people; the second tab emits `ItemEvent::Close`); run again with the Close arm ignoring tabs it does not find among the shown panes | Shown again, notes had its first tab only, in one pane. Before, both tabs and both panes came back |
| 2026-10-21 | A settings list keeps its comments and the entries set aside | `studio_settings` `a_list_keeps_its_comments_and_the_entries_set_aside` (a graph's views: one set aside, one kept, a comment above each; a view saved, one changed, then both deleted), `a_list_edit_keeps_the_comments_between_its_entries` (the agent's command with a comment after its first entry: an entry added, changed, removed), `entries_set_aside_are_named_by_their_place_in_the_text` (two views set aside before one that fits) | The view set aside and both comments stayed through the save and the change, new entries written at the list's own indent; after the deletes the view set aside and its comment stayed under the graph's key. The command's comment stayed through all three. The second view set aside was named `views[1]`, where before it read `views[0]`, its place in the document once the first was taken out. Before, `diff` wrote a changed list whole, dropping the view set aside and every comment inside the list |
| 2026-10-21 | Dead code removed | `ItemHandle::boxed_clone` (no caller: the only `boxed_clone` calls left are `Action`'s); `StudioSettings::parse`, a second way to read the file, deleted in `749286f` once its last caller, `studio` `settings_apply`, read through `StudioSettings::read` and asserted no problem, as `studio_settings`' own tests do (`read_clean`) | Builds and tests as before; no source names `StudioSettings::parse` |
| 2026-10-21 | The activity log says where a held write is | `graph_store` `a_held_write_is_logged_held_and_moves_no_head` (a mutation `as commit`, one `as unattached branch` and a `branch merge … as commit` statement, answered with `unpublished` and `unattached` commits), `a_write_without_a_commit_names_none` (`OutcomeKind::written` by `CommitKind`); `studio` `a_held_write_says_it_is_on_no_branch`; at `7fd9cc9` the server answers such writes with the commit's `kind` (`CommitKindOutput`) | Before, `send_write` passed only a `published` commit, so all three logged `Committed { commit: None }` and read "… committed to ⑂ main" with no commit; the merge's on release. Now each is `Held` with its commit, on the branch it was sent to, no head moves and no `Committed` event; the row reads "was held as commit OPOSALAA to publish to ⑂ main" or "was saved as snapshot APSHOTAA". A commit of a kind this build does not know is an unknown outcome. Not yet seen in the running app |
| 2026-10-21 | A 403 on one graph leaves another graph's access as read | `graph_client` `a_refusal_on_a_graph_is_counted_for_that_graph_alone` (`GraphClient::refusals(graph)`, by the id in `graphs/{id}/…`, escaped or not: `a_path_segment_reads_its_escapes`); `graph_store` `access_is_read_per_branch_and_again_after_a_refusal`, extended with a `notes` session on the same connection | Before, the client counted 403s for the whole server, so a refusal on `knowledge` made the `notes` session forget its answer and read `/access` again, and an answer in flight landed nowhere. Now `knowledge` reads its access again after its 403, and `notes`, asked again, asks nothing and still allows reading |
| 2026-10-21 | Parking a graph lets go of its server's watch | `graph_store` `the_head_watch_sees_other_writers_commits_while_a_window_is_active`, extended (`Connection::watched`) | Watched while shown; parked, no watch; shown again, watched again |
| 2026-10-21 | What builds before 2026-10-20 filed under a server's name is not read (decided: no conversion code) | `graph_store::named_keys` deleted with its callers (`GraphSession::new`'s move, `agent_panel` `AgentSessions::global`'s `move_named_graphs`) and `SessionEvent::SavedQueriesMoved` with the Queries panel's arm for it; their tests `a_name_resolves_only_to_a_known_server`, `settings_kept_under_a_servers_name_move_to_its_address_once`, `saved_queries_move_to_the_graphs_key_without_writing_over_one`, `a_graph_opening_takes_its_saved_queries_along` and `conversations_filed_by_server_name_move_to_the_graphs_key` deleted; no source names `named_keys`, `move_named` or `SavedQueriesMoved` | Supersedes the 2026-10-20 row on one key per graph: settings under `graphs."<server name>/<graph id>"`, query folders `<server>~<graph>` and conversations filed by a server's name stay where they are, unread; Studio writes only under `<server url>/<graph id>` |
| 2026-10-21 | A panic prints only its redacted line | `studio` `a_panic_reports_one_line_without_the_text_it_quotes` (a panic quoting 'Ada Lovelace' and `secret notes` on a named thread, through `panic_line`, which the hook prints); `logging::init` no longer chains to `std::panic::take_hook()` | "panic on thread quoting-panic at crates/studio/src/logging.rs:…: no row for '…' in `…`", nothing else. Before, the line was logged and then Rust's own hook printed the panic whole to standard error, with a backtrace under `RUST_BACKTRACE`. Not yet seen in the running app |
| 2026-10-21 | The activity log draws for outcomes, not for reads | `studio` `the_log_draws_for_outcomes_not_for_reads` (renders counted: `ActivityChanged` and `CountsChanged` with no outcome logged, then an external write refused), failing with the subscription that drew on every event | No draw for the read-like events; one for the outcome. Its times are drawn again once a minute (`TIMES_EVERY`) while it lists writes |
| 2026-10-21 | A link checks no more and no less than "Open a graph" before it opens a session | A read of `studio::open_link`, `open_graph::OpenGraphModal::open` and `show_graph_in` (both open through `show_graph_in`, on any known connection, whatever its state) and of `graph_client`'s contract check on every response | Not changed: "Open a graph" opens a remembered or typed id on a server in any state, as a link does, so a link skips no check it applies. A server speaking another contract still serves nothing: every response without the contract header is refused (constitution I, X), and a refused token answers 401 to every read |
| 2026-10-21 | What the credential may do follows a new token at once | `graph_store` `a_new_token_reads_access_and_every_answer_again` (spec 003's log has the detail) | A read-only token replaced by a write-capable one allowed changing main at once, without a write; the old token's late answer was dropped |
| 2026-10-21 | A parked graph polls nothing | `studio_workspace` `a_graph_reopened_behind_opens_its_tabs_when_first_shown` (shown state of a graph opened behind, then shown, and of the one it replaced); `graph_store` `the_head_watch_sees_other_writers_commits_while_a_window_is_active` | Behind or switched from: not shown, no head probe, branch list or health check of its own; shown again: asks at once |
| 2026-10-21 | ⌘K opens on its first row, found nodes stay out of Recent, and ⌘P and ⌘⇧B keep the palette's text | `command_palette` `only_the_branch_picker_starts_on_the_current_branch` (branches feature and main, on main), `a_found_node_opened_is_not_remembered`, `saved_keys_follow_those_run_and_drop_found_nodes`, `go_to_and_switch_branch_in_the_picker_keep_the_text` (an app-level listener counting what passes the palette) | All on `type:Person`, Branches on `branch:main`, All again on its first row; opening Ada left the recent list empty and opening Person kept `type:Person`; a saved list lost its `node:` keys and kept the order of what was run first; GoTo then SwitchBranch with "mai" typed showed Nodes then Branches (`branch:main` alone) with the text kept, and nothing passed the palette. Run with each fix taken out, the three GPUI tests failed (All on `branch:main` at 2; still All after GoTo; the node recorded): every scope started on the current branch, a found node was recorded among the 50 but never ranked, and both actions reached the workspace, whose toggle closed the palette |
| 2026-10-21 | Text and key caps name keys from one table | `studio_ui` `shortcuts_are_written_as_the_platform_writes_them`, `ds` `key_caps_are_one_box_per_key` | `keystroke_text` joins `ds::key_names`: Escape is "Esc" in text on macOS too (it was ⎋ there, "Esc" on caps), the forward delete ⌦ on a cap (it was backspace's ⌫), Backspace and Delete words on Linux and Windows, "Win" for Windows' platform key |
| 2026-10-21 | Design-system parts no surface uses are gone; the rest is named here | Workspace `grep` for each | No caller: `surface::outline_hover` and `outline_pressed`, `opacity::FAR`, `Button::label_size`, `IconButton::shape` and `selected_icon_color`, and `ds::pill`, which only the gallery's composer drew (the agent panel's picker is a ghost button, as the gallery's now is). `Variant::Secondary` and `Variant::Glass` stay: "Buttons" names them among the six variants |
| 2026-10-21 | The palette's descriptions say what each action does now | Each action's handler read against its doc comment | 30 rewritten, among them GoTo (it finds a node; types are a scope away), SwitchBranch (the window's graph, not a tab), Refresh (the view in focus too), OpenCanvas and OpenOntology (no "graph explorer", no single map), OpenInspector (storage, not the dock), OpenAgent (a tab), NewQuery (a new tab), DeleteRow (marked rows, after asking), the splits (the active pane) and the review's steps (by card) |
| 2026-10-20 | A write's toast acts on the graph and branch written to | `studio_workspace` `a_toasts_follow_up_acts_on_the_graph_and_branch_written_to` (two graphs on fakes answering 404, each with a tab that holds its focus); run once with the window's focus left on the shown graph's undrawn tab | Open history for an unknown outcome on notes/fix, pressed with people shown, showed notes on fix and ran there, notes' tab with the keyboard; pressed after notes moved to main, back on fix; Undo for a delete in parked people ran in people on main, people's tab with the keyboard; with people closed, nothing ran and the toast read "Nothing was done: people is no longer open in this window…". With the focus left on the undrawn tab the action reached no handler (`left: None`), which the fix dispatches from the window to avoid |
| 2026-10-20 | The zoom buttons hand the keyboard over as ⇧⎋ does | `studio_workspace` `a_hidden_or_covered_dock_gives_the_keyboard_back_from_any_field_of_its_panel`, extended with clicks (`simulate_click`) on `zoom-pane-0` and `zoom-dock-Bottom` | The pane's button zoomed out, then zoomed in with the keyboard in the activity panel's field and gave it to the tab; the bottom dock's button took it from the tab to the dock's panel. Before, both buttons set the zoom alone |
| 2026-10-20 | Glass states show in a dark theme | `studio_ui` `glass_takes_the_dark_themes_surface_and_its_states_show_on_it` (each state composited over the light style's `#fcfbfc` and Studio Dark's `#101010`); an offscreen 2x snapshot in Studio Dark with `window_style: glass` on a scratch `script/dev-server` (Person's table open, the pointer on Knows), sampled by pixel | Every state moves its sheet by 3 or more levels in both themes (a well, the faintest: 3.8 light, 3.6 dark); black at the selected row's 5 % moved the dark sheet 0.8 of a level, which is why selection, hover, rules and wells did not show. In the snapshot the sidebar sheet reads 14, the selected Person row 26 and the hovered Knows row 20; the table's dotted rule reads 45 over a cell at 11 |
| 2026-10-20 | One key per graph: settings, saved queries and conversations under `<server url>/<graph id>` | `graph_store` `named_keys` tests (`a_name_resolves_only_to_a_known_server`, `settings_kept_under_a_servers_name_move_to_its_address_once`, `saved_queries_move_to_the_graphs_key_without_writing_over_one`, `a_graph_opening_takes_its_saved_queries_along`); `studio_settings` `a_graph_takes_its_own_limits_then_the_shared_ones`, `a_graph_scope_writes_and_clears_its_own_override`; `agent_panel` `conversations_filed_by_server_name_move_to_the_graphs_key`; `GraphSession::settings_key` and `studio_settings::graph_key` removed, every caller on `overrides_key` | `dev/knowledge` moved to `http://127.0.0.1:8787/knowledge` with its limit and node name, once, and never over an entry the address had; a trailing slash names the same server. The session-level settings move writes through `studio_settings::update`, which tests refuse, so only its pure part is tested. Aliases stay keyed by the CLI target's name. Not yet seen in the running app on a renamed target |
| 2026-10-20 | Dead tokens and stale docs | `grep` over `crates/`: `surface::raised_border` and `surface::pointed_border` had no caller (the canvas lifts a pointed chip with `elevation::Lift::Hover`); `studio_theme` exports only `type_color` | Both tokens removed; the `studio_theme` crate doc and `docs/architecture.md` name the one graph token, not edge and diff colours |
| 2026-10-20 | ⌘P reads a bounded number of node types and shows each as it answers, in place | `command_palette` tests `node_types_show_as_they_answer_and_keep_their_place` (three types, the first sent answering last; red when the rows wait for every type, as they did), `a_search_reads_a_bounded_number_of_types_and_offers_the_rest` (ten named types and two found by key), `new_text_stops_the_reads_for_the_old`, `named_types_are_searched_first_and_empty_ones_last`, `the_palette_shows_it_is_searching_nodes`, all on fakes sending `graph_client::CONTRACT`; the earlier search, which awaited each type's read in schema order before showing any | Rows came in as n2, then n2 n1, then n2 n1 n0, none moving; a pause sent 8 reads and "Search 4 more types" held Named8, Named9, Code0 and Code1, which ↵ read with the palette open; new text dropped the 3 reads under way (the session's count back to 0) and only its own answers showed. Not yet seen against a large graph in the running app |
| 2026-10-20 | The settings tab lists fonts off the main thread | `studio` test `fonts_are_listed_off_the_main_thread`; Zed `v1.23.2` `crates/theme/src/font_family_cache.rs` (`prefetch` lists `all_font_names` on the background executor) and `crates/gpui_macos/src/text_system.rs` (`all_font_names` walks every font descriptor through Core Text) | Before, the first frame of Appearance listed and sorted the fonts on the main thread. Now the list is asked for when a tab opens, once for all tabs, is not there until the background task lands, and a font field offers Default and the font chosen meanwhile. The listing's time on this machine was not measured |
| 2026-10-20 | Stored queries and their aliases open from the palette | `command_palette` test `stored_queries_are_listed_in_the_queries_scope` (a catalog of `people_titled` with a description and `set_title` as a mutation) | Queries listed both; the first row's action is `OpenStoredQuery { name: "people_titled", alias: None }` and the footer shows its description. Aliases come from the registry's CLI config, which no test has: they are read with `aliases_for`, as the panel reads them |
| 2026-10-20 | An action the credential may not take is disabled, says why and is not offered | Fakes answering `/graphs/{id}/access` with `{"graph_id": …, "actor": …, "graph_wide": ["read"], "actions": ["read"]}` and the contract header: `type_table` `a_branch_the_credential_may_not_change_offers_no_writes`, `a_page_on_a_branch_the_credential_may_not_change_sends_nothing`, `a_tab_at_a_commit_or_on_a_refused_branch_writes_nothing`, `a_text_on_a_refused_branch_is_not_saved`, `a_blob_is_written_only_where_it_can_be_and_says_why_not`; `review` `a_merge_the_credential_may_not_make_is_not_offered`; `studio_workspace` `a_branch_the_credential_may_not_change_reads_read_only`, `a_conditional_action_is_registered_only_while_it_holds`; `graph_store` `a_refusal_names_the_action_and_its_branches`; clippy on the seven crates; not yet seen in the running app against a server with a read-only policy | Refused on main: Edit, New row, Delete row, Save drafts, Choose File and Clear not available where the keyboard is and gone from the keys (Peek, Open page, Explore, Copy, Filter by this value, Refresh stay); a click on a value peeks; no write sent, the file refused with "Your credential may not change main", the draft kept. Unknown (the route unreadable): all six available. A review of feature → main read the route once, Merge not available, no dry run or merge sent, the branch not deleted with it. The bar showed no "read only" before an answer or on fix, and showed it on main; the chip's tooltip named act-ada |
| 2026-10-20 | Studio's greys are interfaces.dev's | Its `--color-gray-*`, `--color-gray-background` and `--color-preview-*`, light and dark (`_next/static/immutable/chunks/26aw_19s171nu.css`); `script/studio-theme.py`; `studio_theme` test `studio_themes_are_listed_and_read_as_families`; `studio` test `settings_apply`; offscreen snapshots of Settings, the table, History and the ontology in Studio Light and Studio Dark | Text `#202020`, muted `#646464`, placeholders `#bbb`, borders `#e8e8e8`, chrome `#fcfcfc` and documents white; dark `#101010`, `#0b0b0b`, `#191919`, text `#eee`, rings 8 % white. Vitesse's syntax and status colours kept |
| 2026-10-20 | An on switch shows its knob | Offscreen snapshots of Settings' "Show memory and CPU", light and dark; Zed `v1.23.2` `crates/ui/src/components/toggle.rs` (`thumb_color = base_color`, the text colour) | Before, a dark pill in a grey frame with no knob; now an ink track with a white knob (dark: a light track with a dark knob) |
| 2026-10-20 | Shadows and borders are interfaces.dev's | Its stylesheets (`_next/static/immutable/chunks/26aw_19s171nu.css`: `--shadow-custom`, `-hover`, `-active`, `--shadow-border`, the `.dark` values); a reference page with those values at whole pixels, captured at 2x in the app's browser; offscreen 2x snapshots of the design gallery | Out from a card's bottom edge, the CSS reads 226, 232, 245, 252, 249, 250 and Studio 226, 230, 247, 249, 251, 251; from its left edge 232, 239, 249, 254 and 235, 236, 252, 253. With the ring drawn as the element's border the bottom edge had read 240, 240, 247: the fill hid the drops. Workspace Clippy (`-D warnings`), 609 tests, and the design-token and layer guards pass |
| 2026-10-20 | GPUI's shadow blur is a sigma | Zed `v1.23.2` `crates/gpui_apple/src/shaders.metal` (`gaussian(y, shadow.blur_radius)`, `erf(… (sqrt(0.5) / sigma))`, the `blur_radius == 0` branch) and `crates/gpui/src/window.rs` `paint_drop_shadows` | `blur_radius` is the Gaussian's sigma, half of CSS's blur; a zero blur draws a hard edge with 1 px antialiasing; every shadow keeps the element's corner radii |
| 2026-10-19 | A node has one name everywhere, chosen per graph and type | See spec 006's row of this date (the same checks) | The choice lives on the schema model, so the canvas, tree, inspector, tables and palette take it from one place; the table keeps its columns, filters, order and grouping and reads its rows again, the explorer reads its names again at the same commit with every path as it was, and the inspector renames its title from the values held and reads the neighbours it shows again |
| 2026-10-19 | One hover per kind, one spinner, one way to say a thing | `script/check-design-tokens.py` (new rule: a turning icon outside `ds::spinner`); workspace clippy `-D warnings` and the workspace suite (43 targets); offscreen glass snapshots on a scratch dev server, sampled by pixel: an inspector property row and a sidebar row hovered | Before: an inspector row's hover darkened it by 8–9 levels where a sidebar or table row's darkened by 6–7 (the control fill); the query problems list the same. The agent panel drew work under way four times by hand, a running session as an accent `LoadCircle` and a running proposal as a still icon. "Loading…" in two places where Studio says "Reading…", a "Save View…" menu entry, an "e.g." placeholder, and History's toolbar fact a meta row repeating the tab's branch. After: both rows darken by 6; every running state is `ds::spinner()`; the copy reads one way; History's toolbar reads "History  2 commits" as the table's "Person  200 rows" |
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
| 2026-10-31 | The Nodes panel is gone | `cargo clippy --workspace --all-targets -- -D warnings`; `type_table` and `graph_canvas` tests; a saved bottom panel of `nodes` falls back as any unknown panel id does, from reading the restore code, not run | Pass. A type's table has one home, its tab: Ontology's Browse rows was the only way in, and Open table stays beside it. The panel's empty state and comment still said a click on a type browsed it there, which no click did |
| 2026-10-31 | The design gallery is in debug builds only | `cargo clippy -p studio --all-targets` as it is and with `profile.dev.package.studio.debug-assertions=false`, which builds Studio as a release does; `studio` `a_window_opens_with_every_feature` and `keymaps` | Both clean and passing. A release build's palette offers no such action, since it lists only available ones, and a saved tab of the kind is skipped on restore. `cfg(debug_assertions)`, not a feature, because CI passes no features and would stop compiling it |
| 2026-10-31 | Memory and CPU are off by default | `studio_settings` tests (`default.json` against the struct), `studio_workspace` `leaving_the_window_pauses_the_cpu_reading` with the setting on | Pass. The status bar no longer redraws every two seconds for a reading most people never asked for; the setting stays under Appearance |
| 2026-10-31 | A setting Studio no longer has says nothing, and its text stays | `studio_settings` `document` `a_retired_setting_is_not_a_problem_and_its_text_stays`, `a_retired_setting_is_matched_whole_from_the_root` | Keys under `models` and `panels` are not reported and stay in the file through an edit; `agent.gone` beside the retired `agent` settings is still reported. Before, each removed setting would have raised an error toast at every start |
||||||| parent of 959a047 (refactor(table): remove the Nodes panel)
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
| 2026-10-14 | Every setting with a control is on the page | `studio_settings` field tests (row list, kinds, one-paragraph descriptions); offscreen snapshots in a scratch data folder: a search for "agent", a path typed into Omnigraph CLI then ↵, a second run with Reset, the Ask before writes switch | `agent.ask_before_writes` and `agent.omnigraph_cli`, added for spec 013, had been hidden; text settings had no control at all (the page showed the value read-only), and no switch had been drawn. Now sixteen rows: ↵ wrote `"omnigraph_cli": "/opt/omnigraph/bin/omnigraph"` (mode 0600) and showed Reset, Reset wrote `null` and emptied the field, the switch wrote `true`. Descriptions had kept their doc comments' line breaks, and GPUI breaks lines at `/`, so paths left the descriptions |
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
| 2026-10-21 | The type table is flush in its document and ends at its last column; an agent's write shows its query once | Offscreen snapshots (1440×900 at 2×) against a scratch dev server and the test agent, light and dark, before and after: Person, Project (narrower than the page), Company, Person at 2400×700, and "list people, then make a change"; `agent_panel` `a_write_shows_its_query_once_on_its_card` | Before, the table was a card inset in the page, a second border; its "empty strip" after `tags` was the first 12 px of the next column (`contributes`), the cell's own inset, cut at the card's edge. After, the grid runs from the document's edge under the toolbar, the key's text on the toolbar title's 16 px; a grid narrower than the page ends at its last column with no rule after it (Project ends 26 px short of the edge, the page plain there), and a wider one scrolls with its last column unruled. The chat showed the retitle query twice, highlighted under "Mutate retitle" with its `--branch` line and again on the Mutation card; the call's line is now "Mutate retitle" alone over the card |
| 2026-10-21 | A value's pill looks as the enum's select beside it, in a 30 px row | `studio_ui` `property_and_control_parts_draw_at_their_sizes` (a property row at `size::PROPERTY_ROW`, now 30, its value 22 and 4 px in); offscreen snapshots of the Design system tab and person-000001's peek | A regression review found the select raised on `shadow-custom` beside flat values in a hairline, rows of 32 px above the 28–30 px rows, and this spec saying toolbar buttons have no gap where the chips sit 6 px apart; values now take the outline surface, rows are 30, and the chips' gap is written down |
| 2026-10-21 | A property's value is a pill and a relationship's neighbours are rows | `studio_ui` `property_and_control_parts_draw_at_their_sizes` (a fold row and a flush row 26 px, on one edge, one under the other); offscreen snapshots of the Design system tab (1440 × 2400 at 2×) and of person-000001's peek, at rest and with the pointer on a neighbour and on a group's header | Values in pills beside muted labels; "knows → Person ⌄ … 4" over four rows of a mark and a name; the × over a row's end and the "+" before the count only under the pointer |
| 2026-10-21 | Property rows and control chips are design-system parts | `studio_ui` `property_and_control_parts_draw_at_their_sizes` (a property row 32 px, its boxed value 22, a switch and a wrapping list's first line level with the label, a chip 26 with its button centred, the "+" a value's height); clippy `-D warnings`; design tokens; offscreen snapshots of the Design system tab (1440 px wide at 2×, light and dark) | The gallery's inspector card shows boxed values, a long email cut at its box's end, a switch, a list of tags wrapping under its first line with its "+", a relationship's neighbours with their type marks, and a fold; its chips row reads Views, "Columns 3 hidden ×", "Sort name ×", "Filters 2 ×" and Row beside a filter pill, the value in the accent in both themes. The switch first sat 7 px high, outside a property value |
| 2026-10-21 | The palette, the agent's composer and the type table follow the three reference frames on the design system | Offscreen snapshots (1440×900 at 2×) against a scratch dev server, light, dark and glass, before and after, cropped at 2× beside the references; `studio_ui` `key_caps_are_one_box_per_key`, `a_row_knows_its_place_in_its_panel`, `controls_draw_at_their_sizes` (a tile, a segment and the grid's cells at their tokens), `values_pick_their_kind_icon`; `command_palette` `rows_are_grouped_by_kind_until_text_is_typed`; `agent_panel` `a_querys_rows_are_read_from_what_its_call_returned`, `a_tool_call_keeps_the_result_its_agent_reported`, `a_turn_ends_at_its_last_reply` | The palette is a tray of inset panels, its groups named while nothing is typed and counted once text is, rows led by tiles with their keys as caps; the inset panels' corners stayed square until each end was drawn as tall as its corner (GPUI clamps a corner to half its box). The type table's header was 42 px under Zed's padding; Studio's own header row is 30. In a dark theme a darker header fill did not show, so the header takes the well's ink |
| 2026-10-22 | About shows Studio's icon, name, version and licence, once, and closes as a Mac panel does | `studio` `about::tests` (the version line, one window however often asked, the four parts drawn, `menu::Cancel` and `CloseActiveItem` closing it) and the keymap test; the debug build opened from its app menu with System Events, captured with `screencapture -l`, then Escape, Cmd-W and File → Close Tab sent, and its windows listed through Accessibility | 5 passed. The window showed the icon, "Omnigraph Studio", "Version 0.2.1" and the two notice lines, minimize and zoom greyed; each of Escape, Cmd-W and Close Tab left only the main window. A first check counted the closed window still open: `CGWindowListCopyWindowInfo` with `optionAll` lists windows off screen too |

## Corrections

- 2026-10-22 · The settings section still gave a graph's key as `<server url>/<graph id>` for its
  overrides and node names, and said what builds before 2026-10-20 filed under a server's name is
  not read; the key is `<target>/<graph id>`, which such a name, when it is the target's, matches
  again; persistence was said to be keyed by server. The status bar's example said "contract
  0.13", and a new token was said to come from `omnigraph login` changing the CLI's files, which
  is managed sign-in only. The Omnigraph CLI setting moved from `agent.omnigraph_cli` (Agent) to
  `omnigraph_cli` (Connection), which the settings section now says. A parked graph's first
  reads were said to include the stored queries, which are read once a view wants them.

- 2026-10-22 · The palette's Queries scope no longer lists the CLI's aliases under the stored
  queries they run. Studio no longer reads the CLI's config file, where they were declared, and
  the CLI has no command that lists them, so Studio knows none and `OpenStoredQuery` names only
  a query.

- 2026-10-22 · Saved windows and each graph's saved tabs were keyed by the server's address,
  from Studio's own server list. Studio keeps no server list now (spec 003): a graph is kept by
  its CLI target's name and its id, and a window that names a target the CLI no longer lists,
  or an address (saved before), is not reopened. Nothing converts the old files.

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
