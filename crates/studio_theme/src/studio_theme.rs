//! Zed's settings, themes and assets, set up for Studio, plus graph tokens (node-type palette,
//! edge and diff colors).
//!
//! Layer 1 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
//! Studio uses Zed's `editor` for every text input (spike S4), and the editor reads Zed's
//! settings store and `theme_settings`. So Studio runs Zed's own bootstrap here: the release
//! channel, the settings store with Zed's defaults, the bundled themes and fonts.

use gpui::{App, Hsla, hsla};

pub use assets::Assets;

/// Sets up Zed's settings store, themes and fonts. Must run before any window opens.
pub fn init(cx: &mut App) {
    release_channel::init_test(
        semver::Version::new(0, 1, 0),
        release_channel::ReleaseChannel::Dev,
        cx,
    );
    settings::init(cx);
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
    let hash = type_name.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    let hue = (hash % 360) as f32 / 360.;
    hsla(hue, 0.55, 0.55, 1.)
}

#[cfg(test)]
mod tests {
    use super::type_color;

    #[test]
    fn type_colors_are_stable_and_distinct() {
        assert_eq!(type_color("Person"), type_color("Person"));
        assert_ne!(type_color("Person"), type_color("Company"));
    }
}
