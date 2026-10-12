# 010: Inspectors

Status: built on 0.13 · 2026-10-12 (`crates/inspectors`): a "Storage" tab. Index facts wait on
E13: 0.13's `explain` names none for a scan (see the validation log).

## Problem

When something is slow or odd, people need to see the graph's physical state: which table
versions a commit pins, how many rows each type holds, server and graph health. Delta ships the
same kind of inspector for its own storage.

## Principles

I (read through HTTP only), VI (missing data labelled, never blank).

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

## Deferred

- Engine change E13 (index status), [spec 012](../012-omnigraph-engine-changes/plan.md). The
  interim plan, index facts from `explain` statistics, has nothing to read at 0.13: a scan's
  plan names its table version, projection, filter and row estimate, not the indexes behind it.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Snapshot fields in 0.13.0 | parsed `openapi.json` at `v0.13.0` | `datasets`, `graph_branch`, `graph_manifest_version`, `internal_schema_version` |
| 2026-10-12 | `explain` as a source of index facts | `explain` of a key-equality scan on `knowledge` and a `search` scan on `notes` against the 0.13 dev server | The physical `Scan` carries `table`, `version`, `projection`, `filter` and one statistic, `scan_row_estimate` with its origin ("manifest row count, at most one row under a key equality"); `plan` rows add passes, route, hashes, `assumptions` (dataset path and version, gate policy, memory limit). Nothing names an index or its coverage |
| 2026-10-12 | Every field renders from a live server | offscreen snapshot against `omnigraph-server` 0.13.0 | `/healthz`, `/readyz`, `/graphs` and `/snapshot` values shown and copyable; `source_version` absent and labelled "Not reported by this server" |

## Corrections

- 2026-10-12: the inspector opens as a tab ("Storage", `OpenInspector`) in the graph's window
  rather than a separate window; it covers the session's graph and branch, and every graph's
  state from `GET /graphs`. The snapshot route returns no `graph_commit_id`, so the tab shows the
  manifest version instead.
