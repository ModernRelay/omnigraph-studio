# Developing Studio

How to build Studio, run it against a sample graph, check a change, and cut a release. What
Studio is and how to install it: the [README](../README.md). Rules every change follows: the
[constitution](../.specify/memory/constitution.md); for coding agents, [AGENTS.md](../AGENTS.md).

## Requirements

- Rust 1.98.1, installed by `rust-toolchain.toml`. With Homebrew's `rustup`, put
  `$(brew --prefix rustup)/bin` first on `PATH`: its `cargo` otherwise runs the first `rustc` on
  `PATH`, such as Homebrew's `rust`, whatever its version.
- CMake, which builds wasmtime's C API for Zed's tree-sitter (`brew install cmake` on macOS).
- macOS: the Command Line Tools (GPUI compiles its Metal shaders when Studio starts); full
  Xcode to compile them into a release build (`script/bundle-mac`). Linux: the Wayland, X11 and
  Vulkan development packages listed in `.github/workflows/ci.yml`.
- An Omnigraph server of `v0.14.0` or later (HTTP contract 0.14). Studio refuses a server
  that speaks another contract, such as Omnigraph 0.13, and its query checks follow `v0.14.0`.
- For the servers of the CLI's targets, the agent panel and the sample graph, the `omnigraph`
  CLI of `v0.14.0` or later, which Omnigraph's installer puts in `~/.local/bin`:

  ```bash
  curl -fsSL https://raw.githubusercontent.com/ModernRelay/omnigraph/main/scripts/install.sh | bash
  ```

  Studio looks for it on `PATH`, then in `~/.local/bin`, `/opt/homebrew/bin` and
  `/usr/local/bin`, and drives the first that reports target, credential and access protocol 2.

A clean debug build takes about 2.5 minutes on an Apple M5 Pro and about 8 GB of `target/`; a
release build (`cargo run --release`) about 10 minutes.

## Run against the sample graph

The sample graph has 200 people, 25 companies and 40 projects. Serve it with that release's
`omnigraph` and `omnigraph-server` on `PATH` or named by `OMNIGRAPH_BIN` and
`OMNIGRAPH_SERVER_BIN`:

```bash
script/dev-server    # serves knowledge and notes on 127.0.0.1:8787, token dev-token
```

The first run applies `dev/cluster/` and loads the seeds through a direct target in an operator
home of the script's own, before the server starts; your `~/.omnigraph` takes no part. It stops,
saying which, when a binary is one Studio cannot use: the CLI by its `version --json`, the
started server by the contract header its `/healthz` answers with. `STUDIO_DEV_DATA` and
`STUDIO_DEV_BIND` move its data (default `dev/.data`) and address, and `--reset` starts again
from empty graphs.

```bash
OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:8787 OMNIGRAPH_STUDIO_TOKEN=dev-token \
  OMNIGRAPH_STUDIO_GRAPH=knowledge cargo run
```

Or both in one step: `script/dev` starts `script/dev-server` unless a server already answers at
its address, runs Studio on `knowledge` once the graphs are seeded, and stops the server it
started when Studio quits. Its arguments go to `cargo run` (`script/dev --release`).

To try signing in ([spec 016](../specs/016-oauth-client/plan.md)), `script/dev-sign-in` serves
the same graphs on `https://127.0.0.1:8791` behind a sign-in: an `omnigraph-server` that trusts
a stand-in sign-in service, through TLS from a certificate authority of the script's own. It
writes an operator home with one anonymous target, `oauth-bed`, that trusts that authority:

```bash
OMNIGRAPH_HOME=dev/.sign-in/studio-home OMNIGRAPH_STUDIO_OAUTH_CLIENT=https://studio.test/client.json cargo run
```

## Environment variables

Without them, Studio takes its servers from the `omnigraph` CLI's targets
(`omnigraph target list`) and each server's token from `omnigraph credentials export`; it reads
no operator configuration or credential itself. File → Open Graph… adds a server through the
CLI too (`omnigraph target add`, and `credentials set` for its token).

| Variable | Effect |
|---|---|
| `OMNIGRAPH_STUDIO_SERVER` | Adds a server named `environment` |
| `OMNIGRAPH_STUDIO_TOKEN` | Its bearer token |
| `OMNIGRAPH_STUDIO_GRAPH` | Opens this graph on that server at launch |
| `OMNIGRAPH_STUDIO_BRANCH` | The branch to open (default `main`) |
| `OMNIGRAPH_STUDIO_OAUTH_CLIENT` | The client ID Studio signs in with at any sign-in service, for development (spec 016) |
| `OMNIGRAPH_STUDIO_DATA` | Directory of `settings.json`, `keymap.json` and the session (default `~/Library/Application Support/Omnigraph Studio`, `%APPDATA%\Omnigraph Studio` on Windows, or `$XDG_CONFIG_HOME/omnigraph-studio`) |

## Checks

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 script/check-layers.py
python3 script/check-brand.py
python3 script/test_release_files.py
```

CI runs these checks on macOS, Linux and Windows for each pull request that changes more than
documentation, and not again on merge.

`crates/studio/examples/snapshot.rs` renders Studio offscreen to a PNG while dispatching
actions, clicks and keystrokes. It is how UI changes are checked against the sample graph:

```bash
OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:8787 OMNIGRAPH_STUDIO_TOKEN=dev-token \
  OMNIGRAPH_STUDIO_GRAPH=knowledge cargo run -p studio --example snapshot -- out.png \
  'studio_graph::OpenType={"type_name":"Person"}'
```

The same tool records the README's demo: `script/dev-server`, then `script/demo-video`, which
records the story in `demo/story.steps` and frames it with Remotion. See
[demo-video.md](demo-video.md) for what to install, how to change the story, and how to put a
new video in the README.

## Architecture

Studio is a Cargo workspace (Rust 2024 edition) of 20 crates: 18 for the app and 2 for the
tree-sitter grammars. A crate depends only on crates in lower layers, and feature crates never
depend on each other. `script/check-layers.py` enforces this in CI.

| Layer | Crates |
|---|---|
| 6 | `studio`: the binary, menus, windows, deep links |
| 5 | `type_table`, `relationship_tree`, `graph_canvas`, `query_editor`, `review`, `history`, `agent_panel`, `command_palette` |
| 4 | `studio_workspace`: panes, tabs, docks, modals, toasts |
| 3 | `graph_store`: connections, graph sessions, snapshot pins, exploration |
| 2 | `graph_client` (HTTP, no GPUI), `gq_language` (compiler wrapper, no GPUI), `studio_ui` |
| 1 | `studio_settings`, `studio_theme` |
| 0 | `studio_actions`: every action type |

GPUI and its crates come from git at one release tag ([zed-pin.md](zed-pin.md)). The
tree-sitter grammars for `.gq` and `.pg` are in `grammars/`. [architecture.md](architecture.md)
describes the data flow and the state model.

How Studio uses the Omnigraph API, in full (the README has the short form):

- **HTTP only.** No access to graph storage and no engine crate except `omnigraph-compiler`,
  which is linked for parsing and type checking. Every request sends `omnigraph-http-api: 0.14`,
  a server that answers with another contract is refused, and a response without the header is
  treated as having an unknown effect.
- **One commit per view.** A view records the `graph_commit_id` of its first read and passes it
  as `snapshot` on every later read. When the branch head moves, the view shows "Newer data ·
  Refresh" and keeps its data until refreshed.
- **Bound writes.** A cell edit or bulk set is `update T set { p: $new } where @id = $id and
  p = $old`, so a row changed by someone else is left alone and reported. Row deletes and
  reverts from a review use `POST /mutate/if-graph-commit` with the commit the view read.
- **Three outcomes, no retries.** A write is committed, refused, or unknown (the response was
  lost). Studio never resends a write; an unknown outcome is settled by reading history, and the
  draft is kept until then.
- **Server policy.** Authorization is the server's Cedar policy; Studio enforces no rules of its
  own. A 403 is shown as a refusal and the draft stays.
- **Newer routes on demand.** Routes beyond 0.14 are used only when the server's OpenAPI
  document advertises them. Engine changes Studio would use, and what it does without them, are
  listed in [deferred-engine-changes.md](deferred-engine-changes.md) and
  [spec 012](../specs/012-omnigraph-engine-changes/plan.md).

## Repository

Each feature has a plan in [`specs/`](../specs/README.md): the design, the alternatives
rejected, what is deferred, and a dated validation log of the checks behind each claim.

| Path | Contents |
|---|---|
| `.specify/memory/constitution.md` | Principles every change follows |
| `specs/` | One plan per feature |
| `docs/` | Architecture, the GPUI pin, deferred engine changes, this guide |
| `crates/` | The workspace; `crates/studio` is the binary |
| `grammars/` | Tree-sitter grammars for `.gq` and `.pg` |
| `assets/` | Default keymaps and settings |
| `script/` | Dev server, sample data, demo video, bundling, release files, layer and spelling checks |
| `demo/` | The demo video's Remotion project and story |

## Packaging and releases

`script/bundle-mac` builds an app bundle and a DMG, ad hoc signed unless a signing identity is
configured (`--fast` bundles the optimized `release-fast` build, which rebuilds in seconds, to
run day to day); `script/bundle-linux` builds a tarball with a `.desktop` entry.

The release workflow signs and notarizes the macOS app with a Developer ID of team
`6XQX9HAB25` (EQTR INC), from 0.4.1. A release's updates are also signed with an update key
([spec 017](../specs/017-auto-update/plan.md)). Make it once with `cargo packager signer
generate` (cargo-packager 0.11.8): its public half goes in `crates/studio_update/update-key.pub`,
the private half and its password in the `STUDIO_UPDATER_KEY` and `STUDIO_UPDATER_KEY_PASSWORD`
secrets of the repository's `update-signing` environment. That environment needs a reviewer's
approval and takes only `v*` tags: the release workflow signs a tag's update there, so no
workflow change alone can sign one, and each release waits for the approval. `update-key.pub`
holds the key (2026-11-09), so a release whose macOS update is not signed with it is refused.
The key and its password are also kept in 1Password ("Omnigraph Studio Update Signing Key"),
since a secret cannot be read back. Studio has the key from 0.4.2, so 0.4.1 and earlier say a
newer version is out and link to it; 0.4.2 and later install it.

The Windows installer ([spec 027](../specs/027-windows-installer/plan.md)) puts Studio in
`%LOCALAPPDATA%\Programs\Omnigraph Studio` for one person, with no administrator rights, and
adds it to the Start menu and Installed apps; uninstalling keeps the settings in
`%APPDATA%\Omnigraph Studio`. It registers as `com.modernrelay.omnigraph-studio`, the name
`omnigraph studio` looks up. It ships from 0.4.4. Neither Windows file is signed yet.

## Not built yet

- Updates installed in place on Linux and Windows; there Studio says a newer version is out and
  links to it.
- Approvals and comments on reviews.
- Features that depend on deferred engine changes
  ([spec 012](../specs/012-omnigraph-engine-changes/plan.md)).
