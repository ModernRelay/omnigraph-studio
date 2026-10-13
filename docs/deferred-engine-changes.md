# Deferred OmniGraph engine changes

Studio ships against the released HTTP contract (0.13) and changes nothing in the OmniGraph
repository for now (constitution X). This is the index; the design of each change, its engine
rules and its evidence are in [spec 012](../specs/012-omnigraph-engine-changes/plan.md). When
one is picked up it goes through OmniGraph's own RFC process.

| ID | Change | Waiting spec | Studio does today |
|---|---|---|---|
| E1 | `omnigraph studio` CLI command: resolve the target, serve locally on loopback, single-use code handoff ([#980](https://github.com/ModernRelay/omnigraph/issues/980)) | 003, 013 | Connects to running servers from `~/.omnigraph/config.yaml` or added by URL; local graphs served with `omnigraph-server --cluster <dir>` first |
| E2 | Supported local embedding API in `omnigraph-server`, with a read-only mode | E1 | — |
| E3 | Loopback guard (Host allowlist, Origin check, no CORS) in the launcher | E1 | — |
| E4 | Owner discovery for a graph root (discovery, not fencing) | E1 | — |
| E5 | Writer-safety rules for local sessions | E1 | Enforced by the server Studio talks to |
| E6 | Paged branch-to-branch diff with before and after images | 008 | Change feed folded from the fork, capped at 20,000, "Partial" when capped |
| E7 | Merge base of two refs | 008 | Walk `GET /commits` parents |
| E8 | Merge preview with no effects | 008 | Merge on request; a 409 lists conflicts and nothing moved |
| E9 | Branch details (head, fork, creator, times) | 002, 008 | Derived from `GET /commits?branch=` |
| E10 | Paged commit list | 009 | Full list, virtualized, warning above 5,000 commits |
| E11 | Typed result columns or Arrow IPC | 004, 007 | Column types from `omnigraph-compiler`'s `describe_query_operation` |
| E12 | Query profile over HTTP | 007 | Plans only, through `explain query …` |
| E13 | Index status | 010 | Index facts from `explain` statistics |
| E14 | Read-only schema plan | later | Schema changes stay with `omnigraph cluster apply` |
| E15 | Who am I and what may I do | 002 | Every action shown; a 403 is a denial and the draft stays |
| E16 | Neighbourhood counts per node | 005, 006 | One count query per edge type, concurrent, cached per pinned commit |
| E17 | `omnigraph lsp` language server | 007 | `gq_language` calls `omnigraph-compiler` in-process |
| E18 | Tree-sitter grammars and a Zed extension in the OmniGraph repository | E17 | Grammars in this repository under `grammars/` |
| E19 | Publish `omnigraph-compiler` to crates.io again (stops at 0.8.0) | 001 | Git dependency on tag `v0.13.0` |
| E20 | Delegated, branch-scoped credentials for agents, recorded in history beside the actor ([#978](https://github.com/ModernRelay/omnigraph/issues/978)) | 011, 013 | Agent commits are recorded under the person's actor, on a branch named `agent/<stamp>`; Studio's gateway keeps the agent's writes on that branch |
| E22 | The CLI reports a write refused at authentication, policy or the contract as not started ([#976](https://github.com/ModernRelay/omnigraph/issues/976)) | 013 | `AGENTS.md` says a Studio refusal wrote nothing |
| E23 | `omnigraph version --json` with the HTTP contract ([#977](https://github.com/ModernRelay/omnigraph/issues/977)) | 013 | Each installed CLI is tested once against the gateway |
| E24 | Unix-socket transport for `--server` ([#979](https://github.com/ModernRelay/omnigraph/issues/979)) | 013 | Codex reaches the gateway only with network access the person grants |

Already on OmniGraph main but not in 0.13.0, used only when a server advertises them:

| ID | Feature | Spec |
|---|---|---|
| R1 | Blob writes (`PUT`/`DELETE /graphs/{id}/blob`) | 004 |
| R2 | Unpublished commits (`as_commit`, publish, drop): staged edits as proposals, merge preview | 004, 008 |
