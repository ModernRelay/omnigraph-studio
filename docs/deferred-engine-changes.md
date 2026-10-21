# Deferred OmniGraph engine changes

Studio's oldest supported server is the OmniGraph edge build `edge-20261020-7fd9cc9`, the first
that speaks HTTP contract 0.14, and Studio changes nothing in the OmniGraph repository
(constitution X). This is the index; the design of each change, its engine rules and its
evidence are in [spec 012](../specs/012-omnigraph-engine-changes/plan.md). When one is picked up
it goes through OmniGraph's own RFC process.

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
| E19 | Publish `omnigraph-compiler` to crates.io again (stops at 0.8.0) | 001 | Git dependency on tag `edge-20261020-7fd9cc9` |
| E20 | Delegated, branch-scoped credentials for agents, recorded in history beside the actor ([#978](https://github.com/ModernRelay/omnigraph/issues/978)) | 011, 013 | Agent commits are recorded under the person's actor, on a branch named `agent/<stamp>`; Studio's gateway keeps the agent's writes on that branch |
| E25 | One mutation that both adds and removes (lift rule D2 for one write) | 014 | A relationship replacement saves as two ordered commits, or through a branch and a merge |
| E26 | Edge update (T16) | 014 | An unkeyed edge's properties save as delete and insert, with a new id |
| E27 | Annotation-only schema changes (a display hint on an existing property) | 014 | Every String reads as markdown; a hint can go only on a new property or type |
| E28 | Merge bound to the reviewed source head (a precondition on the source, as `If-Graph-Commit` binds a write to its target) | 008 | Checks the source head just before merging; a commit landing in between is merged too, and the outcome says the merge took a commit made after the review was read |
| E29 | A Blob write bound to an empty cell (`If-None-Match: *` on `PUT /graphs/{id}/blob`; `If-Match` matches only a cell that holds a value; [#1110](https://github.com/ModernRelay/omnigraph/issues/1110)) | 004 | Reads the cell at the head just before sending, and sends only while it is still empty; a value set in between is overwritten |
| E30 | Lance versions declared as wide as the server sends them: `graph_manifest_version` (`CommitOutput`, `SnapshotOutput`, `SchemaApplyOutput`) and the `published_dataset_version` fields are `u64` in `omnigraph-api-types`, and the OpenAPI document says `int64` with `minimum: 0`. A commit held in a detached `__manifest` version (a write `as commit` or `as unattached branch`) carries a version with the high bit set, past `i64::MAX`, which a client generated from the document cannot decode (format `uint64`, or `int64` without the claim) | 004, 007 | Decodes every manifest and dataset version as `u64`; the contract tests compare field names, not widths |

Every server Studio supports serves these (checked at `7fd9cc9`, 2026-10-20):

| ID | Feature | Operation | Spec | Studio |
|---|---|---|---|---|
| E6 | Paged graph diff between two branches or commits, and a branch's diff since its merge base, with `before`/`after` images and `kind`/`type`/`op` filters | `cluster_getDiff`, `cluster_getBranchDiff` | 008 | A branch its target was merged into is reviewed from `GET /diff` from that base |
| E7 | Merge base of two branches | `cluster_getMergeBase` | 008 | A review reads its fork and head from it, and a merge is confirmed by the source head becoming the merge base |
| E8 | Merge preview: `dry_run` on `POST /branches/merge` | `cluster_mergeBranches` with `dry_run` | 008 | A review asks it whether the branch merges cleanly |
| E10 | Paged commit list: `limit`, `page_token`; `head_commit_id`, `next_page_token` (#1013) | `GET /commits` | 009 | History reads a page at a time, the query editor's snapshot picker one page, and Review down to the fork |
| E15 | What the caller's credential may do on a graph and its branches (#1073) | `cluster_getAccess` | 002 | Read for the window's branch when a graph opens or the branch changes and after a 403 or a new token, for a branch as its menu opens, and for a review's branch and target. An action it refuses (edits, inserts, deletes, Blob writes and saves on the branch; deleting and merging a branch) is disabled with what the credential lacks and not registered; the status bar's branch reads "read only", and the actor is in the connection chip's tooltip. Branch creation stays offered, judged only with the new name. A 403 is still a denial that keeps the draft |
| E21 | A branch from a commit: `from_commit`, `from_commit_id`, `branch_start_refusal` (#1016) | `POST /branches` | 008, 009 | History's "New branch here…" opens the new-branch dialog at a published commit; a refusal reads its reason in words; the agent's gateway refuses one started at a commit |
| E22 | The CLI reports a write refused at authentication, policy or the contract as not started (#981) | CLI | 013 | `AGENTS.md` says a Studio refusal wrote nothing |
| E23 | `omnigraph version --json` with the HTTP contract (#987) | CLI | 013 | The agent's CLI is chosen by it |
| E24 | Unix-socket transport (#988) | CLI | 013 | Not used |
| R1 | Blob writes | `cluster_putBlob`, `cluster_clearBlob` | 004 | Choose File, a file dropped from Finder, and Clear, each bound to the value read |
| R2 | Unpublished commits: `as_commit` on writes and merges, publish, drop, `GET /commits?unpublished=` | `cluster_publishCommit`, `cluster_dropCommit` | 004, 008 | A commit a write answers with is a branch's head only when its `kind` is `published`; Studio writes none itself, and the agent's gateway forwards none |
| — | `Omnigraph-If-Actor`: the actor a request expects its credential to resolve to (#1074) | every graph route | 013 | Studio's own requests do not send it; the agent's gateway passes the CLI's `--as` on |
| — | A full-text call on an unindexed property is `T27`; a missing index part is a 409 `full_text_index_required` (#1066) | `POST /query` | 007 | The query editor's check reports `T27`; the 409 is a generic failure |
| — | Execution reports and explain pipelines: `ChangeOutput.executed`, `explain_version` 7 (#1099) | `POST /mutate`, `explain` | 007 | Explain trees without pipelines |

Also served: unattached snapshots (`as unattached branch`; `CommitOutput.kind`,
`GET /commits?unattached=`) and `ReadOutput.page` for a paged `branch diff` read. The CLI
selects a backend with `--target` and reads an operator file of `targets` (#1090); Studio's
agent CLI and its reading of the person's operator file follow it (specs 013 and 003).
