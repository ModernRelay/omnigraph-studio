//! Theme setup on Zed's `theme` crate, the `ThemeSettingsProvider` Zed's `ui` reads, and graph
//! tokens (node-type palette, edge and diff colors).
//!
//! Layer 1 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
//! Zed's `theme::init(LoadThemes::JustBase, cx)` sets up the registry and the default theme
//! without Zed's settings crates. Zed's `ui` components then read fonts, sizes and density
//! through the provider registered here (`theme/src/theme_settings_provider.rs` at Zed
//! v1.23.2).

use gpui::{App, Font, Pixels, font, px};
use theme::{LoadThemes, ThemeSettingsProvider, UiDensity};

/// Sets up Zed's theme system with Studio's font and density settings.
///
/// Must run before any window opens: Zed's `ui` components panic without a registered
/// provider.
pub fn init(cx: &mut App) {
    theme::init(LoadThemes::JustBase, cx);
    theme::set_theme_settings_provider(Box::new(StudioThemeSettings::default()), cx);
}

/// Fonts, sizes and density for Zed's `ui`. Fixed values until `studio_settings` supplies them
/// (spec 002).
struct StudioThemeSettings {
    ui_font: Font,
    buffer_font: Font,
    ui_font_size: Pixels,
    buffer_font_size: Pixels,
    density: UiDensity,
}

impl Default for StudioThemeSettings {
    fn default() -> Self {
        Self {
            ui_font: font(".SystemUIFont"),
            buffer_font: font("Menlo"),
            ui_font_size: px(14.),
            buffer_font_size: px(13.),
            density: UiDensity::Default,
        }
    }
}

impl ThemeSettingsProvider for StudioThemeSettings {
    fn ui_font<'a>(&'a self, _cx: &'a App) -> &'a Font {
        &self.ui_font
    }

    fn buffer_font<'a>(&'a self, _cx: &'a App) -> &'a Font {
        &self.buffer_font
    }

    fn ui_font_size(&self, _cx: &App) -> Pixels {
        self.ui_font_size
    }

    fn buffer_font_size(&self, _cx: &App) -> Pixels {
        self.buffer_font_size
    }

    fn ui_density(&self, _cx: &App) -> UiDensity {
        self.density
    }
}
