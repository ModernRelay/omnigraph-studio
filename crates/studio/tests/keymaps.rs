//! Every binding in Studio's keymaps names an action Studio has, on every platform, and no key
//! that types a character acts while a text field has focus.
//!
//! `harness = false`: the macOS platform must be created on the main thread.

use std::sync::Arc;

use gpui::{HeadlessAppContext, KeyContext};
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
                // A text field is a Zed editor. A binding that matches with one focused takes its
                // key before the field does, so a key that types a character there must not.
                let in_field = [
                    KeyContext::parse("Workspace").unwrap(),
                    KeyContext::parse("Editor mode=single_line").unwrap(),
                ];
                for binding in &key_bindings {
                    let [stroke] = binding.keystrokes() else {
                        continue;
                    };
                    let held = stroke.modifiers();
                    let types = !held.control
                        && !held.alt
                        && !held.platform
                        && !held.function
                        && (stroke.key().chars().count() == 1 || stroke.key() == "space");
                    // `depth_of`, as dispatch does: a context matches at any level of the path.
                    let acts_in_field = binding
                        .predicate()
                        .is_none_or(|predicate| predicate.depth_of(&in_field).is_some());
                    assert!(
                        !(types && acts_in_field),
                        "{platform}: `{}` runs {} in a text field instead of typing",
                        stroke.key(),
                        binding.action().name()
                    );
                }
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
