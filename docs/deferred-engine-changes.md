# Deferred OmniGraph engine changes

Studio's oldest supported server is OmniGraph 0.14 (`edge-20261018-0a62bb7`; its HTTP contract
header is still `0.13`), and Studio changes nothing in the OmniGraph repository (constitution X). This is the index; the design of each change, its engine
rules and its evidence are in [spec 012](../specs/012-omnigraph-engine-changes/plan.md). When
one is picked up it goes through OmniGraph's own RFC process.

| ID | Change | Waiting spec | Studio does today |
|---|---|---|---|
| E1 | `omnigraph studio` CLI command: resolve the target, serve locally on loopback, single-use code handoff ([#980](https://github.com/ModernRelay/omnigraph/issues/980)) | 003, 013 | Connects to running servers from `~/.omnigraph/config.yaml` or added by URL; local graphs served with `omnigraph-server --cluster <dir>` first |
| E2 | Supported local embedding API in `omnigraph-server`, with a read-only mode | E1 | — |
| E3 | Loopback guard (Host allowlist, Origin check, no CORS) in the launcher | E1 | — |
| E4 | Owner discovery for a graph root (discovery, not fencing) | E1 | — |
| E5 | Writer-safety rules for local sessions | E1 | Enforced by the server Studio talks to |
| E9 | Branch details (head, fork, creator, times) | 002, 008 | Derived from `GET /commits?branch=` |
| E11 | Typed result columns or Arrow IPC | 004, 007 | Column types from `omnigraph-compiler`'s `describe_query_operation` |
| E12 | Query profile over HTTP | 007 | Plans only, through `explain query …` |
| E13 | Index status | 010 | Index facts from `explain` statistics |
| E14 | Read-only schema plan | later | Schema changes stay with `omnigraph cluster apply` |
| E16 | Neighbourhood counts per node | 005, 006 | One count query per edge type, concurrent, cached per pinned commit |
| E17 | `omnigraph lsp` language server | 007 | `gq_language` calls `omnigraph-compiler` in-process |
| E18 | Tree-sitter grammars and a Zed extension in the OmniGraph repository | E17 | Grammars in this repository under `grammars/` |
| E19 | Publish `omnigraph-compiler` to crates.io again (stops at 0.8.0) | 001 | Git dependency on tag `edge-20261018-0a62bb7` |
| E20 | Delegated, branch-scoped credentials for agents, recorded in history beside the actor ([#978](https://github.com/ModernRelay/omnigraph/issues/978)) | 011, 013 | Agent commits are recorded under the person's actor, on a branch named `agent/<stamp>`; Studio's gateway keeps the agent's writes on that branch |
| E25 | One mutation that both adds and removes (lift rule D2 for one write) | 014 | A relationship replacement saves as two ordered commits, or through a branch and a merge |
| E26 | Edge update (T16) | 014 | An unkeyed edge's properties save as delete and insert, with a new id |
| E27 | Annotation-only schema changes (a display hint on an existing property) | 014 | Every String reads as markdown; a hint can go only on a new property or type |
| E28 | Merge bound to the reviewed source head (a precondition on the source, as `If-Graph-Commit` binds a write to its target) | 008 | Checks the source head just before merging; a commit landing in between is merged too, and the outcome says the merge took a commit made after the review was read |
| E29 | A Blob write bound to an empty cell (`If-None-Match: *` on `PUT /graphs/{id}/blob`; `If-Match` matches only a cell that holds a value) | 004 | Reads the cell at the head just before sending, and sends only while it is still empty; a value set in between is overwritten |

These landed in OmniGraph 0.14 (`edge-20261017-d883c49`, checked 2026-10-17; the HTTP contract
header is still `0.13`), and so are on every server Studio supports. Studio uses E7, E6, E8
and E23: a review reads its fork and head from the merge base, and a merge is confirmed by the
source head becoming the merge base; a branch its target was merged into is reviewed from
`GET /diff` from that base; a review asks a dry-run merge whether the branch merges cleanly;
the agent's CLI is chosen by its `version --json`. E24 is not used yet.

These landed on OmniGraph main after that build (checked at `4ba904d3`, 2026-10-20), so Studio
adopts them with its next minimum-server bump, or before it only where a server's OpenAPI
document advertises them ([spec 012](../specs/012-omnigraph-engine-changes/plan.md), "On OmniGraph
main, 2026-10-20"):

| ID | Feature | Operation | Spec | Studio does until then |
|---|---|---|---|---|
| E10 | Paged commit list: `limit`, `page_token`; `head_commit_id`, `next_page_token` (#1013) | `GET /commits` | 009 | Adopted: History reads a page at a time, the query editor's snapshot picker one page, and Review down to the fork; a server that pages none answers whole, read as one page |
| E15 | What the caller's credential may do on a graph and its branches (#1073) | `GET /graphs/{id}/access` | 002 | Every action shown; a 403 is a denial and the draft stays |
| E21 | A branch from a commit: `from_commit`, `from_commit_id`, `branch_start_refusal` (#1016) | `POST /branches` | 008, 009 | A branch starts from a branch head |
| E22 | The CLI reports a write refused at authentication, policy or the contract as not started (#981) | CLI | 013 | `AGENTS.md` says a Studio refusal wrote nothing |
| — | A full-text call on an unindexed property is `T27`; a missing index part is a 409 `full_text_index_required` (#1066) | `POST /query` | 007 | A generic failure |
| — | Execution reports and explain pipelines: `ChangeOutput.executed`, `explain_version` 7 (#1099) | `POST /mutate`, `explain` | 007 | Explain trees without pipelines |

One change on main is not additive: the CLI selects a backend with `--target` and reads an
operator file of `targets` (#1090), and no longer accepts `--server`, `defaults.server` or
profiles. Studio's agent CLI and its reading of the person's operator file follow it at once,
since the CLI is whatever is installed (specs 013 and 003).

| ID | Feature | Operation | Spec |
|---|---|---|---|
| R1 | Blob writes (adopted: Choose File, a file dropped from Finder, and Clear, each bound to the value read) | `cluster_putBlob`, `cluster_clearBlob` | 004 |
| R2 | Unpublished commits: `as_commit` on writes and merges, publish, drop, `GET /commits?unpublished=` | `cluster_publishCommit`, `cluster_dropCommit` | 004, 008 |
| E6 | Paged graph diff between two branches or commits, and a branch's diff since its merge base, with `before`/`after` images and `kind`/`type`/`op` filters | `cluster_getDiff`, `cluster_getBranchDiff` | 008 |
| E7 | Merge base of two branches (used by review and merge) | `cluster_getMergeBase` | 008 |
| E8 | Merge preview: `dry_run` on `POST /branches/merge` ("nothing is written"; refused with `as_commit`) | `cluster_mergeBranches` with `dry_run` | 008 |
| E23 | `omnigraph version --json` with the HTTP contract (#987) | CLI | 013 |
| E24 | Unix-socket transport for `--server` (#988) | CLI | 013 |

Also new on main: unattached snapshots (`as unattached branch`; `CommitOutput.kind` and
`published`, `GET /commits?unattached=`), and `ReadOutput.page` for a paged `branch diff` read.
