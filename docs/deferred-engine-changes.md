# Deferred OmniGraph engine changes

Studio ships against the released HTTP contract (0.13) and changes nothing in the OmniGraph
repository for now (constitution X). Each row is something Studio would use, what Studio does
today instead, and when to revisit it. When one of these is picked up, it goes through
OmniGraph's own RFC process; Studio's plan links to the RFC and never redefines it.

## Launch and local graphs

| Change | Why Studio wants it | Studio today | Revisit when |
|---|---|---|---|
| `omnigraph studio` CLI command: resolve the target, start the server on loopback in-process, mint a per-launch token and policy, open Studio with a single-use code | One command from the CLI to a ready window, as with `prisma studio` or `duckdb -ui`; local graphs without running a server by hand | Studio connects to servers from `~/.omnigraph/config.yaml` or added by URL; a local graph is served with `omnigraph-server --cluster <dir>` first | Studio's shell, connections and type tables work end to end |
| Host allowlist and Origin check in `omnigraph-server` | Protects a loopback server from DNS rebinding and cross-site requests | Not needed while Studio is a native app talking to servers the person runs | Together with the launcher, or a web build |
| Read-only serving mode | Inspect a graph next to a live writer without the risk of a second writer | Studio's writes go through the one server that owns the graph | With the launcher |
| Discovery file for the process that owns a graph root | CLI writes route through the running Studio server; no second writer | Not applicable until the launcher exists | With the launcher |

## Read routes

| Change | Why Studio wants it | Studio today | Revisit when |
|---|---|---|---|
| Typed columns in query results (`columns: [{name, type, nullable}]`) or Arrow IPC (RFC 0051) | Column types, alignment and editors in result grids | Types from `omnigraph-compiler`: the compiler describes a query's result columns before it runs | Result grids need types the compiler cannot give (parameters bound at run time) |
| Branch-to-branch diff, paged, with before and after images | Net review of a branch against its target | Changes since the fork from the change feed (`GET /changes?start=after:<commit>`), folded client-side, capped, with "partial" shown when capped | Review on large branches |
| Merge base of two refs | Name the comparison base | Fork commit found by walking `GET /commits` parents | With the diff route |
| Merge preview with no effects | Show conflicts before merging | Merge is attempted on request; a 409 lists conflicts and nothing moved | Before merge becomes a one-click flow |
| Branch details (head, fork point, creator, times) | Sidebar and review headers | Derived from `GET /commits?branch=` | Many branches or long histories |
| Paged commit list | History on long-lived graphs | `GET /commits` in full, rendered virtualized, with a size warning | A graph's history gets slow to list |
| Query profile over HTTP | Explain view with executed row counts | Plan only, via `explain query …` on `/query` | The query editor's explain view |
| Index status | Inspectors | Index facts from `explain` statistics | Inspectors |
| Schema plan (read-only) | Show what a schema change would do | Out of scope; schema changes stay with `omnigraph cluster apply` | Schema editing is planned |
| Who am I and what may I do | Hide actions the actor cannot take | Show every action; a 403 is shown as a denial and the draft stays | Members with mixed permissions use Studio |
| Neighbourhood counts per node (degree per edge type and direction) | Relationship tree and canvas hub counts in one call | One `count` query per edge type, run concurrently and cached for the pinned commit | Tree expansion feels slow on real graphs |

## Language tooling

| Change | Why Studio wants it | Studio today | Revisit when |
|---|---|---|---|
| `omnigraph lsp` language server | One language service for Studio, Zed and other editors | `gq_language` calls `omnigraph-compiler` in-process | A Zed extension for `.gq` is wanted |
| Tree-sitter grammars for `.gq` and `.pg` in the OmniGraph repository | The language's owner keeps its grammar | Grammars live in this repository under `grammars/` | The LSP work starts |
| Publishing `omnigraph-compiler` to crates.io again (crates.io stops at 0.8.0) | Normal versioned dependency | Git dependency on tag `v0.13.0` | Any OmniGraph release |

## Features already on OmniGraph main, not in 0.13.0

Studio uses these only when the server's OpenAPI document advertises them (constitution X):

- `PUT` and `DELETE /graphs/{id}/blob` (Blob writes).
- `POST /graphs/{id}/commits/{commit_id}/publish` and `DELETE /graphs/{id}/commits/{commit_id}`
  (unpublished commits), with `as_commit` on merges. This is the planned way to preview a merge
  and to stage edits as a reviewable proposal.
