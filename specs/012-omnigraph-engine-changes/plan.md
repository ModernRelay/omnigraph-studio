# 012: Omnigraph engine changes

Status: open · 2026-10-11, updated 2026-10-21. Landed on Omnigraph main: E6, E7, E8, E23 and E24
by 2026-10-15, then E10, E15 (as the access route), E21 and E22 by 2026-10-20 (see "On Omnigraph
main, 2026-10-20"). Studio's oldest supported server is the edge build `edge-20261020-7fd9cc9`,
the first that speaks HTTP contract 0.14, until 0.14 is released (`edge-20261018-0a62bb7`, then
`edge-20261017-d883c49`, before; both speak `0.13`, which Studio now refuses) (constitution X). When a change is picked up it becomes an Omnigraph RFC and issue
in `ModernRelay/omnigraph`, follows that repository's invariants and test ownership, and Studio
adopts it once its minimum server serves it, or before then only when a server's OpenAPI
document advertises it.

## Problem

Studio's surfaces are designed to work on Omnigraph 0.13 as released. Some of them would be
simpler, faster or more correct with engine changes: a one-command launch, a real branch diff, a
merge preview, typed result columns, paged history. This plan records each change once, with
its shape, the engine rules it must respect, the Studio spec waiting on it, and what Studio does
until then. `docs/deferred-engine-changes.md` is the short index of the same list.

## Principles

X (the engine moves at its own pace), I (one HTTP path in every mode), IV (policy enforced by
the engine), XII (each change carries its evidence into the Omnigraph RFC).

## What exists today (checked)

Omnigraph `v0.13.0` unless noted; line numbers from the research at main `680fdfef`.

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
- On Omnigraph main, not in 0.13.0: Blob `PUT`/`DELETE`, commit publish and drop, merge
  `as_commit` (unpublished commits).

## On Omnigraph main, 2026-10-15

Checked at main `fe69c039` (server 0.14.0, unreleased; latest release v0.13.0) by diffing its
`openapi.json` with v0.13.0's. The contract header is still `0.13` and every change is
additive, so Studio works against a 0.14 server unchanged; `graph_client` ignores fields it does
not know.

- Landed: E6 (`GET /diff?from=&to=` between branches or commits, `GET /branches/diff?source=&target=`
  since the merge base; paged, pinned to the commits of the first page, `before`/`after`
  images, `kind`/`type`/`op` filters), E7 (`GET /branches/merge-base`), E8 (`dry_run` on
  merge: classification and validation only, "nothing is written", a conflict is the same 409),
  E23 (#987) and E24 (#988), with R1 and R2 as before.
- New beside them: unattached snapshots (`as unattached branch`), so `CommitOutput` gains
  `kind`, `published` and `root_commit_id`, and `GET /commits` gains `unpublished` and
  `unattached` filters; `ReadOutput.page` for a paged `branch diff` read.
- Still open: E1–E5 (#980), E9 (branch list is names only), E10 (`GET /commits` has no limit or
  cursor), E11 (`ReadOutput.columns` is still names), E12–E19, E20 (#978), E21
  (`BranchCreateRequest` is still `name` and `from`), E22 (#976).

## On Omnigraph main, 2026-10-20

Checked at main `4ba904d3` by diffing its `openapi.json` with the contract Studio carries
(`crates/graph_client/openapi/omnigraph-0.14.0-edge.json`, then from `edge-20261017-d883c49`) and
reading the `changelog.d` notes added since. The contract header is still `0.13` and every HTTP
change is additive; the CLI's is not.

- Landed:
  - E10 (#1013): `GET /commits` takes `limit` (1 to 1,000) and an opaque `page_token`, and answers
    `head_commit_id` and, while older commits remain, `next_page_token`; later pages answer for
    that head even after the branch moves. Without either parameter the whole list, as before.
  - E15 as `GET /graphs/{id}/access` (#1073): the caller's actor, the actions its credential
    allows on the graph and, for a `branch` and a `target_branch`, there, decided by the policy
    as the routes that perform them decide; the MCP `access` tool answers the same.
  - E21 (#1016): `from_commit` on `POST /branches` and `from commit "<id>"` in `branch create`
    (GQ language version 2.3); every receipt carries `from_commit_id`. A commit cleanup no longer
    retains answers 410; one under an earlier schema or before the storage upgrade, of a deleted
    branch, unpublished, or an unattached snapshot answers 409; `ErrorOutput.branch_start_refusal`
    names the reason.
  - E22 (#981): a served write refused at authentication, policy or the contract is reported as
    not started.
  - E24 in a new form: a server endpoint may be `unix:///…` (`--target unix:///…`).
- New beside them:
  - **CLI targets** (#1090, breaking). `--target NAME|ADDRESS` and `defaults.target` select one
    backend; `--server`, `--store`, `--cluster`, `--profile`, `--managed`, `--direct`, `--api`,
    `--organization`, graph `--uri`, `use` and profiles are removed. The operator file is
    `target_config_version: 1` with `targets` of kind `direct`, `server` or `managed`; a server
    target has an `endpoint`, a `default_graph` and one `credential` owner (a token stored by
    `omnigraph login --target`, bound to its endpoint, or `OMNIGRAPH_TOKEN_<OWNER>`) or
    `auth: anonymous`. `omnigraph credential get --target NAME` exports a server target's token to
    a pipe; `omnigraph target import` converts a legacy file and keeps it as `config.legacy.yaml`.
    No command reads `OMNIGRAPH_BEARER_TOKEN` (#1030). `version --json` adds `protocols`
    (`target`, `credential`). Specs 003 and 013.
  - **Full text** (#1066): a full-text call (`search`, `fuzzy`, `match_text`, `bm25`) on a String
    property without `@index` is a type error, `T27`; a declared index with no part at the query's
    snapshot is a 409 whose `full_text_index_required` names `Type.property`, lifted by
    `omnigraph optimize` on `main` or `omnigraph rebuild-full-text-indexes --branch <branch>`.
    Spec 007.
  - **Explain and execution** (#1099, #1086): every planner explain document carries `pipelines`
    (`explain_version` 7); a `load csv` receipt's `executed`, and `ChangeOutput.executed` for an
    overwrite load, carry `operators` with each node's rows and timings; query inspection reports
    ANN stage timings, which no route serves, so E12 stays open. Spec 007.
  - **Language** (#1069, #1071): a ranked order on a binding a traversal reaches roots its
    component; an `rrf()` over two bindings one traversal connects is `T59`, and the planner codes
    `P001` and `P005` are withdrawn; `@range(p, ..max)` is an inclusive maximum, a list's elements
    are checked against its range, and integer bounds stay within `I64`. Specs 007 and 014.
  - **Engine catalogue** (#1008, in `edge-20261017-d883c49`): five `@` node types (`@Operator`,
    planner passes, settings, storage format, logical plan kinds), also served as the reserved
    graph id `og_catalog`. They hold no index status, so E13 stays open. Spec 007.
  - **The server's MCP** (#992, #994, #995, #999): read tools (`access`, paged `commits_list`,
    changes) and opt-in write tools (`mutate`, `load`, branch create, delete and merge), with the
    skill's references as resources. Specs 011 and 013.
  - Lance 13 (#1072): storage format 16 is unchanged; nothing for Studio.
- Still open: E1 to E5 (#980; the launcher's RFC and its stack, #1017, are under way), E9 (#1012
  not merged), E11, E12, E13, E14, E16 to E20 (#978), E25 to E28.
- Adopted with the minimum-server bump to `edge-20261020-7fd9cc9` (2026-10-20), which also
  moved the HTTP contract to 0.14 and added `Omnigraph-If-Actor` (#1074): E10, E15, E21,
  `full_text_index_required`, `executed` and `pipelines` are on every server Studio supports.
  Studio pages history by E10; E15 and E21 are next. The CLI change could not wait for the bump:
  the agent's CLI and the person's are whatever is installed, so specs 003 and 013 read both
  operator-file formats.

## On Omnigraph main, 2026-10-21

Checked at main `97788d37` by reading the CLI's source (`operator_schema.rs` and its
`validation.rs` and `yaml.rs`, `credential_sources.rs`, `operator_transport.rs`, `scope.rs`,
`planes.rs`, `commands/targets/`) and `docs/user/cli/targets.md`.

- **The operator configuration, again** (#1115 at `f7db38da`, then #1126 at `ff542202`, both
  breaking). The operator file is `version: 1` with `profiles` and `targets`; a document holding
  `target_config_version` is refused, and the keyed `credentials` file and
  `OMNIGRAPH_TOKEN_<OWNER>` are gone. A server target owns one `auth` provider (#1126; #1115 had a
  map of named entries with `default_auth`): `token` from a `store`, an `env` variable or a
  `file`, an `exec` helper, or `anonymous`, which alone may use plain HTTP, on loopback.
  `transport.tls.ca: {mode: append|replace, file}` adds or replaces the CA roots, and
  `transport.proxy` defaults to `direct`. `config validate` checks a file offline, with
  `--operator-config FILE` to name it. Spec 013 writes the agent's CLI home in this form over
  loopback TLS and takes only a CLI whose `config validate` passes it.
- **No release reads it yet.** The installed `0.14.0-edge+50283ce` is from before #1115;
  `edge-20261021-888223b` has #1115 without #1126 and refuses a single `auth`. The first build that
  reads Studio's home is the edge build made from main at `ff542202` or later, which was not
  published when this was written; until then the agent has no CLI (spec 013, as decided).
- **The version signal** (E34, #1127): `version --json` still reports `protocols.target: 1`, as
  before the change.
- Studio's reading of the person's own operator file (`graph_store::cli_config`, spec 003) still
  reads the `target_config_version` form; it is open.

## The changes

Each change names the Studio spec that waits on it and Studio's behavior until then.

### Launch and local graphs

**E1. `omnigraph studio` CLI command.** Filed with E4 as
[ModernRelay/omnigraph#980](https://github.com/ModernRelay/omnigraph/issues/980).
Resolve the target the way other commands do (`--target`, with `--graph`). For a local target, serve it in-process on loopback (E2); for a server
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
through it. Discovery only: publication safety stays with the manifest version CAS (Omnigraph
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
| E10 | `GET /graphs/{id}/commits?branch=&limit=&before=` | Bounded lineage walk, keyset on generation and id | Cost scales with the page, not history | 009 | Landed as `limit` and `page_token` (#1013); adopted in History (a page at a time), the snapshot picker (one page) and Review (down to the fork) |
| E11 | Typed columns in `ReadOutput` (`columns: [{name, type, nullable}]`) or Arrow IPC (RFC 0051) | The result's Arrow schema | Additive DTO field; regenerate `openapi.json` | 004, 007 | `describe_query_operation` in `omnigraph-compiler` |
| E12 | `POST /graphs/{id}/query?profile=true` | `query_inspected`, made a supported API | Exposes plan internals; gate on `read` or a new `inspect` action | 007 | Plans only, through `explain query …` |
| E13 | `GET /graphs/{id}/indexes` | RFC 0046 index status | Observes only, never builds | 010 | Index facts from `explain` statistics |
| E14 | `POST /graphs/{id}/schema/plan {schema_source}` → migration plan | `plan_schema` | Read-only; apply stays a deployment on clusters and direct-only on standalone graphs | later | Schema changes stay with `omnigraph cluster apply` |
| E15 | `GET /whoami` → actor and permitted actions per graph and branch scope | Policy evaluation for the resolved token | Never lets a client assert identity | 002 | Landed as `GET /graphs/{id}/access` (#1073); until adopted, show every action, and a 403 is a denial and the draft stays |
| E16 | `GET /graphs/{id}/nodes/{type}/{key}/neighborhood` → count per edge type and direction | Graph index degree (the source of filter-free `count { }` blocks) | Pinned to a snapshot; bounded by the schema's edge types | 005, 006 | One count query per edge type, concurrent, cached for the pinned commit |

### Language tooling

**E17. `omnigraph lsp`.** A language server over stdio on `omnigraph-compiler`: diagnostics with
suggested edits, completion, hover, go to definition into the schema. One service for Studio,
Zed and other editors. *Waits:* 007 (optional). *Today:* `gq_language` calls the compiler
in-process.

**E18. Tree-sitter grammars in the Omnigraph repository.** `tree-sitter-gq` and
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

**E22. A refused write is reported as not started.** *Landed (#981).*
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

**E24. Unix-socket transport for `--server`.** *Landed (#988); since #1090 a `unix:///…`
target.* [ModernRelay/omnigraph#979](https://github.com/ModernRelay/omnigraph/issues/979). The CLI speaks
only http(s), and a `unix://` value falls through to storage resolution. Codex's sandbox can
allow one Unix socket but blocks loopback TCP. *Waits:* 013. *Today:* Codex reaches the gateway
only when the person gives the conversation network access.

**E34. The CLI says which operator configuration it reads.**
[ModernRelay/omnigraph#1127](https://github.com/ModernRelay/omnigraph/issues/1127). Since #1115
the CLI reads only the version-1 operator configuration, yet `version --json` reports
`protocols.target: 1` as the CLIs before it did, so a client cannot tell which file a CLI reads
without asking it to validate one. Filed to bump it to 2. *Waits:* 013. *Today:* Studio takes a
CLI that reports 1 or 2 only when `config validate --operator-config` passes the home Studio
wrote, and names the first build that does by commit (`ff542202`).

### Branching

**E21. A branch from a commit.** *Landed (#1016) as `from_commit`; see "On Omnigraph main,
2026-10-20".* `BranchCreateRequest` at 0.13.0 takes `name` and `from`, a
parent branch whose head the new branch forks ("Parent branch to fork from. Defaults to
`main`", `openapi.json`). Forking at a commit of a branch's history (`from_commit`) would let
History start a branch where a change was made, to try a fix or revert there. Engine rule: the
new branch's first manifest names that commit's table pins and lineage, published once
(invariant 2); a commit that is no longer retained is refused, not approximated. *Waits:* 008,
009. *Today:* History opens a read-only query or table at a commit; a branch starts from a
branch head.

### Writes

**E25. A mutation that both adds and removes.** Rule D2 keeps one mutation constructive
(insert, update) or destructive (delete), never both (`docs/dev/writes.md:376-379` at Omnigraph
main), so replacing a relationship from a node is two commits, and a `@card(..1)` replacement
must delete before it inserts. One write holding both, staged and published once, would make it
one commit. *Waits:* 014. *Today:* Studio saves the two parts as two ordered commits and names
both, or saves through a branch and a merge when the person asks for one change.

**E26. Edge update.** Edge types have no update (T16; `docs/user/mutations/index.md:90-92`): a
keyed edge changes by inserting its key again, an unkeyed one by delete and insert, which gives
it a new id. *Waits:* 014. *Today:* an unkeyed edge's properties save as delete and insert, and
Studio says its id changes.

**E27. Annotation-only schema changes.** Schema migration v1 refuses changing a property's
annotations other than `@description` and `@instruction` ("changing annotations beyond
@description/@instruction is not supported in schema migration v1", `omnigraph schema plan`
0.13.0). An annotation with no built-in behaviour (a display hint such as `@format("text")`)
changes no data and no index, so it could apply as metadata. *Waits:* 014. *Today:* Studio
reads every String as markdown; a hint can be declared only on a new property or type.

**E28. Merge bound to the reviewed source head.** A write is bound to its target's head by
`If-Graph-Commit`; a merge has no such binding on its source. Studio reads the source head just
before it merges, so a commit landing between that read and the merge is merged unseen. A
precondition on the source head would refuse such a merge instead. *Waits:* 008. *Today:* the
merge commit's merged parent (or a fast-forward's commit) is compared with the reviewed head,
and the outcome says when the merge took a commit made after the review was read.

**E30. Version fields as wide as the server sends them.** `omnigraph-api-types` holds every
`graph_manifest_version` and `published_dataset_version` as `u64`, and the OpenAPI document
declares them `integer`, `format: int64`, `minimum: 0`. A commit held in a detached
`__manifest` version (a write `as commit` or `as unattached branch`) has a version with the
high bit set, which is no `int64`: the edge server answered one with
`graph_manifest_version: 10415222161995537841`. The document should say `uint64` (or drop the
format), so a client generated from it decodes every answer the server sends. *Waits:* 004,
007. *Today:* Studio decodes them as `u64`; its contract tests compare field names only.

### In 0.14, not in 0.13.0

Studio's minimum server serves these, so it uses them without asking the server's OpenAPI
document (constitution X).

**R1. Blob writes** (`PUT`/`DELETE /graphs/{id}/blob`). Blob upload and clear. *Spec:* 004.

**R2. Unpublished commits** (`as_commit` on mutations and merges, `POST
/commits/{id}/publish`, `DELETE /commits/{id}`). The planned way to stage edits as a reviewable
proposal and to preview a merge. *Specs:* 004, 008.

## Order if picked up

Studio's own work first, for what landed:

1. CLI targets in the agent's CLI and in reading the person's operator file (specs 013, 003):
   the next CLI build refuses Studio's `--server` and `defaults.server`. Then the version-1
   operator configuration (#1115, #1126): done for the agent's CLI on 2026-10-21 (013), which waits
   for an edge build made from `ff542202` or later; open for the person's operator file (003).
2. With the next minimum-server bump: E10 paged history (009), E15 access (002), E21 a branch
   from a commit (008, 009), `full_text_index_required` and the compiler's new codes (007), then
   `executed` and `pipelines` (007).

Then engine changes still open:

3. E11 typed columns: additive, used everywhere.
4. E9 branch details (#1012, open): the review surface.
5. E1 to E5 launcher: the one-command local experience.
6. E20 delegated credentials (an RFC), which retires spec 013's gateway rules.
7. E16 neighbourhood counts.
8. E12, E13, E14, E17, E18, E19 as the matching Studio work starts.

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Defer every engine change | Change the engine alongside Studio | Ragnor, 2026-10-11: build Studio on the released contract first |
| Routes added to the server proper | A Studio-only side channel | Every client of a served deployment gains them too; one access path |
| Loopback guard in the launcher | In `omnigraph-server` | Deployed servers must not gain a browser mode by accident |

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-21 | The CLI reads only the version-1 operator configuration, and no published build reads Studio's home yet | Omnigraph main `97788d37`: `operator_schema.rs` (`target_config_version` refused), the diff `888223b7..ff542202` of `operator_schema.rs` (server `auth` from a map to one provider), `commands/targets/args.rs` (`config validate`); `gh release list` (newest `edge-20261021-888223b`, 2026-10-22 00:21 UTC); `~/.local/bin/omnigraph version --json` and `config validate --operator-config /dev/null` | `0.14.0-edge+50283ce` reports `http_contract` 0.14 and `protocols.target` 1, and answers "unrecognized subcommand 'config'" (exit 2). `888223b` has #1115 without #1126. E34 filed as #1127 |
| 2026-10-21 | The server's version fields are wider than its contract says (E30) | `omnigraph-api-types/src/lib.rs` at `7fd9cc9` (`CommitOutput`, `SnapshotOutput`, `SnapshotDatasetOutput`, `SchemaApplyOutput`, `PublishedDatasetVersionConflictOutput`: `u64`; its own test decodes `9_223_372_036_854_775_809u64`); `crates/graph_client/openapi/omnigraph-0.14.0-edge.json` (`int64`, `minimum: 0`); what the running app was answered by the edge server; `graph_client` `a_detached_manifest_version_decodes` | A write `as commit` was answered with `graph_manifest_version: 10415222161995537841`, which Studio's `i64` refused to decode, so the write read as unknown; as `u64` it decodes, and the query editor says it is held |
| 2026-10-20 | What Omnigraph main serves that Studio's minimum server does not | `openapi.json` at main `4ba904d3` diffed with `crates/graph_client/openapi/omnigraph-0.14.0-edge.json` by operation, parameter and schema property; the `changelog.d` notes added since `d883c49e`; the CLI reference and targets guide at `4ba904d3` | New: `GET /graphs/{id}/access`; `limit`, `page_token`, `head_commit_id`, `next_page_token` on commits; `from_commit` and `from_commit_id` on branch creation; `ChangeOutput.executed`; error details `branch_start_refusal` and `full_text_index_required`. Contract header still `0.13`. The CLI drops `--server` and profiles for targets (#1090) |
| 2026-10-15 | What Omnigraph main serves that 0.13.0 does not | `openapi.json` at `v0.13.0` and at main `fe69c039`, diffed by operation id, schema properties and parameters; issues #976–#980 | New: `cluster_getDiff`, `cluster_getBranchDiff`, `cluster_getMergeBase`, `cluster_putBlob`, `cluster_clearBlob`, `cluster_publishCommit`, `cluster_dropCommit`, OAuth resource metadata; `dry_run` and `as_commit` on merge; commit kinds and filters; `ReadOutput.page`. Contract header still `0.13`. #977 and #979 closed by #987 and #988; #976, #978 and #980 open |
| 2026-10-11 | Route list at 0.13.0 and main | parsed `openapi.json` at `v0.13.0` and main | 0.13.0 lacks Blob PUT/DELETE and commit publish/drop |
| 2026-10-11 | `BranchMergeRequest` at 0.13.0 | parsed `openapi.json` at `v0.13.0` | `source`, `target`, `delete_branch`, `settings`; no `as_commit` |
| 2026-10-11 | `CommitOutput` at 0.13.0 | parsed `openapi.json` at `v0.13.0` | No `published` field; `GET /commits` takes `branch` only |
| 2026-10-11 | Server internals cited above | Studio integration research at Omnigraph main `680fdfef` | Re-check every line reference against the release the RFC targets |

## Corrections

None yet.
