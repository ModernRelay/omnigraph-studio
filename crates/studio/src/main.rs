//! Omnigraph Studio.

// A release build on Windows opens no console window beside the app's own.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui::App;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let command = args.next();
    // On Windows the agent starts through this executable, which runs it without Omnigraph's
    // credentials in its environment (spec 013).
    if command.as_deref() == Some(std::ffi::OsStr::new(agent_panel::LAUNCH_COMMAND)) {
        std::process::exit(agent_panel::launch_agent(args));
    }

    studio::logging::init();
    let app = gpui_platform::application().with_assets(studio_theme::Assets);
    // `omnigraph-studio://` links arrive outside the app's context, at a start or later; each
    // is passed on to the app, which opens it.
    let (links_tx, links_rx) = smol::channel::unbounded::<String>();
    app.on_open_urls(move |urls| {
        for url in urls {
            links_tx.try_send(url).ok();
        }
    });
    // On macOS the app outlives its last window; a click on the Dock icon then reopens the
    // windows that were open, as at a start.
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            studio::open_windows(cx);
            cx.activate(true);
        }
    });
    app.run(move |cx: &mut App| {
        studio::init(cx);
        studio::open_windows(cx);
        cx.activate(true);
        cx.spawn(async move |cx| {
            while let Ok(url) = links_rx.recv().await {
                cx.update(|cx| studio::open_link(&url, cx));
            }
        })
        .detach();
    });
}
