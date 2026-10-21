//! Settings reach Zed's theme: the theme in `settings.json` is the one Studio starts with, a
//! change made in the running app switches the active theme, and the file Studio writes is the
//! one it reads back. A change rewrites only its own setting: the person's comments and an edit
//! Studio has not read yet stay, and a file that does not parse is not written: the change holds
//! until Studio quits, and the settings page says so.
//!
//! `harness = false`: the macOS platform must be created on the main thread, and libtest runs
//! tests on worker threads.

use std::path::Path;
use std::sync::Arc;

use gpui::HeadlessAppContext;
use studio_settings::{StudioSettings, ThemeMode};
use theme::ActiveTheme as _;

fn main() {
    let data = std::env::temp_dir().join(format!("studio-settings-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("settings.json"),
        r#"{"theme": {"mode": "dark", "dark": "Ayu Dark"}}"#,
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
    // Like `Application::run`'s launch callback, this update does not flush effects.
    cx.update(studio::init);
    settings_choose_the_active_theme(&mut cx, &data);
    println!("test settings_choose_the_active_theme ... ok");
    a_change_keeps_the_persons_text(&mut cx, &data);
    println!("test a_change_keeps_the_persons_text ... ok");
    std::fs::remove_dir_all(&data).ok();
}

fn settings_choose_the_active_theme(cx: &mut HeadlessAppContext, data: &Path) {
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.theme().name.to_string()), "Ayu Dark");

    // Input handlers run in a flushed update, as an `AsyncApp` update is.
    let app = cx.update(|cx| cx.to_async());
    app.update(|cx| studio_settings::update(cx, |s| s.theme.mode = ThemeMode::Light));
    settle(cx);
    assert_eq!(
        cx.update(|cx| cx.theme().name.to_string()),
        "Studio Light",
        "Studio's bundled default light theme"
    );

    app.update(|cx| {
        studio_settings::update(cx, |s| {
            s.theme.mode = ThemeMode::Dark;
            s.theme.dark = "Gruvbox Dark".into();
        })
    });
    settle(cx);
    assert_eq!(cx.update(|cx| cx.theme().name.to_string()), "Gruvbox Dark");

    let written = std::fs::read_to_string(data.join("settings.json")).unwrap();
    let reading = StudioSettings::read(&written).unwrap();
    assert!(reading.problems.is_empty(), "{:?}", reading.problems);
    assert_eq!(reading.settings.theme.mode, ThemeMode::Dark);
    assert_eq!(reading.settings.theme.dark, "Gruvbox Dark");
    assert_eq!(cx.update(|cx| studio_settings::file_error(cx)), None);
    let keys: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        keys,
        serde_json::json!({"theme": {"mode": "dark", "dark": "Gruvbox Dark"}}),
        "only what was chosen is written, never the defaults"
    );
}

/// The person edits the file by hand, with a comment, and Studio changes a setting before its
/// watch has read the edit: the edit and the comment stay, and both apply. A file left broken
/// refuses the next change and keeps its text.
fn a_change_keeps_the_persons_text(cx: &mut HeadlessAppContext, data: &Path) {
    let path = data.join("settings.json");
    let by_hand = r#"{
  // Chosen by hand
  "ui_font_size": 13,
  "theme": {"mode": "dark", "dark": "Gruvbox Dark"},
}
"#;
    std::fs::write(&path, by_hand).unwrap();
    let app = cx.update(|cx| cx.to_async());
    app.update(|cx| studio_settings::update(cx, |s| s.studio.tree_page_size = 20));
    settle(cx);
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains("\n  // Chosen by hand\n  \"ui_font_size\": 13,\n"),
        "the comment stays with its key: {written}"
    );
    let reading = StudioSettings::read(&written).unwrap();
    assert!(reading.problems.is_empty(), "{:?}", reading.problems);
    assert_eq!(reading.settings.studio.tree_page_size, 20);
    let applied = cx.update(|cx| StudioSettings::get(cx).clone());
    assert_eq!(
        applied.ui_font_size, 13.,
        "the edit by hand applies with the change"
    );
    assert_eq!(applied.studio.tree_page_size, 20);

    // A change that leaves the text as it is (the value the file already holds) still reads
    // the file: the hand edit applies now, and the watch does not take it as already read.
    let by_hand = written.replace("\"ui_font_size\": 13", "\"ui_font_size\": 12");
    std::fs::write(&path, &by_hand).unwrap();
    app.update(|cx| studio_settings::update(cx, |s| s.studio.tree_page_size = 20));
    settle(cx);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        by_hand,
        "nothing to write"
    );
    assert_eq!(cx.update(|cx| StudioSettings::get(cx).ui_font_size), 12.);

    let broken = "{\n  \"ui_font_size\": 13\n  \"studio\": {}\n}\n";
    std::fs::write(&path, broken).unwrap();
    app.update(|cx| studio_settings::update(cx, |s| s.studio.tree_page_size = 30));
    settle(cx);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        broken,
        "not written"
    );
    assert!(
        cx.update(|cx| studio_settings::file_error(cx))
            .is_some_and(|error| error.contains("holds only until Studio quits")),
    );
    assert_eq!(
        cx.update(|cx| StudioSettings::get(cx).studio.tree_page_size),
        30,
        "the change applies though the file could not take it"
    );
}

/// Waits for the settings file's write, which runs on Studio's write thread, then runs what its
/// answer wakes on the main thread.
fn settle(cx: &mut HeadlessAppContext) {
    studio_settings::writes_done();
    cx.run_until_parked();
}
