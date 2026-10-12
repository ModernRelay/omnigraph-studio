//! Key bindings: Zed's default keymap first, for the editor and menus, then Studio's, which
//! wins where both bind a key (spec 002).

use gpui::App;
use settings::{KeymapFile, KeymapFileLoadResult};

#[cfg(target_os = "macos")]
const STUDIO_KEYMAP: &str = include_str!("../../../assets/keymaps/default-macos.json");
#[cfg(target_os = "windows")]
const STUDIO_KEYMAP: &str = include_str!("../../../assets/keymaps/default-windows.json");
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const STUDIO_KEYMAP: &str = include_str!("../../../assets/keymaps/default-linux.json");

pub fn load(cx: &mut App) {
    // Zed's keymap binds many actions Studio does not have; those bindings are dropped.
    bind(&settings::default_keymap(), cx);
    bind(STUDIO_KEYMAP, cx);
}

fn bind(content: &str, cx: &mut App) {
    match KeymapFile::load(content, cx) {
        KeymapFileLoadResult::Success { key_bindings }
        | KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => cx.bind_keys(key_bindings),
        KeymapFileLoadResult::JsonParseFailure { error } => {
            eprintln!("Studio could not read a keymap: {error}");
        }
    }
}
