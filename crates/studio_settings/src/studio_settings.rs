//! Studio's settings document (spec 002): built-in defaults, then the user's `settings.json` in
//! Studio's data directory. Theme and font keys pass through to Zed's settings store, which Zed's
//! `ui`, `theme_settings` and `editor` read; Studio's own keys live under `studio`.
//!
//! The file is the person's text: it may hold comments, a value that does not fit costs only its
//! own setting, and Studio's writes change only the settings that changed ([`document`]). The
//! problems of `keymap.json` are kept here too, so the settings tab and the toast show both files.
//!
//! Layer 1 (`docs/architecture.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gpui::{App, BorrowAppContext, Global};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{ParseStatus, SettingsStore};

mod document;
mod fields;
mod files;

pub use document::Reading;

pub use files::{Access, set_aside, write_atomic, write_atomic_in_turn, writes_done};

pub use fields::{Choice, FieldKind, Scope, SettingField, SettingsValues, fields, sections};

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
            light: "Vitesse Refined Light".into(),
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

/// Where the bottom panel spans, as Zed's `bottom_dock_layout`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BottomDockLayout {
    /// Under the document only.
    #[default]
    Contained,
    /// Across the whole window, under the sidebar and the inspector too.
    Full,
    /// Under the sidebar and the document; the inspector keeps its full height.
    LeftAligned,
    /// Under the document and the inspector; the sidebar keeps its full height.
    RightAligned,
}

/// A dock, as a panel's placement names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DockSide {
    Left,
    Right,
    Bottom,
}

/// Where a panel sits when the person moved it from its own dock (its status-bar button's
/// right-click menu), as Zed's per-panel `dock` setting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct PanelPlacement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dock: Option<DockSide>,
}

/// The interface font when the person has not chosen one: Inter, Tower's typeface, which
/// `studio_theme` bundles and loads (`assets/fonts/inter/`).
pub const DEFAULT_UI_FONT: &str = "Inter";

/// Studio's own settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct StudioOptions {
    /// Columns on solid panels, or the document as a sheet over a faint wash.
    #[schemars(title = "Window style", extend("x-section" = "Appearance"))]
    pub window_style: WindowStyle,
    /// The corners of cards and menus, in pixels; every other corner follows from it, and
    /// buttons with a label stay round at their ends. 0 is square.
    #[schemars(
        title = "Corner radius",
        range(min = 0, max = 20),
        extend("x-section" = "Appearance", "x-step" = 1)
    )]
    pub corner_radius: u32,
    /// Where the bottom panel spans: under the document, across the window, or with the
    /// sidebar or the inspector beside it.
    #[schemars(title = "Bottom panel", extend("x-section" = "Appearance"))]
    pub bottom_dock_layout: BottomDockLayout,
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
    /// Show Studio's memory and CPU use at the end of the window's footer, read every two
    /// seconds while the window is in front.
    #[schemars(title = "Show memory and CPU", extend("x-section" = "Appearance"))]
    pub show_performance: bool,
}

impl Default for StudioOptions {
    fn default() -> Self {
        Self {
            window_style: WindowStyle::default(),
            corner_radius: 10,
            bottom_dock_layout: BottomDockLayout::default(),
            type_table_page_size: 100,
            tree_page_size: 50,
            canvas_max_nodes: 2000,
            health_check_seconds: 30,
            show_performance: true,
        }
    }
}

/// One graph's own values for the settings marked `x-per-graph`, over `studio` (spec 002),
/// its saved table views (spec 004) and the property each type's nodes are named by. Each
/// value is unset unless the person chose it for this graph.
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
    /// The property that names each type's nodes, by type, where the person chose one; other
    /// types are named by their `name`, `title` or key (spec 002, "Node names").
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
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

/// The property that names one type's nodes in a graph, and the ones that can (spec 002, "Node
/// names"): what a chip's "Show as", a table's "Name rows by" and the settings tab offer.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeNaming {
    /// The graph's `graphs` key ([`graph_key`]).
    pub graph: String,
    pub type_name: String,
    /// The property chosen for this graph, when one is and it can name a node.
    pub chosen: Option<String>,
    /// What names the nodes when none is chosen.
    pub default: String,
    /// Every property that can name the nodes, in schema order.
    pub candidates: Vec<String>,
}

impl TypeNaming {
    /// What names the nodes now.
    pub fn in_effect(&self) -> &str {
        self.chosen.as_deref().unwrap_or(&self.default)
    }

    /// Names the type's nodes by `choice` in this graph, or by its default when `None`.
    pub fn choose(&self, choice: Option<String>, cx: &mut App) {
        let (graph, type_name) = (self.graph.clone(), self.type_name.clone());
        update(cx, move |settings| {
            settings.set_label(&graph, &type_name, choice)
        });
    }
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
    /// The font for menus, tables and panels. Default is Inter, Tower's face, which Studio
    /// bundles.
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
    /// Panels moved from their own dock, by panel id (`explore`, `history`, …); edited from a
    /// panel's button in the status bar.
    #[schemars(extend("x-hidden" = true))]
    pub panels: BTreeMap<String, PanelPlacement>,
}

impl Default for StudioSettings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::default(),
            ui_font_family: None,
            // Tower's root size (93.75 %): its text-sm is 13 px and text-xs 11 px, and Zed's
            // label sizes at this rem land on them.
            ui_font_size: 15.,
            buffer_font_family: None,
            buffer_font_size: 14.,
            ui_density: Density::default(),
            studio: StudioOptions::default(),
            agent: AgentSettings::default(),
            graphs: BTreeMap::new(),
            panels: BTreeMap::new(),
        }
    }
}

impl Global for StudioSettings {}

struct SettingsFileState {
    modified: Option<SystemTime>,
    /// Why the file could not be read at all (its syntax), or why the last change was not saved.
    error: Option<String>,
    /// Each value in the file that was not applied and each key that does nothing.
    problems: Vec<String>,
    /// Each binding in `keymap.json` that was not loaded, or why the file could not be read.
    keymap: Vec<String>,
    /// False for settings held in memory by a test: every change is refused.
    writable: bool,
    /// Changes made so far: a write that comes back for an older one does not apply its
    /// reading over a newer change.
    edits: u64,
    /// Changes still being written: the watch reads the file again only once they are in,
    /// since a reading of an earlier change would undo a newer one held in memory.
    writing: usize,
}

impl Global for SettingsFileState {}

/// Studio's data directory, for every crate: `OMNIGRAPH_STUDIO_DATA`, else
/// `~/Library/Application Support/OmniGraph Studio` on macOS, `%APPDATA%\OmniGraph Studio` on
/// Windows, `$XDG_CONFIG_HOME/omnigraph-studio` elsewhere. A binary run from a Cargo build
/// directory (`cargo run`, an example) uses a sibling directory ending in "Dev" instead, so a
/// development run never rewrites the tabs, drafts and conversations of the app the person
/// uses. A bundled app is not a development run, whichever profile built it. A test binary
/// (Cargo's `deps`) has a directory of its own, so no crate's test reads or rewrites a
/// development run's, nor the person's data, whatever `CARGO_TARGET_DIR` names the build
/// directory. Found once, at the first call: a run keeps one directory.
pub fn data_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(find_data_dir).clone()
}

fn find_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OMNIGRAPH_STUDIO_DATA")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let exe = std::env::current_exe().ok();
    if exe.as_deref().is_some_and(is_test_binary) {
        return std::env::temp_dir().join(format!("omnigraph-studio-test-{}", std::process::id()));
    }
    let dev = exe.as_deref().is_some_and(is_build_output);
    let home = std::env::home_dir().unwrap_or_default();
    if cfg!(windows) {
        return std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Roaming"))
            .join(if dev {
                "OmniGraph Studio Dev"
            } else {
                "OmniGraph Studio"
            });
    }
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
/// profile directory (or its `examples` or `deps`) of a Cargo build directory, and not in a
/// macOS bundle. A build directory is one named `target` or one holding the `CACHEDIR.TAG`
/// Cargo writes into every build directory, so `CARGO_TARGET_DIR` may name it anything.
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
        .is_some_and(|build| {
            build.file_name().is_some_and(|name| name == "target")
                || build.join("CACHEDIR.TAG").is_file()
        })
}

/// A test binary Cargo built: in a `deps` directory, named for its crate and the hexadecimal
/// hash Cargo appends to every file there (`studio_settings-f7f2cad5771331a9`). The name is
/// enough on its own, so a test whose build directory is not recognised still keeps out of the
/// person's data.
fn is_test_binary(exe: &std::path::Path) -> bool {
    let in_deps = exe
        .parent()
        .and_then(|dir| dir.file_name())
        .is_some_and(|name| name == "deps");
    let hashed = exe
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.rsplit_once('-'))
        .is_some_and(|(_, hash)| !hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit()));
    in_deps && (hashed || is_build_output(exe))
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

/// The person's key bindings, loaded over Studio's defaults (spec 002).
pub fn keymap_path() -> PathBuf {
    data_dir().join("keymap.json")
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

    /// The property chosen to name each type's nodes in a graph, by type.
    pub fn labels_for(&self, graph: &str) -> BTreeMap<String, String> {
        self.graphs
            .get(graph)
            .map(|overrides| overrides.labels.clone())
            .unwrap_or_default()
    }

    /// Names a type's nodes in a graph by `property`, or by its default when `None`.
    pub fn set_label(&mut self, graph: &str, type_name: &str, property: Option<String>) {
        let labels = &mut self.graphs.entry(graph.to_owned()).or_default().labels;
        match property {
            Some(property) => {
                labels.insert(type_name.to_owned(), property);
            }
            None => {
                labels.remove(type_name);
            }
        }
        self.graphs.retain(|_, overrides| !overrides.is_empty());
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

    /// Reads a settings file: comments and trailing commas are allowed, missing keys take
    /// defaults, and a value that does not fit takes its default and is reported with every key
    /// that does nothing. An error is a file whose syntax is broken.
    pub fn read(text: &str) -> Result<Reading, String> {
        document::read(text)
    }

    /// The settings of a file that has no problem at all.
    pub fn parse(text: &str) -> Result<Self, String> {
        let reading = Self::read(text)?;
        if reading.problems.is_empty() {
            Ok(reading.settings)
        } else {
            Err(reading.problems.join("\n"))
        }
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
        zed["ui_font_family"] = self
            .ui_font_family
            .as_deref()
            .unwrap_or(DEFAULT_UI_FONT)
            .into();
        if let Some(family) = &self.buffer_font_family {
            zed["buffer_font_family"] = family.clone().into();
        }
        zed.to_string()
    }
}

/// Loads the settings file and applies it. Call after `settings::init`.
pub fn init(cx: &mut App) {
    let (reading, modified, error) = read_file();
    let (settings, problems) = match reading {
        Some(reading) => (reading.settings, reading.problems),
        None => (StudioSettings::default(), Vec::new()),
    };
    cx.set_global(SettingsFileState {
        modified,
        error,
        problems,
        keymap: Vec::new(),
        writable: true,
        edits: 0,
        writing: 0,
    });
    apply(settings, cx);
    watch(cx);
}

/// Default settings for a test, held in memory: every change is refused, so no test reaches
/// the settings file.
#[cfg(any(test, feature = "test-support"))]
pub fn init_test(cx: &mut App) {
    cx.set_global(SettingsFileState {
        modified: None,
        error: None,
        problems: Vec::new(),
        keymap: Vec::new(),
        writable: false,
        edits: 0,
        writing: 0,
    });
    cx.set_global(StudioSettings::default());
}

/// What is wrong with `settings.json` and `keymap.json`, in a sentence or two for a toast, as
/// plain text; the settings tab lists each problem ([`settings_problems`], [`keymap_problems`]).
pub fn file_error(cx: &App) -> Option<String> {
    let state = cx.global::<SettingsFileState>();
    let summary = |file: &str, problems: &[String]| match problems {
        [] => None,
        [problem] => Some(format!("{file}: {}", problem.replace('`', ""))),
        problems => Some(format!(
            "{file} has {} problems; the rest of it applies.",
            problems.len()
        )),
    };
    let settings = match &state.error {
        Some(error) => Some(error.clone()),
        None => summary("settings.json", &state.problems),
    };
    let keymap = summary("keymap.json", &state.keymap);
    match (settings, keymap) {
        (Some(settings), Some(keymap)) => Some(format!("{settings}\n{keymap}")),
        (settings, keymap) => settings.or(keymap),
    }
}

/// Everything wrong with `settings.json`: why it could not be read or saved, then each value
/// that was not applied and each key that does nothing.
pub fn settings_problems(cx: &App) -> Vec<String> {
    let state = cx.global::<SettingsFileState>();
    state.error.iter().chain(&state.problems).cloned().collect()
}

/// Each binding in `keymap.json` that was not loaded, or why the file could not be read.
pub fn keymap_problems(cx: &App) -> &[String] {
    &cx.global::<SettingsFileState>().keymap
}

/// Records what loading `keymap.json` found, for the toast and the settings tab.
pub fn set_keymap_problems(problems: Vec<String>, cx: &mut App) {
    if cx.global::<SettingsFileState>().keymap == problems {
        return;
    }
    cx.global_mut::<SettingsFileState>().keymap = problems;
    notify(cx);
}

/// The same settings again, so what shows the files' problems (the workspace's toast, the
/// settings tab) hears of a change in them.
fn notify(cx: &mut App) {
    let unchanged = StudioSettings::get(cx).clone();
    cx.set_global(unchanged);
    cx.refresh_windows();
}

/// What the settings file says, or `None` with the problem when its syntax is broken or it
/// cannot be read. A missing file is the defaults.
fn read_file() -> (Option<Reading>, Option<SystemTime>, Option<String>) {
    let path = settings_path();
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let defaults = Reading {
                settings: StudioSettings::default(),
                problems: Vec::new(),
            };
            return (Some(defaults), modified, None);
        }
        Err(error) => return (None, modified, Some(unreadable(&path, &error.to_string()))),
    };
    match StudioSettings::read(&text) {
        Ok(reading) => (Some(reading), modified, None),
        Err(error) => (None, modified, Some(unreadable(&path, &error))),
    }
}

fn unreadable(path: &std::path::Path, error: &str) -> String {
    format!(
        "{} could not be read: {error}. The settings in effect stay until it reads again.",
        path.display()
    )
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

/// Applies a change, and writes it into the settings file on the write thread, in turn with
/// every other write: reading, editing and syncing the file can take tens of milliseconds, too
/// long for a click. The file is read again first, so an edit another program made since
/// Studio last read it stays, and only the settings the change touches are rewritten
/// ([`document::edit`]); comments, order and every other key are kept. A file whose syntax is
/// broken is not written: the person's text may hold what they meant, so the change holds only
/// until Studio quits, and the settings page says why.
pub fn update(cx: &mut App, f: impl FnOnce(&mut StudioSettings)) {
    let old = StudioSettings::get(cx).clone();
    let mut new = old.clone();
    f(&mut new);
    if !cx.global::<SettingsFileState>().writable {
        notify(cx);
        return;
    }
    let state = cx.global_mut::<SettingsFileState>();
    state.edits += 1;
    state.writing += 1;
    let edit = state.edits;
    apply(new.clone(), cx);
    let path = settings_path();
    let (done, written) = futures::channel::oneshot::channel();
    files::run_in_turn(move || {
        done.send(write_change(&path, &old, &new)).ok();
    });
    cx.spawn(async move |cx| {
        // A write that stopped without an answer still ends, or the file would never be
        // watched again.
        let written = written
            .await
            .unwrap_or_else(|_| Written::Unread("the write stopped before it finished".into()));
        cx.update(|cx| saved(edit, written, cx));
    })
    .detach();
}

/// How writing one change into the settings file went.
enum Written {
    /// The file could not be read, its syntax is broken, or the change could not be edited
    /// into it: nothing was written.
    Unread(String),
    Saved {
        /// The file as it reads now, edits from elsewhere included; `None` when the write
        /// failed.
        reading: Option<Box<Reading>>,
        modified: Option<SystemTime>,
        error: Option<String>,
    },
}

/// How many times a change is edited into the file again when the file was saved elsewhere
/// while the change was being written.
const SAVE_ATTEMPTS: usize = 3;

/// The settings file's text, empty when there is none yet.
fn settings_text(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("it could not be read ({error})")),
    }
}

/// Reads the settings file, edits the change from `old` to `new` into it, and writes it. A
/// save made elsewhere while it is written is not replaced: the change is edited into that
/// text instead.
fn write_change(path: &Path, old: &StudioSettings, new: &StudioSettings) -> Written {
    let reading = |text: &str| StudioSettings::read(text).ok().map(Box::new);
    for _ in 0..SAVE_ATTEMPTS {
        let text = match settings_text(path) {
            Ok(text) => text,
            Err(error) => return Written::Unread(error),
        };
        let edited = match document::edit(&text, old, new) {
            Ok(edited) => edited,
            Err(error) => return Written::Unread(error),
        };
        if edited == text {
            return Written::Saved {
                reading: reading(&edited),
                modified: std::fs::metadata(path).and_then(|m| m.modified()).ok(),
                error: None,
            };
        }
        let unchanged = || settings_text(path).is_ok_and(|now| now == text);
        // Private: `agent.env` may hold a model provider's key.
        match files::write_atomic_unless_changed(
            path,
            edited.as_bytes(),
            Access::Private,
            unchanged,
        ) {
            Ok(files::Replaced::Written(modified)) => {
                return Written::Saved {
                    reading: reading(&edited),
                    modified,
                    error: None,
                };
            }
            Ok(files::Replaced::Changed) => continue,
            Err(error) => {
                return Written::Saved {
                    reading: None,
                    modified: std::fs::metadata(path).and_then(|m| m.modified()).ok(),
                    error: Some(format!("{} could not be saved: {error}", path.display())),
                };
            }
        }
    }
    Written::Unread("it was saved elsewhere each time Studio wrote it".into())
}

/// Takes in how change `edit` was written. The file's time is taken as read, so the watch does
/// not read it again. Only the newest change's reading applies, and only when it differs from
/// what applies already (an edit made elsewhere): an older one would undo a newer change. A
/// change that could not be written applies until Studio quits, and the error says so.
fn saved(edit: u64, written: Written, cx: &mut App) {
    let path = settings_path();
    let latest = cx.global::<SettingsFileState>().edits == edit;
    let state = cx.global_mut::<SettingsFileState>();
    state.writing = state.writing.saturating_sub(1);
    match written {
        Written::Unread(error) => {
            state.error = Some(format!(
                "{} was not changed: {error}. The change holds only until Studio quits; fix the \
                 file and try again.",
                path.display()
            ));
            // They were found in an earlier version of the file.
            state.problems.clear();
            notify(cx);
        }
        Written::Saved {
            reading,
            modified,
            error,
        } => {
            state.modified = modified;
            let failed = error.is_some();
            state.error = error;
            match reading {
                Some(reading) if latest => {
                    let reading = *reading;
                    state.problems = reading.problems;
                    if reading.settings != *StudioSettings::get(cx) {
                        apply(reading.settings, cx);
                    } else {
                        notify(cx);
                    }
                }
                _ if failed => notify(cx),
                _ => {}
            }
        }
    }
}

/// Reloads the file when another program changes it (checked every two seconds).
fn watch(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let path = settings_path();
            // Off the main thread, as the read below: a slow disk would hold up frames.
            let modified = cx
                .background_executor()
                .spawn(async move { std::fs::metadata(&path).and_then(|m| m.modified()).ok() })
                .await;
            let changed = cx.update(|cx| {
                let state = cx.global::<SettingsFileState>();
                state.writing == 0 && state.modified != modified
            });
            if changed {
                let (reading, modified, error) =
                    cx.background_executor().spawn(async { read_file() }).await;
                cx.update(|cx| {
                    let state = cx.global_mut::<SettingsFileState>();
                    // A change Studio began writing meanwhile takes in the file when it is
                    // saved; this reading may be older than that change.
                    if state.writing > 0 {
                        return;
                    }
                    state.modified = modified;
                    state.error = error;
                    match reading {
                        Some(reading) => {
                            state.problems = reading.problems;
                            apply(reading.settings, cx);
                        }
                        // Broken syntax keeps the settings in effect; the problem is shown
                        // until the file reads again, without those of an earlier version.
                        None => {
                            state.problems.clear();
                            notify(cx);
                        }
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

    /// A change is edited into the file as it is now, an edit made elsewhere kept.
    #[test]
    fn a_change_is_written_into_the_file_as_it_is_now() {
        let dir =
            std::env::temp_dir().join(format!("studio-settings-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        // Someone else set the buffer font size meanwhile.
        std::fs::write(&path, "{\n  // mine\n  \"buffer_font_size\": 15\n}\n").unwrap();
        let old = StudioSettings::default();
        let new = StudioSettings {
            ui_font_size: old.ui_font_size + 2.,
            ..old.clone()
        };
        let Written::Saved { reading, error, .. } = write_change(&path, &old, &new) else {
            panic!("unread");
        };
        assert_eq!(error, None);
        let reading = reading.unwrap();
        assert_eq!(reading.settings.ui_font_size, new.ui_font_size);
        assert_eq!(reading.settings.buffer_font_size, 15.);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("// mine"), "{text}");
        // A file whose syntax is broken is not written.
        std::fs::write(&path, "{ \"ui_font_size\": ").unwrap();
        assert!(matches!(
            write_change(&path, &old, &new),
            Written::Unread(_)
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ \"ui_font_size\": "
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A write that comes back for an older change does not apply its reading over a newer
    /// change, while the file's time is still taken as read.
    #[gpui::test]
    fn an_older_changes_reading_does_not_apply(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            init_test(cx);
            cx.global_mut::<SettingsFileState>().edits = 2;
            let older = StudioSettings {
                ui_font_size: 20.,
                ..StudioSettings::default()
            };
            let modified = Some(SystemTime::UNIX_EPOCH);
            let written = Written::Saved {
                reading: Some(Box::new(Reading {
                    settings: older,
                    problems: vec!["from the older write".into()],
                })),
                modified,
                error: None,
            };
            saved(1, written, cx);
            assert_eq!(*StudioSettings::get(cx), StudioSettings::default());
            let state = cx.global::<SettingsFileState>();
            assert!(state.problems.is_empty());
            assert_eq!(state.modified, modified);
        });
    }

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
        // A test binary keeps a directory of its own.
        let test = |path: &str| is_test_binary(std::path::Path::new(path));
        assert!(test(
            "/w/omnigraph-studio/target/debug/deps/studio-0123abcd"
        ));
        assert!(!test("/w/omnigraph-studio/target/debug/omnigraph-studio"));
        assert!(!test("/w/omnigraph-studio/target/debug/examples/snapshot"));
        // `CARGO_TARGET_DIR` may name the build directory anything: a test binary is known by
        // its hashed name, and a build directory by the tag Cargo writes into it.
        assert!(test(
            "/w/elsewhere/debug/deps/studio_settings-f7f2cad5771331a9"
        ));
        assert!(test(
            "C:/w/elsewhere/debug/deps/studio_settings-f7f2cad5771331a9.exe"
        ));
        assert!(!test("/opt/deps/omnigraph-studio"));
        let build = std::env::temp_dir().join(format!("studio-build-dir-{}", std::process::id()));
        let run = build.join("debug").join("omnigraph-studio");
        let example = build.join("debug").join("examples").join("snapshot");
        std::fs::create_dir_all(&build).unwrap();
        assert!(!is_build_output(&run));
        std::fs::write(
            build.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        assert!(is_build_output(&run));
        assert!(is_build_output(&example));
        assert!(!is_test_binary(&run));
        std::fs::remove_dir_all(&build).unwrap();
        if std::env::var_os("OMNIGRAPH_STUDIO_DATA").is_none() {
            assert!(
                data_dir().starts_with(std::env::temp_dir()),
                "{:?}",
                data_dir()
            );
        }
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
        assert_eq!(
            zed["ui_font_family"], DEFAULT_UI_FONT,
            "unset is Studio's bundled Inter"
        );
        assert_eq!(zed["unstable.ui_density"], "default");
        settings.ui_font_family = Some("IBM Plex Sans".into());
        settings.ui_density = Density::Compact;
        let zed: serde_json::Value = serde_json::from_str(&settings.zed_user_settings()).unwrap();
        assert_eq!(zed["ui_font_family"], "IBM Plex Sans");
        assert_eq!(zed["unstable.ui_density"], "compact");
    }

    #[test]
    fn missing_keys_take_defaults_and_unknown_keys_are_reported() {
        let reading = StudioSettings::read(
            r#"{"ui_font_size": 13, "extra": 1, "studio": {"tree_page_size": 20}}"#,
        )
        .unwrap();
        assert_eq!(
            reading.problems,
            ["`extra` (line 1) is not a setting, so it does nothing."]
        );
        let settings = reading.settings;
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
    fn views_and_node_names_are_saved_per_graph_and_type() {
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
        // A type's node name is kept per graph, and choosing the default drops it.
        settings.set_label("dev/knowledge", "Person", Some("title".into()));
        assert_eq!(settings.labels_for("dev/knowledge")["Person"], "title");
        assert!(settings.labels_for("prod/knowledge").is_empty());
        let text = serde_json::to_string(&settings).unwrap();
        assert!(text.contains(r#""labels":{"Person":"title"}"#), "{text}");
        assert_eq!(StudioSettings::parse(&text).unwrap(), settings);
        settings.set_label("dev/knowledge", "Person", None);
        assert!(settings.graphs.is_empty(), "the default is no entry");
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
                "ui_font_family",
                "ui_font_size",
                "unstable.ui_density"
            ]
        );
    }
}
