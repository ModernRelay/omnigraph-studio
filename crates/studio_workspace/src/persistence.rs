//! What a workspace remembers between runs (spec 002): the window's bounds, and per server and
//! graph the open tabs, the active tab and which panels are shown. Plain JSON under Studio's
//! data directory; no database.
//!
//! A tab is remembered only if its item returns [`SavedItem`] from `Item::serialize`, and comes
//! back only if a feature registered its kind with [`register_item_kind`]. A query tab keeps its
//! draft, so the files are readable by their owner only.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use gpui::{App, Bounds, Context, Global, Pixels, Window, WindowId, point, px, size};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Workspace;

/// One remembered tab: the kind its feature registered, and what that feature needs to open it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedItem {
    pub kind: String,
    #[serde(default)]
    pub data: Value,
}

/// One pane's tabs: the active one and how many are pinned (they come first).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedPane {
    pub items: Vec<SavedItem>,
    pub active: usize,
    pub pinned: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWorkspace {
    /// The tabs of a file written before splits; read as one pane.
    pub items: Vec<SavedItem>,
    pub active: usize,
    /// The panes in reading order, and the split tree over their positions in this list.
    pub panes: Vec<SavedPane>,
    pub layout: Option<crate::Member>,
    pub active_pane: usize,
    pub sidebar_open: bool,
    pub inspector_open: bool,
    pub activity_open: bool,
    /// Each dock's active panel.
    pub left_panel: Option<String>,
    pub right_panel: Option<String>,
    pub bottom_panel: Option<String>,
    /// Each dock's size in pixels, once dragged.
    pub left_size: Option<f32>,
    pub right_size: Option<f32>,
    pub bottom_size: Option<f32>,
}

/// Opens a remembered tab of one kind.
pub type OpenSavedItem = fn(&Value, &mut Workspace, &mut Window, &mut Context<Workspace>);

#[derive(Default)]
struct ItemKinds(HashMap<String, OpenSavedItem>);

impl Global for ItemKinds {}

/// Lets tabs of `kind` come back after a restart.
pub fn register_item_kind(cx: &mut App, kind: &str, open: OpenSavedItem) {
    cx.default_global::<ItemKinds>()
        .0
        .insert(kind.to_owned(), open);
}

pub(crate) fn opener(cx: &App, kind: &str) -> Option<OpenSavedItem> {
    cx.try_global::<ItemKinds>()
        .and_then(|kinds| kinds.0.get(kind).copied())
}

/// `OMNIGRAPH_STUDIO_NO_RESTORE=1` starts with no remembered tabs (the snapshot tool sets it).
pub fn restore_enabled() -> bool {
    std::env::var("OMNIGRAPH_STUDIO_NO_RESTORE").map_or(true, |v| v.is_empty() || v == "0")
}

fn workspaces_dir() -> PathBuf {
    studio_settings::data_dir().join("workspaces")
}

/// A file name for a server and graph: a stable FNV-1a hash, so URLs need no escaping.
fn key(server_url: &str, graph: &str) -> String {
    let hash = format!("{server_url}\n{graph}")
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}.json")
}

pub(crate) fn load(server_url: &str, graph: &str) -> Option<SavedWorkspace> {
    let path = workspaces_dir().join(key(server_url, graph));
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&text) {
        Ok(saved) => Some(saved),
        // Kept beside it, so the next save does not overwrite tabs and drafts that may be
        // recoverable.
        Err(_) => {
            studio_settings::set_aside(&path);
            None
        }
    }
}

/// Saves a graph's tabs and drafts, unless restoring is off: a run that did not read them
/// (the snapshot tool) must not replace them.
pub(crate) fn save(server_url: &str, graph: &str, saved: &SavedWorkspace) {
    if !restore_enabled() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(saved) {
        write_private(&workspaces_dir().join(key(server_url, graph)), &text);
    }
}

/// Replaces a file in one step (a crash leaves the old file or the new one, never an empty
/// one), readable only by its owner, including a file an older build created. Written on the
/// write thread, in the order asked, so a save never holds up a frame.
fn write_private(path: &Path, text: &str) {
    studio_settings::write_atomic_in_turn(
        path.to_path_buf(),
        text.as_bytes().to_vec(),
        studio_settings::Access::Private,
    );
}

/// The graphs a window had open, in the order they were opened, and which one it showed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedSession {
    pub graphs: Vec<SavedGraph>,
    #[serde(default)]
    pub shown: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedGraph {
    pub server_url: String,
    pub graph: String,
    pub branch: String,
}

/// A window to reopen at the next start: its graphs and where it was.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    #[serde(flatten)]
    pub session: SavedSession,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bounds: Option<SavedBounds>,
}

impl SavedWindow {
    /// Where the window was, if that still fits on a connected display.
    pub fn bounds(&self, cx: &App) -> Option<Bounds<Pixels>> {
        self.bounds?.on_screen(cx)
    }
}

/// `session.json`: the windows open when Studio last quit, in the order they were opened. A file
/// written before several windows were remembered holds one window's `graphs` and `shown`.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SessionFile {
    windows: Vec<SavedWindow>,
    #[serde(skip_serializing)]
    graphs: Vec<SavedGraph>,
    #[serde(skip_serializing)]
    shown: usize,
}

fn session_path() -> PathBuf {
    studio_settings::data_dir().join("session.json")
}

/// The windows to reopen, if a session was saved and restoring is on.
pub fn load_windows() -> Vec<SavedWindow> {
    if !restore_enabled() {
        return Vec::new();
    }
    let path = session_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    if serde_json::from_str::<SessionFile>(&text).is_err() {
        studio_settings::set_aside(&path);
        return Vec::new();
    }
    parse_session(&text)
}

fn parse_session(text: &str) -> Vec<SavedWindow> {
    let Ok(file) = serde_json::from_str::<SessionFile>(text) else {
        return Vec::new();
    };
    if file.windows.is_empty() && !file.graphs.is_empty() {
        let session = SavedSession {
            graphs: file.graphs,
            shown: file.shown,
        };
        return vec![SavedWindow {
            session,
            bounds: None,
        }];
    }
    file.windows
}

/// This run's windows, in the order they opened: what `session.json` says.
#[derive(Default)]
struct SessionWindows {
    open: BTreeMap<u64, SavedWindow>,
    next: u64,
    /// The last window, closed while no other was open: it comes back at the next start unless
    /// another window opens first.
    last_closed: Option<SavedWindow>,
}

impl SessionWindows {
    fn open(&mut self) -> u64 {
        let slot = self.next;
        self.next += 1;
        self.open.insert(slot, SavedWindow::default());
        self.last_closed = None;
        slot
    }

    fn set_session(&mut self, slot: u64, session: &SavedSession) {
        if let Some(window) = self.open.get_mut(&slot) {
            window.session = session.clone();
        }
    }

    fn set_bounds(&mut self, slot: u64, bounds: SavedBounds) {
        if let Some(window) = self.open.get_mut(&slot) {
            window.bounds = Some(bounds);
        }
    }

    /// A closed window is forgotten, unless it was the last one open.
    fn close(&mut self, slot: u64) {
        if let Some(window) = self.open.remove(&slot)
            && self.open.is_empty()
        {
            self.last_closed = Some(window);
        }
    }

    /// The windows with a graph open, to reopen at the next start.
    fn saved(&self) -> Vec<SavedWindow> {
        let windows: Vec<&SavedWindow> = if self.open.is_empty() {
            self.last_closed.iter().collect()
        } else {
            self.open.values().collect()
        };
        windows
            .into_iter()
            .filter(|window| !window.session.graphs.is_empty())
            .cloned()
            .collect()
    }
}

/// The windows of this run and what was last written for them. Closing a window forgets it;
/// quitting keeps every window, so the next start reopens them all.
struct WindowSession {
    windows: SessionWindows,
    slots: HashMap<WindowId, u64>,
    written: Option<Vec<SavedWindow>>,
    quitting: bool,
}

impl Global for WindowSession {}

impl WindowSession {
    fn update(cx: &mut App, change: impl FnOnce(&mut SessionWindows)) {
        Self::install(cx);
        change(&mut cx.global_mut::<WindowSession>().windows);
        Self::write(cx);
    }

    fn install(cx: &mut App) {
        if cx.has_global::<WindowSession>() {
            return;
        }
        cx.set_global(WindowSession {
            windows: SessionWindows::default(),
            slots: HashMap::new(),
            written: None,
            quitting: false,
        });
        cx.on_window_closed(|cx, id| {
            let session = cx.global_mut::<WindowSession>();
            let Some(slot) = session.slots.remove(&id) else {
                return;
            };
            if !session.quitting {
                session.windows.close(slot);
                Self::write(cx);
            }
        })
        .detach();
        cx.on_app_quit(|cx| {
            cx.global_mut::<WindowSession>().quitting = true;
            // The last state written before the app goes.
            studio_settings::writes_done();
            async {}
        })
        .detach();
    }

    fn write(cx: &mut App) {
        if cfg!(test) || !restore_enabled() {
            return;
        }
        let session = cx.global_mut::<WindowSession>();
        let saved = session.windows.saved();
        if session.written.as_ref() != Some(&saved) {
            write_session(&saved);
            session.written = Some(saved);
        }
    }
}

/// Gives a new window its place in the session; the place is dropped when the window closes.
pub(crate) fn window_opened(window: &Window, cx: &mut App) -> u64 {
    let id = window.window_handle().window_id();
    let mut slot = 0;
    WindowSession::update(cx, |windows| slot = windows.open());
    cx.global_mut::<WindowSession>().slots.insert(id, slot);
    window_moved(slot, window.bounds(), cx);
    slot
}

pub(crate) fn window_graphs_changed(slot: u64, session: &SavedSession, cx: &mut App) {
    WindowSession::update(cx, |windows| windows.set_session(slot, session));
}

pub(crate) fn window_moved(slot: u64, bounds: Bounds<Pixels>, cx: &mut App) {
    WindowSession::update(cx, |windows| {
        windows.set_bounds(slot, SavedBounds::from(bounds))
    });
}

fn write_session(windows: &[SavedWindow]) {
    let file = SessionFile {
        windows: windows.to_vec(),
        ..SessionFile::default()
    };
    if let Ok(text) = serde_json::to_string_pretty(&file) {
        write_private(&session_path(), &text);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct SavedBounds {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl From<Bounds<Pixels>> for SavedBounds {
    fn from(bounds: Bounds<Pixels>) -> Self {
        Self {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        }
    }
}

impl SavedBounds {
    /// These bounds, if they are a usable size and their centre is on a connected display.
    fn on_screen(self, cx: &App) -> Option<Bounds<Pixels>> {
        if self.width < 400. || self.height < 300. {
            return None;
        }
        let bounds = Bounds::new(
            point(px(self.x), px(self.y)),
            size(px(self.width), px(self.height)),
        );
        let on_screen = cx
            .displays()
            .iter()
            .any(|display| display.bounds().contains(&bounds.center()));
        on_screen.then_some(bounds)
    }
}

fn bounds_path() -> PathBuf {
    studio_settings::data_dir().join("window.json")
}

/// Where the last moved window was, for a new window, if that still fits on a display.
pub fn saved_window_bounds(cx: &App) -> Option<Bounds<Pixels>> {
    let text = std::fs::read_to_string(bounds_path()).ok()?;
    serde_json::from_str::<SavedBounds>(&text)
        .ok()?
        .on_screen(cx)
}

pub(crate) fn save_window_bounds(bounds: Bounds<Pixels>) {
    if !restore_enabled() {
        return;
    }
    let saved = SavedBounds::from(bounds);
    if let Ok(text) = serde_json::to_string(&saved) {
        write_private(&bounds_path(), &text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn workspace_files_are_private_even_when_they_existed() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("studio-persistence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspace.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, r#"{"active": 1}"#);
        // On the write thread: done once every write asked before this mark is.
        studio_settings::writes_done();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"active": 1}"#);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn graph(name: &str) -> SavedGraph {
        SavedGraph {
            server_url: "https://example.test".into(),
            graph: name.into(),
            branch: "main".into(),
        }
    }

    fn showing(names: &[&str]) -> SavedSession {
        SavedSession {
            graphs: names.iter().map(|name| graph(name)).collect(),
            shown: 0,
        }
    }

    #[test]
    fn saved_windows_read_back_and_an_older_file_is_one_window() {
        let windows = vec![
            SavedWindow {
                session: showing(&["crm", "notes"]),
                bounds: Some(SavedBounds {
                    x: 10.,
                    y: 20.,
                    width: 1200.,
                    height: 800.,
                }),
            },
            SavedWindow {
                session: showing(&["knowledge"]),
                bounds: None,
            },
        ];
        let file = SessionFile {
            windows: windows.clone(),
            ..SessionFile::default()
        };
        let text = serde_json::to_string(&file).unwrap();
        let keys: Vec<String> = serde_json::from_str::<serde_json::Map<String, Value>>(&text)
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, ["windows"], "only the new shape is written");
        assert_eq!(parse_session(&text), windows);

        let older = parse_session(
            r#"{"graphs": [{"server_url": "https://example.test", "graph": "crm", "branch": "main"}], "shown": 0}"#,
        );
        assert_eq!(
            older,
            vec![SavedWindow {
                session: showing(&["crm"]),
                bounds: None
            }]
        );
        let without_shown = parse_session(
            r#"{"graphs": [{"server_url": "https://example.test", "graph": "crm", "branch": "main"}]}"#,
        );
        assert_eq!(without_shown[0].session.shown, 0);
        assert!(parse_session("{}").is_empty());
        assert!(parse_session("not json").is_empty());
    }

    #[test]
    fn every_open_window_is_kept_and_a_closed_one_is_forgotten() {
        let mut windows = SessionWindows::default();
        let first = windows.open();
        let second = windows.open();
        let empty = windows.open();
        windows.set_session(second, &showing(&["notes"]));
        windows.set_session(first, &showing(&["crm"]));
        let names = |windows: &SessionWindows| -> Vec<String> {
            windows
                .saved()
                .iter()
                .map(|window| window.session.graphs[0].graph.clone())
                .collect()
        };
        assert_eq!(
            names(&windows),
            ["crm", "notes"],
            "in the order the windows opened; a window with no graph is not kept"
        );

        windows.close(first);
        windows.close(empty);
        assert_eq!(names(&windows), ["notes"]);

        windows.close(second);
        assert_eq!(
            names(&windows),
            ["notes"],
            "the last window closed comes back at the next start"
        );
        let third = windows.open();
        assert!(
            windows.saved().is_empty(),
            "until another window opens in its place"
        );
        windows.set_session(third, &showing(&["knowledge"]));
        windows.set_session(second, &showing(&["closed"]));
        assert_eq!(
            names(&windows),
            ["knowledge"],
            "a closed window takes no change"
        );
    }

    #[gpui::test]
    fn a_closed_window_leaves_the_session_and_the_others_stay(cx: &mut gpui::TestAppContext) {
        struct Blank;
        impl gpui::Render for Blank {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::Empty
            }
        }
        let open = |name: &'static str, cx: &mut gpui::TestAppContext| {
            cx.add_window(|window, cx| {
                let slot = window_opened(window, cx);
                window_graphs_changed(slot, &showing(&[name]), cx);
                Blank
            })
        };
        let saved = |cx: &mut gpui::TestAppContext| -> Vec<String> {
            cx.read(|cx| {
                cx.global::<WindowSession>()
                    .windows
                    .saved()
                    .iter()
                    .map(|window| window.session.graphs[0].graph.clone())
                    .collect()
            })
        };
        let first = open("crm", cx);
        let _second = open("notes", cx);
        assert_eq!(saved(cx), ["crm", "notes"]);
        first
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(saved(cx), ["notes"]);
    }

    #[test]
    fn keys_are_stable_and_distinct() {
        assert_eq!(key("http://a", "g"), key("http://a", "g"));
        assert_ne!(key("http://a", "g"), key("http://a", "h"));
        assert_ne!(key("http://a", "g"), key("http://b", "g"));
    }

    #[test]
    fn a_saved_workspace_round_trips_and_tolerates_missing_keys() {
        let mut layout = crate::Member::Pane(0);
        layout.split(0, 1, crate::SplitDirection::Right);
        let saved = SavedWorkspace {
            items: Vec::new(),
            active: 0,
            panes: vec![
                SavedPane {
                    items: vec![SavedItem {
                        kind: "type_table".into(),
                        data: serde_json::json!({"type_name": "Person"}),
                    }],
                    active: 0,
                    pinned: 1,
                },
                SavedPane::default(),
            ],
            layout: Some(layout),
            active_pane: 1,
            sidebar_open: true,
            inspector_open: false,
            activity_open: true,
            left_panel: Some("types".into()),
            right_panel: None,
            bottom_panel: None,
            left_size: Some(300.),
            right_size: None,
            bottom_size: None,
        };
        let text = serde_json::to_string(&saved).unwrap();
        assert_eq!(
            serde_json::from_str::<SavedWorkspace>(&text).unwrap(),
            saved
        );
        let partial: SavedWorkspace = serde_json::from_str(r#"{"active": 2}"#).unwrap();
        assert_eq!(partial.active, 2);
        assert!(partial.items.is_empty());
        assert!(
            partial.panes.is_empty() && partial.layout.is_none(),
            "a file from before splits"
        );
    }
}
