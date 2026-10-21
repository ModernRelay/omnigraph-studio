# 010: Inspectors

Status: built on 0.13 · 2026-10-12 (`crates/inspectors`): a "Storage" tab. Index facts wait on
E13: 0.13's `explain` names none for a scan (see the validation log).

## Problem

When something is slow or odd, people need to see the graph's physical state: which table
versions a commit pins, how many rows each type holds, server and graph health. Delta ships the
same kind of inspector for its own storage.

## Principles

I (read through HTTP only), II and V (the snapshot shown stays until Refresh), VI (missing data
labelled, never blank).

## What exists today (checked)

- OmniGraph `v0.13.0`: `GET /snapshot?branch=` returns `graph_manifest_version`,
  `internal_schema_version` and per-dataset `entity_kind`, `type_name`, `dataset_path`,
  `published_dataset_version`, `entity_count`. `GET /readyz` returns serving state and graph
  counts; `GET /graphs` per-graph state and failures. `explain query …` returns plan rows with
  index facts.
- Delta's History / Worktree Inspector: a master list of histories with health, then detail
  cards with copyable ids (observed in the app, 2026-10-11).

## Design

- A window (Studio menu → Open Inspector) with a list of graphs and their state, and per graph:
  manifest version, schema version, a table of datasets (type, kind, rows, version, path), and
  readiness.
- Every value copyable; ids in monospace; unknown values say "Not reported by this server".
- Index facts for a type from an `explain` of a simple scan, until an index route exists.
- The snapshot shown stays when its branch moves (constitution II, V). The snapshot route
  names no commit, so the tab keeps the head the session knew when it asked; a head move on
  the branch to another commit, Studio's own or another writer's, shows "Newer snapshot ·
  Refresh" in the toolbar and the tab's stale dot. The chip, the toolbar's Refresh and ⌘R
  read it again (⌘R also goes on to the workspace's refresh of the graph); another branch is
  read at once. A new token on the connection reads everything again under it, showing none of
  the old token's answers meanwhile (`SessionEvent::CredentialChanged`; a parked graph's when
  it is shown again), since the new one may see less (since 2026-10-21). Row counts are
  grouped, "12,480".
- The snapshot's tables are a virtualized list (`uniform_list`, constitution IX): only the
  rows in view are built, each value with its copy button, nodes first and each kind by type
  name as before. The list fills what the server's and the graphs' facts leave, the snapshot's
  part keeping at least twelve rows' height; on a short window those facts scroll above it
  (since 2026-10-21; before, every table's row and its five copy buttons were built at every
  redraw, about 3,000 buttons on a graph of 653 tables).

## Deferred

- Engine change E13 (index status), [spec 012](../012-omnigraph-engine-changes/plan.md). The
  interim plan, index facts from `explain` statistics, has nothing to read at 0.13: a scan's
  plan names its table version, projection, filter and row estimate, not the indexes behind it.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-21 | The snapshot's tables build only the rows in view | `crates/inspectors/src/storage.rs` `render_snapshot` (a `uniform_list` over `snapshot.datasets`, its rows built by `dataset_rows` for the range shown, each one line tall) and `tables_in_order` unchanged; the `inspectors` tests; workspace clippy | Builds and the inspector's tests pass. Not checked by drawing: a drawing test needs `studio_theme`, which `inspectors` does not depend on, and adding it would change `Cargo.lock`; not yet seen in the running app on a large graph |
| 2026-10-21 | A new token reads the storage again | `inspectors` `a_new_token_reads_the_storage_again` (the connection's token replaced) | Right after the change the graphs and the snapshot read as loading, then a second snapshot read lands under the new token. Before, the tab heard only branch changes and head moves, and kept the old token's graphs and tables until a Refresh |
| 2026-10-21 | Row counts are grouped | `inspectors` `row_counts_are_grouped` | "12,480"; before, "12480" |
| 2026-10-20 | The snapshot shown stays when the branch moves | `inspectors` test `a_head_move_offers_the_newer_snapshot_and_reads_it_only_on_refresh` on `graph_store::test_server` (another writer's commit seen by the head watch, then Refresh, the same head again, then another branch); the same test against the earlier subscription | One snapshot read and the chip and stale dot after the commit; a second read only on Refresh, none for the head it was read at, a third on the branch switch. The earlier code read again at the commit (2 reads, no chip) |
| 2026-10-11 | Snapshot fields in 0.13.0 | parsed `openapi.json` at `v0.13.0` | `datasets`, `graph_branch`, `graph_manifest_version`, `internal_schema_version` |
| 2026-10-12 | `explain` as a source of index facts | `explain` of a key-equality scan on `knowledge` and a `search` scan on `notes` against the 0.13 dev server | The physical `Scan` carries `table`, `version`, `projection`, `filter` and one statistic, `scan_row_estimate` with its origin ("manifest row count, at most one row under a key equality"); `plan` rows add passes, route, hashes, `assumptions` (dataset path and version, gate policy, memory limit). Nothing names an index or its coverage |
| 2026-10-12 | Every field renders from a live server | offscreen snapshot against `omnigraph-server` 0.13.0 | `/healthz`, `/readyz`, `/graphs` and `/snapshot` values shown and copyable; `source_version` absent and labelled "Not reported by this server" |

## Corrections

- 2026-10-12: the inspector opens as a tab ("Storage", `OpenInspector`) in the graph's window
  rather than a separate window; it covers the session's graph and branch, and every graph's
  state from `GET /graphs`. The snapshot route returns no `graph_commit_id`, so the tab shows the
  manifest version instead.
- 2026-10-20: the tab read the snapshot again at every head move of its branch, which fires
  for other writers' commits too, so the table changed under the reader. It now offers the
  newer snapshot and reads only on Refresh or another branch.
