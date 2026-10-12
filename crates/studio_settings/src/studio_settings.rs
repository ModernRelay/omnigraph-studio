//! Studio's settings document (spec 002): built-in defaults, then the user's `settings.json` in
//! Studio's data directory. Theme and font keys pass through to Zed's settings store, which Zed's
//! `ui`, `theme_settings` and `editor` read; Studio's own keys live under `studio`.
//!
//! Layer 1 (`docs/architecture.md`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui::{App, BorrowAppContext, Global};
use serde::{Deserialize, Serialize};
use settings::{ParseStatus, SettingsStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeChoice {
    pub mode: ThemeMode,
    pub light: String,
    pub dark: String,
}

impl Default for ThemeChoice {
    fn default() -> Self {
        Self {
            mode: ThemeMode::System,
            light: "One Light".into(),
            dark: "One Dark".into(),
        }
    }
}

/// How the window's surfaces are drawn (spec 002, "Visual language").
/// Spacing in Zed's `ui` components.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    Compact,
    #[default]
    Default,
    Comfortable,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowStyle {
    /// Columns on solid panels, split by hairlines.
    #[default]
    Flat,
    /// The document and inspector float as translucent sheets over a faint wash.
    Glass,
}

/// Studio's own settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioOptions {
    /// Flat columns, or glass sheets.
    pub window_style: WindowStyle,
    /// Rows per page in type tables.
    pub type_table_page_size: u32,
    /// Neighbours per page in the relationship tree.
    pub tree_page_size: u32,
    /// Most nodes the canvas draws before offering a table instead.
    pub canvas_max_nodes: u32,
    /// Seconds between server health checks.
    pub health_check_seconds: u64,
}

impl Default for StudioOptions {
    fn default() -> Self {
        Self {
            window_style: WindowStyle::default(),
            type_table_page_size: 100,
            tree_page_size: 50,
            canvas_max_nodes: 2000,
            health_check_seconds: 30,
        }
    }
}

/// The agent the agent panel runs (spec 011): a program and its arguments, and extra
/// environment variables. Empty until the person chooses one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// The whole settings document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioSettings {
    pub theme: ThemeChoice,
    /// The interface font; `null` keeps Zed's.
    pub ui_font_family: Option<String>,
    pub ui_font_size: f32,
    /// The query editor's font; `null` keeps Zed's.
    pub buffer_font_family: Option<String>,
    pub buffer_font_size: f32,
    pub ui_density: Density,
    pub studio: StudioOptions,
    pub agent: AgentSettings,
}

impl Default for StudioSettings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::default(),
            ui_font_family: None,
            ui_font_size: 16.,
            buffer_font_family: None,
            buffer_font_size: 14.,
            ui_density: Density::default(),
            studio: StudioOptions::default(),
            agent: AgentSettings::default(),
        }
    }
}

impl Global for StudioSettings {}

struct SettingsFileState {
    modified: Option<SystemTime>,
    error: Option<String>,
}

impl Global for SettingsFileState {}

/// Studio's data directory, for every crate: `OMNIGRAPH_STUDIO_DATA`, else
/// `~/Library/Application Support/OmniGraph Studio` on macOS, `$XDG_CONFIG_HOME/omnigraph-studio`
/// elsewhere.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OMNIGRAPH_STUDIO_DATA")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/OmniGraph Studio")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("omnigraph-studio")
    }
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

impl StudioSettings {
    pub fn get(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// Parses a settings file; unknown keys are ignored, missing keys take defaults.
    pub fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|error| error.to_string())
    }

    /// The keys Zed's settings store understands.
    fn zed_user_settings(&self) -> String {
        let mode = match self.theme.mode {
            ThemeMode::System => "system",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        };
        let density = match self.ui_density {
            Density::Compact => "compact",
            Density::Default => "default",
            Density::Comfortable => "comfortable",
        };
        let mut zed = serde_json::json!({
            "theme": { "mode": mode, "light": self.theme.light, "dark": self.theme.dark },
            "ui_font_size": self.ui_font_size,
            "buffer_font_size": self.buffer_font_size,
            "unstable.ui_density": density,
        });
        if let Some(family) = &self.ui_font_family {
            zed["ui_font_family"] = family.clone().into();
        }
        if let Some(family) = &self.buffer_font_family {
            zed["buffer_font_family"] = family.clone().into();
        }
        zed.to_string()
    }
}

/// Loads the settings file and applies it. Call after `settings::init`.
pub fn init(cx: &mut App) {
    let (settings, modified, error) = read_file();
    cx.set_global(SettingsFileState { modified, error });
    apply(settings, cx);
    watch(cx);
}

/// The last problem reading `settings.json`, for a notification.
pub fn file_error(cx: &App) -> Option<String> {
    cx.global::<SettingsFileState>().error.clone()
}

fn read_file() -> (StudioSettings, Option<SystemTime>, Option<String>) {
    let path = settings_path();
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    match std::fs::read_to_string(&path) {
        Ok(text) => match StudioSettings::parse(&text) {
            Ok(settings) => (settings, modified, None),
            Err(error) => (
                StudioSettings::default(),
                modified,
                Some(format!("{} could not be read: {error}", path.display())),
            ),
        },
        Err(_) => (StudioSettings::default(), modified, None),
    }
}

fn apply(settings: StudioSettings, cx: &mut App) {
    let zed = settings.zed_user_settings();
    cx.set_global(settings);
    let status = cx.update_global::<SettingsStore, _>(|store, cx| {
        store.set_user_settings(&zed, cx).parse_status
    });
    if let ParseStatus::Failed { error } = status {
        cx.global_mut::<SettingsFileState>().error =
            Some(format!("Theme and font settings were not applied: {error}"));
    }
    cx.refresh_windows();
}

/// Writes the settings file and applies it.
pub fn update(cx: &mut App, f: impl FnOnce(&mut StudioSettings)) {
    let mut settings = StudioSettings::get(cx).clone();
    f(&mut settings);
    let path = settings_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let written = serde_json::to_string_pretty(&settings)
        .map_err(|e| e.to_string())
        .and_then(|text| std::fs::write(&path, text).map_err(|e| e.to_string()));
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    cx.set_global(SettingsFileState {
        modified,
        error: written.err(),
    });
    apply(settings, cx);
}

/// Reloads the file when another program changes it (checked every two seconds).
fn watch(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let path = settings_path();
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            let changed = cx.update(|cx| cx.global::<SettingsFileState>().modified != modified);
            if changed {
                let (settings, modified, error) = read_file();
                cx.update(|cx| {
                    cx.set_global(SettingsFileState { modified, error });
                    apply(settings, cx);
                });
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `assets/settings/default.json` documents the defaults (spec 002); it must say what the
    /// code does.
    #[test]
    fn the_documented_defaults_are_the_defaults() {
        let text = include_str!("../../../assets/settings/default.json");
        assert_eq!(
            StudioSettings::parse(text).unwrap(),
            StudioSettings::default()
        );
        let documented: serde_json::Value = serde_json::from_str(text).unwrap();
        let actual = serde_json::to_value(StudioSettings::default()).unwrap();
        assert_eq!(documented, actual, "default.json names every setting");
    }

    #[test]
    fn fonts_and_density_reach_zed() {
        let mut settings = StudioSettings::default();
        let zed: serde_json::Value = serde_json::from_str(&settings.zed_user_settings()).unwrap();
        assert!(
            zed.get("ui_font_family").is_none(),
            "unset keeps Zed's font"
        );
        assert_eq!(zed["unstable.ui_density"], "default");
        settings.ui_font_family = Some("IBM Plex Sans".into());
        settings.ui_density = Density::Compact;
        let zed: serde_json::Value = serde_json::from_str(&settings.zed_user_settings()).unwrap();
        assert_eq!(zed["ui_font_family"], "IBM Plex Sans");
        assert_eq!(zed["unstable.ui_density"], "compact");
    }

    #[test]
    fn missing_keys_take_defaults_and_unknown_keys_are_ignored() {
        let settings = StudioSettings::parse(
            r#"{"ui_font_size": 13, "extra": 1, "studio": {"tree_page_size": 20}}"#,
        )
        .unwrap();
        assert_eq!(settings.ui_font_size, 13.);
        assert_eq!(settings.studio.tree_page_size, 20);
        assert_eq!(settings.studio.type_table_page_size, 100);
        assert_eq!(settings.theme, ThemeChoice::default());
    }

    #[test]
    fn zed_keys_are_the_only_ones_passed_through() {
        let json: serde_json::Value =
            serde_json::from_str(&StudioSettings::default().zed_user_settings()).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "buffer_font_size",
                "theme",
                "ui_font_size",
                "unstable.ui_density"
            ]
        );
    }
}
