//! The actions run from the palette, most recent first, kept in Studio's data directory
//! (`command-palette.json`). Read once at startup and written after each run, both off the
//! main thread.

use std::path::PathBuf;

use gpui::{App, AppContext as _, Global};
use serde::{Deserialize, Serialize};

use crate::matching::remember;

const CAP: usize = 50;

#[derive(Default)]
struct Recent {
    keys: Vec<String>,
}

impl Global for Recent {}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    recent: Vec<String>,
}

fn path() -> PathBuf {
    graph_store::data_dir().join("command-palette.json")
}

/// Starts reading the saved list. The palette ranks without it until it arrives.
pub fn load(cx: &mut App) {
    cx.set_global(Recent::default());
    cx.spawn(async move |cx| {
        let keys = cx
            .background_spawn(async move {
                std::fs::read_to_string(path())
                    .ok()
                    .and_then(|text| serde_json::from_str::<Saved>(&text).ok())
                    .map(|saved| saved.recent)
                    .unwrap_or_default()
            })
            .await;
        cx.update(|cx| {
            let recent = cx.default_global::<Recent>();
            // Keys recorded before the file arrived stay in front.
            for key in keys {
                if !recent.keys.contains(&key) && recent.keys.len() < CAP {
                    recent.keys.push(key);
                }
            }
        });
    })
    .detach();
}

pub fn keys(cx: &App) -> Vec<String> {
    cx.try_global::<Recent>()
        .map(|recent| recent.keys.clone())
        .unwrap_or_default()
}

/// Moves `key` to the front and saves the list.
pub fn record(key: &str, cx: &mut App) {
    let recent = cx.default_global::<Recent>();
    remember(&mut recent.keys, key, CAP);
    let saved = Saved {
        recent: recent.keys.clone(),
    };
    // Written on the main thread: the list is small, and two runs started close together must
    // land in order, the later list last.
    if let Ok(text) = serde_json::to_string_pretty(&saved) {
        studio_settings::write_atomic(&path(), text.as_bytes(), studio_settings::Access::Private)
            .ok();
    }
}
