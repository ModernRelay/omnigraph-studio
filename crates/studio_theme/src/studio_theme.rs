//! Zed's settings, themes and assets, set up for Studio, plus graph tokens (node-type palette,
//! edge and diff colors).
//!
//! Layer 1 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
//! Studio uses Zed's `editor` for every text input (spike S4), and the editor reads Zed's
//! settings store and `theme_settings`. So Studio runs Zed's own bootstrap here: the release
//! channel, the settings store with Zed's defaults, the bundled themes and fonts.

use std::borrow::Cow;

use gpui::{App, AssetSource, Hsla, SharedString};

mod icons;

/// Themes Studio bundles beside Zed's, by asset path (`assets/themes/` in this repository, with
/// their notices in `assets/themes/LICENSES`). Zed's theme loader lists `themes/` and loads each
/// family, so these are registered with Zed's own.
const STUDIO_THEMES: &[(&str, &[u8])] = &[(
    "themes/vitesse-refined/vitesse-theme-refined.json",
    include_bytes!("../../../assets/themes/vitesse-refined/vitesse-theme-refined.json"),
)];

/// Zed's assets (fonts, icons, themes, keymaps), Studio's own themes, and Studio's icons in
/// place of Zed's (`icons`).
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some(svg) = icons::nucleo(path) {
            return Ok(Some(Cow::Borrowed(svg)));
        }
        match STUDIO_THEMES.iter().find(|(own, _)| *own == path) {
            Some((_, bytes)) => Ok(Some(Cow::Borrowed(bytes))),
            None => assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut list = assets::Assets.list(path)?;
        list.extend(
            STUDIO_THEMES
                .iter()
                .filter(|(own, _)| own.starts_with(path))
                .map(|(own, _)| SharedString::from(*own)),
        );
        Ok(list)
    }
}

impl Assets {
    /// Loads Zed's bundled fonts into the text system.
    pub fn load_fonts(&self, cx: &App) -> gpui::Result<()> {
        assets::Assets.load_fonts(cx)
    }
}

/// Sets up the release channel and Zed's settings store with Zed's defaults. Put the user's
/// settings into the store next, then call [`init_themes`].
pub fn init_settings_store(cx: &mut App) {
    release_channel::init_test(
        semver::Version::new(0, 1, 0),
        release_channel::ReleaseChannel::Dev,
        cx,
    );
    settings::init(cx);
}

/// Loads the bundled themes and fonts and selects the theme the settings store names. Must run
/// before any window opens.
///
/// The first theme is read from the store here, not from a later store notification: GPUI's
/// launch callback runs without flushing effects, so a notification sent during launch reaches
/// `theme_settings`' observer only if it arrives after the observer is active, and it does not.
/// Zed's own `main` orders its bootstrap the same way.
pub fn init_themes(cx: &mut App) {
    theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);
    if let Err(error) = Assets.load_fonts(cx) {
        log::warn!("Studio could not load its bundled fonts: {error}");
    }
}

/// Zed's settings store and its base theme, without fonts or bundled themes: enough for a test
/// window to draw Studio's views and editors.
#[cfg(any(test, feature = "test-support"))]
pub fn init_test(cx: &mut App) {
    init_settings_store(cx);
    theme_settings::init(theme::LoadThemes::JustBase, cx);
}

/// A stable color for a node or edge type, derived from its name so the same type has the same
/// color in tables, the tree and the canvas.
pub fn type_color(type_name: &str) -> Hsla {
    // Eight muted tints, chosen by a hash of the name: grey, slate blue, green, terracotta,
    // violet, brick, teal, ochre.
    const TINTS: [u32; 8] = [
        0x6b7280, 0x3f6aa8, 0x467350, 0xb0663a, 0x7a5ba8, 0xa55651, 0x2f7d86, 0x8a6d1f,
    ];
    let hash = type_name.encode_utf16().fold(0_u32, |h, unit| {
        h.wrapping_mul(31).wrapping_add(u32::from(unit))
    });
    gpui::rgb(TINTS[(hash % 8) as usize]).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Studio's themes are listed with Zed's, so Zed's loader registers them, and each parses
    /// as a theme family with its themes named.
    #[test]
    fn studio_themes_are_listed_and_read_as_families() {
        let listed = Assets.list("themes/").unwrap();
        assert!(
            listed.iter().any(|path| path.ends_with("one/one.json")),
            "Zed's"
        );
        for (path, bytes) in STUDIO_THEMES {
            assert!(listed.iter().any(|listed| listed == path), "{path} listed");
            assert_eq!(Assets.load(path).unwrap().as_deref(), Some(*bytes));
            let family: serde_json::Value = serde_json::from_slice(bytes).expect(path);
            assert!(
                family["themes"]
                    .as_array()
                    .is_some_and(|themes| !themes.is_empty())
            );
        }
        let names: Vec<String> = STUDIO_THEMES
            .iter()
            .flat_map(|(_, bytes)| {
                let family: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                family["themes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|theme| theme["name"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(
            names.iter().any(|name| name == "Vitesse Refined Light"),
            "{names:?}"
        );
    }

    #[test]
    fn a_type_name_always_gets_the_same_tint() {
        assert_eq!(type_color("Person"), type_color("Person"));
        // Project slate blue, Note green, Topic ochre, Person brick.
        assert_eq!(type_color("Project"), gpui::rgb(0x3f6aa8).into());
        assert_eq!(type_color("Note"), gpui::rgb(0x467350).into());
        assert_eq!(type_color("Topic"), gpui::rgb(0x8a6d1f).into());
        assert_eq!(type_color("Person"), gpui::rgb(0xa55651).into());
    }
}
