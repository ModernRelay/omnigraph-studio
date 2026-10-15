//! Every binding in Studio's keymaps names an action Studio has, on every platform, and no key
//! that types a character acts while a text field has focus. The person's `keymap.json` wins over
//! Studio's bindings and is read again when it changes.
//!
//! `harness = false`: the macOS platform must be created on the main thread.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui::{Action, HeadlessAppContext, KeyContext};
use settings::{KeymapFile, KeymapFileLoadResult};
use studio_actions::workspace::{OpenGraph, ToggleCommandPalette};

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

/// The platform's command modifier, as keymaps write it.
const MODIFIER: &str = if cfg!(target_os = "macos") {
    "cmd"
} else {
    "ctrl"
};

fn main() {
    // A data directory of this binary's own, holding the person's keymap before Studio starts.
    let data = std::env::temp_dir().join(format!("studio-keymaps-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let keymap = data.join("keymap.json");
    std::fs::write(
        &keymap,
        format!(
            r#"[
  // Mine
  {{
    "bindings": {{
      "{MODIFIER}-k": null,
      "{MODIFIER}-alt-o": "studio::OpenGraph",
      "{MODIFIER}-alt-j": "studio::NoSuchAction",
    }},
  }},
]"#
        ),
    )
    .unwrap();
    // This binary is the only reader of the environment while it runs.
    unsafe { std::env::set_var("OMNIGRAPH_STUDIO_DATA", &data) };

    let text_system = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text_system,
        Arc::new(studio_theme::Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(studio::init);
    every_keymap_binding_names_a_studio_action(&mut cx);
    println!("test every_keymap_binding_names_a_studio_action ... ok");
    the_persons_keymap_wins_and_is_read_again(&mut cx, &keymap);
    println!("test the_persons_keymap_wins_and_is_read_again ... ok");
    std::fs::remove_dir_all(&data).ok();
}

fn every_keymap_binding_names_a_studio_action(cx: &mut HeadlessAppContext) {
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
}

/// Whether `action` is bound to the platform's modifier and `key`, with Alt when `alt`.
fn bound(cx: &mut HeadlessAppContext, action: &dyn Action, key: &str, alt: bool) -> bool {
    cx.update(|cx| {
        cx.key_bindings()
            .borrow()
            .bindings_for_action(action)
            .any(|binding| {
                let [stroke] = binding.keystrokes() else {
                    return false;
                };
                let held = stroke.modifiers();
                let primary = if cfg!(target_os = "macos") {
                    held.platform && !held.control
                } else {
                    held.control && !held.platform
                };
                stroke.key() == key && primary && held.alt == alt && !held.shift
            })
    })
}

fn keymap_problems(cx: &mut HeadlessAppContext) -> Vec<String> {
    cx.update(|cx| studio_settings::keymap_problems(cx).to_vec())
}

/// Lets the file watch see a change: past its two-second check, on the simulated clock.
fn saved(cx: &mut HeadlessAppContext, path: &Path, text: &str) {
    // A later modification time than the last write, on any file system.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(path, text).unwrap();
    cx.advance_clock(Duration::from_secs(3));
    cx.run_until_parked();
}

/// The person's bindings load over Studio's: one adds a key, `null` takes one away, and one
/// naming no action is reported while the rest apply. A file saved broken keeps the bindings in
/// effect; a file saved again replaces them, and Studio's own come back.
fn the_persons_keymap_wins_and_is_read_again(cx: &mut HeadlessAppContext, keymap: &Path) {
    assert!(bound(cx, &OpenGraph, "o", true), "the person's binding");
    assert!(
        !bound(cx, &ToggleCommandPalette, "k", false),
        "removed with null"
    );
    assert!(
        cx.update(|cx| studio_ui::shortcut_text(&ToggleCommandPalette, cx))
            .is_some_and(|text| !text.ends_with('K')),
        "text naming the palette's key shows the one left"
    );
    let problems = keymap_problems(cx);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("studio::NoSuchAction"), "{problems:?}");
    assert!(
        cx.update(|cx| studio_settings::file_error(cx))
            .is_some_and(|error| error.starts_with("keymap.json: ") && !error.contains('`')),
        "the toast names the file, in plain text"
    );

    saved(cx, keymap, "[ { \"bindings\": ");
    let problems = keymap_problems(cx);
    assert!(
        problems.len() == 1 && problems[0].contains("could not be read"),
        "{problems:?}"
    );
    assert!(
        bound(cx, &OpenGraph, "o", true),
        "a broken file keeps the bindings"
    );

    saved(cx, keymap, "[]");
    assert_eq!(keymap_problems(cx), Vec::<String>::new());
    assert!(
        !bound(cx, &OpenGraph, "o", true),
        "the person's binding is gone"
    );
    assert!(
        bound(cx, &ToggleCommandPalette, "k", false),
        "Studio's binding is back"
    );
}
