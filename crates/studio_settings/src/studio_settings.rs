//! Studio's settings document (spec 002): built-in defaults, then the user's `settings.json` in
//! Studio's data directory. Theme and font keys pass through to Zed's settings store, which Zed's
//! `ui`, `theme_settings` and `editor` read; Studio's own keys live under `studio`.
//!
//! Layer 1 (`docs/architecture.md`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui::{App, BorrowAppContext, Global};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{ParseStatus, SettingsStore};

mod fields;
mod files;

pub use files::{Access, set_aside, write_atomic};

pub use fields::{Choice, FieldKind, Scope, SettingField, fields, sections};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ThemeChoice {
    /// Follow the system, or always use the light or dark theme.
    #[schemars(title = "Theme")]
    pub mode: ThemeMode,
    /// The theme used in light appearance.
    #[schemars(title = "Light theme", extend("x-control" = "theme"))]
    pub light: String,
    /// The theme used in dark appearance.
    #[schemars(title = "Dark theme", extend("x-control" = "theme"))]
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

/// Spacing in Zed's `ui` components.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    Compact,
    #[default]
    Default,
    Comfortable,
}

/// How the window's surfaces are drawn (spec 002, "Visual language").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowStyle {
    /// Columns on solid panels, split by hairlines.
    #[default]
    Flat,
    /// The document and inspector float as translucent sheets over a faint wash.
    Glass,
}

/// Studio's own settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct StudioOptions {
    /// Columns on solid panels, or the document as a sheet over a faint wash.
    #[schemars(title = "Window style", extend("x-section" = "Appearance"))]
    pub window_style: WindowStyle,
    /// Rows read per page in type tables.
    #[schemars(
        title = "Table page size",
        range(min = 25, max = 1000),
        extend("x-section" = "Browsing", "x-step" = 25, "x-per-graph" = true)
    )]
    pub type_table_page_size: u32,
    /// Neighbours shown per page in the relationship tree.
    #[schemars(
        title = "Relationship page size",
        range(min = 10, max = 500),
        extend("x-section" = "Browsing", "x-step" = 10, "x-per-graph" = true)
    )]
    pub tree_page_size: u32,
    /// Most nodes the canvas draws before offering a table instead.
    #[schemars(
        title = "Canvas node limit",
        range(min = 500, max = 20000),
        extend("x-section" = "Browsing", "x-step" = 500, "x-per-graph" = true)
    )]
    pub canvas_max_nodes: u32,
    /// Seconds between checks that the server still answers.
    #[schemars(
        title = "Health check interval",
        range(min = 5, max = 600),
        extend("x-section" = "Connection", "x-step" = 5)
    )]
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

/// One graph's own values for the settings marked `x-per-graph`, over `studio` (spec 002),
/// and its saved table views (spec 004). Each value is unset unless the person chose it for
/// this graph.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct GraphOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_table_page_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree_page_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canvas_max_nodes: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<SavedView>,
}

/// A named filter, order, grouping and set of columns for one type's table (spec 004).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SavedView {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub filters: Vec<SavedFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<SavedSort>,
    /// The property the rows are grouped by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
    /// Column keys the table does not show.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<String>,
    /// Column keys in the order the table shows them; empty is schema order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
}

/// A filter as the table holds it: a property or relationship key, an operator's key
/// (`gq_language::FilterOp::key`) and its value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SavedFilter {
    pub property: String,
    pub op: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SavedSort {
    pub property: String,
    pub descending: bool,
}

impl GraphOverrides {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The browsing limits in effect for one graph: its overrides, else `studio`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphOptions {
    pub type_table_page_size: u32,
    pub tree_page_size: u32,
    pub canvas_max_nodes: u32,
}

/// The `graphs` key for a graph on a server: `<server name>/<graph id>`, as the settings tab
/// writes it.
pub fn graph_key(server: &str, graph: &str) -> String {
    format!("{server}/{graph}")
}

/// The agent the agent panel runs (spec 011): a program and its arguments, and extra
/// environment variables. Empty until the person chooses one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AgentSettings {
    /// The program the agent panel runs over the Agent Client Protocol.
    #[schemars(title = "Agent", extend("x-control" = "agent"))]
    pub command: Vec<String>,
    /// Extra environment for the agent; edited in `settings.json`.
    #[schemars(extend("x-hidden" = true))]
    pub env: BTreeMap<String, String>,
    // Spec 013. The doc comments below are the settings tab's descriptions.
    /// Hold each write the agent makes until you allow it. Off: writes to the agent's own branch
    /// go through, each shown as a card.
    #[schemars(title = "Ask before writes")]
    pub ask_before_writes: bool,
    /// The omnigraph program the agent runs. Empty: Studio finds it on PATH or in the usual
    /// install folders.
    #[schemars(title = "OmniGraph CLI")]
    pub omnigraph_cli: Option<String>,
}

/// The whole settings document. Each field's doc comment is its row's description in the
/// settings tab, and its `schemars` attributes give the title, section, range and control
/// ([`fields`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct StudioSettings {
    #[schemars(extend("x-section" = "Appearance"))]
    pub theme: ThemeChoice,
    /// The font for menus, tables and panels. Default keeps Zed's.
    #[schemars(
        title = "Interface font",
        extend("x-section" = "Appearance", "x-control" = "font")
    )]
    pub ui_font_family: Option<String>,
    /// Text size for menus, tables and panels.
    #[schemars(
        title = "Interface font size",
        range(min = 10, max = 24),
        extend("x-section" = "Appearance", "x-step" = 1)
    )]
    pub ui_font_size: f32,
    /// The font of the query editor. Default keeps Zed's.
    #[schemars(
        title = "Editor font",
        extend("x-section" = "Appearance", "x-control" = "font")
    )]
    pub buffer_font_family: Option<String>,
    /// Text size in the query editor.
    #[schemars(
        title = "Editor font size",
        range(min = 10, max = 24),
        extend("x-section" = "Appearance", "x-step" = 1)
    )]
    pub buffer_font_size: f32,
    /// Spacing in lists, buttons and panels.
    #[schemars(title = "Density", extend("x-section" = "Appearance"))]
    pub ui_density: Density,
    pub studio: StudioOptions,
    #[schemars(extend("x-section" = "Agent"))]
    pub agent: AgentSettings,
    /// Per-graph overrides, keyed by [`graph_key`]; edited in the settings tab's graph scope.
    #[schemars(extend("x-hidden" = true))]
    pub graphs: BTreeMap<String, GraphOverrides>,
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
            graphs: BTreeMap::new(),
        }
    }
}

impl Global for StudioSettings {}

struct SettingsFileState {
    modified: Option<SystemTime>,
    error: Option<String>,
    /// The file exists but does not parse, so Studio must not overwrite it.
    unreadable: bool,
}

impl Global for SettingsFileState {}

/// Studio's data directory, for every crate: `OMNIGRAPH_STUDIO_DATA`, else
/// `~/Library/Application Support/OmniGraph Studio` on macOS, `$XDG_CONFIG_HOME/omnigraph-studio`
/// elsewhere. A binary run from a Cargo build directory (`cargo run`, an example) uses a
/// sibling directory ending in "Dev" instead, so a development run never rewrites the tabs,
/// drafts and conversations of the app the person uses. A bundled app is not a development run,
/// whichever profile built it.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OMNIGRAPH_STUDIO_DATA")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let dev = std::env::current_exe().is_ok_and(|exe| is_build_output(&exe));
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support").join(if dev {
            "OmniGraph Studio Dev"
        } else {
            "OmniGraph Studio"
        })
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join(if dev {
                "omnigraph-studio-dev"
            } else {
                "omnigraph-studio"
            })
    }
}

/// Whether an executable is Cargo's build output rather than an installed app: it sits in a
/// profile directory (or its `examples`) under a `target` directory, and not in a macOS bundle.
fn is_build_output(exe: &std::path::Path) -> bool {
    let parts: Vec<&std::ffi::OsStr> = exe.iter().collect();
    if parts
        .windows(3)
        .any(|w| w[0].to_string_lossy().ends_with(".app") && w[1] == "Contents" && w[2] == "MacOS")
    {
        return false;
    }
    let mut dirs = exe.ancestors().skip(1);
    let parent = dirs.next();
    let profile = match parent.and_then(|dir| dir.file_name()) {
        Some(name) if name == "examples" || name == "deps" => dirs.next(),
        _ => parent,
    };
    profile
        .and_then(|profile| profile.parent())
        .and_then(|target| target.file_name())
        .is_some_and(|name| name == "target")
}

/// Where a graph's saved queries live (spec 007): one folder per [`graph_key`] under the data
/// directory, its name made safe for any file system.
pub fn queries_dir(graph: &str) -> PathBuf {
    let safe: String = graph
        .chars()
        .map(|c| match c {
            '/' => '~',
            c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') => c,
            _ => '_',
        })
        .collect();
    data_dir().join("queries").join(safe)
}

/// A graph's saved `.gq` files, the most recently changed first.
pub fn saved_queries(graph: &str) -> Vec<(PathBuf, SystemTime)> {
    let Ok(entries) = std::fs::read_dir(queries_dir(graph)) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, SystemTime)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "gq"))
        .map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (path, modified)
        })
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    files
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

impl StudioSettings {
    pub fn get(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// The views saved for one type of a graph (key from [`graph_key`]).
    pub fn views_for(&self, graph: &str, type_name: &str) -> Vec<SavedView> {
        self.graphs
            .get(graph)
            .map(|overrides| {
                overrides
                    .views
                    .iter()
                    .filter(|view| view.type_name == type_name)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Saves `view` for a graph, replacing one of the same type and name.
    pub fn save_view(&mut self, graph: &str, view: SavedView) {
        let views = &mut self.graphs.entry(graph.to_owned()).or_default().views;
        match views
            .iter_mut()
            .find(|v| v.type_name == view.type_name && v.name == view.name)
        {
            Some(existing) => *existing = view,
            None => views.push(view),
        }
    }

    /// Removes a graph's view of a type by name.
    pub fn delete_view(&mut self, graph: &str, type_name: &str, name: &str) {
        if let Some(overrides) = self.graphs.get_mut(graph) {
            overrides
                .views
                .retain(|v| !(v.type_name == type_name && v.name == name));
        }
        self.graphs.retain(|_, overrides| !overrides.is_empty());
    }

    /// The limits in effect for `graph` on the server named `server`.
    pub fn for_graph(&self, server: &str, graph: &str) -> GraphOptions {
        let own = self.graphs.get(&graph_key(server, graph));
        let pick = |get: fn(&GraphOverrides) -> Option<u32>, fallback: u32| {
            own.and_then(get).unwrap_or(fallback)
        };
        GraphOptions {
            type_table_page_size: pick(
                |o| o.type_table_page_size,
                self.studio.type_table_page_size,
            ),
            tree_page_size: pick(|o| o.tree_page_size, self.studio.tree_page_size),
            canvas_max_nodes: pick(|o| o.canvas_max_nodes, self.studio.canvas_max_nodes),
        }
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
    let unreadable = settings.is_none() && error.is_some();
    cx.set_global(SettingsFileState {
        modified,
        error,
        unreadable,
    });
    apply(settings.unwrap_or_default(), cx);
    watch(cx);
}

/// The last problem reading `settings.json`, for a notification.
pub fn file_error(cx: &App) -> Option<String> {
    cx.global::<SettingsFileState>().error.clone()
}

/// The settings in the file, or `None` with the problem when it exists but cannot be parsed.
/// A missing file is the defaults.
fn read_file() -> (Option<StudioSettings>, Option<SystemTime>, Option<String>) {
    let path = settings_path();
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    match std::fs::read_to_string(&path) {
        Ok(text) => match StudioSettings::parse(&text) {
            Ok(settings) => (Some(settings), modified, None),
            Err(error) => (
                None,
                modified,
                Some(format!("{} could not be read: {error}", path.display())),
            ),
        },
        Err(_) => (Some(StudioSettings::default()), modified, None),
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

/// Writes the settings file and applies it. A file Studio could not parse is not overwritten:
/// the person's text, comments and all, may hold what they meant, so the change is refused
/// until the file reads again.
pub fn update(cx: &mut App, f: impl FnOnce(&mut StudioSettings)) {
    let path = settings_path();
    if cx.global::<SettingsFileState>().unreadable {
        cx.global_mut::<SettingsFileState>().error = Some(format!(
            "{} could not be read, so the change was not saved. Fix the file and try again.",
            path.display()
        ));
        // The same settings again, so whatever shows the file's problem (the workspace's
        // toast) hears of it.
        let unchanged = StudioSettings::get(cx).clone();
        cx.set_global(unchanged);
        cx.refresh_windows();
        return;
    }
    let mut settings = StudioSettings::get(cx).clone();
    f(&mut settings);
    // Private: `agent.env` may hold a model provider's key.
    let written = serde_json::to_string_pretty(&settings)
        .map_err(|e| e.to_string())
        .and_then(|text| {
            write_atomic(&path, text.as_bytes(), Access::Private).map_err(|e| e.to_string())
        });
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    cx.set_global(SettingsFileState {
        modified,
        error: written.err(),
        unreadable: false,
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
                // A file that no longer parses keeps the settings in effect; the problem is
                // shown until the file reads again.
                let (settings, modified, error) = read_file();
                cx.update(|cx| {
                    cx.set_global(SettingsFileState {
                        modified,
                        unreadable: settings.is_none(),
                        error,
                    });
                    match settings {
                        Some(settings) => apply(settings, cx),
                        None => cx.refresh_windows(),
                    }
                });
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_directory_binary_is_a_development_run_and_a_bundle_is_not() {
        let dev = |path: &str| is_build_output(std::path::Path::new(path));
        assert!(dev("/w/omnigraph-studio/target/debug/omnigraph-studio"));
        assert!(dev(
            "/w/omnigraph-studio/target/release-fast/omnigraph-studio"
        ));
        assert!(dev("/w/omnigraph-studio/target/debug/examples/snapshot"));
        assert!(dev("/w/omnigraph-studio/target/debug/deps/studio-0123abcd"));
        assert!(!dev(
            "/w/omnigraph-studio/target/bundle/OmniGraph Studio.app/Contents/MacOS/omnigraph-studio"
        ));
        assert!(!dev(
            "/Applications/OmniGraph Studio.app/Contents/MacOS/omnigraph-studio"
        ));
        assert!(!dev("/opt/omnigraph-studio/bin/omnigraph-studio"));
        assert!(!dev("/usr/local/bin/omnigraph-studio"));
    }

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
    fn a_graph_takes_its_own_limits_then_the_shared_ones() {
        let settings = StudioSettings::parse(
            r#"{
                "studio": {"canvas_max_nodes": 3000},
                "graphs": {"dev/knowledge": {"tree_page_size": 10}}
            }"#,
        )
        .unwrap();
        let dev = settings.for_graph("dev", "knowledge");
        assert_eq!(dev.tree_page_size, 10, "the graph's own value");
        assert_eq!(dev.canvas_max_nodes, 3000, "else the shared value");
        let other = settings.for_graph("prod", "knowledge");
        assert_eq!(other.tree_page_size, 50, "a key names one server's graph");
    }

    #[test]
    fn views_are_saved_per_graph_and_type_and_replaced_by_name() {
        let mut settings = StudioSettings::default();
        let view = |name: &str, op: &str| SavedView {
            name: name.into(),
            type_name: "Person".into(),
            filters: vec![SavedFilter {
                property: "title".into(),
                op: op.into(),
                value: "Founder".into(),
            }],
            sort: Some(SavedSort {
                property: "name".into(),
                descending: false,
            }),
            group_by: Some("title".into()),
            hidden: vec!["email".into()],
            order: Vec::new(),
        };
        settings.save_view("dev/knowledge", view("Founders", "eq"));
        settings.save_view("dev/knowledge", view("Founders", "ne"));
        let saved = settings.views_for("dev/knowledge", "Person");
        assert_eq!(saved.len(), 1, "same name replaces");
        assert_eq!(saved[0].filters[0].op, "ne");
        assert!(settings.views_for("dev/knowledge", "Company").is_empty());
        assert!(settings.views_for("prod/knowledge", "Person").is_empty());
        let text = serde_json::to_string(&settings).unwrap();
        assert!(text.contains(r#""hidden":["email"]"#), "{text}");
        assert!(!text.contains("order"), "schema order is left out: {text}");
        assert_eq!(StudioSettings::parse(&text).unwrap(), settings);
        settings.delete_view("dev/knowledge", "Person", "Founders");
        assert!(settings.graphs.is_empty(), "an empty entry is dropped");
    }

    #[test]
    fn query_folders_are_safe_names_per_graph() {
        let dir = queries_dir("environment/knowledge");
        assert!(dir.ends_with("queries/environment~knowledge"), "{dir:?}");
        assert!(queries_dir("http://a b/g").ends_with("queries/http_~~a_b~g"));
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
