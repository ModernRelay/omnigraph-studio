# 002: App shell

Status: draft · 2026-10-11. The command palette landed 2026-10-12 (`crates/command_palette`);
its scope tabs are not built yet.

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
- `studio_theme` implements `theme::ThemeSettingsProvider` from these settings: UI font,
  buffer font, sizes, density.

### Notifications

- Silent success for routine work. A toast only after a durable publish, with one follow-up
  ("Committed to feature-x · View commit"); 10 s, paused on hover.
- Errors as notifications that say what happened and what to do; they stay until dismissed.
- The activity log (bottom dock) keeps every write with its outcome.

### Visual language

Delta's window, measured from the mock on delta.dev (its CSS uses Zed's One Light tokens and
IBM Plex Sans, as Studio does), with Tower's quiet rules. Implemented as functions in
`studio_ui::chrome` and in `studio_workspace`'s render:

- **Columns, not bars.** No global title bar or status bar. Each column (sidebar, document,
  inspector) has its own 32 px header strip, and the document and sidebar their own footer
  strip, so the rules line up across the window. Strips are window drag areas; as in Zed, a
  move starts on the first mouse move after a press, so clicks on strip buttons land.
- **Sidebar column** (`panel_background`, 248 px): the header holds room for the traffic
  lights, the sidebar toggle and search (⌘K); the footer holds the graph, server and a status
  dot, and opens "Open a graph", as Delta's footer holds the account.
- **Document column** (`editor_background`): the header is the tab row (text tabs split by
  hairlines; the active tab joins the content; inactive tabs sit on `tab_bar_background` at
  35% and show their close button on hover), then the branch menu, running reads, the activity
  and inspector toggles. Each document draws its own footer strip.
- **Inspector column** (`editor_background`, 320 px): a header strip with its title.
- **Hairlines.** `border_variant` at 60%, never a solid rule.
- **Text sizes.** The window's rem size is `ui_font_size` (15 px by default), as in Zed, so
  every `ui` size scales with it. Body text is `Small`; labels, counts and metadata `XSmall`.
- **Sidebar rows.** Section labels are muted, not bold. Rows are inset `ListItem`s; the
  selected row is a grey pill. Counts are muted and right-aligned. Footnotes become tooltips.
- **Context once.** The branch appears once, in the document header's branch menu.
- **Tables.** Like Linear: a 40 px toolbar, a one-line header (name, then the type muted), 30
  px rows split by hairlines, no column rules, the cursor as a ring and the selected row as a
  pill.
- **Empty states.** One muted sentence.

### Motion

Palette, J/K, cell cursor and tab switches never animate. Dialogs and peeks enter with a
120–180 ms ease-out and leave faster. Reduced motion keeps fades only.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Fork and trim `workspace`, `picker`, `command_palette` | Depend on them | They require `project`, `client`, `db`, `language`; Delta also wrote its own panes and picker |
| JSON files for layout persistence | Zed's SQLite `db` | Small state; no migrations to carry |
| Palette lists only available actions | A global list of every command | Zed's model: a listed action always works where you are |

## Deferred

- Palette scope tabs (Types, Nodes, Queries, Branches, Commands) and the Ask hand-off.
- Multi-window session restore across launches beyond the last window.
- A URL scheme (`omnigraph-studio://`) for deep links, until the launcher (E1) needs it. Deep
  links will fill a query tab and never run it.
- Engine changes, [spec 012](../012-omnigraph-engine-changes/plan.md):
  - E9 branch details: the branch menu derives head and fork from `GET /commits?branch=` until then.
  - E15 who am I: every action stays visible; a 403 is shown as a denial and the draft stays.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Fork sources exist at the tag | `git ls-tree v1.23.2 crates/workspace/src crates/picker/src crates/command_palette/src` | Present, as listed above |
| 2026-10-11 | `ui` components needed by the frame exist | `git ls-tree -r v1.23.2 crates/ui/src/components` | Present, as listed above |
| 2026-10-12 | The palette's rows come from where the person was | `gpui/src/window.rs` `available_actions` (the focused node in the rendered frame, plus global listeners) and `dispatch_action` (captures focus when called) at `v1.23.2` | The list is built before the modal takes focus; on ↵ focus is restored, the modal dismissed, then the action dispatched |
| 2026-10-12 | Zed's global handlers appear as rows | palette snapshot | `editor::init` registers `workspace::NewFile` and `workspace::NewWindow` globally; their handlers read Zed's `AppState` with `cx.global`, which panics in Studio. The palette hides `workspace::`, `zed::` and `menu::`; ⌘N is rebound to `studio_graph::NewQuery` at the root (spec 007), so the key no longer reaches `NewFile` |
| 2026-10-12 | With nothing focused, an action reaches only global handlers | offscreen snapshots: `studio::ToggleSidebar` and `studio::OpenGraph` did nothing until the workspace had focus | A window opens with no focus, so the workspace's handlers and key bindings are unreachable until something is clicked. The snapshot example now focuses the workspace first; the app should focus it when a window opens |

## Corrections

- 2026-10-12 · The palette is not forked from Zed's `picker`, which depends on `project`,
  `workspace` and `db`. It is a small list of its own (a uniform list, Studio's matcher and
  recency file), following `command_palette.rs` at `v1.23.2` for the focus and dispatch order.
