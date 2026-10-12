//! Zed's settings, themes and assets, set up for Studio, plus graph tokens (node-type palette,
//! edge and diff colors).
//!
//! Layer 1 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
//! Studio uses Zed's `editor` for every text input (spike S4), and the editor reads Zed's
//! settings store and `theme_settings`. So Studio runs Zed's own bootstrap here: the release
//! channel, the settings store with Zed's defaults, the bundled themes and fonts.

use gpui::{App, Hsla};

pub use assets::Assets;

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
        log_font_error(&error);
    }
}

fn log_font_error(error: &dyn std::fmt::Display) {
    eprintln!("Studio could not load its bundled fonts: {error}");
}

/// A stable color for a node or edge type, derived from its name so the same type has the same
/// color in tables, the tree and the canvas.
pub fn type_color(type_name: &str) -> Hsla {
    // Tower's eight muted tints (lib/ego-layout `TYPE_TINTS`), chosen by the same hash, so a type
    // reads the same in both: grey, slate blue, green, terracotta, violet, brick, teal, ochre.
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
    use super::type_color;

    #[test]
    fn type_colors_are_towers_tints() {
        assert_eq!(type_color("Person"), type_color("Person"));
        // As Tower draws them: Organization terracotta, Criterion slate blue, ICP teal.
        assert_eq!(type_color("Organization"), gpui::rgb(0xb0663a).into());
        assert_eq!(type_color("Criterion"), gpui::rgb(0x3f6aa8).into());
        assert_eq!(type_color("ICP"), gpui::rgb(0x2f7d86).into());
    }
}
