//! What a workspace remembers between runs (spec 002): the window's bounds, and per server and
//! graph the open tabs, the active tab and which panels are shown. Plain JSON under Studio's
//! data directory; no database.
//!
//! A tab is remembered only if its item returns [`SavedItem`] from `Item::serialize`, and comes
//! back only if a feature registered its kind with [`register_item_kind`]. A query tab keeps its
//! draft, so the files are readable by their owner only.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use gpui::{App, Bounds, Context, Global, Pixels, Window, point, px, size};
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWorkspace {
    pub items: Vec<SavedItem>,
    pub active: usize,
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
pub(crate) fn restore_enabled() -> bool {
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
    let text = std::fs::read_to_string(workspaces_dir().join(key(server_url, graph))).ok()?;
    serde_json::from_str(&text).ok()
}

pub(crate) fn save(server_url: &str, graph: &str, saved: &SavedWorkspace) {
    let dir = workspaces_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(saved) {
        write_private(&dir.join(key(server_url, graph)), &text).ok();
    }
}

/// Writes `text` to a file only its owner can read, including a file an older build created.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(text.as_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct SavedBounds {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn bounds_path() -> PathBuf {
    studio_settings::data_dir().join("window.json")
}

/// The last window's bounds, if they still fit on a connected display.
pub fn saved_window_bounds(cx: &App) -> Option<Bounds<Pixels>> {
    let text = std::fs::read_to_string(bounds_path()).ok()?;
    let saved: SavedBounds = serde_json::from_str(&text).ok()?;
    if saved.width < 400. || saved.height < 300. {
        return None;
    }
    let bounds = Bounds::new(
        point(px(saved.x), px(saved.y)),
        size(px(saved.width), px(saved.height)),
    );
    let on_screen = cx
        .displays()
        .iter()
        .any(|display| display.bounds().contains(&bounds.center()));
    on_screen.then_some(bounds)
}

pub(crate) fn save_window_bounds(bounds: Bounds<Pixels>) {
    let saved = SavedBounds {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
    };
    if let Some(dir) = bounds_path().parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Ok(text) = serde_json::to_string(&saved) {
        std::fs::write(bounds_path(), text).ok();
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
        write_private(&path, r#"{"active": 1}"#).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"active": 1}"#);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keys_are_stable_and_distinct() {
        assert_eq!(key("http://a", "g"), key("http://a", "g"));
        assert_ne!(key("http://a", "g"), key("http://a", "h"));
        assert_ne!(key("http://a", "g"), key("http://b", "g"));
    }

    #[test]
    fn a_saved_workspace_round_trips_and_tolerates_missing_keys() {
        let saved = SavedWorkspace {
            items: vec![SavedItem {
                kind: "type_table".into(),
                data: serde_json::json!({"type_name": "Person"}),
            }],
            active: 0,
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
    }
}
