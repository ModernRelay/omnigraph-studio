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

pub use files::{
    Access, run_in_turn, set_aside, write_atomic, write_atomic_in_turn, write_new, writes_done,
};

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
            light: "Studio Light".into(),
            dark: "Studio Dark".into(),
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

/// How a review lays out a branch's changes (spec 008).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLayout {
    /// One sentence per type changed, its entities in a table under it.
    #[default]
    Feed,
    /// The changes listed beside one record at a time.
    Split,
}

impl ReviewLayout {
    pub fn other(self) -> Self {
        match self {
            Self::Feed => Self::Split,
            Self::Split => Self::Feed,
        }
    }
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
    /// How a review lays out a branch's changes: a feed of the types changed, or the changes
    /// listed beside one record at a time.
    #[schemars(title = "Review layout", extend("x-section" = "Browsing"))]
    pub review_layout: ReviewLayout,
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
    /// Check for a newer Studio half a minute after launch and every hour, and install it on
    /// macOS, ready at the next restart. Off, Studio checks only when asked.
    #[schemars(title = "Update automatically", extend("x-section" = "Updates"))]
    pub auto_update: bool,
    /// What Studio sends to help improve it (spec 018). Never a graph's names, queries, values
    /// or server addresses; nothing from a development build, or while `DO_NOT_TRACK` or
    /// `OMNIGRAPH_STUDIO_TELEMETRY=0` is set.
    #[schemars(extend("x-section" = "Privacy"))]
    pub telemetry: TelemetryOptions,
}

/// Studio's two telemetry switches (spec 018).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct TelemetryOptions {
    /// What happens to a report of a crash: Studio asks at the next start, sends it, or deletes
    /// it.
    #[schemars(title = "Crash reports")]
    pub crash_reports: CrashReports,
    /// Which features are used and how fast Studio is, never what they were used on. Anonymous
    /// by default: each run of Studio is a stranger to the next, unless you choose to be counted
    /// across runs with a random install ID.
    #[schemars(title = "Usage data")]
    pub usage: UsageSharing,
}

/// What Studio does with a crash report (spec 018).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CrashReports {
    /// Ask at the next start, showing the report.
    #[default]
    Ask,
    /// Send each report without asking.
    Always,
    /// Delete each report unsent.
    Never,
}

/// How Studio shares usage data (spec 018): anonymously unless the person chooses otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageSharing {
    /// Share anonymous usage data and a daily performance summary, tied to one run of Studio,
    /// once the person has answered the status bar's question or chosen here.
    #[default]
    Anonymous,
    /// Share them with a random install ID, so your runs are counted as one person's.
    WithInstallId,
    /// Share nothing.
    Off,
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
            review_layout: ReviewLayout::default(),
            health_check_seconds: 30,
            show_performance: true,
            auto_update: true,
            telemetry: TelemetryOptions::default(),
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

/// A named filter, order, grouping and set of columns for one type's table (spec 004), listed
/// in the sidebar and opened in its own tab (spec 024).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SavedView {
    /// What names the view while its name changes: its tab and its row. Empty in a view saved
    /// before views had ids, which goes by [`SavedView::key`] until it is next changed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub name: String,
    /// The shape it opens in: the one last shown.
    #[serde(skip_serializing_if = "ViewShape::is_table")]
    pub shape: ViewShape,
    /// The type a type view shows; empty in a query view.
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    pub type_name: String,
    /// The query a query view runs; `None` in a type view (spec 024).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<ViewQuery>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
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

/// The query a view runs (spec 024): a stored query the server serves by name, or one query of
/// a `.gq` file, with the values of its parameters. The parameters are the view's filters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ViewQuery {
    /// The stored query's name in the graph's catalog; empty in a view of a file's query.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stored: String,
    /// The `.gq` file whose query the view runs, by its path; empty in a view of a stored query.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub file: String,
    /// The name of the query in `file` the view runs.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The parameters' values, as the parameter bar holds them.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub params: serde_json::Map<String, serde_json::Value>,
}

impl ViewQuery {
    /// What the view runs, as a row's tooltip says it: the stored query's name, or the file's
    /// query as `people in people.gq`.
    pub fn label(&self) -> String {
        if self.file.is_empty() {
            return self.stored.clone();
        }
        let file = std::path::Path::new(&self.file)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.file.clone());
        format!("{} in {file}", self.name)
    }

    /// The view runs a query of a `.gq` file, not a stored one.
    pub fn is_file(&self) -> bool {
        !self.file.is_empty()
    }
}

/// How a view shows its rows (spec 024).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewShape {
    #[default]
    Table,
    Tree,
    Canvas,
}

impl ViewShape {
    fn is_table(&self) -> bool {
        *self == ViewShape::Table
    }
}

impl SavedView {
    /// The view's id: its own, or for one saved before views had ids, one made from its type
    /// and name, which stay unique per graph until it is changed and given its own.
    pub fn key(&self) -> String {
        if self.id.is_empty() {
            format!("{}/{}", self.type_name, self.name)
        } else {
            self.id.clone()
        }
    }

    /// Whether `other` shows the same rows the same way: the same filters, order, grouping and
    /// columns, whatever it is named.
    pub fn shows_as(&self, other: &SavedView) -> bool {
        let hidden = |view: &SavedView| {
            let mut hidden = view.hidden.clone();
            hidden.sort();
            hidden
        };
        self.type_name == other.type_name
            && self.query == other.query
            && self.filters == other.filters
            && self.sort == other.sort
            && self.group_by == other.group_by
            && hidden(self) == hidden(other)
            && self.order == other.order
    }
}

/// A new view's id: 16 random bytes in hex.
pub fn new_view_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // A clock is unique enough for one person's views when the system has no randomness.
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        bytes = nanos.to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
    /// The graph's `graphs` key ([`overrides_key`]).
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
    /// Names the type's nodes by `choice` in this graph, or by its default when `None`.
    pub fn choose(&self, choice: Option<String>, cx: &mut App) {
        let (graph, type_name) = (self.graph.clone(), self.type_name.clone());
        update(cx, move |settings| {
            settings.set_label(&graph, &type_name, choice)
        });
    }
}

/// A graph's one key for everything Studio saves of it: its server's key and its id,
/// `<server>/<graph id>`. The server's key is its CLI target's name and address,
/// `<target>@<address>` (`graph_store::ServerEntry::saved_key`), and a graph id holds no `/`.
/// Its `graphs` settings, its tabs and drafts, its saved queries' folder ([`queries_dir`]) and
/// its agent conversations are kept under it, as the windows a start reopens name the graph.
pub fn overrides_key(target: &str, graph: &str) -> String {
    format!("{target}/{graph}")
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
}

/// Model calls through OpenRouter (spec 019): the model each kind of task runs on, whether a
/// key is stored, and the tasks allowed to send their input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ModelSettings {
    /// The model for quick answers of a line or two: a table filter from a sentence, a cell's
    /// value. Claude Haiku 5.5 was right on every filter tried in 1.5 to 2.8 s.
    #[schemars(title = "Fast model", extend("x-control" = "model"))]
    pub fast: String,
    /// The model for careful answers: a branch summary. Claude Sonnet 5.5 wrote the best one
    /// tried, in about 10 s.
    #[schemars(title = "Careful model", extend("x-control" = "model"))]
    pub careful: String,
    /// For Studio's own model features, not the agent panel, which uses your Claude or Codex
    /// sign-in. Studio sends a task's input to OpenRouter and the model named above, with your key,
    /// which the system keychain holds: a table filter sends the type's property names and
    /// your sentence, a branch summary the changed records' titles and property names, a
    /// column fill the row's values in the columns you pick. Nothing is sent without a key.
    /// Your own Anthropic, OpenAI or Google key, set in your OpenRouter workspace, is used for
    /// that provider's models and billed to that account.
    #[schemars(title = "OpenRouter key", extend("x-control" = "openrouter-key"))]
    pub key_stored: bool,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            fast: "anthropic/claude-haiku-5.5".into(),
            careful: "anthropic/claude-sonnet-5.5".into(),
            key_stored: false,
        }
    }
}

/// Omnigraph Cloud's control plane, which Studio offers by default (spec 021).
pub const OMNIGRAPH_CLOUD: &str = "https://platform.omnigraph.dev";

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
    /// What each server is, by its address. Set from the top bar's environment chip.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend("x-hidden" = true))]
    pub servers: BTreeMap<String, ServerOverrides>,
    /// The omnigraph program Studio runs to list its servers and read their tokens, and that the
    /// agent runs to reach the graph. Studio tries it first, then each omnigraph on PATH and in
    /// the usual install folders, and runs the first that fits.
    #[schemars(title = "Omnigraph CLI", extend("x-section" = "Connection"))]
    pub omnigraph_cli: Option<String>,
    /// The Omnigraph Cloud control plane Studio offers to sign in to and lists the clusters of,
    /// besides those your CLI profiles name. Empty turns it off.
    #[schemars(title = "Address", extend("x-section" = "Omnigraph Cloud"))]
    pub omnigraph_cloud: String,
    #[schemars(extend("x-section" = "Agent"))]
    pub agent: AgentSettings,
    #[schemars(extend("x-section" = "Models"))]
    pub models: ModelSettings,
    /// Per-graph overrides, keyed by [`overrides_key`]; edited in the settings tab's graph
    /// scope.
    #[schemars(extend("x-hidden" = true))]
    pub graphs: BTreeMap<String, GraphOverrides>,
    /// Panels moved from their own dock, by panel id (`explore`, `history`, …); edited from a
    /// panel's button in the status bar.
    #[schemars(extend("x-hidden" = true))]
    pub panels: BTreeMap<String, PanelPlacement>,
}

/// What a server is, which the top bar marks (spec 020). `Unset` says so: a server nobody has
/// named must not look safe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServerEnvironment {
    #[default]
    Unset,
    Development,
    Staging,
    Production,
}

impl ServerEnvironment {
    pub const ALL: [Self; 4] = [
        Self::Production,
        Self::Staging,
        Self::Development,
        Self::Unset,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Unset => "Not set",
            Self::Development => "Development",
            Self::Staging => "Staging",
            Self::Production => "Production",
        }
    }
}

/// One server's settings, kept by its address so renaming a target cannot reset them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ServerOverrides {
    pub environment: ServerEnvironment,
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
            servers: BTreeMap::new(),
            omnigraph_cli: None,
            omnigraph_cloud: OMNIGRAPH_CLOUD.to_owned(),
            agent: AgentSettings::default(),
            models: ModelSettings::default(),
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
    /// A test's settings that take changes in memory, never written ([`keep_changes_in_memory`]).
    in_memory: bool,
    /// Changes made so far: a write that comes back for an older one does not apply its
    /// reading over a newer change.
    edits: u64,
    /// Changes still being written: the watch reads the file again only once they are in,
    /// since a reading of an earlier change would undo a newer one held in memory.
    writing: usize,
}

impl Global for SettingsFileState {}

/// Studio's data directory, for every crate: `OMNIGRAPH_STUDIO_DATA`, else
/// `~/Library/Application Support/Omnigraph Studio` on macOS, `%APPDATA%\Omnigraph Studio` on
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
                "Omnigraph Studio Dev"
            } else {
                "Omnigraph Studio"
            });
    }
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support").join(if dev {
            "Omnigraph Studio Dev"
        } else {
            "Omnigraph Studio"
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

/// Whether `exe` is an installed Studio rather than a build or a test: only a release build
/// checks for updates (spec 017). A bundle `script/bundle-mac` wrote into `target/bundle/` is
/// not one: a release must never replace a build, though that bundle keeps the installed data
/// directory ([`is_build_output`]).
pub fn is_release_build(exe: &std::path::Path) -> bool {
    !is_build_output(exe) && !is_test_binary(exe) && !under_build_dir(exe)
}

/// Whether a folder above `exe` is a Cargo build directory: named `target`, or holding the
/// `CACHEDIR.TAG` Cargo writes into every one.
fn under_build_dir(exe: &std::path::Path) -> bool {
    exe.ancestors().skip(1).any(|dir| {
        dir.file_name().is_some_and(|name| name == "target") || dir.join("CACHEDIR.TAG").is_file()
    })
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

/// Where a graph's saved queries live (spec 007): one folder per graph key ([`overrides_key`])
/// under the data directory, its name made safe for any file system.
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

    /// The views saved for one type of a graph (key from [`overrides_key`]).
    pub fn views_for(&self, graph: &str, type_name: &str) -> Vec<SavedView> {
        self.graphs
            .get(graph)
            .map(|overrides| {
                overrides
                    .views
                    .iter()
                    .filter(|view| view.query.is_none() && view.type_name == type_name)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Saves `view` for a graph, replacing one of the same type and name, whose id it keeps;
    /// a new one is given an id.
    pub fn save_view(&mut self, graph: &str, mut view: SavedView) {
        let views = &mut self.graphs.entry(graph.to_owned()).or_default().views;
        match views
            .iter_mut()
            .find(|v| v.query.is_none() && v.type_name == view.type_name && v.name == view.name)
        {
            Some(existing) => {
                view.id = existing.key();
                view.shape = existing.shape;
                *existing = view;
            }
            None => {
                if view.id.is_empty() {
                    view.id = new_view_id();
                }
                views.push(view);
            }
        }
    }

    /// Saves a new view of a stored query for a graph, named `name`, opening as a table.
    /// Returns its id, or `None` when the name is blank or another view of the graph has it.
    pub fn save_query_view(&mut self, graph: &str, name: &str, query: ViewQuery) -> Option<String> {
        let name = name.trim();
        if name.is_empty() || self.view_name_taken(graph, name, None) {
            return None;
        }
        let view = SavedView {
            id: new_view_id(),
            name: name.to_owned(),
            query: Some(query),
            ..SavedView::default()
        };
        let id = view.id.clone();
        self.graphs
            .entry(graph.to_owned())
            .or_default()
            .views
            .push(view);
        Some(id)
    }

    /// Every view of a graph, in the order they were made.
    pub fn graph_views(&self, graph: &str) -> &[SavedView] {
        self.graphs
            .get(graph)
            .map(|overrides| overrides.views.as_slice())
            .unwrap_or_default()
    }

    /// A graph's view by its id ([`SavedView::key`]).
    pub fn view(&self, graph: &str, id: &str) -> Option<&SavedView> {
        self.graph_views(graph).iter().find(|view| view.key() == id)
    }

    /// Whether a graph has a view named `name`, other than the one with id `except`.
    pub fn view_name_taken(&self, graph: &str, name: &str, except: Option<&str>) -> bool {
        self.graph_views(graph)
            .iter()
            .any(|view| view.name == name && except != Some(view.key().as_str()))
    }

    /// Changes what the view with `id` shows, keeping its id, name and shape. False when the
    /// graph has no such view.
    pub fn update_view(&mut self, graph: &str, id: &str, shown: &SavedView) -> bool {
        let Some(view) = self.view_mut(graph, id) else {
            return false;
        };
        *view = SavedView {
            id: view.key(),
            name: view.name.clone(),
            shape: view.shape,
            ..shown.clone()
        };
        true
    }

    /// Renames the view with `id`. A blank name, or one another view of the graph has, is
    /// refused: false.
    pub fn rename_view(&mut self, graph: &str, id: &str, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || self.view_name_taken(graph, name, Some(id)) {
            return false;
        }
        let Some(view) = self.view_mut(graph, id) else {
            return false;
        };
        view.id = view.key();
        view.name = name.to_owned();
        true
    }

    /// A copy of the view with `id`, named "<name> copy" (then "copy 2", …), after it. Returns
    /// the copy's id.
    pub fn duplicate_view(&mut self, graph: &str, id: &str) -> Option<String> {
        let views = self.graph_views(graph);
        let at = views.iter().position(|view| view.key() == id)?;
        let original = &views[at];
        let name = (1..)
            .map(|n| match n {
                1 => format!("{} copy", original.name),
                n => format!("{} copy {n}", original.name),
            })
            .find(|name| !self.view_name_taken(graph, name, None))?;
        let copy = SavedView {
            id: new_view_id(),
            name,
            ..original.clone()
        };
        let id = copy.id.clone();
        self.graphs.get_mut(graph)?.views.insert(at + 1, copy);
        Some(id)
    }

    /// Takes the view with `id` out of a graph: where it was and what it was, for Undo.
    pub fn remove_view(&mut self, graph: &str, id: &str) -> Option<(usize, SavedView)> {
        let views = &mut self.graphs.get_mut(graph)?.views;
        let at = views.iter().position(|view| view.key() == id)?;
        let removed = views.remove(at);
        self.graphs.retain(|_, overrides| !overrides.is_empty());
        Some((at, removed))
    }

    /// Puts a removed view back where it was, unless a view with its id or name is there now.
    pub fn restore_view(&mut self, graph: &str, at: usize, view: SavedView) -> bool {
        if self.view(graph, &view.key()).is_some() || self.view_name_taken(graph, &view.name, None)
        {
            return false;
        }
        let views = &mut self.graphs.entry(graph.to_owned()).or_default().views;
        views.insert(at.min(views.len()), view);
        true
    }

    /// Records the shape the view with `id` was last shown in.
    pub fn set_view_shape(&mut self, graph: &str, id: &str, shape: ViewShape) {
        if let Some(view) = self.view_mut(graph, id)
            && view.shape != shape
        {
            view.id = view.key();
            view.shape = shape;
        }
    }

    fn view_mut(&mut self, graph: &str, id: &str) -> Option<&mut SavedView> {
        self.graphs
            .get_mut(graph)?
            .views
            .iter_mut()
            .find(|view| view.key() == id)
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

    /// The limits in effect for `graph` on the CLI target `target`.
    pub fn for_graph(&self, target: &str, graph: &str) -> GraphOptions {
        let own = self.graphs.get(&overrides_key(target, graph));
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
        in_memory: false,
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
        in_memory: false,
        edits: 0,
        writing: 0,
    });
    cx.set_global(StudioSettings::default());
}

/// Lets a test's settings ([`init_test`]) take changes, held in memory and never written.
#[cfg(any(test, feature = "test-support"))]
pub fn keep_changes_in_memory(cx: &mut App) {
    cx.global_mut::<SettingsFileState>().in_memory = true;
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
        if cx.global::<SettingsFileState>().in_memory {
            cx.set_global(new);
            cx.refresh_windows();
        } else {
            notify(cx);
        }
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

    /// The settings of `text`, read as Studio reads its file, which must have no problem.
    fn read_clean(text: &str) -> StudioSettings {
        let reading = StudioSettings::read(text).unwrap();
        assert!(reading.problems.is_empty(), "{:?}", reading.problems);
        reading.settings
    }

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
    fn only_an_installed_build_is_a_release_build() {
        let release = |path: &str| is_release_build(std::path::Path::new(path));
        assert!(release(
            "/Applications/Omnigraph Studio.app/Contents/MacOS/omnigraph-studio"
        ));
        assert!(release(
            "/home/ada/omnigraph-studio-0.3.0-linux-x86_64/bin/omnigraph-studio"
        ));
        assert!(!release(
            "/w/omnigraph-studio/target/debug/omnigraph-studio"
        ));
        assert!(!release(
            "/w/omnigraph-studio/target/debug/deps/studio_update-f7f2cad5771331a9"
        ));
        assert!(
            !release(
                "/w/omnigraph-studio/target/bundle/Omnigraph Studio.app/Contents/MacOS/omnigraph-studio"
            ),
            "a bundle built here is never replaced by a release"
        );
        // A folder of its own: another test makes and deletes `studio-build-dir-<pid>` meanwhile.
        let elsewhere =
            std::env::temp_dir().join(format!("studio-release-dir-{}", std::process::id()));
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("CACHEDIR.TAG"), "Signature").unwrap();
        let bundle = elsewhere.join("bundle/Omnigraph Studio.app/Contents/MacOS/omnigraph-studio");
        let release_there = is_release_build(&bundle);
        std::fs::remove_dir_all(&elsewhere).ok();
        assert!(!release_there, "a build directory of any name");
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
            "/w/omnigraph-studio/target/bundle/Omnigraph Studio.app/Contents/MacOS/omnigraph-studio"
        ));
        assert!(!dev(
            "/Applications/Omnigraph Studio.app/Contents/MacOS/omnigraph-studio"
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
        assert_eq!(read_clean(text), StudioSettings::default());
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

    /// The CLI Studio and the agent run is one setting at the top of the document. A file that
    /// still names it under `agent`, where it was, holds a key that does nothing, and is told so
    /// as any other.
    #[test]
    fn the_cli_is_one_setting_and_its_old_place_is_not_a_setting() {
        let settings = read_clean(r#"{"omnigraph_cli": "/opt/omnigraph/bin/omnigraph"}"#);
        assert_eq!(
            settings.omnigraph_cli.as_deref(),
            Some("/opt/omnigraph/bin/omnigraph")
        );
        let reading =
            StudioSettings::read("{\n  \"agent\": {\"omnigraph_cli\": \"/opt/og\"}\n}").unwrap();
        assert_eq!(reading.settings, StudioSettings::default());
        assert_eq!(
            reading.problems,
            ["`agent.omnigraph_cli` (line 2) is not a setting, so it does nothing."]
        );
    }

    #[test]
    fn a_graph_takes_its_own_limits_then_the_shared_ones() {
        let settings = read_clean(
            r#"{
                "studio": {"canvas_max_nodes": 3000},
                "graphs": {"dev/knowledge": {"tree_page_size": 10}}
            }"#,
        );
        let dev = settings.for_graph("dev", "knowledge");
        assert_eq!(dev.tree_page_size, 10, "the graph's own value");
        assert_eq!(dev.canvas_max_nodes, 3000, "else the shared value");
        let other = settings.for_graph("prod", "knowledge");
        assert_eq!(other.tree_page_size, 50, "a key names one target's graph");
    }

    /// A view made before views had ids goes by its type and name until it changes, and keeps
    /// that id when renamed; names are unique per graph; a removed view comes back where it was.
    #[test]
    fn a_query_view_keeps_its_stored_query_and_values() {
        let mut settings = read_clean(
            r#"{"graphs":{"dev/k":{"views":[
            {"name":"Founders","type":"Person","filters":[]}]}}}"#,
        );
        let mut params = serde_json::Map::new();
        params.insert("city".into(), serde_json::json!("Berlin"));
        let query = ViewQuery {
            stored: "people_in".into(),
            params,
            ..Default::default()
        };
        assert_eq!(
            settings.save_query_view("dev/k", "Founders", query.clone()),
            None,
            "a name a type view has"
        );
        let id = settings
            .save_query_view("dev/k", "In Berlin", query.clone())
            .unwrap();
        let view = settings.view("dev/k", &id).unwrap();
        assert_eq!(view.query.as_ref(), Some(&query));
        assert!(settings.views_for("dev/k", "").is_empty(), "not a type's");
        let text = serde_json::to_string(&settings.graphs["dev/k"].views[1]).unwrap();
        assert_eq!(
            text,
            format!(
                r#"{{"id":"{id}","name":"In Berlin","query":{{"stored":"people_in","params":{{"city":"Berlin"}}}}}}"#
            )
        );
        // Other values are a change; saving them keeps the query view's id, name and query.
        let mut shown = view.clone();
        shown.query.as_mut().unwrap().params["city"] = serde_json::json!("Paris");
        assert!(!view.shows_as(&shown));
        assert!(settings.update_view("dev/k", &id, &shown));
        assert!(settings.view("dev/k", &id).unwrap().shows_as(&shown));
        let copy = settings.duplicate_view("dev/k", &id).unwrap();
        assert_eq!(
            settings.view("dev/k", &copy).unwrap().query,
            shown.query,
            "a copy runs the same query"
        );
    }

    #[test]
    fn a_view_keeps_its_id_through_rename_duplicate_and_undo() {
        let old = r#"{"graphs":{"dev/k":{"views":[
            {"name":"Founders","type":"Person","filters":[]},
            {"name":"Big","type":"Company","filters":[]}]}}}"#;
        let mut settings = read_clean(old);
        let founders = "Person/Founders".to_owned();
        assert_eq!(settings.view("dev/k", &founders).unwrap().name, "Founders");
        assert!(
            !settings.rename_view("dev/k", &founders, "Big"),
            "a name another has"
        );
        assert!(
            !settings.rename_view("dev/k", &founders, "  "),
            "a blank name"
        );
        assert!(settings.rename_view("dev/k", &founders, "Founders 2026"));
        assert_eq!(
            settings.view("dev/k", &founders).unwrap().name,
            "Founders 2026"
        );
        let text = serde_json::to_string(&settings).unwrap();
        assert!(
            text.contains(r#""id":"Person/Founders""#),
            "the id is kept: {text}"
        );
        assert!(!text.contains("shape"), "a table is the default: {text}");
        let copy = settings.duplicate_view("dev/k", &founders).unwrap();
        assert_ne!(copy, founders);
        let names: Vec<_> = settings
            .graph_views("dev/k")
            .iter()
            .map(|v| v.name.clone())
            .collect();
        assert_eq!(names, ["Founders 2026", "Founders 2026 copy", "Big"]);
        let again = settings.duplicate_view("dev/k", &founders).unwrap();
        assert_eq!(
            settings.view("dev/k", &again).unwrap().name,
            "Founders 2026 copy 2"
        );
        settings.set_view_shape("dev/k", &copy, ViewShape::Canvas);
        let (at, removed) = settings.remove_view("dev/k", &copy).unwrap();
        assert_eq!((at, removed.shape), (2, ViewShape::Canvas));
        assert!(settings.view("dev/k", &copy).is_none());
        assert!(settings.restore_view("dev/k", at, removed.clone()));
        assert!(
            !settings.restore_view("dev/k", at, removed),
            "it is there already"
        );
        assert_eq!(settings.graph_views("dev/k")[2].key(), copy);
        // Saving over a view by type and name keeps its id and shape.
        let shown = SavedView {
            name: "Founders 2026 copy".into(),
            type_name: "Person".into(),
            group_by: Some("title".into()),
            ..SavedView::default()
        };
        settings.save_view("dev/k", shown);
        let saved = settings.view("dev/k", &copy).unwrap();
        assert_eq!(
            (saved.shape, saved.group_by.as_deref()),
            (ViewShape::Canvas, Some("title"))
        );
        assert!(settings.update_view("dev/k", &founders, &SavedView::default()));
        assert_eq!(
            settings.view("dev/k", &founders).unwrap().name,
            "Founders 2026"
        );
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
            ..SavedView::default()
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
        assert_eq!(read_clean(&text), settings);
        settings.delete_view("dev/knowledge", "Person", "Founders");
        assert!(settings.graphs.is_empty(), "an empty entry is dropped");
        // A type's node name is kept per graph, and choosing the default drops it.
        settings.set_label("dev/knowledge", "Person", Some("title".into()));
        assert_eq!(settings.labels_for("dev/knowledge")["Person"], "title");
        assert!(settings.labels_for("prod/knowledge").is_empty());
        let text = serde_json::to_string(&settings).unwrap();
        assert!(text.contains(r#""labels":{"Person":"title"}"#), "{text}");
        assert_eq!(read_clean(&text), settings);
        settings.set_label("dev/knowledge", "Person", None);
        assert!(settings.graphs.is_empty(), "the default is no entry");
    }

    #[test]
    fn query_folders_are_safe_names_per_graph() {
        let dir = queries_dir(&overrides_key("dev.example.com", "knowledge"));
        assert!(
            dir.ends_with("queries/dev.example.com~knowledge"),
            "{dir:?}"
        );
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
