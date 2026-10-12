# 010: Inspectors

Status: draft · 2026-10-11

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

Index status route (deferred engine change).

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-11 | Snapshot fields in 0.13.0 | parsed `openapi.json` at `v0.13.0` | `datasets`, `graph_branch`, `graph_manifest_version`, `internal_schema_version` |

## Corrections

None yet.
