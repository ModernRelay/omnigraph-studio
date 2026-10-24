# The Zed pin

Studio depends on Zed's crates from git, all at one release tag (constitution XI). Zed's crates
are not published to crates.io, and Zed's main branch changes daily, so the pin moves only on
purpose.

**Current pin:** `v1.23.2` (commit `c0199504b9cce79bc27524950022ed701bc3ebd5`, released
2026-10-07). Rust `1.98.1`, Zed's toolchain at that tag.

## What Studio takes from Zed

| Treatment | Crates | License |
|---|---|---|
| Depend | `gpui`, `gpui_platform`, `gpui_tokio`, `refineable`, `sum_tree`, `collections`, `util`, `http_client`, `reqwest_client` | Apache-2.0 |
| Depend, at the revision and features Zed's workspace pins | `reqwest` (`zed-reqwest`, Zed's fork) | MIT or Apache-2.0 |
| Depend | `ui`, `theme`, `component`, `icons`, `menu` | GPL-3.0-or-later |
| Depend (spike S4 passed) | `editor`, `multi_buffer`, `language`, `settings`, `theme_settings`, `release_channel`, `assets` | GPL-3.0-or-later |
| Fork and trim into Studio crates | `workspace`, `picker`, `command_palette`, `settings_ui`, `tabular_data_preview`, `git_ui` (commit graph), `markdown` | GPL-3.0-or-later |

`ui` depends only on `gpui`, `component`, `theme`, `icons`, `menu` and small helper crates.
`picker`, `workspace`, `command_palette` and `markdown` depend on Zed's project, client,
database or language crates, which is why they are forked rather than depended on. Delta, Zed's
own second app, makes the same cut: it links `gpui`, `ui`, `theme` and `component` and ships
its own editor, picker, panes and settings.

## Starting Zed's settings and theme

Zed's `editor` reads Zed's settings store and `theme_settings`, so Studio runs Zed's own
bootstrap in `studio::init`, in this order:

1. `studio_theme::init_settings_store`: the release channel and the settings store with Zed's
   defaults.
2. `studio_settings::init`: Studio's `settings.json`, whose theme and font keys go into the
   store as Zed user settings.
3. `studio_theme::init_themes`: the bundled themes and fonts; `theme_settings::init` selects
   the first theme from the store.

The order matters. GPUI's launch callback (`Application::run`) and
`HeadlessAppContext::update` run without flushing effects, and `theme_settings` reacts to later
store changes through a global observer that becomes active only when effects flush. A store
change made during launch is therefore never seen, and the first theme must already be in the
store when `theme_settings::init` runs, as in Zed's `main`. Changes after launch arrive in
flushed updates (input handlers, `AsyncApp::update`) and reach the observer.
`crates/studio/tests/settings_apply.rs` guards both paths.

## Bumping the pin

1. Pick the newest stable tag (`gh api repos/zed-industries/zed/releases`, not a `-pre`).
2. Update every Zed entry in `[workspace.dependencies]` to the new tag.
3. Copy Zed's `[patch.crates-io]` entries that Studio's graph reaches into Studio's
   `Cargo.toml`, and Zed's `rust-toolchain.toml` channel into Studio's. Patches do not carry
   over from a dependency's workspace.
4. Check `deny.toml`'s `allow-git` list against Zed's git dependencies at the new tag.
5. For every forked file, compare it with its source at the new tag and record the new commit
   in its header, or note why it stays behind.
6. Build, test and run the screenshot tests on macOS and Linux. Record the number of breaks and
   the hours spent in `specs/001-foundation/plan.md` under "Upgrade drills".

Bump about once a month, or sooner for a fix Studio needs.
