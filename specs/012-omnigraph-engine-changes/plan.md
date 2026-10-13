# 012: OmniGraph engine changes

Status: deferred · 2026-10-11. Nothing here is built now; Studio ships against the 0.13
contract (constitution X). When a change is picked up it becomes an OmniGraph RFC and issue in
`ModernRelay/omnigraph`, follows that repository's invariants and test ownership, and Studio
adopts it only when a server's OpenAPI document advertises it.

## Problem

Studio's surfaces are designed to work on OmniGraph 0.13 as released. Some of them would be
simpler, faster or more correct with engine changes: a one-command launch, a real branch diff, a
merge preview, typed result columns, paged history. This plan records each change once, with
its shape, the engine rules it must respect, the Studio spec waiting on it, and what Studio does
until then. `docs/deferred-engine-changes.md` is the short index of the same list.

## Principles

X (the engine moves at its own pace), I (one HTTP path in every mode), IV (policy enforced by
the engine), XII (each change carries its evidence into the OmniGraph RFC).

## What exists today (checked)

OmniGraph `v0.13.0` unless noted; line numbers from the research at main `680fdfef`.

- The server binary boots clusters only (`crates/omnigraph-server/src/lib.rs:192-197`). The
  library can serve one standalone graph: `AppState::open`, `open_with_bearer_tokens` or
  `new_single(uri, db, tokens, policy, workload)` (`lib.rs:515-680`), mounted at
  `/graphs/default/…` (`lib.rs:739-744`). These open read-write; there is no read-only serving
  mode. They are public but not documented as a supported embedding API.
- The CLI already links `omnigraph-server` (`crates/omnigraph-cli/Cargo.toml`), so a launcher
  adds little to the binary. The installed 0.13 CLI is 227 MB and macOS release builds need
  `-C code-model=large` (`.github/workflows/release.yml:59-62`).
- Tokens without a policy allow reads only (`handlers.rs:661`); `graph_list` is denied without a
  cluster policy; `--unauthenticated` records no actor.
- The server has no `Host`, `Origin` or CORS handling.
- One mutation-capable writer process per cluster, through `__cluster/lock.json`, retained after
  a write command (`crates/omnigraph-cluster/src/admission.rs`); standalone graphs get no lock;
  read-only open beside a live writer is supported (`GraphReadAuthority` + `open_read_only`).
- HTTP gaps: `GET /branches` returns names only (`handlers.rs:2561-2566`); `GET /commits` has no
  limit or cursor (`crates/omnigraph-api-types/src/lib.rs:1503-1510`); `ReadOutput.columns` holds
  names only; no diff between refs, merge base, merge preview, query profile, index status,
  schema plan for standalone graphs, or identity introspection over HTTP.
- Engine capabilities already present but not served: `diff_between` and `diff_commits` (ids and
  ops, unbounded), `query_inspected` (`#[doc(hidden)]`), `plan_schema`, `list_commits`.
- On OmniGraph main, not in 0.13.0: Blob `PUT`/`DELETE`, commit publish and drop, merge
  `as_commit` (unpublished commits).

## The changes

Each change names the Studio spec that waits on it and Studio's behavior until then.

### Launch and local graphs

**E1. `omnigraph studio` CLI command.** Filed with E4 as
[ModernRelay/omnigraph#980](https://github.com/ModernRelay/omnigraph/issues/980).
Resolve the target the way other commands do (`--store`, `--cluster --graph`, `--server
--graph`, profile). For a local target, serve it in-process on loopback (E2); for a server
target, pass the server through. Mint a per-launch token tied to `operator.actor` and a
generated policy granting that actor the graph actions. Open Studio with a single-use code
through `omnigraph-studio://connect?code=…`; Studio redeems it once at the loopback server for
its token. Print the URL; `--no-open --json` for agents and SSH. Never put a token in argv.
*Waits:* 003. *Today:* Studio connects to running servers from the CLI's config.

**E2. A supported local embedding API in `omnigraph-server`.**
Bless one constructor (for example `AppState::local(uri, LocalOptions)`) instead of the
test-shaped helpers, with read-only and read-write modes. Read-only must refuse mutating routes
at the router, because a read-only handle does not refuse writes itself.
*Waits:* E1. *Today:* not needed.

**E3. Loopback guard in the launcher, not the server.**
Bind `127.0.0.1` on port 0; refuse any `Host` other than `127.0.0.1:PORT`, `localhost:PORT`,
`[::1]:PORT`; refuse a present `Origin` that differs; send no CORS headers. This stays in the
CLI so deployed servers never gain a browser mode by accident. Never `--unauthenticated`.
*Waits:* E1. *Today:* not needed for a native app talking to servers the person runs.

**E4. Owner discovery for a graph root.**
A file under `$OMNIGRAPH_HOME` naming the process, port and session that owns a graph root, so
a second `omnigraph studio` attaches instead of opening a second writer, and CLI writes can route
through it. Discovery only: publication safety stays with the manifest version CAS (OmniGraph
invariants deny process-local locks presented as fencing).
*Waits:* E1. *Today:* not applicable.

**E5. Writer-safety rules for local sessions.**
Default to read-only; never take cluster admission from Studio; refuse writes when no actor is
set; never offer maintenance or schema apply against a live cluster graph; never replay a write
whose outcome is unknown.
*Waits:* E1. *Today:* enforced by the server Studio talks to.

### Read routes

Each is a read on one captured snapshot behind the existing `read` gate unless noted. No new
mutating route is needed: edits, loads, merges, branch and commit control already exist.

| ID | Route | Engine source | Invariant notes | Waits | Studio today |
|---|---|---|---|---|---|
| E6 | `GET /graphs/{id}/diff?from=&to=&kind=&type=&op=&page_token=&limit=` → paged entity changes with optional before/after images | `diff_between`, plus a paged variant with images | Both ends resolve once; page tokens pin commit ids, never branch names; bounded pages | 008 | Fold the change feed from the fork, capped at 20,000 changes, "Partial" when capped |
| E7 | `GET /graphs/{id}/merge-base?a=&b=` | Commit lineage (`generation` on graph commits) | Pure read of `__manifest` lineage | 008 | Walk `GET /commits` parents |
| E8 | `POST /graphs/{id}/branches/merge/preview {source, target}` → outcome, merge base, conflicts, change summary | A merge-planning step that stops before staging | Must stage nothing durable, not even a detached commit; `read` on both branches. If the engine can only answer by staging, keep `as_commit` as the preview | 008 | Merge on request; a 409 lists conflicts and nothing moved |
| E9 | `GET /graphs/{id}/branches?detail=true` → name, head, fork commit, creator, created and updated times | Branch refs and lineage | Derived from `__manifest`; no shadow branch table | 002, 008 | Derive from `GET /commits?branch=` |
| E10 | `GET /graphs/{id}/commits?branch=&limit=&before=` | Bounded lineage walk, keyset on generation and id | Cost scales with the page, not history | 009 | Load the full list; warn above 5,000 commits |
| E11 | Typed columns in `ReadOutput` (`columns: [{name, type, nullable}]`) or Arrow IPC (RFC 0051) | The result's Arrow schema | Additive DTO field; regenerate `openapi.json` | 004, 007 | `describe_query_operation` in `omnigraph-compiler` |
| E12 | `POST /graphs/{id}/query?profile=true` | `query_inspected`, made a supported API | Exposes plan internals; gate on `read` or a new `inspect` action | 007 | Plans only, through `explain query …` |
| E13 | `GET /graphs/{id}/indexes` | RFC 0046 index status | Observes only, never builds | 010 | Index facts from `explain` statistics |
| E14 | `POST /graphs/{id}/schema/plan {schema_source}` → migration plan | `plan_schema` | Read-only; apply stays a deployment on clusters and direct-only on standalone graphs | later | Schema changes stay with `omnigraph cluster apply` |
| E15 | `GET /whoami` → actor and permitted actions per graph and branch scope | Policy evaluation for the resolved token | Never lets a client assert identity | 002 | Show every action; a 403 is a denial and the draft stays |
| E16 | `GET /graphs/{id}/nodes/{type}/{key}/neighborhood` → count per edge type and direction | Graph index degree (the source of filter-free `count { }` blocks) | Pinned to a snapshot; bounded by the schema's edge types | 005, 006 | One count query per edge type, concurrent, cached for the pinned commit |

### Language tooling

**E17. `omnigraph lsp`.** A language server over stdio on `omnigraph-compiler`: diagnostics with
suggested edits, completion, hover, go to definition into the schema. One service for Studio,
Zed and other editors. *Waits:* 007 (optional). *Today:* `gq_language` calls the compiler
in-process.

**E18. Tree-sitter grammars in the OmniGraph repository.** `tree-sitter-gq` and
`tree-sitter-pg` move to the language's owner, with a Zed extension for `.gq` and `.pg` beside
them. *Waits:* E17. *Today:* `grammars/` in this repository.

**E19. Publish `omnigraph-compiler` to crates.io again.** crates.io stops at 0.8.0; the repo is
at 0.13. *Waits:* 001. *Today:* git dependency on tag `v0.13.0`.

### Agent identity

**E20. Commits made on behalf of an agent.** Filed as delegated, branch-scoped credentials,
[ModernRelay/omnigraph#978](https://github.com/ModernRelay/omnigraph/issues/978) (RFC needed):
the credential also limits the agent's writes to its own branches, which spec 013's gateway does
on Studio's side until then. Spec 011 runs agent writes through the person's
token, so the commit records the person's actor. Recording an agent beside the actor (for
example an `on_behalf_of` field set by the server from a token the person scoped for the agent)
would let history and review say which changes an agent made. Engine rule: the server resolves
both identities from bearer tokens at the boundary (invariant 10); a client never asserts the
agent identity. Studio today: agent writes go only to a branch named `agent/<stamp>`, so the
branch name says who made them.

### The agent's CLI (spec 013)

**E22. A refused write is reported as not started.**
[ModernRelay/omnigraph#976](https://github.com/ModernRelay/omnigraph/issues/976). After a write
is sent, the 0.13 CLI reports every failure except a typed 429 and a verified 412 as `effects:
unknown, action: reconcile`, including 401, 403 and the contract 400
(`crates/omnigraph-cli/src/command_outcome.rs:165-258` at `d8d2e3a8`). These refusals happen
before effects. Engine rule: confirm each code is produced only before staging. *Waits:* 013.
*Today:* `AGENTS.md` tells the agent that a Studio refusal wrote nothing.

**E23. `omnigraph version --json` with the HTTP contract.**
[ModernRelay/omnigraph#977](https://github.com/ModernRelay/omnigraph/issues/977). No command
reports the contract a binary speaks (`main.rs:152-158, 612-620`; `cli.rs:428`). *Waits:* 013.
*Today:* Studio tests each installed CLI once against the gateway.

**E24. Unix-socket transport for `--server`.**
[ModernRelay/omnigraph#979](https://github.com/ModernRelay/omnigraph/issues/979). The CLI speaks
only http(s), and a `unix://` value falls through to storage resolution. Codex's sandbox can
allow one Unix socket but blocks loopback TCP. *Waits:* 013. *Today:* Codex reaches the gateway
only when the person gives the conversation network access.

### Branching

**E21. A branch from a commit.** `BranchCreateRequest` at 0.13.0 takes `name` and `from`, a
parent branch whose head the new branch forks ("Parent branch to fork from. Defaults to
`main`", `openapi.json`). Forking at a commit of a branch's history (`from_commit`) would let
History start a branch where a change was made, to try a fix or revert there. Engine rule: the
new branch's first manifest names that commit's table pins and lineage, published once
(invariant 2); a commit that is no longer retained is refused, not approximated. *Waits:* 008,
009. *Today:* History opens a read-only query or table at a commit; a branch starts from a
branch head.

### Already on OmniGraph main, not in 0.13.0

**R1. Blob writes** (`PUT`/`DELETE /graphs/{id}/blob`). Studio offers Blob upload and clear only
when the server advertises `cluster_putBlob` and `cluster_clearBlob`. *Spec:* 004.

**R2. Unpublished commits** (`as_commit` on mutations and merges, `POST
/commits/{id}/publish`, `DELETE /commits/{id}`). The planned way to stage edits as a reviewable
proposal and to preview a merge. Used only when the server advertises
`cluster_publishCommit`. *Specs:* 004, 008.

## Order if picked up

1. E22 refused-write outcomes and E23 `version --json`: small CLI changes the agent's CLI needs
   (spec 013); started 2026-10-13.
2. E11 typed columns and E10 paged commits: small, additive, used everywhere.
3. E9 branch details, E7 merge base, E6 diff: the review surface.
4. E1–E5 launcher: the one-command local experience.
5. E20 delegated credentials (an RFC) and E24 Unix sockets, which retire spec 013's gateway
   rules and let sandboxed agents in.
6. E8 merge preview, E16 neighbourhood counts, E15 whoami.
7. E12, E13, E14, E17, E18, E19 as the matching Studio work starts.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Defer every engine change | Change the engine alongside Studio | Ragnor, 2026-10-11: build Studio on the released contract first |
| Routes added to the server proper | A Studio-only side channel | Served deployments and Tower gain them too; one access path |
| Loopback guard in the launcher | In `omnigraph-server` | Deployed servers must not gain a browser mode by accident |

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Route list at 0.13.0 and main | parsed `openapi.json` at `v0.13.0` and main | 0.13.0 lacks Blob PUT/DELETE and commit publish/drop |
| 2026-10-11 | `BranchMergeRequest` at 0.13.0 | parsed `openapi.json` at `v0.13.0` | `source`, `target`, `delete_branch`, `settings`; no `as_commit` |
| 2026-10-11 | `CommitOutput` at 0.13.0 | parsed `openapi.json` at `v0.13.0` | No `published` field; `GET /commits` takes `branch` only |
| 2026-10-11 | Server internals cited above | Studio integration research at OmniGraph main `680fdfef` | Re-check every line reference against the release the RFC targets |

## Corrections

None yet.
