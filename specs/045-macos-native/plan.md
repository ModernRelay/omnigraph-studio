# 045: Native macOS features

Status: draft · 2026-11-19 · step 1 (notifications for agent conversations) prototyped on macOS

## Problem

Studio looks like a Mac app but stops at its window's edge. Work that takes minutes happens
while the person is in another app: an agent proposes changes on its branch, a merge runs, a
large read or a Blob download finishes. Today the only signs are inside Studio: the
conversation's tab, an unread dot in the Conversations panel, a toast. Someone in their editor
or browser misses them until they look. The Mac has a place for each of these: Notification
Center for "it's done", the Dock icon for "this many wait for you", a menu bar extra for
"what is running", Quick Look for "show me this file", Spotlight for "find that thing". This
plan says which of them Studio uses, for what, in what order, and what each costs.

## Principles

- I (the engine is the authority): nothing here reads graph data a new way. Spotlight's index
  holds names Studio already read (spec 031), and a notification names a graph and a branch. No
  new request goes to a server, except step 3's background branch-list read, which is the head
  watch's own read at a slower pace.
- V (nothing moves under you): a notification, a badge and a menu bar extra are outside the
  window. None of them changes the layout of a view.
- VI (every control explains itself): every new setting says what it does when on; a menu bar
  item or a Dock menu item is an action with its binding shown where it has one.
- VII (loudness matches cost): routine success inside Studio stays silent. A notification is
  posted only while no Studio window is in front, and only for work that took long enough for
  the person to leave (an agent's turn, a merge, a read past a threshold).
- VIII (keyboard first): every new command is an action in `studio_actions`, with a binding in
  `assets/keymaps/default-macos.json` (and the Linux and Windows files where it applies) and an
  entry in the palette.
- IX (work is bounded): Spotlight indexes at most a fixed number of names per graph; Quick
  Look previews at most `VIEW_MAX` bytes (64 MiB); notifications are one per conversation.
- The constitution forbids logging tokens, query text and row data. Notifications and Spotlight
  are not logs, but they put text where others can see it (a locked screen, Notification
  Center, Spotlight's on-disk index), so the same rule applies to them: names and types only.

## What exists today (checked)

### Apple's Human Interface Guidelines

Read 2026-11-19 from Apple's documentation JSON for each page
(`developer.apple.com/tutorials/data/design/human-interface-guidelines/<page>.json`; the HTML
pages render in JavaScript). The rules this plan follows:

- **Designing for macOS:** people keep several apps open and switch between them often,
  expecting an app to move smoothly between active and inactive; support full screen; put
  every command in the menu bar; handle keyboard shortcuts.
- **Notifications:** be concise; don't send several for one thing; don't tell people to do a
  task in the app; use an alert, not a notification, for an error; when the app is in front,
  show the news inside it instead (Mail adds the message to the list); never put sensitive or
  personal data in one. Titles use title-style capitalization and no end punctuation; the body
  is complete sentences in sentence case; leave out the app's name and icon. No action that
  only opens the app: a click on the body already does. A badge shows only the number of
  unread notifications, is never the only sign of something, and is cleared as they are read.
- **The menu bar, menu bar extras:** a menu bar extra is a template symbol (24 pt bar), opens
  a menu rather than a popover, is added by the person in Settings rather than by the app, may
  be hidden by the system at any time, and its commands must exist elsewhere too (the Dock
  menu).
- **Dock menus:** offer high-value items, such as open or recent windows and actions useful
  when the app is not in front; every item also exists in the menu bar or the window.
- **File management, Quick Look previews:** use a Quick Look viewer to preview files the app
  cannot open itself.
- **Searching, systemwide search:** make the app's content findable in Spotlight with
  descriptive metadata; take privacy into account before showing what people searched.
- **Going full screen:** use the system's full screen, which keeps clear of the camera housing;
  let people choose when to enter it (the green button, View › Enter Full Screen, ⌃⌘F); keep the
  Dock reachable.
- **Layout, macOS:** keep content out from behind the camera housing at the window's top edge
  (`NSPrefersDisplaySafeAreaCompatibilityMode`).
- **Keyboards:** ⌘Space is Spotlight's; ⌃⌘F enters full screen; ⌘, opens settings.

### GPUI at Zed `v1.27.0` (`c84af5b`)

Paths are under `~/.cargo/git/checkouts/zed-a70e2ad075855582/c84af5b/crates/`.

- **System notifications exist and are cross-platform.** `gpui/src/platform.rs:535-566`:
  `Platform::show_system_notification`, `dismiss_system_notification`,
  `on_system_notification_response` and `set_app_identity`, with `SystemNotification { tag,
  title, body, actions }` (`:678`). `App` wraps them (`gpui/src/app.rs:1934-1972`).
  - macOS: `gpui_macos/src/system_notifications.rs`, through `UNUserNotificationCenter`. It
    does nothing until the first post, then asks for authorization (`Alert | Sound`, not
    `Badge`). It turns itself off when the process has no bundle identifier, which is how
    `cargo run` starts Studio ("system notifications disabled: not running from an app
    bundle"). A tag is the request identifier, so a newer notification with the same tag
    replaces the older one. Its delegate shows banners even while the app is in front
    (`willPresentNotification` answers `Banner | List`): whether to post is the app's call.
  - Windows: toast notifications (`gpui_windows/src/system_notifications.rs`), with the
    AppUserModelID from `set_app_identity` (`gpui_windows/src/platform.rs:929`).
  - Linux: the XDG notifications D-Bus interface through `notify-rust`
    (`gpui_linux/src/linux/system_notifications.rs`).
  - Tests: `TestPlatform` records what was shown, delivered and dismissed, and posts nothing
    until `set_app_identity` was called; `TestAppContext::shown_system_notifications`,
    `delivered_system_notifications` and `simulate_system_notification_response`
    (`gpui/src/app/test_context.rs:445-462`).
- **Dock menu:** `Platform::set_dock_menu(Vec<MenuItem>, &Keymap)` (`gpui_macos/src/platform.rs:1151`),
  items being actions, as the app menus are. No Dock badge: GPUI never touches `NSDockTile`.
- **Recent documents:** `add_recent_document(&Path)` notes a file URL with
  `NSDocumentController` (`gpui_macos/src/platform.rs:1161`). A graph is not a file.
- **URL scheme:** `on_open_urls` and `register_url_scheme` (`gpui/src/platform.rs:446-448`).
- **Window tabs:** `WindowOptions::tabbing_identifier` turns on AppKit's automatic window
  tabbing (`gpui_macos/src/window.rs:965-970`).
- **Full screen:** `Window::toggle_fullscreen`, `is_fullscreen`.
- **Active state:** `Window::is_window_active` follows the window becoming and resigning key
  (`gpui/src/window.rs:3018`), so no Studio window is active while another app is in front.
  `App::active_window` asks `NSApp.mainWindow` (`gpui_macos/src/window.rs:1286`).
- **Not in GPUI:** `NSStatusItem` (menu bar extras), `QLPreviewPanel`, Core Spotlight, and the
  app delegate's `application:continueUserActivity:restorationHandler:`, which is how a
  Spotlight result or a Handoff reaches the app. GPUI's delegate class
  (`gpui_macos/src/platform.rs:78-158`) adds launch, reopen, terminate, menu, Dock menu,
  open-URLs, keyboard-layout, screen and sleep methods only.
- **AppKit from Rust:** `gpui_macos` depends on `objc2-app-kit` and `objc2-user-notifications`
  (`gpui_macos/Cargo.toml:45,52`), so the crates are already in the build.

### Studio (`main` at `e112c795`)

- `crates/studio/src/main.rs` takes `omnigraph-studio://` links through `on_open_urls` (and the
  start's arguments on Linux and Windows) and hands them to `studio::open_link`.
  `crates/studio/src/deep_link.rs` parses `graph`, `node`, `query`, `open` and `open-cluster`
  links. A `node` link (`?server=&graph=&type=&id=[&branch=]`) opens a node's page; Copy Link
  (#142) writes one.
- `crates/studio/src/menus.rs` sets the app menus with `set_menus`, including the Services
  submenu. Studio sets no Dock menu and notes no recent documents.
- `script/bundle-mac` writes `Info.plist` (bundle id `com.modernrelay.omnigraph-studio`,
  `CFBundleURLTypes` for `omnigraph-studio`, `LSMinimumSystemVersion` 12.0) and signs with
  `resources/macos/studio.entitlements` (`allow-jit`, `allow-unsigned-executable-memory`), with
  the Developer ID when `STUDIO_SIGN_IDENTITY` is set (hardened runtime, `--options runtime`),
  else the dev identity, else ad hoc.
- `objc2-core-foundation` and `objc2-core-graphics` are workspace dependencies, used by
  `type_table/src/pdf.rs` on macOS. PR #149 (`fix/canvas-animations`) adds `objc2-app-kit`
  (`NSAccessibility`, `NSWorkspace`) and `crates/studio/src/reduce_motion.rs`, a module of the
  `studio` crate with `#[cfg(target_os = "macos")]` and `#[cfg(target_os = "windows")]` arms,
  which reads the system's Reduce motion at launch and whenever a window comes to the front.
  That is the pattern this plan follows.
- Agent conversations (spec 011): `agent_panel::sessions::AgentSessions` is the one index of
  conversations, with each one's graph key, agent branch and whether it was created, what it is
  doing (`Live::Running`, `Waiting`, `Failed`) and whether it finished unseen (`unread`). The
  panel marks a conversation unread when its turn ends while its tab is not shown
  (`panel.rs`, `HostEvent::TurnEnded`). It emitted no event: other crates could not learn that
  a turn ended.
- The head watch reads each watched branch's head every 30 s only while a Studio window is
  active and shows the graph (`docs/architecture.md`, Snapshot pins). In the background Studio
  sees no other writer's commit or branch.
- Blob values (spec 004): the peek and the Blob tab (`type_table/src/blob_view.rs`) read a value
  at a commit; Open in its application writes it to a file and calls `open_with_system`.
  PR #144 (`feat/gallery-view`, spec 035) adds the Gallery view, where **Space toggles the
  inspector** and ↵ opens the file. Space is the inspector's key in every view
  (`assets/keymaps/default-macos.json`).
- Studio's own Keychain items (the OpenRouter key, the UI tools token) go through GPUI's
  `write_credentials` (`graph_store/src/sign_in.rs`); a server's token is the CLI's, exported
  through a pipe (spec 003).

## Design

### Order

Ranked by what each gives a person against what it costs. Each step ships alone.

| Step | Feature | Value | Cost | Needs |
|---|---|---|---|---|
| 1 | Notifications: agent conversations | High: agents run for minutes, and their branch waits for review | Low: GPUI posts them; one event out of `agent_panel` | A bundle; the person's yes to macOS's prompt |
| 2 | Notifications: merges, publishes and long reads | Medium | Low to medium: hooks on the outcome log and the read ledger | Step 1 |
| 3 | Dock badge and Dock menu | Medium: the count is visible from any app; recent graphs one click away | Low: `set_dock_menu` exists; the badge is one AppKit call | Step 1 for the count |
| 4 | Quick Look for Blob files | Medium for graphs with files (gallery, peek) | Medium: `QLPreviewPanel` needs a responder GPUI lacks, and a file on disk | PR #144 for the gallery |
| 5 | Menu bar extra for agent activity | Medium for people who run many agents; off by default | Medium: `NSStatusItem` and an `NSMenu` target class | Step 1's events |
| 6 | Spotlight for nodes | Medium, and a privacy cost | High: Core Spotlight, an index lifecycle, a delegate method GPUI lacks | Spec 031's names; a GPUI change or a runtime method |
| — | The notch | None | — | No public API (below) |

**Step 1 ships first.** It is the only one whose platform half is already written and tested
in GPUI, it answers the most common wait (an agent's turn), and it is the base of steps 2, 3
and 5.

### Where the code lives

- macOS-only code sits in the `studio` crate (layer 6), one module per feature, each declared
  `#[cfg(target_os = "macos")]`, as `reduce_motion.rs` does. A feature crate never names
  AppKit. A view never names a notification, a badge or the menu bar: it emits what happened,
  and `studio` decides what macOS shows. So `script/check-layers.py` needs no change and no new
  crate is added. A new platform crate would be layer 4 or 5 and is only worth it when a second
  binary needs the same code; none does.
- What a feature crate tells `studio` is a plain event with names only (step 1:
  `agent_panel::Attention`). `studio` subscribes in `init`.
- AppKit calls go through `objc2-app-kit` (already in the build through `gpui_macos`; PR #149
  adds it as a direct dependency of `studio` on macOS). New features list each framework they
  link: `UserNotifications` (step 1, through GPUI), `AppKit` (steps 3 to 5), `Quartz` /
  `QuickLookUI` (step 4), `CoreSpotlight` (step 6).
- Settings live in `studio_settings`: a new `notifications` table, section "Notifications",
  shown on macOS only until another platform posts (`x-hidden` elsewhere, set with
  `cfg_attr`). Per-graph values go in `GraphOverrides`, edited in the settings tab's graph
  scope.
- Every new command is an action in `studio_actions` with a default binding and a palette
  entry (VIII); the Dock menu and the menu bar extra list actions, as `menus.rs` does.

### What every feature needs from the bundle

| Need | Steps | Today |
|---|---|---|
| A bundle with `CFBundleIdentifier` (a `.app`, not `cargo run`) | 1 to 6 | `script/bundle-mac`; `cargo run` gets none, and GPUI posts nothing then |
| Developer ID signature and notarization, for a person's Mac | 1 to 6 (prompts and Notification Center settings follow the signed identity) | `STUDIO_SIGN_IDENTITY` in the release job |
| Entitlements | None of these needs one: Studio is not sandboxed, and notifications, Dock, status items, Quick Look and Core Spotlight need no entitlement outside the App Sandbox | `allow-jit`, `allow-unsigned-executable-memory` |
| `Info.plist` keys | Step 1: none. Step 6: `NSUserActivityTypes` holding `com.apple.corespotlightitem`. Handoff, if taken: its activity type there too | `CFBundleURLTypes` and the basics |
| Permission prompts | Step 1: macOS asks once, at the first notification ("“Omnigraph Studio” Would Like to Send You Notifications"); step 3's badge needs no prompt for `NSDockTile` | — |

### Step 1: notifications for agent conversations (prototyped)

**What it does.** The person starts an agent on a task and switches to their editor. When the
agent's turn ends, or the agent waits for an answer, macOS shows a banner:

| Event | Title | Body |
|---|---|---|
| Turn ended, branch made | Agent Finished | Its changes on `agent/20261119-1042` in knowledge are ready for review. |
| Turn ended, no branch | Agent Finished | It replied in a conversation on knowledge. |
| Turn ended short (a limit, a refusal, an error) | Agent Stopped | Its turn on knowledge ended before it finished. |
| A change or a tool waits for an answer | Agent Is Waiting | It needs your answer before it changes `agent/…` in knowledge. (Without a branch: "It needs your answer in a conversation on knowledge.") |

Clicking the banner brings Studio to the front, brings forward the window that shows the
conversation's graph, and opens the conversation there (`studio_agent::OpenConversation`, not
as a preview), which finds its open tab. The conversation's own strip then offers Review of its
branch (spec 011). With no window on that graph, Studio only comes to the front: a
conversation opened in another graph's window would speak for the wrong graph.

**Rules.**

- Posted only while no Studio window is active (`Window::is_window_active` on every window). In
  front, the tab and the Conversations panel's unread dot already say it (HIG: show news in the
  app when it is in front).
- One notification per conversation: the tag is `agent:<session id>`, so a newer one replaces
  the last (HIG: never several for one thing).
- A stop the person asked for (`StopReason::Cancelled`) posts nothing.
- It names the graph's id and the agent's branch, never the conversation's title, the
  person's message or the agent's reply: the title is the start of the person's first message
  when the agent gave none, and either can quote row data.
- No action buttons: a click on the body opens the conversation, and a "Review" button would
  open Studio too (HIG: no action that only opens the app). Step 2 may add Allow and Reject to
  "Agent Is Waiting" once a decision can be made from the notification alone.
- Setting: `notifications.agent_turns`, default on, titled "Notify about agent conversations":
  "While Studio is in the background, a notification says when an agent finishes, stops short
  or waits for your answer. Clicking it opens the conversation. It names the graph and the
  agent's branch, never what was said." Default on, because the person started the agent and
  macOS asks before the first one shows; off removes the only cost (the prompt).

**Code.** `agent_panel::sessions` gains `Attention { session, graph, branch, kind }` and
`AttentionKind { Finished, Stopped, Waiting }`; `AgentSessions` emits it from `turn_ended`
(called by the panel on `HostEvent::TurnEnded`) and from `set_live` when a conversation begins
to wait. `agent_panel::on_attention` subscribes to it and `agent_panel::graph_of_conversation`
answers a conversation's graph key. `crates/studio/src/notifications.rs`
(`#[cfg(target_os = "macos")]`) checks the setting and that Studio is not in front, posts, and
handles the click.

**Windows and Linux.** GPUI posts there too (toasts; the XDG D-Bus interface). Step 1 keeps
them off and hides the setting until each is checked on its platform: Windows needs the
installer's Start-menu shortcut to carry the AppUserModelID for toasts to show (spec 027), and a
Linux desktop without a notification daemon drops them. Until then, both have what they have
today: the tab, the unread dot and the taskbar's attention flash.

### Step 2: notifications for merges, publishes and long reads

- **A merge or publish Studio ran** that ends while Studio is in the background, from
  `GraphSession::outcomes`: "Merge Finished · agent/x merged into main in knowledge." / "Merge
  Refused · main in knowledge moved; review agent/x again." / "Merge Outcome Unknown · Studio
  lost the answer; History will say whether it landed." Committed, refused and unknown are the
  three outcomes (III); the unknown one says how it is settled, never offers to send it again.
  Clicking opens the review or History. An error in front stays an in-window banner (HIG: an
  alert, not a notification, for an error while the person is there).
- **A read that took longer than 10 s** and ends in the background: a query run in the query
  editor, Expand all, a Blob saved or opened in its application. "Query Finished · people.gq
  returned 12,480 rows on knowledge." The query's name or file, never its text. Threshold and
  hook from the read ledger (spec 030) once it lands; until then from the query editor's run.
- Settings: `notifications.merges` (on), `notifications.long_reads` (on).
- **Other writers' branches** (an agent outside Studio opening `agent/*` for review) need the
  branch list read while Studio is in the background. The head watch reads only while a window
  is active; step 2 adds a background read of each open graph's branch list every 5 minutes
  while `notifications.new_branches` is on (default off), and posts "New Branch · agent/x on
  knowledge." once per branch. A server push would remove the poll; it is not in Omnigraph's
  contract today and is not needed for this step.

### Step 3: the Dock badge and the Dock menu

- **Badge.** `NSApp.dockTile.badgeLabel` shows the number of conversations whose news the person
  has not seen: those notified in step 1 and not opened since (the `unread` set, plus those
  waiting). Opened, the number drops; at zero the label is cleared. It counts notifications
  only (HIG: a badge counts unread notifications and nothing else), so it is not a count of
  branches or rows. Setting `notifications.badge` (on). Never the only sign: the
  Conversations panel shows the same.
- **Dock menu** (`set_dock_menu`), rebuilt when the open graphs or the conversations change:
  - Recent graphs, newest first, at most 8, from the saved windows: each an `OpenLink` with the
    graph's `omnigraph-studio://graph` link.
  - Conversations waiting for review: those with a created branch whose turn ended, at most
    5, each `OpenConversation`; and "Review agent/x" (`OpenBranch` then `ReviewBranch`).
  - New Window, Open Graph…, New Conversation.

  Each item is already in the File menu, the Conversations panel or the palette (HIG).
- Windows: the taskbar's jump list and overlay icon are the counterpart; Linux: the Unity
  launcher API's count, where the desktop has it. Both later, behind the same settings.

### Step 4: Quick Look for Blob files

- **What it does.** In the gallery (PR #144), the Blob cell's peek and the Blob tab, ⌘Y shows
  the value in the system's Quick Look panel: a PDF with all its pages, a video or audio file
  that plays, a Keynote or Word file, a 3D model, any type Quick Look reads, where Studio's own
  view shows a hex dump today. ↑ ↓ ← → in the gallery move the preview with the cursor, as in
  Finder.
- **Key.** Finder uses Space and ⌘Y. Space is Studio's inspector key in every view, so Quick
  Look takes ⌘Y (`studio_blob::QuickLook`, contexts `TypeTable`, `Gallery`, `BlobView`; ⌘Y's
  other uses, `studio_agent::Allow` and `studio_review::AcceptAndNext`, are in other contexts).
- **API.** `QLPreviewPanel.sharedPreviewPanel` with a data source of `NSURL` file URLs. AppKit
  hands the panel to the first responder that answers `acceptsPreviewPanelControl:`; GPUI's
  view does not. Either a small GPUI patch adds the three responder methods to its view and
  asks the app for items, or Studio's module adds them to GPUI's view class at run time
  (`class_addMethod`). The plan prefers the patch, kept as a fork of one file (XI) until Zed
  takes it.
- **The file.** Quick Look reads files, so the value is written, range by range as Open in its
  application does (`save_value`), to `<cache>/quicklook/<random>/<suggested name>`, a folder
  only the person can read (0700). It is removed when the panel closes, and the folder is
  emptied at every start. This is a copy of row data on disk for as long as the panel is open:
  Studio never reads it back or treats it as true (I), and nothing is written until the person
  asks. At most `VIEW_MAX` (64 MiB); a larger value says "Too large to preview here · Open in
  its application".
- **Windows and Linux:** the Blob tab (↵), which shows images, PDFs on macOS, text and a hex
  dump, and Open in its application.

### Step 5: a menu bar extra for agent activity

- **What it does.** A template symbol in the menu bar shows that agents are working (a filled
  mark while any conversation runs, a dot when one waits). Its menu lists conversations that
  run, wait or finished unseen, by graph and branch ("knowledge · agent/x · Waiting"); choosing
  one opens it. Then "Open Omnigraph Studio" and "Hide This Icon". The menu, not a popover
  (HIG).
- **Off by default.** `agent.menu_bar_extra`, "Show agent activity in the menu bar", is turned
  on in Settings or by the action `studio_agent::ToggleMenuBarExtra` (palette); the first time
  an agent runs past a minute, the Conversations panel offers it once. The person decides, not
  Studio (HIG). Everything in it is also in the Dock menu and the Conversations panel.
- **API.** `NSStatusBar.systemStatusBar.statusItemWithLength(NSVariableStatusItemLength)`, an
  `NSMenu` whose items target one Objective-C object Studio defines (`define_class!`) that
  dispatches the action on GPUI's main thread. The symbol is a template image drawn from
  Studio's icon set, 18 pt in the 24 pt bar.
- **Windows:** a notification-area icon with the same menu (later). **Linux:** the
  StatusNotifierItem D-Bus interface where the desktop offers it (later).

### Step 6: Spotlight for nodes

- **What it does.** ⌘Space, "Ada Lovelace", shows "Ada Lovelace · Person · knowledge" under
  Omnigraph Studio; choosing it opens her node page through the existing deep link
  (`omnigraph-studio://node?server=…&graph=knowledge&type=Person&id=ada`).
- **Off by default, per graph.** `graphs.<key>.spotlight` (`GraphOverrides::spotlight`),
  "Find this graph's nodes in Spotlight", in the settings tab's graph scope, and the action
  `studio_graph::ToggleSpotlight`. Its description says what is kept: "Spotlight keeps each
  node's name and type on this Mac, in an index only you can read, until you turn this off.
  Nothing else of the graph is indexed."
- **What is indexed.** For each node whose name Studio read in spec 031's name index: the
  name (the title), the type and graph (the description), the deep link as the item's unique
  identifier, and the graph's key as its domain identifier. Never another property, a Blob, a
  query or a token. At most 50,000 names a graph. Re-indexed from the name index when it is read
  again; removed (`deleteSearchableItemsWithDomainIdentifiers`) when the setting goes off, the
  graph is forgotten or its CLI target is removed. A node removed from the graph leaves
  Spotlight at the next read of its type.
- **API.** `CSSearchableIndex.defaultSearchableIndex`, `CSSearchableItem` with a
  `CSSearchableItemAttributeSet` (`contentTypeUTType` `.content`); a choice reaches the app as
  an `NSUserActivity` of type `CSSearchableItemActionType` whose `userInfo` holds the item's
  identifier, the deep link, which goes to `studio::open_link`. GPUI's app delegate does not
  answer `application:continueUserActivity:restorationHandler:`, so this needs a GPUI patch
  (the method, handing the activity to a callback, as `on_open_urls` does) or a method added at
  run time. `Info.plist` gains `NSUserActivityTypes`.
- **Not `NSUserActivity` indexing:** an activity is indexed only while it is current, as the
  person views a page; Core Spotlight indexes what the person did not open too, which is the
  point.
- **Windows and Linux:** the palette's Go To (⌘P, spec 031) inside Studio; neither system has a
  search index an app can feed without a separate service.

### The notch

Studio does not draw in or around the camera housing. macOS gives no API to draw into it:
`NSScreen.safeAreaInsets` and `auxiliaryTopLeftArea`/`auxiliaryTopRightArea` say where it is so
an app can stay clear, and the system's full screen already keeps windows below it. Apps that
put content there place a borderless window at the menu bar's level over it, which the HIG asks
against (Layout, macOS), which hides under the menu bar
when the pointer reaches the top, and which any macOS release can break. The menu bar extra
(step 5) shows the same news, agent activity, in the place macOS gives apps for it, beside the
notch rather than under it.

### Also worth adopting, briefly

- **Full screen:** Studio already uses the system's (`toggle_fullscreen`, focus mode). Check
  that View › Enter Full Screen and ⌃⌘F are offered (AppKit adds the item to a menu titled View
  when the app does not) and that the Dock is reachable in focus mode. Cost: a check and maybe
  one menu item.
- **Window tabs:** `tabbing_identifier` would let a person merge windows (one graph each) into
  one tabbed window. It adds a second row of tabs over Studio's own, so it waits for a person
  who asks; then it is a setting, off by default.
- **Services:** the Services submenu is already in the app menu. Studio could provide "Open in
  Omnigraph Studio" for a selected `omnigraph-studio://` link in any app (`NSServices` in
  `Info.plist`, one provider method). Low value: a click on the link does the same.
- **Handoff:** once step 6's activity plumbing exists, a node page could publish an
  `NSUserActivity` another Mac's Studio continues. Both Macs need the same server reachable and
  known; value is low until people work across Macs.
- **Touch ID for the Keychain:** server tokens are the CLI's (spec 003), so Studio's own items
  are the OpenRouter key and the UI tools token. Touch ID needs them in the data protection
  keychain with an access control (`kSecAccessControlUserPresence`), which needs a
  `keychain-access-groups` entitlement and so a Developer ID provisioning profile. Worth it when
  the release is provisioned for another reason.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Notifications first | Quick Look first; Dock first | GPUI already posts them, with a test platform; the agent's turn is the longest common wait; steps 2, 3 and 5 build on its events |
| Post only when no Studio window is active | Post always; post when the conversation's tab is not shown | The HIG asks for news in the app while it is in front; the tab and the unread dot say it there. GPUI's delegate would show the banner in front too, so Studio decides |
| Studio window activity as "in front" | `NSApplication.isActive` | It works in GPUI's tests (`deactivate_window`) and needs no AppKit call. The difference: a system sheet (an open panel) as key window counts as background for a moment, which posts nothing unless a turn ends right then |
| One notification per conversation, replaced | One per event | HIG: never several for one thing |
| Names only: graph id and branch | The conversation's title; the agent's last sentence | Either can hold row data or the person's words, shown on a locked screen and kept in Notification Center |
| Title case titles | Studio's sentence case | The HIG sets title-style capitalization for notification titles; they are a system element type, read beside other apps' |
| Setting on by default | Off by default; ask in Studio first | The person started the agent; macOS asks before the first banner, and the setting turns off the only cost |
| `Attention` event out of `agent_panel` | `studio` reads `AgentSessions` and diffs it; the panel posts itself | The panel is a feature crate and must not name the platform; a diff in `studio` would have to guess why a conversation changed (a closed tab also clears `Live`) |
| macOS-only modules in `studio` | A new `studio_macos` crate | One binary uses them; `reduce_motion.rs` set the pattern; no layer table change |
| Windows and Linux later, setting hidden there | On everywhere now | GPUI's backends exist but each needs a check on its platform (AppUserModelID shortcut; a daemon); a toggle that does nothing would break VI |
| Quick Look on ⌘Y | Space, as Finder | Space toggles the inspector in every view |
| Spotlight off by default, per graph | On for every graph; on for all graphs at once | Its index lives on disk, outside Studio; a graph's names can be private, and the person decides per graph |
| No drawing in the notch | A borderless window over it | No public API, against the HIG, and fragile; the menu bar extra serves the same need |

## Deferred

- Steps 2 to 6, in the order above.
- Windows and Linux notifications, the badge's counterparts and a tray icon, each after a check
  on its platform.
- `hiddenPreviewsBodyPlaceholder` (the generic body shown when the person hides previews):
  GPUI's `SystemNotification` has no field for it; until a GPUI change, macOS shows its default.
- No Omnigraph engine change is needed. A server push of branch and head changes would replace
  step 2's background read; recorded here, not yet in spec 012, since the poll works.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-19 | GPUI at `c84af5b` posts system notifications on macOS, Windows and Linux, and its test platform records them | Read `gpui/src/platform.rs:535-566,678-710`, `gpui_macos/src/system_notifications.rs`, `gpui_windows/src/system_notifications.rs`, `gpui_linux/src/linux/system_notifications.rs`, `gpui/src/platform/test/platform.rs:420-460,705-740` | Yes. macOS asks `Alert | Sound` at the first post, skips a process with no bundle identifier, and replaces by tag; the test platform posts nothing without `set_app_identity` |
| 2026-11-19 | GPUI has a Dock menu but no Dock badge, status item, Quick Look or Spotlight, and its app delegate has no `continueUserActivity` | `grep` of `gpui_macos/src` for `dockTile`, `NSStatusItem`, `QLPreview`, `CSSearchable`, `NSUserActivity`; the delegate's `add_method` list at `gpui_macos/src/platform.rs:78-158` | None found; `set_dock_menu` at `:1151`, `add_recent_document` at `:1161` |
| 2026-11-19 | No Studio window is active while another app is in front, in GPUI's tests too | `gpui/src/window.rs:3018` (`is_window_active` follows key status); `VisualTestContext::deactivate_window` | `studio` `notifications::tests::only_studio_in_the_background_is_notified` passes |
| 2026-11-19 | A turn's end and a wait are told with the graph and the branch, a stop the person asked for is not | `agent_panel` `panel::tests::a_turn_that_ends_says_how`: EndTurn, Cancelled, MaxTokens, an error, then a wait (twice) | Finished, Stopped, Stopped, Waiting; the branch only once created; a second wait tells nothing |
| 2026-11-19 | Studio posts only in the background, one notification a conversation, and not when the setting is off or the conversation is unknown | `studio` `notifications::tests::only_studio_in_the_background_is_notified` | In front: none. Background: two shown, one delivered (the newer replaced the older). Off: none. Unknown conversation: none |
| 2026-11-19 | A click opens the conversation in the window of its graph, and nothing for a graph no window shows | `studio` `notifications::tests::a_click_opens_the_conversation_where_its_graph_is` | `OpenConversation { session: "s-here", preview: false }` reaches that window; none for the other graph |
| 2026-11-19 | Copy names the graph and branch only, in the HIG's form | `studio` `notifications::tests::a_notification_names_the_graph_and_the_branch` | Passes |
| 2026-11-19 | The Notifications section shows on macOS only | `studio_settings` `fields::tests::every_setting_is_a_row_in_declaration_order` (expects it on macOS, not elsewhere) | Passes on macOS; the other platforms are checked by CI |
| 2026-11-19 | A banner appears on a real Mac and its click opens the conversation | Not checked: needs a Developer ID or dev-signed `.app` from `script/bundle-mac` and the person's yes to the prompt, with an agent signed in | Open. To do before step 1 is called built |
| 2026-11-19 | `NSDockTile.badgeLabel` shows without notification authorization | Not checked | Open, for step 3 |

## Corrections

None yet.
