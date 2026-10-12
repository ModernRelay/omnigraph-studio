# OmniGraph Studio

A native desktop app for [OmniGraph](https://github.com/ModernRelay/omnigraph) graphs: a table
per node and edge type, a relationship tree, a graph canvas, a `.gq` query editor, branches,
review and merge, and commit history. It is built on [Zed](https://github.com/zed-industries/zed)'s
GPUI and UI crates and talks to OmniGraph servers over HTTP.

Status: foundation. The repository, specs and crate layout are in place; the first build and
the spikes in `specs/001-foundation/plan.md` come next.

## Run

```bash
cargo run
```

Requirements: Rust 1.98.1 (installed by `rust-toolchain.toml`), full Xcode on macOS for the
Metal shader compiler, and the Wayland, X11 and Vulkan development packages on Linux. The first
build compiles Zed's GPUI crates and needs several GB of free disk.

Studio connects to running OmniGraph servers (0.13). It reads servers and tokens from the
OmniGraph CLI's `~/.omnigraph/config.yaml` and `credentials`, or you add a server by URL. To open
a local graph, serve it first with `omnigraph-server --cluster <dir>`.

## Layout

| Path | What |
|---|---|
| `.specify/memory/constitution.md` | The principles every change follows |
| `docs/architecture.md` | Crate layers, data flow, state model, window frame |
| `docs/zed-pin.md` | Which Zed crates Studio uses, and how to bump the pin |
| `docs/deferred-engine-changes.md` | OmniGraph changes Studio would use later, and what it does today |
| `specs/` | One plan per feature |
| `crates/` | The workspace; `crates/studio` is the binary |
| `assets/` | Default keymaps and settings |
| `grammars/` | Tree-sitter grammars for `.gq` and `.pg` |

## License

GPL-3.0-or-later. Studio links Zed's GPL-3.0 UI crates; see `LICENSE`. OmniGraph itself is MIT
and unaffected: Studio talks to it over HTTP and links only its MIT-licensed compiler crate.
