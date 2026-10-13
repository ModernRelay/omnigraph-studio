# OmniGraph Studio

**The Knowledge IDE.** Work on an [OmniGraph](https://github.com/ModernRelay/omnigraph)
knowledge graph the way you work on code: walk it as a map, edit it as tables, query it in an
editor that knows your schema, and review every change on a branch before it merges.

![OmniGraph Studio: a person's relationships as a tree on the left, the same walk drawn as a map in the middle, and the person's properties and neighbours in the inspector on the right](docs/images/studio.png)

## Why an IDE for knowledge

Code has IDEs and Git. You can see the whole structure, jump to anything, change it with the
language's help, and review every change before it lands. Knowledge rarely gets that. A graph
usually comes with a query console, where you must already know the query language, or a
picture you can look at but cannot safely change.

OmniGraph keeps knowledge as a typed, versioned graph: every type is a table, and every change
is a commit on a branch. Studio is the IDE for it. It's a native app built on
[Zed](https://github.com/zed-industries/zed)'s GPUI, so large tables stay fast and the keyboard
reaches everything.

![Starting from one person: open who they know, open their company and the people who work there, then jump to the Person table and group it by company](docs/images/explore.gif)

## Everything an IDE gives code, for your graph

| In a code IDE | In Studio |
|---|---|
| Project tree and files | Every type with its row count, and a table per type, paged and filtered on the server |
| Go to definition, find references | ⌘P to any type or node; walk relationships in a tree and on a map that stay in step |
| An editor that knows the language | A query editor with completion, hover and diagnostics from OmniGraph's own compiler, and query plans |
| Edit, refactor, multi-cursor | Edit cells, stage edits across rows and save them together, bulk-set a column, group by a property or a relationship |
| Git branches, diff, review, merge | Branches, a row-by-row review against the base, and a merge that lists conflicts when the engine refuses |
| Blame and history | Every commit in a lane graph; any table, query or node read as it was at an older commit |
| A coding agent | Claude or Codex working on its own branch through Studio's tools, without ever seeing your token |

Also saved views, Blob previews, the schema's source, a storage inspector, several windows
restored on launch, and `omnigraph-studio://` links to a graph, node or query.

## Changes you can trust

An IDE is only useful if you trust what it does to your data. Studio's rules, from its
[constitution](.specify/memory/constitution.md):

- **The server is the authority.** Studio reads and writes only through an OmniGraph server's
  HTTP API, and never keeps a copy of your data that it treats as true.
- **Every view reads one commit.** When the branch moves, a tab says "Newer data · Refresh"
  instead of changing under you.
- **Every write checks what you saw.** Each edit only goes through if the value still holds what
  you read, so a change made elsewhere is caught, not overwritten.
- **Every write has a known outcome.** Committed, refused, or unknown, and never retried behind
  your back. Your draft stays until the outcome is known.
- **Every write names its branch.** Deleting asks you to type what will be deleted, and the
  server's policy has the final word.

## Try it

You need Rust (the right version installs itself from `rust-toolchain.toml`), the OmniGraph
**0.13** binaries on your `PATH`, and full Xcode on macOS or the Wayland, X11 and Vulkan
development packages on Linux.

Serve the sample graph: 200 people, 25 companies and 40 projects, with the token `dev-token`.

```bash
script/dev-server
```

Open it in Studio from a second terminal:

```bash
OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:8787 OMNIGRAPH_STUDIO_TOKEN=dev-token OMNIGRAPH_STUDIO_GRAPH=knowledge cargo run
```

The first build compiles Zed's GPUI and needs about 8 GB of disk. A clean debug build takes
under 3 minutes on an Apple M5 Pro, and a release build (`cargo run --release`) about 10.

**Your own graphs.** Studio picks up the servers and tokens you already use with the `omnigraph`
CLI (`~/.omnigraph/config.yaml` and `credentials`). File → Open Graph… adds a server by URL.

**Packaging.** `script/bundle-mac` builds the app bundle and a DMG; `script/bundle-linux` builds
a tarball with a `.desktop` entry.

### Keys to start with

| Key | Does |
|---|---|
| ⌘K | Command palette |
| ⌘P | Go to a type or a node |
| ⌘⇧E / ⌘⇧O | Graph explorer / ontology map |
| ⌘N, then ⌘↵ | New query, then run the query under the cursor |
| ⌘⇧B | Switch branch |
| ⌘⇧A | Agent |
| ? | Every shortcut |

## How it's built

- **UI:** Zed's GPUI and UI crates, pinned to one Zed release (`docs/zed-pin.md`).
- **Language:** OmniGraph's MIT-licensed `omnigraph-compiler`, linked in-process for parsing and
  type checking; tree-sitter grammars for `.gq` and `.pg` in `grammars/`. It is the only engine
  crate Studio links.
- **Layers:** each crate depends only on lower layers, and feature crates never depend on each
  other. `script/check-layers.py` enforces this in CI.

```text
studio (the app)
  ↓ type_table · relationship_tree · graph_canvas · query_editor · review · history · inspectors · agent_panel · command_palette
  ↓ studio_workspace (window, panes, docks)
  ↓ graph_store (sessions, snapshot pins, exploration)
  ↓ graph_client (HTTP) · gq_language (compiler) · studio_ui
  ↓ studio_settings · studio_theme
  ↓ studio_actions
```

`docs/architecture.md` has the data flow and the state model.

## Status

Studio is built against OmniGraph 0.13, macOS first. Not built yet:

- Windows support, signed releases and auto-update.
- Sign-in through an identity provider (OIDC). Studio uses bearer tokens today.
- Approvals and comments on reviews.
- Features that wait on engine changes, each listed with what Studio does meanwhile in
  [spec 012](specs/012-omnigraph-engine-changes/plan.md).

## Working on Studio

Every feature has a plan in [`specs/`](specs/README.md) with its design, the alternatives it
rejected, and a dated log of the checks behind each claim. A change starts with its plan. CI
runs:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 script/check-layers.py
```

| Path | What |
|---|---|
| `.specify/memory/constitution.md` | The principles every change follows |
| `specs/` | One plan per feature, with its validation log |
| `docs/architecture.md` | Crate layers, data flow, state model, window frame |
| `docs/zed-pin.md` | Which Zed crates Studio uses, and how to move the pin |
| `docs/deferred-engine-changes.md` | OmniGraph changes Studio would use, and what it does today |
| `crates/` | The workspace; `crates/studio` is the app |
| `grammars/` | Tree-sitter grammars for `.gq` and `.pg` |
| `assets/` | Default keymaps and settings |
| `script/` | Dev server, sample data, bundling, the layer check |

## License

GPL-3.0-or-later, because Studio links Zed's GPL-3.0 UI crates; see `LICENSE`. OmniGraph
itself is MIT and unaffected: Studio talks to it over HTTP and links only its MIT-licensed
compiler crate.
