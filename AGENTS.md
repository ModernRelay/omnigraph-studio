# OmniGraph Studio agent guide

This file is the map for coding agents. Keep it short: rules live in the constitution, design
in `docs/`, decisions in `specs/`, exact behavior in code and tests.

## Required reading

Before every task:

1. The [constitution](.specify/memory/constitution.md). It binds every change.
2. The [architecture](docs/architecture.md): crate layers, data flow, state model.
3. The plan in `specs/` for the area you touch, including its validation log.
4. [Spec 012](specs/012-omnigraph-engine-changes/plan.md) and its index
   [docs/deferred-engine-changes.md](docs/deferred-engine-changes.md) before designing around a
   gap in the OmniGraph API. Cite the change ID (E1–E19, R1–R2) in the plan that waits on it.

`CLAUDE.md` is a symlink to this file. Edit `AGENTS.md` only.

## What Studio is

A native GPUI desktop app for OmniGraph, built on Zed's crates (GPL-3.0-or-later). It talks to
OmniGraph servers over HTTP only and links `omnigraph-compiler` for offline schema and query
intelligence. See `README.md`.

## Pins

| What | Pin | Where |
|---|---|---|
| Zed crates | tag `v1.23.2` (`c0199504b9cc`) | `Cargo.toml` `[workspace.dependencies]` |
| Rust | `1.98.1`, Zed's toolchain at that tag | `rust-toolchain.toml` |
| OmniGraph HTTP contract | `0.14` (the `v0.14.0` release) | `crates/graph_client/openapi/omnigraph-0.14.0.json` |
| `omnigraph-compiler` | tag `v0.14.0` | `Cargo.toml` |

Bump Zed only with [the procedure](docs/zed-pin.md). Copy Zed's `[patch.crates-io]` entries
when you bump: patches do not carry over from a dependency's workspace.

## Build and test

```bash
cargo check --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
python3 script/check-layers.py   # crate layer rule; keep its table in step with docs/architecture.md
```

- macOS needs the Command Line Tools: the default `runtime-shaders` feature compiles GPUI's Metal
  shaders when Studio starts. `script/bundle-mac` compiles them at build time when full Xcode's
  Metal compiler is present (`xcrun -sdk macosx metal`); on macOS 26, also run
  `xcodebuild -downloadComponent MetalToolchain` once.
- Every platform needs CMake: Zed's `language` crate turns on tree-sitter's `wasm` feature, whose
  `wasmtime-c-api-impl` build runs `cmake`. CI runners have it; a fresh Mac does not.
- Linux needs the Wayland, X11, xkbcommon, fontconfig and Vulkan development packages.
- A first build of Zed's GPUI crates needs several GB of disk. Check free space first.
- Test keyboard behavior headlessly, as Zed does: `#[gpui::test]`, `studio_theme::init_test`
  (feature `test-support`), `add_window_view`, `simulate_input`, `simulate_keystrokes` and
  `advance_clock`, with `http_client::FakeHttpClient` behind the session
  (`type_table/src/dialogs.rs` tests). Offscreen snapshots (`crates/studio/examples/snapshot.rs`)
  check what a view looks like.
- Write files with `studio_settings::write_atomic`, never by truncating in place.
- A function used on one platform carries the same `cfg` as its caller, so the other platforms
  still build.

## Rules

- Follow the layers in `docs/architecture.md`. Feature crates never depend on each other.
- Only `graph_client` makes HTTP requests. Only `graph_store` turns responses into entities.
- Every new action goes in `studio_actions`, gets a default binding in `assets/keymaps/`, and
  appears in the palette.
- Copying from Zed is allowed (GPL). Keep the file's copyright notice and add a
  `// Forked from zed-industries/zed <path> @ <commit>` line. Keep forks small.
- No OmniGraph engine changes from this repo. Add the need to spec 012 with an ID, list it in
  `docs/deferred-engine-changes.md`, and design against today's API.
- Never add a second copy of GPUI (GPUI Kit, `gpui-pre`, `gpui-ce`, crates.io `gpui`).
- Never retry a write automatically. Never log tokens, query text or row data.
- Size nested rounded surfaces with `ds::radius::around`/`within`, never by hand: concentric
  corners, and Zed's buttons are always `radius::BUTTON` (spec 002, Visual language).
- Keep a keystroke from drawing more than it changed (`docs/drawing.md`): no field inside a
  large view, large parts as cached views made with their tab, `notify` only for state the
  view draws, and a draw-count test for every costly view.

## Specs

Specs follow a lean Spec Kit shape; see `specs/README.md`. One numbered folder per
feature with a `plan.md`; add `validation.md` when the log grows long. Keep folders after they
land.

## Commits and pull requests

Conventional Commits: `type(scope): description`, for example `feat(type_table): keyset
paging`. One logical change per commit.
