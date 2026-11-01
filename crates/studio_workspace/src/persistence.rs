//! What a workspace remembers between runs (spec 002): the window's bounds, and per graph (its
//! one key, `<target>@<address>/<graph>`) the open tabs, the active tab and which panels are
//! shown. Plain JSON under Studio's data directory; no database.
//!
//! A tab is remembered only if its item returns [`SavedItem`] from `Item::serialize`, and comes
//! back only if a feature registered its kind with [`register_item_kind`]. A query tab keeps its
//! draft, so the files are readable by their owner only.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

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
    /// Where the person placed nodes and relationship pills on the canvas this graph had before
    /// canvases each had a root: a new canvas starts from these (spec 020).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub canvas_positions: std::collections::BTreeMap<String, [f32; 2]>,
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

/// Whether state is written to the data directory. This crate's tests write none of it: their
/// data directory is the person's own.
fn saving() -> bool {
    !cfg!(test) && restore_enabled()
}

fn workspaces_dir() -> PathBuf {
    studio_settings::data_dir().join("workspaces")
}

/// A file name for a graph's key (`<target>@<address>/<graph>`, `GraphSession::overrides_key`):
/// a stable FNV-1a hash, so names need no escaping.
fn file_name(key: &str) -> String {
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}.json")
}

/// A save asked for whose write has not landed: its text, and whether its write failed.
struct Unwritten {
    text: Arc<str>,
    failed: bool,
}

/// The saves asked for whose writes have not landed, by file. Saves are written on the write
/// thread in turn ([`write_state`]), so for a moment the file can hold an older one: a graph
/// closed and opened again then reads its last save from here. A save whose write failed stays
/// until a later one lands.
fn unwritten() -> MutexGuard<'static, HashMap<PathBuf, Unwritten>> {
    static UNWRITTEN: OnceLock<Mutex<HashMap<PathBuf, Unwritten>>> = OnceLock::new();
    UNWRITTEN
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Writes `text` over the state file at `path` as [`write_private`] does, kept in
/// [`unwritten`] until it lands. A failed write (a full disk) is marked: the state it held is
/// what the window still shows, so nothing changes to write it again, and the next flush asks
/// [`write_failed`] instead.
fn write_state(path: PathBuf, text: String) {
    let text: Arc<str> = text.into();
    unwritten().insert(
        path.clone(),
        Unwritten {
            text: text.clone(),
            failed: false,
        },
    );
    studio_settings::run_in_turn(move || {
        let written =
            studio_settings::write_atomic(&path, text.as_bytes(), studio_settings::Access::Private);
        if let Err(error) = &written {
            log::warn!("Studio could not write {}: {error}", path.display());
        }
        let mut unwritten = unwritten();
        // A later save on its way replaces this one either way.
        if let Some(last) = unwritten.get_mut(&path)
            && Arc::ptr_eq(&last.text, &text)
        {
            if written.is_ok() {
                unwritten.remove(&path);
            } else {
                last.failed = true;
            }
        }
    });
}

/// Whether the last write of the state file at `path` failed, and no save has replaced it.
fn write_failed(path: &Path) -> bool {
    unwritten().get(path).is_some_and(|last| last.failed)
}

/// Whether the last save of the graph whose key is `key` failed to be written.
pub(crate) fn save_failed(key: &str) -> bool {
    write_failed(&workspaces_dir().join(file_name(key)))
}

/// The saved state of the graph whose key is `key`.
pub(crate) fn load(key: &str) -> Option<SavedWorkspace> {
    let path = workspaces_dir().join(file_name(key));
    let pending = unwritten().get(&path).map(|last| last.text.clone());
    let text = match pending {
        Some(text) => text.to_string(),
        None => read_or_set_aside(&path)?,
    };
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

/// The text of the file at `path`, or none when there is none. A file that is there but cannot
/// be read (it is not text, or reading it failed) is set aside, as one that does not parse is:
/// taken for no file, it would be written over by the next save.
fn read_or_set_aside(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            studio_settings::set_aside(path);
            None
        }
    }
}

/// Saves a graph's tabs and drafts, unless restoring is off: a run that did not read them
/// (the snapshot tool) must not replace them.
pub(crate) fn save(key: &str, saved: &SavedWorkspace) {
    if !saving() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(saved) {
        write_state(workspaces_dir().join(file_name(key)), text);
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

/// A graph a window had open, by its CLI target's name and address and its id (spec 002), as
/// its saved state is filed (`GraphSession::overrides_key`): a window whose target the CLI no
/// longer lists at that address is not reopened, so a name given to another server does not
/// reopen the graphs of the server that had it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedGraph {
    pub target: String,
    /// The target's address (`ServerEntry::url`).
    pub url: String,
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

/// `session.json`: the windows open when Studio last quit, in the order they were opened.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SessionFile {
    windows: Vec<SavedWindow>,
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
    let Some(text) = read_or_set_aside(&path) else {
        return Vec::new();
    };
    parse_session(&text).unwrap_or_else(|| {
        studio_settings::set_aside(&path);
        Vec::new()
    })
}

/// The windows `text` holds, or `None` when it does not parse.
fn parse_session(text: &str) -> Option<Vec<SavedWindow>> {
    serde_json::from_str::<SessionFile>(text)
        .ok()
        .map(|file| file.windows)
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
    /// The windows the last run saved stay for the next start ([`keep_saved_windows`]).
    kept: bool,
}

impl Global for WindowSession {}

/// Leaves the windows the last run saved for the next start: this run opened one graph of its
/// own (the environment's), and writing its window over them would lose them.
pub fn keep_saved_windows(cx: &mut App) {
    WindowSession::install(cx);
    cx.global_mut::<WindowSession>().kept = true;
}

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
            kept: false,
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
            // A write that failed is tried once more, and the last state written before the
            // app goes.
            Self::write(cx);
            studio_settings::writes_done();
            async {}
        })
        .detach();
    }

    fn write(cx: &mut App) {
        if !saving() {
            return;
        }
        let session = cx.global_mut::<WindowSession>();
        if session.kept {
            return;
        }
        let saved = session.windows.saved();
        // A window reports its graphs after its first draw: until one does, the windows the last
        // run left are what to reopen, and an empty list written over them would lose them to
        // a crash or a quit at launch.
        if session.written.is_none() && saved.is_empty() {
            return;
        }
        if session.written.as_ref() != Some(&saved) || write_failed(&session_path()) {
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
    };
    if let Ok(text) = serde_json::to_string_pretty(&file) {
        write_state(session_path(), text);
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
/// The window bounds saved last this run: a window opened after the first takes them from here,
/// not from `window.json` read again on the main thread. `None` until the first is read or
/// saved.
fn last_bounds() -> MutexGuard<'static, Option<Option<SavedBounds>>> {
    static LAST: OnceLock<Mutex<Option<Option<SavedBounds>>>> = OnceLock::new();
    LAST.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

pub fn saved_window_bounds(cx: &App) -> Option<Bounds<Pixels>> {
    let saved = *last_bounds().get_or_insert_with(|| {
        let text = std::fs::read_to_string(bounds_path()).ok()?;
        serde_json::from_str::<SavedBounds>(&text).ok()
    });
    saved?.on_screen(cx)
}

pub(crate) fn save_window_bounds(bounds: Bounds<Pixels>) {
    if !saving() {
        return;
    }
    let saved = SavedBounds::from(bounds);
    *last_bounds() = Some(Some(saved));
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

    #[cfg(unix)]
    #[test]
    fn a_state_write_that_failed_is_remembered_until_one_lands() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("studio-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        std::fs::write(&path, "old").unwrap();
        // A folder the write cannot add its temporary file to, as a full disk refuses one.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        write_state(path.clone(), "new".into());
        studio_settings::writes_done();
        let kept = unwritten().get(&path).map(|last| last.text.to_string());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(write_failed(&path), "the next flush writes it again");
        assert_eq!(
            kept.as_deref(),
            Some("new"),
            "read back from here meanwhile"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");

        write_state(path.clone(), "newer".into());
        studio_settings::writes_done();
        assert!(!write_failed(&path));
        assert!(
            unwritten().get(&path).is_none(),
            "the file is the state again"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn graph(name: &str) -> SavedGraph {
        SavedGraph {
            target: "example".into(),
            url: "https://example.test".into(),
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
    fn saved_windows_read_back_and_a_file_without_them_is_none() {
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
        };
        let text = serde_json::to_string(&file).unwrap();
        let keys: Vec<String> = serde_json::from_str::<serde_json::Map<String, Value>>(&text)
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, ["windows"]);
        assert_eq!(parse_session(&text), Some(windows));
        assert_eq!(parse_session("{}"), Some(Vec::new()));
        assert_eq!(parse_session("not json"), None);
        // An earlier build's file names its graphs by their server's address alone, or by
        // their target's name alone: it is set aside.
        for graph in [
            r#"{"server_url":"http://a","graph":"g","branch":"main"}"#,
            r#"{"target":"a","graph":"g","branch":"main"}"#,
        ] {
            let file = format!(r#"{{"windows":[{{"graphs":[{graph}]}}]}}"#);
            assert_eq!(parse_session(&file), None, "{graph}");
        }
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
        let open = |name: &'static str, cx: &mut gpui::TestAppContext| {
            cx.add_window(|window, cx| {
                let slot = window_opened(window, cx);
                window_graphs_changed(slot, &showing(&[name]), cx);
                gpui::EmptyView
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
        assert_eq!(file_name("a/g"), file_name("a/g"));
        assert_ne!(file_name("a/g"), file_name("a/h"));
        assert_ne!(file_name("a/g"), file_name("b/g"));
    }

    #[test]
    fn a_saved_workspace_round_trips_and_tolerates_missing_keys() {
        let mut layout = crate::Member::Pane(0);
        layout.split(0, 1, crate::SplitDirection::Right);
        let saved = SavedWorkspace {
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
            canvas_positions: [("n:Person:p1".to_owned(), [12., 34.])].into(),
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
        let partial: SavedWorkspace = serde_json::from_str(r#"{"active_pane": 2}"#).unwrap();
        assert_eq!(partial.active_pane, 2);
        assert!(partial.panes.is_empty() && partial.layout.is_none());
    }
}
