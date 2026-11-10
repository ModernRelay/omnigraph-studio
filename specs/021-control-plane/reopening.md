# 021, amended: a cluster is reopened, and opened from a link

Status: built; not yet checked against the live control plane · 2026-11-10

Amends [the plan](plan.md): Listing clusters, Omnigraph Cloud by default ("Without the CLI"),
and the decision "A cluster without a CLI target opens for this run only".

## Problem

A cluster opened from the control plane's list was open for that run. The next start reopened
only windows whose server a CLI target names, so someone who uses Omnigraph Cloud with no
target for the cluster got back another window, or the Open dialog. In an account with several
organizations that other window was the graph of whichever organization a CLI target happened
to name. `omnigraph studio` refused a managed target, so a cluster could not be opened from a
terminal either. Both made a CLI target the only way to keep a cluster, which "Omnigraph Cloud
by default" set out to remove.

## Principles

- I: the control plane stays the authority on clusters. Studio keeps no cluster list: a saved
  window names the one cluster it had open, and every data token for it is asked for in the
  cluster's organization. A token for another address or another incarnation is dropped.
- V: a window that had a graph open has it at the next start, whether or not a CLI target
  names its cluster.
- VI: the CLI command that makes a cluster a target is still shown, as the way to use the
  cluster from the CLI. Studio needs none.
- IX: a start waits for a control plane's kept sign-in for a bounded time, and a link reads
  one cluster once.
- XII: the validation log below; the live check is still to do.

## What exists today (checked)

- Studio (main at `0211c68`):
  - A window saves each graph as `{target, url, graph, branch}` (`SavedGraph`,
    `crates/studio_workspace/src/persistence.rs`), and a start reopens a graph only on a server
    named `target` at `url` among those the registry holds once the CLI's targets are listed
    (`fill_reopened`, `crates/studio/src/lib.rs`).
  - A cluster no CLI target names becomes a server only through
    `ConnectionRegistry::open_cluster`, from a `ClusterView` the open list answered
    (`crates/graph_store/src/registry.rs`). Nothing saved names it as a cluster.
  - Its data token source (`ControlTokenSource`, `crates/graph_store/src/control_token.rs`)
    drops a token whose `endpoint` is not the cluster's address, and gives no token without the
    plane's sign-in; the connection takes the plane's token when the plane signs in
    (`Connection::take_plane_sign_in`).
- The control plane (source at `ad0655f`, read with its owners' access, and Studio's pinned
  document):
  - `POST /v1/clusters/{id}/tokens` answers, in `meta`, the `cluster_id` and `incarnation` its
    token is for; `Meta` declares both, optional.
  - A hosted cluster's data host is its id and incarnation under the graph domain, so another
    incarnation of a cluster has another address.
  - `GET /v1/clusters/{id}` answers one cluster with its `incarnation`, `name`, `state` and,
    while data access is available, `data_endpoint`.
- The Omnigraph CLI (ModernRelay/omnigraph#1343, from main at `2c4b2f12`):
  `omnigraph studio --target NAME` for a managed cluster target prints and opens the link under
  The link below, from the operator file alone. Before it refused the target
  (`target_kind_unsupported`; RFC #1017 deferred it to its appendix A2, which has shipped).

## Design

### What a window saves

A saved graph gains `cluster`: `{route, organization, cluster_id, incarnation}`, the cluster as
its control plane names it (`ClusterIdentity`). It is written for a graph whose server is a
managed cluster with a known organization: one opened from a control plane, or a CLI target's,
by the target's profile. It is absent otherwise, and in a file an earlier Studio wrote, which
reads as before.

`target` and `url` are saved as before, and what Studio saves of a graph stays filed under
`<target>@<url>/<graph>` (spec 002).

### Reopening

At a start, once the CLI's targets are listed, each saved graph finds its server in this order
(`ConnectionRegistry::connection_for_saved`):

1. The server named `target` at `url`, as before.
2. With a `cluster`: the server that reaches that cluster now, under whatever name: a CLI
   target naming it, or one already opened.
3. With a `cluster` whose control plane Studio holds (Omnigraph Cloud, or one a CLI profile
   names, at the same route): the cluster opened again from what was saved, named `target`, at
   `url`.

A graph that finds none is not reopened, as before: a server target the CLI no longer lists at
that address, and a cluster of a control plane Studio no longer holds.

Nothing is read from the control plane to reopen a window, so it opens at once, as a CLI
target's does, with or without the network. The control plane decides when the first data
token is asked for, in the cluster's organization: it checks the person's membership and
admission and that the cluster exists. Its answer must name the saved address as `endpoint`,
and, where `meta.incarnation` is answered, the saved incarnation. Otherwise the token is
dropped and the connection says why:

| The control plane answered | The connection says |
|---|---|
| A token for another address | As before (`ControlError::EndpointMismatch`) |
| A token for another incarnation | "The control plane's token is for a newer cluster with this id: the one opened here was deleted and made again. Open it again from its organization's clusters" |
| 403, 404 | "The control plane gave no data token:" and its words (plan, Errors) |
| No sign-in held | "Sign in to the organization's control plane to reach this cluster"; it takes the plane's token when the plane signs in |

The incarnation check holds for every managed cluster, a CLI target's included, since the
target pins one.

A control plane reads the sign-in its keychain item kept as it is made, off the main thread.
A start that reopens a window by step 3 waits for that read first, 5 s at most for all of them
(`ControlPlane::when_kept_read`), so the cluster is asked for the plane's token and not, in the
meantime, for a sign-in of its own. Past 5 s the window reopens anyway and takes the token when
the sign-in is held. A start with no such window waits for nothing more than before.

### The link

`omnigraph studio --target NAME`, for a managed cluster target, sends:

```
omnigraph-studio://open?api=<route>&organization=<org>&cluster=<id>&incarnation=<id>[&graph=<id>]
```

- It names no `target`. A link that names one is a server target's (spec 002), whatever else
  it names.
- `api` is the control plane's address: HTTP or HTTPS, with no user, query or fragment. The
  three ids are letters, digits, `_` and `-`, 128 at most. Anything else is refused as it is
  read.
- `graph` is the graph `--graph` named, else the target's `default_graph`. Studio resolves no
  target here, so the link carries the default.
- The control plane must be one Studio holds, with a sign-in held. A link adds no control
  plane and signs in to none.
- A server that reaches the cluster already opens the graph, and the control plane is asked
  nothing. Otherwise Studio reads `GET /v1/clusters/{id}` in the organization, once (10 s,
  2 MiB), and opens the cluster as from the list. The answer's incarnation must be the link's,
  and it must have a data endpoint.
- With no graph named, the first graph known on the server opens; with none known, "Open a
  graph" shows with the server chosen.

| What is wrong | Shown |
|---|---|
| Studio holds no control plane at `api` | "Studio does not know the control plane at <host>. A link never adds one: Omnigraph Cloud's address is in the settings, and another is one a CLI profile names." |
| No sign-in held | "Sign in to <plane> under “Open a graph”, then open the link again; a link never signs in." |
| The control plane refused or did not answer | "<plane> did not answer for the link's cluster:" and its words |
| Another incarnation | "<cluster> was deleted and made again since the link was made. Make the link again with `omnigraph studio`." |
| No data endpoint | "<cluster> has no data endpoint to open yet: it is <state>." |

### Wording

A CLI target is a label a cluster may have, not what keeps it.

| Where | Before | Now |
|---|---|---|
| A listed cluster no target names | "Not a CLI target · Ready" | "Ready" |
| Its row, read aloud | "Open acme for this run. To keep it, add it as a CLI target: …" | "Open acme. To use it from the CLI too, add it as a target: …" |
| Its server in the Open dialog | "from the control plane, for this run" | "from the control plane" |
| Its token's tooltip | "… Opened for this run: to keep it, add it as a CLI target with …" | "… To use this cluster from the CLI too, add it as a target with …" |

### Limits

| What | Limit |
|---|---|
| A start's wait for the control planes' kept sign-ins | 5 s in all |
| A link's wait for its control plane's kept sign-in | 5 s |
| A link's read of its cluster | Once, 10 s, 2 MiB |

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| The window saves the cluster it had open | Register a CLI target from Studio; keep a list of opened clusters | Registering is the CLI's (spec 003). A window naming what it had open is what `session.json` already is for a server; a list would be a second source of clusters beside the control plane |
| Reopen from what was saved, the control plane deciding at the first token | Read the organization's clusters, or the cluster, before reopening | The token request already checks membership, admission and the cluster, and names its address and incarnation. A read first would hold the window on the network and lose it at a start with none |
| Check the incarnation the token's answer names | Rely on the address alone | A hosted address changes with the incarnation; a private one need not. The answer says which incarnation at no cost |
| An answer that names no incarnation is accepted | Refuse it | The pinned document makes it optional, and the address check stands either way |
| A CLI target's window saves its cluster too | Only a cluster opened from a control plane | A target renamed, removed, or read from another operator home then still reopens, through the control plane |
| A link that names the cluster, with no digests | A target's name with digests, as a server target's link | A name must resolve in Studio's operator home, which is what kept a cluster tied to the CLI. The cluster's identity resolves through the control plane Studio already holds |
| Saved state stays filed by name and address | File a managed cluster's by its identity | It would orphan what is saved for every managed target today; Deferred |

## Deferred

- Filing what Studio saves of a managed cluster's graphs by the cluster's identity, so a
  rename keeps tabs and drafts.
- Renaming a reopened server to the cluster's name when the control plane lists it under
  another.
- Saying why a window was not reopened when its control plane is no longer held.
- Opening links on Linux and Windows, as for every link (spec 002).
- No Omnigraph engine change is needed; nothing here goes in
  `docs/deferred-engine-changes.md`.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-11-10 | A token's answer names its cluster and incarnation | The control plane's source at `ad0655f`, read with its owners' access; `Meta` in the pinned document | The handler answers `meta` for the cluster and its incarnation; both fields are declared, optional |
| 2026-11-10 | A saved cluster reopens with no CLI target, and a server target does not | `cargo test -p graph_store a_saved_cluster_reopens_without_a_cli_target` | Opened under the saved name at the saved address with the control plane as its source; found again by name and by cluster; another incarnation is another server; nothing for a control plane Studio does not hold, nor for a graph with no cluster |
| 2026-11-10 | A CLI target's window names its cluster and finds it under a new name | `cargo test -p graph_store a_saved_cluster_finds_the_cli_target_naming_it` | The target's cluster is its profile's route and organization with its id and incarnation; renamed, the old name finds it only through the cluster |
| 2026-11-10 | A token for another incarnation is dropped | `cargo test -p graph_store a_data_token_is_asked_for_in_the_targets_organization`, whose fake control plane names the incarnation as production does | A source expecting another incarnation gets no token and the reason; the check removed, it gets the token and the test fails |
| 2026-11-10 | The read of a kept sign-in can be waited for | `cargo test -p graph_store the_read_of_a_kept_sign_in_can_be_waited_for` | The wait ends with the sign-in held; at once when the read is over, with nothing kept, and for a plane that cannot sign in |
| 2026-11-10 | A start reopens a window on a cluster and saves it the same again | `cargo test -p studio a_window_on_a_cluster_reopens_without_a_cli_target` | The graph reopened on the cluster's server; the window's next save equals the one read; a window on another control plane's cluster closed |
| 2026-11-10 | `session.json` reads with and without `cluster` | `cargo test -p studio_workspace saved_windows_read_back_and_a_file_without_them_is_none` | A graph with a cluster reads back; one from an earlier file reads as before and writes no `cluster` |
| 2026-11-10 | A managed target's link is read, and refused when malformed | `cargo test -p studio an_open_link_names_a_managed_cluster` | The CLI's own test link reads, its address percent-encoded; a missing part, an id with a path or header byte, and an address with a user or query are refused; with a `target` it is a server target's link |
| 2026-11-10 | A link opens its cluster, or says why not | `cargo test -p studio a_link_from_the_cli_opens_a_managed_cluster` | Refused for a control plane not held and with no sign-in; another incarnation, no data endpoint and a failed read each say so and open nothing; a matching answer opens the server; then the link opens the graph and asks the control plane nothing |
| 2026-11-10 | The CLI sends this link for a managed target | In ModernRelay/omnigraph#1343, `cargo test -p omnigraph-cli studio_opens_a_managed_cluster_target_by_its_identity`, by reading | The link names `api`, `organization`, `cluster`, `incarnation` and the graph, built from the operator file alone; Studio's parser test reads that test's link |

Not checked yet: a real start of Studio against Omnigraph Cloud (a window on a cluster no CLI
target names, quit and started again), and `omnigraph studio --target` on a managed target
opening the installed Studio. Both need the built app and a person at the keychain prompt.

## Corrections

None yet.
