---
name: omnigraph-studio
description: Read and change this OmniGraph graph from OmniGraph Studio with the conversation's omnigraph CLI. Use before writing a GQ query, a mutation or an NDJSON load, and when reading the schema, searching, reading Blob values or following changes. Covers GQ syntax and what differs in Studio from a plain omnigraph setup.
license: GPL-3.0-or-later; GUIDE.md and references/ are MIT (see LICENSE)
metadata:
  author: OmniGraph Studio
  guide-version: "0.13.0"
  http-contract: "0.13"
---

# OmniGraph in Studio

You work on one OmniGraph graph with a person in OmniGraph Studio. `AGENTS.md` in your working
directory holds the rules for this graph; this skill teaches the CLI and the GQ language. If
another `omnigraph` skill is installed, prefer this one here: its advice fits this
conversation's CLI.

## What is different in Studio

- Run the CLI by the absolute path the `omnigraph_cli` tool gives you, with
  `--server studio --graph <graph>`, or through the `omnigraph_run` tool, which adds both. Never
  another `omnigraph` on PATH, and never `--store`, a storage URI, `--profile` or `--as`.
- The CLI is already set up. Skip the guide's parts on installing, `~/.omnigraph/config.yaml`,
  credentials, servers, clusters, deployments, aliases, upgrades, migrations and server policy:
  those are the person's.
- Read any branch. Write only to your own branch: create it first, then pass `--branch` to
  `mutate`, `load` and Blob writes. The person reviews and merges it in Studio.
- No schema changes: `schema plan` and `schema apply` are not available. Propose a schema change
  to the person instead.
- Branch statements (`branch create|delete|merge` inside a query) are refused; use the
  `branch` commands.
- An outcome of `effects: unknown` means the answer was lost. Check
  `commit list --branch <your branch>` before trying again; never repeat a write blindly. A
  Studio refusal says "Nothing was sent": nothing was written.
- Check a query before running it: save the schema with `schema show`, then
  `lint --query <file> --schema <file>`. Lint works offline, so it also runs where your shell
  has no network.

## Where to look

- `GUIDE.md`, the OmniGraph guide: "Essentials" (reads, mutations, loads), "`.gq` grammar",
  "CLI Reference" and "Common Gotchas".
- `references/queries.md`: parameters, matching, traversal, correlated blocks, filters,
  aggregation, search functions, mutations and branch statements.
- `references/data.md`: choosing `mutate` or `load`, NDJSON loads and their `--mode`.
- `references/schema.md`: reading a `.pg` schema.
- `references/search.md`: vector, full-text and hybrid search.
- `references/blobs.md`, `references/changes.md`, `references/stored-queries.md`.
- `references/remote-ops.md`: write receipts, unknown outcomes and reading large output.
- `references/commands.md`: every command and flag.

Not for Studio: `references/cluster.md`, `references/migrations.md`,
`references/server-policy.md` and `references/aliases.md`.
