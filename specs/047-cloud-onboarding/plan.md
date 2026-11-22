# 047: Studio without Omnigraph Cloud, and the Cloud waitlist

Status: in progress; built 2026-11-21: the dead ends, and the waitlist in nine places with one
rule. Waits on the site change that credits signups to Studio (outside this repository).

Design study: `docs/ux/2026-11-21-cloud-onboarding.md`, in
[pull request #163](https://github.com/ModernRelay/omnigraph-studio/pull/163). This plan builds
the study's touchpoints 1 to 5 and more places the user asked for. It says where it departs from
the study.

## Problem

Omnigraph Cloud is in private preview: invitations go out from a waitlist. Studio presents it as a
place anyone can sign in to ("Sign in and open a cluster of your organization."), and a person
without an invitation who signs in anyway reaches **No organizations**, told to ask an owner they
do not have. An organization with no clusters says only "No clusters". A failed sign-in in
Settings has no way to try again until Studio restarts, and calls the control plane "the server".
With Cloud turned off, the dialog describes a path to the setting instead of going there. Nothing
in Studio says what Cloud's status is, or where the waitlist is.

## Principles

- **I.** Studio makes no new request. A waitlist link opens the browser; the page asks for the
  email, not Studio.
- **VI.** Every state says what to do next; No clusters shows the CLI command that makes one.
- **VII.** Loudness matches cost: the waitlist is a quiet link where the person already looks at
  Cloud, and the primary button only where it is the one way on. One toast, after Copy Link to a
  server on this computer, says a fact the person needs (the link reaches no one else), as Studio's
  other Info toasts do (`show_server_notices`); see Decisions.
- **VIII.** `studio::JoinCloudWaitlist` is an action with a binding, in the Help menu and the
  palette.

## What exists today (checked)

- Get started's Cloud row: `crates/studio/src/first_run.rs:55`; choosing it with no plane says
  the path in words (`open_graph.rs:775-781` at `74eedf31`).
- The plane's pane and No organizations: `crates/studio/src/organizations.rs:366-471`; No
  clusters: `:533-537`. Settings: `crates/studio/src/cloud_settings.rs:251-254` (no
  organization), `:355` (no clusters), `:470` (Unavailable draws no control).
- "The server's sign-in details …" is `oauth_client`'s wording for any server
  (`crates/oauth_client/src/discovery.rs:80-122`), shown as it is for a control plane.
- `ControlPlane::is_default` marks the plane at the `omnigraph_cloud` address, which Studio
  titles "Omnigraph Cloud" (`crates/graph_store/src/control_plane.rs:196`,
  `organizations::plane_name`); a plane only a CLI profile names is called by its host.
- The CLI makes a managed cluster: `omnigraph cluster create <NAME>`, a managed-account command
  run with `--profile` (`crates/omnigraph-cli/src/commands/cluster/args.rs:99-107` and
  `planes.rs:135` at `v0.15.0`, `373834b`; `omnigraph cluster create --help`, CLI 0.16.0).
- Copy Link wrote the link and said nothing (`crates/studio/src/lib.rs`, `copy_link`).
  `graph_client::is_loopback` says whether an address is `localhost`, `127.0.0.0/8` or `[::1]`.
- `Workspace::register_action_when` makes an action available only while its condition holds;
  the palette lists only available actions and the menu disables the others (constitution VIII).
- `App::open_url` reaches the platform; GPUI's test platform records the last URL
  (`gpui/src/platform/test/platform.rs:619` at Zed `c84af5b99cbc`), which the tests read.

## Design

### Copy

Studio's Cloud copy is factual, never promotional. It states the fact and the action: no
rhetorical question, no pitch line or benefit slogan ("managed for you", "runs it for you"), no
exclamation mark or urgency, and nothing the site does not say (no price, date or feature).
`waitlist::tests::the_copy_is_plain` checks the shared strings.

### The link

`https://www.omnigraph.dev/cloud?utm_source=omnigraph-studio&utm_medium=app&utm_content=<surface>#join`

- The page address is `studio_settings::OMNIGRAPH_CLOUD_WAITLIST`, beside `OMNIGRAPH_CLOUD`.
  `waitlist::url(Surface)` adds the constant query; `Surface` is an enum, so a test checks every
  link and that none carries anything else.
- Opened with `cx.open_url`. Studio sends nothing, stores nothing and logs nothing.
- "Join the waitlist ↗": a ghost button with the `ArrowUpRight` icon at its end, tooltip "Opens
  omnigraph.dev in your browser" (`waitlist::link`); primary on No organizations. A one-line note
  (`waitlist::note`) is a muted lead, "·", and the link at text size.

### One rule (`waitlist::offered`, `waitlist::offered_now`)

Every place follows it. The waitlist shows on the plane at the `omnigraph_cloud` address
(`is_default`), only while that address is Omnigraph Cloud's own (`OMNIGRAPH_CLOUD`, a last `/`
aside), and not while it holds a sign-in, unless that sign-in lists no organization.

| State | Waitlist |
|---|---|
| Signed out of Omnigraph Cloud | shown |
| Signed in, organizations not read yet | hidden (no link that comes and goes while they are read) |
| Signed in, no organization | shown |
| Signed in, an organization | hidden |
| `omnigraph_cloud` empty (Cloud off) | hidden |
| `omnigraph_cloud` at another address | hidden |
| A plane only a CLI profile names | hidden |

### The places

| Place | `utm_content` | Shown | Strings |
|---|---|---|---|
| Get started, Cloud row | `get-started` | the rule | line "In private preview." · Join the waitlist ↗ at the row's end |
| The Cloud pane, signed out | `sign-in` | the rule | "Omnigraph Cloud is in private preview. Invitations go out from the waitlist. Once invited, sign in to open your organization's clusters." · **Sign in** (primary, ↵) · Join the waitlist ↗ |
| No organizations (dialog, Settings → Clusters) | `no-organizations` | the rule | "Signed in as ACCOUNT. No organization has you as a member yet. Omnigraph Cloud is in private preview. An owner of an organization can invite you, and invitations go out from the waitlist." · **Join the waitlist ↗** (primary in the dialog) · Refresh · Sign out |
| Open dialog sidebar, Omnigraph Cloud's row | `sidebar` | the rule, signed out | a second line: "Private preview · Join the waitlist ↗" |
| Settings → Omnigraph Cloud, Account | `settings` | the rule, signed out | "Not signed in. Omnigraph Cloud is in private preview. Invitations go out from the waitlist." · Join the waitlist ↗ · **Sign in** |
| Copy Link toast | `copy-link` | the copied link's server is loopback | "This link opens only on this Mac. Omnigraph Cloud: private preview." · Join the waitlist (the toast's follow-up), the second sentence and the button only by the rule; elsewhere "this computer". A server others reach: no toast, as before |
| Help menu, palette, key | `help` | the rule | "Join the Omnigraph Cloud Waitlist" (`studio::JoinCloudWaitlist`, ⌘⌥⇧J; Ctrl+Alt+Shift+J on Linux and Windows). Otherwise disabled in the Help menu and not in the palette |
| About window | `about` | the rule | under the version: "Omnigraph Cloud: private preview · Join the waitlist ↗" |
| Under the local recipe | `local-recipe` | the rule | "Omnigraph Cloud: private preview · Join the waitlist ↗", above Cancel and Connect, which it never moves |

`JoinCloudWaitlist { from }` names a surface by its `utm_content`; anything else, or none, is
`help`, so the address never carries text from outside the list.

### The dead ends

| Where | Before | After |
|---|---|---|
| No organizations, another plane | …Ask an owner of one to invite you, then Refresh. | unchanged |
| No clusters (dialog and Settings) | No clusters / No clusters in this organization. | "This organization has no clusters. A member whose role allows it creates one with the Omnigraph CLI, through a profile in this organization. Then Refresh." · `omnigraph cluster create <NAME> --profile P` with Copy, P the CLI profile naming the organization, or `<PROFILE>` to fill when none does: the CLI picks the organization by profile, and its default may be another organization's |
| Sign-in unavailable (dialog and Settings) | The server's sign-in details could not be read: … | "Omnigraph Cloud's sign-in details could not be read: …" ("the control plane" for another plane); Settings gains **Try again**, as the dialog has |
| Cloud off, the row | Sign in and open a cluster of your organization. | "Turned off in Settings." |
| Cloud off, on choosing it | Omnigraph Cloud's address is not set. Set it in Settings → Omnigraph Cloud. (as an error) | "Omnigraph Cloud is off: its address is empty." · Open Omnigraph Cloud settings (closes the dialog, opens Settings at that section), muted. A plane a CLI profile names is no longer opened in Cloud's place |
| Signed in to an organization, Get started's row | Sign in and open a cluster of your organization. | unchanged |

### Screenshots

A debug build run with `OMNIGRAPH_STUDIO_KEYCHAIN=none` keeps no sign-in and reads none
(`sign_in::NoKeychain`), so the offscreen snapshot can draw Omnigraph Cloud at its own address
without reading this computer's keychain. A release build ignores the variable. Nothing is sent
to the control plane before Sign in, which the screenshots never press at that address.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| The waitlist shows only at Omnigraph Cloud's own address | Any address in the setting (`is_default` alone), as this plan's first version did | An admin may point the setting at another control plane, whose people are not on Omnigraph Cloud's waitlist. The user asked for the address rule |
| Help's action disabled in the menu and hidden from the palette by the rule | Hidden from the menu too; always shown | Disabling follows from `register_action_when` with no menu rebuild; the menu keeps its shape (constitution V) |
| A binding for `JoinCloudWaitlist` (⌘⌥⇧J) | The study's exception with no binding | AGENTS.md asks every action for one. The chord is one nobody hits by accident |
| A toast after Copy Link to a loopback server | Silence, as before; a tooltip on Copy Link | A link to `127.0.0.1` opens nowhere else, which the person needs to know before sending it. The toast is Info, says that fact, and offers one follow-up, as Studio's server notices do. It departs from VII's letter ("only after a durable publish"), which Studio's Info notices already do, and the user asked for it |
| The row's line shortened to "In private preview." | "In private preview. Invitations go out from the waitlist." | At 1280 px the longer line was cut; the link beside it names the waitlist |
| The sidebar note starts under the row's icon, its link at text size | Under the row's label; a compact button | The dialog's sidebar is narrow: either wrapped the link onto a third line |
| No clusters names a member "whose role allows it" | "An owner can create one" | Whether a role may create a cluster is the control plane's policy (constitution IV) |
| The reason's "the server" rewritten in the view (`plane_words`) | New wording in `oauth_client` | `oauth_client` words every server's sign-in; only the control plane's views know what to call it |

## Deferred

- The site change that credits signups to Studio (the study's step 3) is outside this
  repository.
- A Help menu item for omnigraph.dev itself ("Omnigraph Website"), which the study pairs with the
  waitlist, was not asked for here.

No engine change.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-21 | Each surface's link is the Cloud page at `#join` with only `utm_source`, `utm_medium` and its own `utm_content`; an action names only a known surface | `studio` `waitlist::tests::each_surface_opens_the_page_with_only_its_name`, `an_action_names_only_a_known_surface` | pass |
| 2026-11-21 | The rule: signed out and in no organization shown; in an organization, organizations unread, another address, another plane hidden | `waitlist::tests::the_waitlist_is_offered_only_where_it_answers`; `open_graph::tests::another_cloud_address_offers_no_waitlist`, `another_control_plane_offers_no_waitlist`, `no_clusters_gives_the_command_that_makes_one` | pass |
| 2026-11-21 | The shared copy has no question, exclamation or "for you" | `waitlist::tests::the_copy_is_plain` | pass |
| 2026-11-21 | Get started and the pane say private preview, open their own URL, and ↵ still goes to Sign in | `open_graph::tests::cloud_says_it_is_in_private_preview_and_offers_the_waitlist` | pass |
| 2026-11-21 | The sidebar row and the local recipe show the note signed out, open their URLs, and hide it in an organization | `open_graph::tests::the_sidebar_and_the_recipe_offer_the_waitlist` | pass |
| 2026-11-21 | No organizations offers Join the waitlist first | `open_graph::tests::no_organizations_offers_the_waitlist_first`; Settings: `cloud_settings::tests::clusters_point_forward_where_there_are_none` | pass |
| 2026-11-21 | Settings' Account row offers the waitlist signed out, not in an organization or at another address | `cloud_settings::tests::the_account_row_offers_the_waitlist_signed_out` | pass |
| 2026-11-21 | Copy Link: no toast for a server others reach; for `127.0.0.1`, `localhost` and `[::1]` the note, with the waitlist only by the rule; Help's action is not offered once in an organization | `start_tests::a_copied_link_to_this_computer_says_so` (lib.rs) | pass |
| 2026-11-21 | About shows the note by the rule and opens `about` | `about::tests::the_window_offers_the_waitlist_by_its_rule` | pass |
| 2026-11-21 | No clusters shows `omnigraph cluster create <NAME> --profile work`, copied with its Copy, and `<PROFILE>` without one | `open_graph::tests::no_clusters_gives_the_command_that_makes_one`; Settings: `clusters_point_forward_where_there_are_none` | pass |
| 2026-11-21 | Settings → Omnigraph Cloud offers Try again when sign-in is unavailable, in Omnigraph Cloud's name | `cloud_settings::tests::sign_in_unavailable_offers_try_again`; `open_graph::tests::a_failed_sign_in_names_the_control_plane` | pass |
| 2026-11-21 | With Cloud off, choosing the row shows the link to its setting, not another plane | `open_graph::tests::cloud_off_links_to_its_setting_and_offers_no_waitlist` | pass |
| 2026-11-21 | `omnigraph cluster create` exists in the pinned CLI and takes the organization from `--profile` | `crates/omnigraph-cli/src/commands/cluster/args.rs:99-107`, `planes.rs:135`, `managed/lifecycle.rs:58-80` at `v0.15.0` | `Create { name }`, a `ManagedAccount` command |
| 2026-11-21 | Each place that can be drawn offscreen draws as designed | `crates/studio/examples/snapshot.rs` at 1280×800, `OMNIGRAPH_HOME` an empty folder, `OMNIGRAPH_STUDIO_KEYCHAIN=none`; `omnigraph_cloud` at its default for the waitlist, at `http://127.0.0.1:8874` (nothing listens) for Sign-in unavailable, `""` for Cloud off; Copy Link with `OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:8873` (nothing listens) | Get started with the sidebar note, the pane, the recipe, Settings, Sign-in unavailable, Cloud off and the Copy Link toast as in the PR. The About window and the menus are not drawn offscreen; No organizations and No clusters need a control plane that answers a sign-in. Those are covered by the headless tests |

## Corrections

- 2026-11-21, review: No clusters left `--profile` off when no CLI profile names the
  organization, so the copied command ran in the CLI's default profile, which may be another
  organization's. It now names `<PROFILE>` to fill. And choosing Get started's Cloud row with
  Cloud off opened the first plane a CLI profile names, though the row said Cloud was off; it now
  shows the link to the setting, as the row says.
- 2026-11-21, the user: the copy was made plain and factual (see Copy). "Hosted Omnigraph, in
  private preview." became "In private preview."; the pane's "…private preview, and invitations…"
  became two sentences; No organizations dropped "but" and "join the waitlist for an invitation";
  "Nothing to open yet." became "This organization has no clusters.". The rule gained Omnigraph
  Cloud's own address, and six more places were added.
