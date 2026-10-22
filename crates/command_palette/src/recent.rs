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
    studio_settings::data_dir().join("command-palette.json")
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
        cx.update(|cx| add_saved(&mut cx.default_global::<Recent>().keys, keys));
    })
    .detach();
}

/// Adds the saved keys after those recorded before the file arrived, which stay in front, up
/// to the cap.
fn add_saved(keys: &mut Vec<String>, saved: Vec<String>) {
    for key in saved {
        if !keys.contains(&key) && keys.len() < CAP {
            keys.push(key);
        }
    }
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
    // Written on the write thread in the order asked: two runs started close together land
    // in order, the later list last.
    if let Ok(text) = serde_json::to_string_pretty(&saved) {
        studio_settings::write_atomic_in_turn(
            path(),
            text.into_bytes(),
            studio_settings::Access::Private,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The saved list follows what was run before it was read, in its own order, each key
    /// once and no more than the cap.
    #[test]
    fn saved_keys_follow_those_run() {
        let mut keys = vec!["type:Person".to_owned()];
        let saved = ["branch:feature", "type:Person", "type:Topic"];
        add_saved(&mut keys, saved.map(str::to_owned).to_vec());
        assert_eq!(keys, ["type:Person", "branch:feature", "type:Topic"]);

        let many = (0..CAP + 5).map(|n| format!("type:T{n}")).collect();
        add_saved(&mut keys, many);
        assert_eq!(keys.len(), CAP);
        assert_eq!(keys[..3], ["type:Person", "branch:feature", "type:Topic"]);
    }
}
