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
| Depend | `ui`, `theme`, `component`, `icons`, `menu` | GPL-3.0-or-later |
| Depend if spike S4 passes | `editor`, `multi_buffer`, `buffer_diff`, `language` | GPL-3.0-or-later |
| Fork and trim into Studio crates | `workspace`, `picker`, `command_palette`, `settings`, `settings_ui`, `tabular_data_preview`, `git_ui` (commit graph), `markdown` | GPL-3.0-or-later |

`ui` depends only on `gpui`, `component`, `theme`, `icons`, `menu` and small helper crates.
`picker`, `workspace`, `command_palette` and `markdown` depend on Zed's project, client,
database or language crates, which is why they are forked rather than depended on. Delta, Zed's
own second app, makes the same cut: it links `gpui`, `ui`, `theme` and `component` and ships
its own editor, picker, panes and settings.

## Starting Zed's theme without Zed's settings

`theme::init(LoadThemes::JustBase, cx)` sets up the registry and the default theme without
touching settings. Zed's `ui` components then read fonts, font sizes and density through a
`theme::ThemeSettingsProvider` registered with `theme::set_theme_settings_provider`, which
`studio_theme` implements. Studio never links Zed's `settings` or `theme_settings`.

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
