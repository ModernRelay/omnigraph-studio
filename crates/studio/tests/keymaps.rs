//! Every binding in Studio's keymaps names an action Studio has, on every platform, and the
//! keys feature crates once bound in code are in the keymaps.
//!
//! `harness = false`: the macOS platform must be created on the main thread.

use std::sync::Arc;

use gpui::HeadlessAppContext;
use settings::{KeymapFile, KeymapFileLoadResult};

const KEYMAPS: [(&str, &str); 3] = [
    (
        "macos",
        include_str!("../../../assets/keymaps/default-macos.json"),
    ),
    (
        "linux",
        include_str!("../../../assets/keymaps/default-linux.json"),
    ),
    (
        "windows",
        include_str!("../../../assets/keymaps/default-windows.json"),
    ),
];

fn main() {
    let text_system = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text_system,
        Arc::new(studio_theme::Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(studio::init);
    for (platform, content) in KEYMAPS {
        let result = cx.update(|cx| KeymapFile::load(content, cx));
        match result {
            KeymapFileLoadResult::Success { key_bindings } => {
                let blocks: Vec<serde_json::Value> = serde_json::from_str(content).unwrap();
                let entries: usize = blocks
                    .iter()
                    .map(|block| block["bindings"].as_object().map_or(0, |b| b.len()))
                    .sum();
                assert_eq!(key_bindings.len(), entries, "{platform}: bindings dropped");
            }
            KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => {
                panic!("{platform}: some bindings failed: {error_message:?}")
            }
            KeymapFileLoadResult::JsonParseFailure { error } => {
                panic!("{platform}: not valid JSON: {error}")
            }
        }
    }
    println!("test every_keymap_binding_names_a_studio_action ... ok");
}
