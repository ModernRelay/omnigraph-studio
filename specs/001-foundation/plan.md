# 001: Foundation

Status: in progress · 2026-10-11

## Problem

Studio needs a repository that builds one GPUI application on Zed's crates, with fixed pins, a
crate graph that keeps features apart, license and dependency rules that hold by construction,
CI on the platforms it ships to, and a way to sign and ship a macOS app. None of it exists yet.

## Principles

- I: only `graph_client` reaches the network; the engine is never linked.
- XI: Zed crates at one tag; one GPUI; GPL-3.0-or-later; forks keep notices.
- XII: every pin and claim below is checked against source.

## What exists today (checked)

- Zed `v1.23.2` is the newest stable release (2026-10-07), commit `c0199504b9cc`. Its
  `rust-toolchain.toml` pins Rust `1.98.1`.
- At that tag, `crates/ui/Cargo.toml` declares `GPL-3.0-or-later` and depends only on `gpui`,
  `component`, `theme`, `icons`, `menu`, `ui_macros`, `gpui_util` and small third-party
  crates. `theme` depends on `gpui`, `refineable`, `syntax_theme`, `collections` and serde.
- `crates/picker` depends on `project`, `workspace`, `db`, `language` and `settings`;
  `crates/workspace` depends on `client`, `project`, `db`, `remote`, `session`, `node_runtime`
  and more. These are forked, not depended on (docs/zed-pin.md).
- `theme::init(LoadThemes::JustBase, cx)` sets up the registry and default theme without
  settings (`crates/theme/src/theme.rs:119-145`). `ui` reads fonts and density through
  `theme::ThemeSettingsProvider`, registered with `theme::set_theme_settings_provider`
  (`crates/theme/src/theme_settings_provider.rs`).
- `gpui_platform::application()` returns a `gpui::Application`; the `font-kit` feature is
  required on macOS for text, `wayland` and `x11` select Linux backends
  (`crates/gpui_platform/Cargo.toml`, `src/gpui_platform.rs`).
- `http_client` (Apache-2.0) has no GPUI dependency and provides the `HttpClient` trait and a
  fake for tests; `reqwest_client` implements it.
- Zed's root `Cargo.toml` at the tag has a `[patch.crates-io]` table (tree-sitter-language,
  async-process, async-task, windows-capture, calloop, livekit, libwebrtc, notify, notify-types,
  webrtc-sys, scratch). Patches apply only in the workspace that declares them.
- Delta (`com.zed-industries.delta`, CLI 0.17.0) links a Zed commit that is not public and
  carries git checkouts of async-task, async-process, tree-sitter, font-kit and reqwest,
  consistent with it repeating Zed's patches.
- Omnigraph `v0.13.0`: `omnigraph-compiler` is MIT, edition 2024, "Zero Lance dependency"
  (`crates/omnigraph-compiler/Cargo.toml`). crates.io has `omnigraph-compiler` only up to 0.8.0.
- This machine had 7.1 GiB free when the repository was created, too little for a first build
  of Zed's GPUI crates; the first build is still to be done.

## Design

### Repository

```
LICENSE              GPL-3.0 text
AGENTS.md            agent map (CLAUDE.md links to it)
Cargo.toml           workspace, pins, patches, profiles
rust-toolchain.toml  1.98.1
deny.toml            licenses, one GPUI, allowed git sources
.specify/            constitution and plan template (Spec Kit layout)
specs/               one folder per feature
docs/                architecture, Zed pin, deferred engine changes
crates/              see docs/architecture.md for layers
assets/              keymaps, settings defaults, themes, icons, fonts (spec 002)
grammars/            tree-sitter-gq, tree-sitter-pg (spec 007)
script/              bundle-mac, bundle-linux, bundle-windows (later in this plan)
.github/workflows/   ci.yml now; release.yml with signing later
```

### Crate graph

As in `docs/architecture.md`. Every crate exists from the start with a short `lib.rs` that
states its responsibility, so the graph is visible before the code is.

### CI

`ci.yml` runs on macOS (Apple Silicon), Ubuntu and Windows (2025): `cargo fmt --check`,
`cargo clippy -D warnings` (with `--keep-going`, so one run reports every crate), `cargo test`
(with `--no-fail-fast`), and `cargo deny check`. On Windows git checks Zed out with long paths
through its own CLI. CI caches `~/.cargo/git` and `target/` keyed on `Cargo.lock`. A
`structure` job on Ubuntu runs `script/check-layers.py`, `script/check-design-tokens.py` and
`script/check-brand.py`, which refuses the product's name written other than Omnigraph in any
tracked text file but the unmodified upstream copies (the HTTP contract, the operator-config
schema, the agent skill's `upstream/`).

### Shipping (after 002)

- macOS: `script/bundle-mac` builds `Omnigraph Studio.app` (identifier
  `com.modernrelay.omnigraph-studio`, minimum macOS 11) and a DMG in `target/bundle`. With
  `STUDIO_SIGN_IDENTITY` it signs the app and DMG with a Developer ID, the hardened runtime and
  a secure timestamp; with `STUDIO_NOTARY_PROFILE` it notarizes with `notarytool` and staples.
  Without them the app is signed ad hoc. Entitlements (`resources/macos/studio.entitlements`)
  are only the JIT pair Zed's wasmtime needs. Signing needs a ModernRelay Apple Developer
  account. The icon is the Modern Relay mark (from ModernRelay/brand) in white on a black
  macOS-shaped tile; `script/make-icon.py` draws it into `resources/macos/AppIcon.icns` and
  `resources/linux/omnigraph-studio.png`, which the bundle scripts install. A release build
  compiles GPUI's Metal shaders at build time (`--no-default-features` drops the
  `runtime-shaders` default) when Xcode's Metal compiler is present; with only the Command Line
  Tools they compile when Studio starts, as in a debug build.
- Linux: `script/bundle-linux` builds a tarball (`bin/`, a `.desktop` entry, `LICENSE`).
  A release runs on glibc 2.35 (Ubuntu 22.04, Debian 12) and later: it is built on Ubuntu 22.04,
  and the script refuses a binary that needs a newer glibc. AppImage later.
- Windows: `script/bundle-windows`, run in Git Bash, builds a zip (`omnigraph-studio.exe`,
  `LICENSE`, `THIRD-PARTY-NOTICES.md`) for Windows 10 1809 and later, the first Windows with Unix
  sockets, which the agent's tool socket uses (`uds_windows`). A release build opens no console
  window. The executable is not signed, has no icon yet, and there is no installer. Settings live
  in `%APPDATA%\Omnigraph Studio`. `.gitattributes` checks text out with LF everywhere, so the
  files Studio embeds are the same bytes on every system. The agent panel says it is not
  available on Windows: its `omnigraph` wrapper is a shell script (spec 013).
- Every bundle carries `THIRD-PARTY-NOTICES.md` beside `LICENSE`: the licences of the fonts,
  icons and themes Studio embeds from Zed's assets, the themes Studio bundles itself
  (`assets/themes/LICENSES`: Vitesse Theme Refined and Vitesse, MIT), then every crate linked into the app on the
  three shipped targets. `script/generate-licenses` writes it with cargo-about 0.9.2, accepting
  the licences `deny.toml` allows; a crate whose licence cannot be read fails the run
  (`script/licenses/about.toml` clarifies `language_core`, which declares none). A release
  bundle needs it; a debug bundle has it when cargo-about is installed. CI's `deny` job
  generates it on every change.
- CI bundles the debug build on all three systems so the scripts keep working.
- Releases: pushing a tag `v<version>` that names the workspace version runs
  `.github/workflows/release.yml`. It bundles release builds for macOS (Apple silicon), Linux
  (x86_64 and arm64) and Windows (x86_64) and publishes a GitHub Release with the files, `SHA256SUMS` and install
  notes; 0.x releases are marked pre-release. The macOS build is signed and notarized when the
  workflow's Apple secrets are set, and signed ad hoc otherwise. Run by hand
  (`workflow_dispatch`), the workflow builds the same files and publishes nothing, to check a
  release before it is tagged.
- Homebrew cask in a ModernRelay tap. Updates: [spec 017](../017-auto-update/plan.md).

### Spikes that gate the MVP

| Spike | Goal | Passes when |
|---|---|---|
| S0 First build | `cargo run -p studio` opens a window with Zed's `ui` and `theme` | Builds on macOS and Linux; record clean build time, `target/` size and binary size |
| S1 Table | Zed's `ui` table with a forked CSV-preview engine, on a type with 1M nodes, staged edits | Frame p95 < 8 ms while scrolling; a page within 150 ms of scroll stop; 20 edits publish as one commit; < 400 MB |
| S2 Canvas | Custom `canvas` at 1k, 10k and 50k nodes, layout off the main thread | Pan and zoom p95 < 16 ms at 10k nodes and 30k edges; hover pick < 1 ms; 1k layout settles < 1 s |
| S4 Editor | Zed's `editor` with no project as the `.gq` editor, compiler-backed providers; a two-entity diff in a multibuffer | Diagnostics < 100 ms after typing stops on a 500-line file; build time and binary size recorded. If either is unacceptable, write Studio's own editor, as Delta did |
| S6 DAG and diff | Commit-graph lanes re-specified from Zed's property tests; per-type property diff | Property tests pass on 10k random DAGs; a 5,000-change commit opens < 300 ms |
| S7 Upgrade drill | Move the spikes to the next Zed tag | Breaks and hours recorded below |

## Decisions

| Decision | Rejected alternatives | Why |
|---|---|---|
| Zed crates from git at one release tag | GPUI Kit on `gpui-pre`; `gpui-ce`; crates.io `gpui` 0.2.2 | GPL accepted (2026-10-11), so Zed's `ui` and `theme` are usable; GPUI Kit is built on a different GPUI copy and cannot share a window with them; crates.io `gpui` is a year old |
| Release tag, not main | Tracking main | Main changes daily; tags are what Zed ships |
| Separate repository | A workspace inside Omnigraph's repo | Omnigraph's `deny.toml` forbids git dependencies, pins Rust 1.97.1, and its CLI binary is already 227 MB; Studio needs its own release line and signing |
| `omnigraph-compiler` by git tag | crates.io | crates.io stops at 0.8.0 |
| Every planned crate created now as a stub | Create crates when needed | The layer rules are visible and enforced from the first commit |

## Deferred

- A Windows installer, a signed executable and its icon.
- Auto-update, until there is a first signed release ([spec 017](../017-auto-update/plan.md)).
- Engine change E19 (publish `omnigraph-compiler` to crates.io again), [spec 012](../012-omnigraph-engine-changes/plan.md). Until then the
  compiler is a git dependency on the tag of Studio's oldest supported server,
  `edge-20261020-7fd9cc9` since 2026-10-20 (`edge-20261018-0a62bb7` and `edge-20261017-d883c49`
  before). It moves with that server, so the query editor checks text by the rules the server it
  targets applies: `T27`, `T59` and exact `@range` bounds came with the bump to `0a62bb7`, and
  `from commit "<id>"` in `branch create` (GQ 2.3) with this one. Schemas the server
  holds are read with its compatibility parser for accepted contracts, so a graph admitted
  before a rule tightened still opens.

## Validation log

| Date | Claim | Check | Result |
|---|---|---|---|
| 2026-10-17 | A Mac builds and runs Studio with CMake and the Command Line Tools, without Xcode | `cargo build` on macOS 27.0.1 (Apple silicon) with the Command Line Tools only (`xcrun -sdk macosx --find metal` finds nothing), first without CMake, then with CMake 4.4.4 on `PATH`; the `snapshot` example opening Person on `script/dev-server` (Omnigraph 0.14.0) | Without CMake the build stopped in `wasmtime-c-api-impl`'s build script, "failed to spawn `cmake`": Zed v1.23.2 `crates/language/Cargo.toml` turns on tree-sitter's `wasm` feature, and Zed's `docs/src/development/macos.md` lists CMake for it. CI runners have CMake, so CI never showed it. With CMake: built, `target/` 8.5 GB; the snapshot drew the Person table (200 rows, footer "contract 0.13") with its shaders compiled at launch |
| 2026-10-16 | Studio builds, tests and bundles on Windows | CI on `windows-2025` for PR #1: `cargo clippy --workspace --all-targets --keep-going -D warnings`, `cargo test --workspace --no-fail-fast`, `script/bundle-windows --debug` | All pass, the GPUI headless tests (`keymaps`, `settings_apply`) and the tool socket's tests among them; the zip `omnigraph-studio-0.2.0-windows-x86_64.zip` was written. Three fixes on the way: a test module's import unused on Windows, `Async` refusing `uds_windows` streams (the halves are `Unblock` now), and CRLF checkouts plus `\` separators in the skill test |
| 2026-10-11 | Newest stable Zed is v1.23.2 | `gh api repos/zed-industries/zed/releases` | v1.23.2, 2026-10-07; v1.24.1-pre is a prerelease |
| 2026-10-11 | Zed toolchain at the tag | `git show v1.23.2:rust-toolchain.toml` | `1.98.1` |
| 2026-10-11 | `ui` license and dependencies | `git show v1.23.2:crates/ui/Cargo.toml` | GPL-3.0-or-later; gpui, component, theme, icons, menu, ui_macros, gpui_util + third party |
| 2026-10-11 | `picker` and `workspace` are heavy | `git show v1.23.2:crates/{picker,workspace}/Cargo.toml` | picker → project, workspace, db, language; workspace → client, project, db, remote, session, … |
| 2026-10-11 | Theme starts without settings | `crates/theme/src/theme.rs`, `theme_settings_provider.rs` at the tag | `init(LoadThemes::JustBase, cx)`; provider trait + setter exported at crate root |
| 2026-10-11 | `ui::prelude` exports what the first window uses | `crates/ui/src/prelude.rs` at the tag | `v_flex`, `Label`, `LabelCommon`, `LabelSize`, `Color`, `Headline`, `ActiveTheme`, GPUI prelude |
| 2026-10-11 | `http_client` is GPUI-free | `crates/http_client/Cargo.toml` at the tag | anyhow, futures, http, serde, url, …; no gpui; Apache-2.0 |
| 2026-10-11 | Zed's git sources | grep `git =` in Zed's root and platform manifests at the tag | Recorded in `deny.toml` `allow-git` |
| 2026-10-11 | `omnigraph-compiler` at v0.13.0 | `git show v0.13.0:crates/omnigraph-compiler/Cargo.toml` (Omnigraph repo) | MIT, edition 2024, no Lance |
| 2026-10-11 | Workspace manifests are valid | `cargo metadata --no-deps --offline` | 17 members read |
| 2026-10-11 | Crate layers hold | `python3 script/check-layers.py` | `layers ok: 17 crates` |
| 2026-10-11 | Assets parse | JSON parse of `assets/**/*.json` | Keymaps and default settings valid |
| 2026-10-12 | `script/bundle-mac` produces a valid app and DMG | `script/bundle-mac --debug`; `codesign -dv`, `codesign -d --entitlements -`, `hdiutil verify` | App bundle (arm64), ad hoc signature valid and satisfying its designated requirement, two entitlements, DMG checksum valid (113 MB from the debug build). Developer ID signing and notarization not exercised: no identity on this machine |
| 2026-10-11 | First build | not run | Pending: 5.8 GiB free on the machine; run S0 with ≥ 25 GiB free |
| 2026-10-12 | S0 first build: clean build time, `target/` size, binary size | `cargo clean`, then `RUSTC_WRAPPER= cargo build --locked -p studio --bin omnigraph-studio` and the same with `--release` (no sccache); Apple M5 Pro, 18 cores, 48 GB, rustc 1.98.1 | Debug: 152 s, `target/` 7.6 GB, binary 472 MB. Release (thin LTO, one codegen unit, limited debug info, unstripped): 615 s, `target/release` 6.9 GB, binary 114 MB. With the sccache wrapper in `~/.cargo/config.toml` a warm-cache clean debug build took 53 s. Builds on macOS; Linux is left to CI |
| 2026-10-20 | `script/dev-server` refuses a server of another contract, not only a CLI | the script on a scratch data directory and port (`STUDIO_DEV_DATA`, `STUDIO_DEV_BIND=127.0.0.1:18787`): once with `OMNIGRAPH_SERVER_BIN` naming a stand-in that answers `/readyz` and `/healthz` with `omnigraph-http-api: 0.13`, once with `omnigraph-server` `0.14.0-edge+7fd9cc9` | The stand-in: "dev-server needs an omnigraph-server that speaks HTTP contract 0.14 (edge-20261020-7fd9cc9 or later); … answers contract 0.13 at http://127.0.0.1:18787/healthz (set OMNIGRAPH_SERVER_BIN)", exit 1, the server stopped by the script's trap. The edge server: "Serving http://127.0.0.1:18787 …", `/healthz` answering `0.14`; stopped after |
| 2026-10-21 | `script/dev-server` says a server exited, or the address is taken, rather than that it answers no contract | the script on scratch data directories and ports: `OMNIGRAPH_SERVER_BIN=/usr/bin/false` at `127.0.0.1:18791`; `127.0.0.1:18792` with `python3 -m http.server` already answering there; `omnigraph-server` `0.14.0-edge+7fd9cc9` at `127.0.0.1:18793` | The first: "dev-server: /usr/bin/false exited (status 1) before it served http://127.0.0.1:18791; its output is above (is the address in use?)", exit 1, at once rather than after 30 s; the second: "dev-server: something already answers at http://127.0.0.1:18792; stop it, or set STUDIO_DEV_BIND to another address", exit 1, before a server started; the edge server: "Serving http://127.0.0.1:18793 …", stopped after by the script's trap. Before, a server that exited was asked for its contract until the readiness wait ran out, then reported as one that "answers no contract", and one on a taken address as whatever answered there |
| 2026-10-22 | `script/dev` runs Studio on a dev server it starts and stops; `script/dev-server` seeds whatever `~/.omnigraph/config.yaml` holds | `script/dev-server --reset` with Omnigraph `0.14.0-edge+7fd9cc9` while `~/.omnigraph/config.yaml` held the `version: 1` operator format of later edge builds (`7fff9df`), before and after the CLI got a home of its own; `script/dev` with that build on scratch data and ports, once with the real `cargo`, then with a stand-in `cargo` that sleeps: on fresh data, interrupted by SIGINT to its process group, with a server already answering, and with `OMNIGRAPH_SERVER_BIN=/usr/bin/false` | Before: the first `cluster apply` stopped, "legacy operator routing is removed; run `omnigraph target import`". After: seeded and "Serving …". `script/dev` built and opened Studio once "Serving" appeared (Company's table, 25 rows, footer "contract 0.14"); the stand-in's exit stopped the server (exit 0, address free); SIGINT exited 130 with the server stopped; a running server was used and left running; the failed start printed dev-server's "/usr/bin/false exited (status 1) …" and exited 1 without starting Studio. Separately, the newer `7fff9df` CLI refuses the `target_config_version: 1` file the seed loads write, so dev-server needs `7fd9cc9` |
| 2026-10-22 | A signal stops `script/dev-server` with its own status, not a report of a server that exited | the script on a scratch data directory and port with Omnigraph `0.14.0-edge+7fd9cc9`: SIGTERM, and SIGINT to its process group, while it waited for a stand-in server that never answers `/readyz`; SIGTERM after "Serving …" with the real server; `script/dev` again on fresh data, interrupted, with a server already answering, and with a failed start | Before (trap on `EXIT INT TERM`, so the script carried on after the trap): SIGTERM printed "… stuck-server exited (status 143) before it served … (is the address in use?)" and every case exited 1. After (cleanup on `EXIT` only, which bash runs on a fatal signal): exit 143, 130 and 143 at once, no message, the server stopped and the CLI's home removed. `script/dev`: unchanged, exit 0, 130, the running server left up, 1 |
| 2026-10-22 | The product's name is written Omnigraph in every tracked file but the upstream copies, and CI refuses another form | `script/check-brand.py` before and after replacing the capital-G form in 79 files; the script in a scratch repository holding each wrong form (capital G, a space, a hyphen before either case), the right ones (`Omnigraph`, `omnigraph`, `OMNIGRAPH_HOME`), a file under each upstream path, a symlink and a binary file; `cargo fmt --all --check` and `cargo test --workspace --no-fail-fast` after | Before: 323 misspellings. After: "brand ok: 335 files", the upstream copies unchanged. The scratch repository: exactly the four wrong lines, exit 1. fmt clean (both spellings are nine letters, so nothing reflowed); tests 885 passed, 0 failed, 5 ignored |

## Upgrade drills

None yet.

## Corrections

- 2026-10-13 · The first v0.1.0 release (built from `f7f4ba6`) published three defects, and
  was deleted with no downloads. Its Linux builds needed glibc 2.39 (built on Ubuntu 24.04;
  `pidfd_spawnp` and `pidfd_getpid` are versioned `GLIBC_2.39` and not weak), so they would not
  start on Ubuntu 22.04 or Debian 12. Its macOS build kept the `runtime-shaders` default, though
  the comment on the feature said releases drop it. No download carried notices for the
  third-party licences. Each is now handled in the bundle scripts and the workflow, as above.
