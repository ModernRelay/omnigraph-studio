# 047: Studio without Omnigraph Cloud, and the Cloud waitlist

Status: in progress; step 1 (the dead ends, and waitlist links 1 and 2) built · 2026-11-21

Design study: `docs/ux/2026-11-21-cloud-onboarding.md`, in
[pull request #163](https://github.com/ModernRelay/omnigraph-studio/pull/163). This plan builds its order of work, steps 1 and 2, and says where it departs.

## Problem

Omnigraph Cloud is in private preview: invitations go out from a waitlist. Studio presents it as a
place anyone can sign in to ("Sign in and open a cluster of your organization."), and a person
without an invitation who signs in anyway reaches **No organizations**, told to ask an owner they
do not have. An organization with no clusters says only "No clusters". A failed sign-in in
Settings has no way to try again until Studio restarts, and calls the control plane "the server".
With Cloud turned off, the dialog describes a path to the setting instead of going there.

## Principles

- **I.** Studio makes no new request. A waitlist link opens the browser; the page asks for the
  email, not Studio.
- **VI.** Every state says what to do next; No clusters shows the CLI command that makes one.
- **VII.** Loudness matches cost: the waitlist is a quiet link where the person already looks at
  Cloud, and the primary button only where it is the one way on. No toast, badge or banner.
- **VIII.** No new action in this step (below), so no binding or palette entry is added.

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
- `App::open_url` reaches the platform; GPUI's test platform records the last URL
  (`gpui/src/platform/test/platform.rs:619` at Zed `c84af5b99cbc`), which the tests read.

## Design

### The link

`https://www.omnigraph.dev/cloud?utm_source=omnigraph-studio&utm_medium=app&utm_content=<surface>#join`

- The page address is `studio_settings::OMNIGRAPH_CLOUD_WAITLIST`, beside `OMNIGRAPH_CLOUD`.
  `waitlist::url(Surface)` adds the constant query; `Surface` is an enum, so a test checks every
  link and that none carries anything else. This step's surfaces: `get-started`, `sign-in`,
  `no-organizations`.
- Opened with `cx.open_url`. Studio sends nothing, stores nothing and logs nothing.
- "Join the waitlist ↗": a ghost button with the `ArrowUpRight` icon at its end, tooltip "Opens
  omnigraph.dev in your browser" (`waitlist::link`); primary on No organizations.

### When it shows (`waitlist::offered`)

On the plane at the `omnigraph_cloud` address (`is_default`) only, and not while it holds a
sign-in, unless that sign-in lists no organization. So:

| State | Links |
|---|---|
| Signed out of Omnigraph Cloud | Get started's row, the sign-in pane |
| Signed in, organizations not read yet | none (no link that comes and goes while they are read) |
| Signed in, no organization | No organizations, in the dialog and Settings → Clusters |
| Signed in, an organization | none |
| `omnigraph_cloud` empty | none |
| A plane only a CLI profile names | none |

### Copy

| Where | Before | After |
|---|---|---|
| Get started, Cloud row | Sign in and open a cluster of your organization. | Hosted Omnigraph, in private preview. · Join the waitlist ↗ at the row's end. Signed in to an organization, the old line; Cloud off, "Turned off in Settings." |
| The Cloud pane, signed out | One sign-in lists every organization you are a member of, with its clusters. · Sign in | Omnigraph Cloud is in private preview, and invitations go out from the waitlist. Once invited, sign in to open your organization's clusters. · **Sign in** (primary, ↵) · Join the waitlist ↗ |
| No organizations, Omnigraph Cloud | …Ask an owner of one to invite you, then Refresh. · Refresh · Sign out | You are signed in as ACCOUNT, but no organization has you as a member yet. Omnigraph Cloud is in private preview: an owner of an organization can invite you, or join the waitlist for an invitation. · **Join the waitlist ↗** · Refresh · Sign out |
| No organizations, another plane | unchanged | unchanged |
| Settings → Clusters, no organization | You are a member of no organization here. | The No organizations sentence, and Join the waitlist ↗ on Omnigraph Cloud |
| No clusters (dialog and Settings) | No clusters / No clusters in this organization. | Nothing to open yet. A member whose role allows it creates a cluster with the Omnigraph CLI, through a profile in this organization, then Refresh. · `omnigraph cluster create <NAME> --profile P` with Copy, P the CLI profile naming the organization, or `<PROFILE>` to fill when none does: the CLI picks the organization by profile, and its default profile may be another organization's |
| Sign-in unavailable (dialog and Settings) | The server's sign-in details could not be read: … | Omnigraph Cloud's sign-in details could not be read: … ("the control plane" for another plane); Settings gains **Try again**, as the dialog has |
| Cloud off, on choosing the row | Omnigraph Cloud's address is not set. Set it in Settings → Omnigraph Cloud. (as an error) | Omnigraph Cloud is off: its address is empty. · Open Omnigraph Cloud settings (closes the dialog, opens Settings at that section), muted rather than an error |

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| "Omnigraph Cloud" for the waitlist is `is_default`, the plane at the setting's address | Only when the address equals `OMNIGRAPH_CLOUD` | Studio already titles that plane "Omnigraph Cloud" everywhere, so the waitlist follows the name; the study's screenshots (a stand-in address) rely on it. A non-default plane, one a CLI profile names, hides the links. An admin who points the setting at another control plane sees the waitlist under the name "Omnigraph Cloud", as they already see that name; revisit if such deployments appear |
| No action in this step | The study's `studio::JoinCloudWaitlist { surface }` | Buttons open the link directly. The action, with its palette entry and Help menu item, comes with touchpoint 5, where the palette needs it; an action now would need a binding AGENTS.md asks for and the study calls pointless |
| The row's link at its end, the row's line shortened | The study's longer line "… Sign in, or join the waitlist." | At 1280 px the long line was cut before "join"; the link beside it says it |
| No clusters names a member "whose role allows it" | "An owner can create one" | Whether a role may create a cluster is the control plane's policy (constitution IV); Studio does not know which roles may |
| The reason's "the server" rewritten in the view (`plane_words`) | New wording in `oauth_client` | `oauth_client` words every server's sign-in; only the control plane's views know what to call it |
| Links hidden while a signed-in plane's organizations are unread | Shown until they are read | The Open dialog lets the lists go when it closes, so a member would see the link appear and vanish on every open (constitution V) |

## Deferred

The study's touchpoints 3 to 5, next:

- **3.** Settings → Omnigraph Cloud, Account row signed out: "Omnigraph Cloud is in private
  preview; invitations go out from the waitlist." with the link (surface `settings`).
- **4.** The local recipe, one muted line under its steps (surface `local-recipe`).
- **5.** Help menu "Omnigraph Cloud Waitlist…" beside a new "Omnigraph Website" item, and the
  palette, through one action `studio::JoinCloudWaitlist` (surface `help`).
- The brand-repo change that credits signups to Studio (the study's step 3) is outside this
  repository.

No engine change.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-21 | Each surface's link is the Cloud page at `#join` with only `utm_source`, `utm_medium`, `utm_content` | `studio` `waitlist::tests::each_surface_opens_the_page_with_only_its_name` | pass |
| 2026-11-21 | The waitlist shows signed out and in no organization, never in an organization, while organizations are unread, or on another plane | `waitlist::tests::the_waitlist_is_offered_only_where_it_answers`; `open_graph::tests::another_control_plane_offers_no_waitlist`; `no_clusters_gives_the_command_that_makes_one` (no link in an organization, on its pane or Get started) | pass |
| 2026-11-21 | Get started and the pane say private preview; their links open the right URL; the link does not take the row, and ↵ still goes to Sign in | `open_graph::tests::cloud_says_it_is_in_private_preview_and_offers_the_waitlist` (`TestAppContext::opened_url`) | pass |
| 2026-11-21 | No organizations offers Join the waitlist first and opens `no-organizations` | `open_graph::tests::no_organizations_offers_the_waitlist_first`; Settings: `cloud_settings::tests::clusters_point_forward_where_there_are_none` | pass |
| 2026-11-21 | No clusters shows `omnigraph cluster create <NAME> --profile work`, copied with its Copy | `open_graph::tests::no_clusters_gives_the_command_that_makes_one`; Settings: `clusters_point_forward_where_there_are_none` | pass |
| 2026-11-21 | Settings → Omnigraph Cloud offers Try again when sign-in is unavailable, in Omnigraph Cloud's name | `cloud_settings::tests::sign_in_unavailable_offers_try_again`; `open_graph::tests::a_failed_sign_in_names_the_control_plane` | pass |
| 2026-11-21 | With Cloud off, choosing the row shows the link to its setting, which closes the dialog and opens nothing in the browser | `open_graph::tests::cloud_off_links_to_its_setting_and_offers_no_waitlist` | pass |
| 2026-11-21 | `omnigraph cluster create` exists in the pinned CLI and takes the organization from `--profile` | `crates/omnigraph-cli/src/commands/cluster/args.rs:99-107`, `planes.rs:135`, `managed/lifecycle.rs:58-80` at `v0.15.0` | `Create { name }`, a `ManagedAccount` command |
| 2026-11-21 | Each screen draws as designed | `crates/studio/examples/snapshot.rs` at 1280×800, `OMNIGRAPH_HOME` an empty folder, `OMNIGRAPH_STUDIO_DATA` holding only `omnigraph_cloud` = `http://127.0.0.1:8874` (nothing listens; `lsof` checked), or `""` for Cloud off | Get started, the Cloud pane, Sign-in unavailable in the dialog and Settings, Cloud off: as in the PR's screenshots. No organizations and No clusters need a control plane that answers a sign-in and are covered by the headless tests only |

## Corrections

- 2026-11-21, review: No clusters left `--profile` off when no CLI profile names the
  organization, so the copied command ran in the CLI's default profile, which may be another
  organization's. It now names `<PROFILE>` to fill. And choosing Get started's Cloud row with
  Cloud off opened the first plane a CLI profile names, though the row said Cloud was off; it now
  shows the link to the setting, as the row says.
