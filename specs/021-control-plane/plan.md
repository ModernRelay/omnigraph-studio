# 021: Studio speaks to the managed control plane

Status: draft; waits on the control plane accepting Studio's sign-in (CP1) · 2026-11-03

## Problem

Studio reaches a managed cluster only through the Omnigraph CLI. It knows the clusters the CLI
has registered as targets and nothing else: not the other clusters in the person's
organization, not whether a cluster is ready or still provisioning, not whether its last
deployment converged. Where a cluster offers no sign-in of its own, Studio's data token is the
one the CLI exchanges from `omnigraph login` (spec 003), so a managed cluster needs a CLI login
that is current, waits on the CLI's lock, and does not work on Windows, where the CLI exports no
token. The control plane answers all of this over HTTP; Studio does not ask it.

## Principles

- I: the control plane is the authority on clusters, as the server is on graphs. Studio keeps
  no cluster list of its own and shows what the control plane answered, with its `meta`
  (`stale`, `observed_at`). A second crate making outside requests amends I (below).
- III: the first slice makes no control-plane write. Issuing a data token changes no cluster
  and no graph; it is the one `POST` Studio sends, and it is never retried on its own.
- IV: Studio reflects the control plane's permissions and enforces none: a 403 is shown as a
  denial in the organization's words (`scope_missing`), never hidden ahead of time.
- VI: every cluster action the CLI has and Studio does not (`cluster push`, `plan`, `apply`,
  `delete`) shows its CLI command.
- VII: listing clusters and reading their state is silent; a browser opens only on Sign in.
- IX: every request has a time limit and a size cap, and the status of a cluster is read only
  while something shows it.
- XII: every claim below is checked against the control plane's live service and its
  published contract. Its source is private: what it must change is described here, and the
  specifics are tracked in the control plane's own issue tracker.

## What exists today (checked)

- Studio (main at `31a3d9a6`):
  - Managed targets come from `omnigraph target list --json` (`ManagedTarget`: name, profile,
    `cluster_id`, `cluster_incarnation`, optional data endpoint;
    `crates/graph_store/src/cli_config.rs:155`). For each profile Studio runs
    `credentials status --profile NAME --json`, which reads no secret and returns
    `owner.organization_id` and `owner.route`, the control plane's origin
    (`{"kind":"profile","route":"https://platform.omnigraph.dev","organization_id":"org_…"}`).
  - A managed cluster whose 401 offers a sign-in is signed in to through Studio's own OAuth
    client (spec 016), in the profile's organization; one whose 401 offers none is reached with
    the token `credentials export` returns, which the CLI gets from the control plane
    (`CliCredential::Managed`, `cli_tokens.rs:781`).
  - Studio's OAuth client (`oauth_client`) does PKCE with a loopback redirect, keeps its refresh
    token in the OS keychain, and identifies itself with its Client ID Metadata Document,
    `https://www.omnigraph.dev/oauth/studio-client-metadata.json` (spec 016).
  - Constitution I gives one crate each set of outside requests: `graph_client` to Omnigraph
    servers, `studio_update` to Studio's releases, `model` to OpenRouter. None calls a control
    plane.
- The managed control plane (live at `https://platform.omnigraph.dev`, checked 2026-11-03):
  - Its contract is the OpenAPI document it serves at `/openapi.json`, version `0.1.0`. Every 2xx is `{data, meta}` with `meta.provenance`, `assurance`, `stale` and
    `observed_at`; every refusal is RFC 9457 `{type, title, status, detail?}`.
  - Public, unauthenticated: `GET /healthz` (`ok`), `GET /v1/auth/config` (`provider:
    workos_authkit`, the application's `client_id`, `issuer`, authorization, device and token
    endpoints) and `GET /v1/auth/browser` (the same, plus the web console's settings). Checked
    live: all three answer 200, and `GET /v1/clusters` without a bearer answers
    `401 unauthenticated`.
  - Every `/v1` call takes `Authorization: Bearer` with a human access token from its own
    sign-in (the one the CLI uses), in an organization the person is a current member of. It
    accepts only tokens issued to its own application, so **a token issued to Studio's client
    today is refused** (CP1). A graph data token is refused too.
  - `GET /v1/clusters` lists the clusters of the token's organization that the caller may see
    the status of. Each is a `ClusterView`: `cluster_id`, `incarnation`, `name`, `state`
    (`provisioning`, `ready`, …), `created_at`, `created_by`, `binding`, `cell`, and, only while
    data access is available, `data_endpoint` and `mcp_url` (`{data_endpoint}/mcp`, the
    cluster's OAuth resource).
  - `GET /v1/clusters/{id}` adds `service_observation`; `GET /v1/clusters/{id}/status` returns
    `source`, `requested`, `delivery` (the latest deployment) and `observed`, which the
    document leaves untyped.
  - `POST /v1/clusters/{id}/tokens` with `{"version": 2, "ttl_seconds": 60–86400}` (default
    3600) returns `{version, token, endpoint, expires_at, actor, kid}`. It needs the person's
    explicit admission to the cluster, refuses an `Idempotency-Key`, and caps its body at
    64 KiB. This is the exchange the CLI makes for Studio today.
  - The rest is configuration, deployments (plans, delivery, a 60 s event stream, cancel,
    history, the change lease), cluster lifecycle (create, adopt, delete, undo) and the
    organization's members and invitations.
- The Omnigraph CLI (`0.14.1-edge+269299b`): `omnigraph cluster list --json` lists a profile's
  clusters through the same API, and `cluster status`, `history`, `members`, `push`, `plan`,
  `apply` and `delete` cover the rest. Run without a current login it answers
  `refresh_outcome_unknown` ("run login --target NAME explicitly to recover"), as it did here.

## Design

### Where the control plane is

Studio reads no operator file (spec 003). A control plane is the `owner.route` that
`credentials status` reports for a CLI profile, with that profile's `organization_id`. Each
distinct pair of route and organization is one `ControlPlane`; two profiles on the same pair
share it. The route must be HTTPS (HTTP only on loopback), and Studio sends nothing to it until
its sign-in is held.

### Signing in

The control plane is signed in to as a cluster is (spec 016), with these differences:

- The authorization server comes from the control plane's protected-resource metadata
  (CP1.1), as a cluster's does (spec 016: 10 s, at most 64 KiB, no redirect followed), not from
  `/v1/auth/config`: that names the CLI's sign-in (a user-management issuer with no
  metadata-document support), not the one Studio can use.
- The client is Studio's metadata document, never the `client_id` the config names: that ID is
  the control plane's own application, and using it would make Studio pass for the CLI
  (Decisions).
- The token is requested for the control plane's resource (`resource=<route>`, RFC 8707), and
  sent only to the route; a cluster's token is never sent there, nor this one to a cluster.
- The token's `org_id` must be the profile's organization; a token for another is refused and
  said ("Signed in to org_… · this profile is in org_…"), as in spec 016.
- The refresh token goes to the keychain under `omnigraph-control:<route>#<organization>`,
  with spec 016's custody rules (replace, sign out, revoke where a revocation endpoint is
  published; AuthKit publishes none).
- **Sign in** is on the profile's row in the Open dialog and on a managed connection's menu.
  Before it is held, the profile shows its CLI targets as today, and its row says "Sign in to
  list this organization's clusters".

### Listing clusters

With a sign-in held, opening the Open dialog reads `GET /v1/clusters` for each control plane
(10 s, at most 2 MiB, once per opening and on Refresh; never on a timer). Under the profile:

| Cluster | Row |
|---|---|
| A CLI target names it (same `cluster_id` and `incarnation`) | The target's row, as today, with the cluster's `state` |
| No CLI target names it, and it has a `data_endpoint` | The cluster's `name`, "Not a CLI target"; opening it connects for this run only and shows `omnigraph target add` for it (VI) |
| No `data_endpoint` (provisioning, or data access off) | Dimmed, with its `state`: "Provisioning", or "No data endpoint" |
| A CLI target names a cluster the list does not have | The target's row, marked "Not in org_… on the control plane" |

Studio keeps no list: the answer is held while the dialog is open and dropped after.

### A cluster's state

A connection to a managed cluster shows its `state` in the connection chip's hover card, and,
while the card is open, reads `GET /v1/clusters/{id}/status` once and every 30 s, showing the
latest delivery's outcome and `meta.observed_at` ("Deployed 12 min ago · converged"). A
`meta.stale` answer says "as of <observed_at>". `observed` is not shown until its shape is
typed (Deferred).

### The data token

For a cluster whose 401 offers no sign-in, Studio asks the control plane for the data token in
place of `credentials export`:

- `POST /v1/clusters/{id}/tokens` with `{"version": 2, "ttl_seconds": 900}`, 15 s, at most
  64 KiB. The token is kept in memory only and sent only to the returned `endpoint`, which must
  equal the cluster's `data_endpoint`; any other is refused and the token dropped.
- It is asked for again 60 s before `expires_at`, and on a 401 from the cluster once per
  minute at most. A request whose answer is lost is not re-sent on its own; the next read that
  needs the token asks again.
- Without a control-plane sign-in, the CLI's export stays the token source (spec 003).
- This makes managed clusters usable on Windows, where the CLI exports no token.

A cluster that offers its own sign-in keeps it (spec 016): a token from the control plane is
not preferred over one the cluster's own sign-in gives.

### Errors

| Answer | Shown |
|---|---|
| 401 (`unauthenticated`, an expired or refused token) | The control plane's row: "Sign in needed"; one silent refresh first |
| 403 `scope_missing` | "Your role in <organization> does not allow this", with the problem's `detail` |
| 404 on a cluster | "This cluster is no longer in <organization>" |
| 503, 5xx, no answer | "Control plane unreachable · Retry"; graph reads to a cluster whose token is held go on |
| A 2xx that is not `{data, meta}` | "The control plane answered in a form Studio does not read" |

### The crate

`control_client`, layer 2, no `gpui`, beside `graph_client`: DTOs for the routes above, the
`{data, meta}` envelope and RFC 9457 refusals, through Zed's `HttpClient`. Its contract tests
check every DTO against a pinned copy of the document it serves, under
`crates/control_client/openapi/`, as `graph_client`'s do. `graph_store`
holds one `ControlPlane` entity per route and organization: its sign-in, the clusters' answers
while shown, and the data tokens.

Building it amends constitution I (a minor version): "`control_client` alone calls a managed
control plane, with a control-plane token, never a graph token; a data token it is given goes
only to the endpoint it names." `docs/architecture.md`'s layer table gains the crate, and
`script/check-layers.py` its row.

### Limits

| What | Limit |
|---|---|
| Auth config and metadata | 10 s, 64 KiB |
| Cluster list | 10 s, 2 MiB, once per Open dialog and on Refresh |
| Cluster status | 10 s, 2 MiB, every 30 s while its card is open |
| Data token | 15 s, 64 KiB, TTL 900 s, renewed 60 s before expiry |
| Token after a cluster's 401 | Once a minute at most |

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Studio signs in to the control plane as its own client | Sign in with the `client_id` the control plane publishes, as the CLI does | The control plane could not tell Studio from the CLI, the same objection that made Studio its own OAuth client for clusters (spec 016; RFC #1017, A2) |
| Call the API from Studio | Run `omnigraph cluster list --json` and its siblings | The CLI holds its operator lock for each command, needs a login it can renew without a terminal (it answered `refresh_outcome_unknown` here), and exports no token on Windows |
| The control plane found through `credentials status`'s `route` | Parse `~/.omnigraph/config.yaml`; a setting with the URL | Spec 003: Studio reads no operator file. The profile already names its route and organization, without a secret |
| A cluster without a CLI target opens for this run only | Register it as a target from Studio | Studio keeps no server list (spec 003); registering is the CLI's, and Studio shows its command |
| Data tokens of 900 s | The 3600 s default, or 86400 s like a cached `cluster token` | Removing admission does not retract a data token; the shorter it lives, the sooner a removal holds |
| Read only, plus the data token, in the first slice | Deployments, members and cluster lifecycle | Each is a write with its own outcome rules, and the CLI already does them; reading first shows whether people want them here |

## Deferred

- Deployment history, plans and the delivery event stream (`/history`, `/plans`,
  `/deployments/{id}/events`) as a view.
- Cluster create, adopt, delete and undo; configuration push; members and invitations.
- `observed` in a cluster's status, until the document types it.
- Managed agents reaching a cluster's `mcp_url` through Studio's sign-in (spec 016, A3).
- No Omnigraph engine change is needed; nothing here goes in
  `docs/deferred-engine-changes.md`.

### Prerequisites outside Studio

- **CP1, the control plane**: accept a standard OAuth access token from Studio on its own API.
  Today the API accepts only tokens from its own sign-in, issued to its own application; the
  OAuth sign-in Studio uses (spec 016, the same identity provider's OAuth server) is accepted
  only by clusters. Until CP1, Studio's control-plane sign-in is refused, and nothing in this
  plan turns on. What it needs:
  1. **A resource for the API.** The API is an OAuth resource of its own,
     `https://platform.omnigraph.dev`, registered with the identity provider (not as its
     default resource). It serves RFC 9728 metadata at `/.well-known/oauth-protected-resource`
     naming that resource and its authorization server, and answers a 401 with
     `WWW-Authenticate: Bearer resource_metadata="…"`. A token for it carries that `aud`.
     Leaving `resource` out is not a substitute: such a token's audience is the provider
     environment's own, shared by every client that leaves it out.
  2. **A verifier for these tokens.** Its own credential profile, chosen before any claim is
     trusted: the OAuth server's issuer, `aud` exactly the API's resource, `client_id` in a named
     list of accepted clients (Studio's first), each with a ceiling on the permissions it may
     use, and no actor or impersonation claims. A token's `sid` here names a consent, not a
     session, so logout refuses such a token and Studio signs out locally.
  3. **The same authority.** Membership of the token's organization checked on every request
     and service permissions read as for the CLI's sign-in. A cluster's token (whose `aud` is
     its `…/mcp`) and a graph token stay refused, which (1) and (2) give without more.

  The OAuth server's issuer is one pinned setting, the one clusters are already given, so the
  API and its clusters cannot trust different issuers. Studio pins nothing: it reads the issuer
  from the API's resource metadata. The setting is checked before the API serves (a canonical
  HTTPS origin, discovery that names exactly it, and the provider's keys), and changing it has a
  procedure: the token issuer follows the provider's custom domain, so a new domain refuses
  every token from the old one at once. The API and each cluster accept both issuers until the
  old one's last access token has expired (300 s), then drop the old one.
- **CP2, the control plane**: say whether its `openapi.json` is a stable contract Studio may pin
  (it is `0.1.0`), and how a breaking change is announced.

## Open questions

1. Which `client_id` claim AuthKit gives a token issued to a metadata-document client: the
   document's URL, or an ID AuthKit assigns (`client_…`, as for a registered application).
   WorkOS documents only the registered case; Omnigraph's server tests assume the URL. CP1's
   list must name the one a real sign-in shows.
2. Settled (2026-11-03): the OAuth server signs with the same key as the control plane's own
   sign-in.
3. How a person in several organizations signs in to the profile's (spec 016, question 4).
4. Whether the list should show clusters the person may see but not open (no admission): the
   control plane lists them; `POST /tokens` then answers 403.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-03 | The control plane's routes, envelope, refusals and schemas are as stated | Its OpenAPI document, version `0.1.0` | 39 routes; `ClusterView`, `DataCredential`, `IdentityRequest`, `Meta` and `ProblemDetails` as quoted |
| 2026-11-03 | Its auth discovery is public and `/v1` needs a bearer | `curl https://platform.omnigraph.dev/{healthz,v1/auth/config,v1/auth/browser,v1/clusters}` | 200 `ok`; 200 with `provider: workos_authkit`; 200; 401 `unauthenticated` |
| 2026-11-03 | The control plane refuses a token issued to another client, and does not take the OAuth server's tokens | The control plane's source, read with its owners' access | As stated; the specifics are in its issue tracker |
| 2026-11-03 | A profile names its control plane and organization without a secret | `omnigraph credentials status --profile NAME --json` | `owner.route: https://platform.omnigraph.dev`, `owner.organization_id: org_…` |
| 2026-11-03 | The API's issuer is not the one Studio signs in with, and both sign with one key | `curl` of `/v1/auth/browser`, of each issuer's discovery document and of each key set | Different issuers; only the OAuth server supports metadata-document clients; one shared key |
| 2026-11-03 | Production signs Studio in without registration, and has no default resource | The identity provider's application and resource listings, read-only; the OAuth server's `/.well-known/oauth-authorization-server` | No registered applications; each resource is a cluster's `…/mcp`, none the default; `client_id_metadata_document_supported: true` |
| 2026-11-03 | Without `resource`, a token's `aud` is the environment's; an OAuth token's `sid` is a consent | WorkOS docs, `authkit/mcp` ("Set a default Resource Indicator") and `reference/workos-connect/token` | As stated |
| 2026-11-03 | Clusters are given the OAuth server's issuer | `curl` of production clusters' `/.well-known/oauth-protected-resource/mcp` | Each names `authorization_servers: ["https://login.platform.omnigraph.dev"]` |
| 2026-11-03 | The CLI lists a profile's clusters, and needs a current login to | `omnigraph cluster list --profile NAME --json` | `refresh_outcome_unknown`: the login could not be renewed without a terminal |

## Corrections

None yet.
