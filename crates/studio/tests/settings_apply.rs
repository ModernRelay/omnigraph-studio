//! Settings reach Zed's theme: the theme in `settings.json` is the one Studio starts with, a
//! change made in the running app switches the active theme, and the file Studio writes is the
//! one it reads back.
//!
//! `harness = false`: the macOS platform must be created on the main thread, and libtest runs
//! tests on worker threads.

use std::sync::Arc;

use gpui::HeadlessAppContext;
use studio_settings::{StudioSettings, ThemeMode};
use theme::ActiveTheme as _;

fn main() {
    settings_choose_the_active_theme();
    println!("test settings_choose_the_active_theme ... ok");
}

fn settings_choose_the_active_theme() {
    let data = std::env::temp_dir().join(format!("studio-settings-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("settings.json"),
        r#"{"theme": {"mode": "dark", "dark": "Ayu Dark"}}"#,
    )
    .unwrap();
    // This binary holds this one test, so nothing reads the environment concurrently.
    unsafe { std::env::set_var("OMNIGRAPH_STUDIO_DATA", &data) };

    let text_system = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text_system,
        Arc::new(studio_theme::Assets),
        gpui_platform::current_headless_renderer,
    );
    // Like `Application::run`'s launch callback, this update does not flush effects.
    cx.update(studio::init);
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.theme().name.to_string()), "Ayu Dark");

    // Input handlers run in a flushed update, as an `AsyncApp` update is.
    let app = cx.update(|cx| cx.to_async());
    app.update(|cx| studio_settings::update(cx, |s| s.theme.mode = ThemeMode::Light));
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.theme().name.to_string()), "One Light");

    app.update(|cx| {
        studio_settings::update(cx, |s| {
            s.theme.mode = ThemeMode::Dark;
            s.theme.dark = "Gruvbox Dark".into();
        })
    });
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.theme().name.to_string()), "Gruvbox Dark");

    let written = std::fs::read_to_string(data.join("settings.json")).unwrap();
    let read = StudioSettings::parse(&written).unwrap();
    assert_eq!(read.theme.mode, ThemeMode::Dark);
    assert_eq!(read.theme.dark, "Gruvbox Dark");
    assert_eq!(cx.update(|cx| studio_settings::file_error(cx)), None);

    std::fs::remove_dir_all(&data).ok();
}
