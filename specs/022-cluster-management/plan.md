# 022: Managing clusters from Studio

Status: draft; waits on CP5 (Studio's sign-in allowed to make lifecycle writes) · 2026-11-06

## Problem

Studio lists every cluster of every organization the person is a member of (spec 021), shows each
one's state, and copies its MCP link. To change a cluster the person still leaves Studio for the
CLI: creating one, deleting one and undoing that within its retention window, pushing its
configuration and deploying it, and seeing who may reach it. The Settings page's Manage section
gives the CLI command for each (constitution VI). This plan makes those changes in Studio.

## Principles

- I: the control plane is the authority on clusters. Studio shows what it answered, with its
  `meta`, and keeps no cluster list of its own; after a write, the list is read again.
- III: every write here has three outcomes. Committed is the control plane's `202` with its
  operation; refused is a 4xx, and nothing changed; unknown is a lost answer. An unknown outcome is
  settled by reading the operation or the cluster list, never by sending the write again, and
  Studio never retries a write by itself.
- IV: a destructive action says what it destroys before it runs. Delete is confirmed by typing
  the cluster's name, and says that its graphs go with it, and how long Undo stays possible. A
  403 is shown as a denial in the organization's words (`scope_missing`); nothing is hidden
  ahead of time.
- V: a cluster deleted or created stays where it was in the list, its state changing, until the
  list is read again.
- VI: each action shows the CLI command that does the same.
- VII: a durable lifecycle change ends in one toast with one follow-up action; a plan that is
  only previewed is silent.
- IX: every request has a time limit and a size cap. An operation is followed by reading it every
  5 s while something shows it, for at most 10 minutes; then Studio says it is still running and
  stops reading.
- XII: each route below is in the control plane's published document, pinned as
  `crates/control_client/openapi/control-plane-0.1.0.json`.

## What exists today (checked)

- `control_client` (spec 021) reads organizations, clusters, a cluster's detail and status, and
  issues data tokens. Its client names the organization of each request
  (`Omnigraph-Organization`).
- The Omnigraph Cloud page of Settings (`crates/studio/src/cloud_settings.rs`) lists each
  organization's clusters, and a cluster's Manage section shows its state and latest delivery
  (`ClusterCard`, read every 30 s while open) and copies the CLI commands for status, history,
  push, plan, apply and delete.
- The control plane's document (checked 2026-11-06) has:
  - `POST /v1/clusters` (`CreateRequest`: `name`, 1–64 characters, and an optional `binding`),
    `202` with the created cluster, "accepted; not yet ready".
  - `POST /v1/clusters/{id}:delete` (`incarnation`, `retention_seconds` 0–2,592,000, default
    86,400) and `POST /v1/clusters/{id}:undo` (`incarnation`, `deletion_id`), each `202`.
  - `GET /v1/operations/{id}`: a lifecycle operation (`create`, `delete` or `undo`) and its state.
  - `GET` and `POST /v1/clusters/{id}/config` (the bound source head; commit configuration
    files), `POST …/plans` and `GET …/plans/{id}` (a preview), `POST …/deployments`,
    `GET …/deployments/{id}`, its `…/events` stream (every 2 s for up to 60 s) and `…:cancel`,
    `GET …/history` and `GET …/lease`.
  - `GET …/members` (the organization's members eligible for the cluster), and the
    organization's members and invitations under `/v1/organization/…`.
  - Each lifecycle and configuration write takes an `Idempotency-Key`.
- The CLI does each of these: `omnigraph cluster create`, `delete`, `undo-delete`, `push`, `plan`,
  `apply`, `cancel`, `history`, `members` and `operation` (`omnigraph cluster --help`, CLI
  `0.14.1-edge+269299b`).

## Design

Each action is on the cluster's Manage section in Settings, and Create cluster is on its
organization's card. The connection chip's card links to the same section.

### Create

"New cluster" on an organization's card asks for a name (1–64 characters) and, optionally, the
repository it is bound to. Create sends `POST /v1/clusters` once, with a new `Idempotency-Key`,
and the organization's header. On `202` the cluster is added to the list as the control plane
answered it ("Provisioning") and its operation is followed. A lost answer reads the cluster list
for a cluster of that name; until it shows, the dialog says the outcome is unknown and offers
Check again, which reads, never resends.

### Delete and Undo

Delete says: "Delete <name>? Its graphs and their history go with it. You can undo this for 24
hours." Retention is one of 0, 1, 7 or 30 days, 24 hours by default. It is confirmed by typing
the cluster's name. `POST …:delete` names the exact `incarnation` shown, so a cluster that was
replaced meanwhile is refused (`409`) rather than deleted. While the deletion is retained, the
cluster's row says "Deleted · Undo until <time>" and offers Undo, which is confirmed with ↵ and
sends `POST …:undo` with the deletion's ID.

### Configuration and deployment

Push takes a folder holding `cluster.yaml`, uploads only the files it references with the
source head Studio read (`expected_revision`), and a message; a head that moved is refused
(`409`) and said. Plan previews a pushed revision; the preview is shown as the control plane
discloses it. Deploy applies that exact plan; its delivery is followed through the events stream
while the section shows, and Cancel is offered only while it is queued and never attempted. The
change lease, when another person holds it, is shown with who and until when.

### Members

The cluster's eligible members are listed, read-only. Inviting, changing a role and offboarding
stay in the CLI and the web console.

### Errors

As spec 021, Errors. A `409` says what changed ("<name> was replaced since you opened it; read
the list again"). A `422` shows the problem's `detail` next to the field it names.

## Decisions

| Chosen | Rejected | Why |
|---|---|---|
| Typing the name confirms Delete | ↵ | Studio cannot put a purged cluster back; within retention Undo can, but the default may be 0 |
| An unknown outcome is read, not resent | Resending with the same `Idempotency-Key` | Constitution III; a replay is the control plane's guarantee, not a reason for Studio to send twice |
| Members read-only | Invitations and roles | They are the organization's, not the cluster's; the web console owns them |
| Actions in Settings' Manage section | A cluster window of its own | It is where the clusters are listed; a cluster page can follow if people manage many |

## Deferred

- Adopting an existing cluster (`POST /v1/clusters/adopt`).
- Organization members, invitations and roles.
- Downloading a revision (`…/config/export`), which the CLI's `cluster pull` does.
- No Omnigraph engine change is needed; nothing here goes in
  `docs/deferred-engine-changes.md`.

### Prerequisites outside Studio

- **CP5, the control plane**: allow Studio's OAuth client the lifecycle and configuration
  permissions. Spec 021's sign-in reads and issues data tokens; each client has a ceiling on the
  permissions it may use (spec 021, CP1), and whether Studio's ceiling includes these writes is
  not yet checked. Until it does, each write here answers `403 scope_missing`, which Studio shows
  as a denial and pairs with the CLI command.

## Open questions

1. Whether Studio's ceiling should include deletion, or only create, push, plan and deploy.
2. Whether Studio should follow a deployment past the events stream's 60 s, by reading the
   deployment, and for how long.

## Validation log

| Date | Claim | How | Result |
|---|---|---|---|
| 2026-11-06 | The lifecycle routes and their bodies are as stated | The pinned `openapi.json`, `paths` and `components.schemas` | `CreateRequest`, `DeleteClusterRequest`, `UndoDeleteClusterRequest` and `LifecycleOperation` as quoted; each write takes `Idempotency-Key` |
| 2026-11-06 | The CLI does each action | `omnigraph cluster --help` and each subcommand's help, CLI `0.14.1-edge+269299b` | `create`, `delete --incarnation`, `undo-delete`, `push --expected-revision --message`, `plan --rev`, `apply --plan`, `cancel`, `history`, `members`, `operation` |

## Corrections

None yet.
