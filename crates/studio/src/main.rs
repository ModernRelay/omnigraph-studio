//! OmniGraph Studio.

use gpui::App;

fn main() {
    // The agent panel (spec 011) starts this executable as the agent's stdio MCP server; it
    // only copies bytes to Studio's tool socket, before any window or setting is touched.
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new(agent_panel::PROXY_COMMAND)) {
        let Some(socket) = args.next() else {
            eprintln!(
                "usage: omnigraph-studio {} <socket>",
                agent_panel::PROXY_COMMAND
            );
            std::process::exit(2);
        };
        if let Err(error) = agent_panel::proxy(std::path::Path::new(&socket)) {
            eprintln!("omnigraph-studio {}: {error}", agent_panel::PROXY_COMMAND);
            std::process::exit(1);
        }
        return;
    }

    let app = gpui_platform::application().with_assets(studio_theme::Assets);
    // On macOS the app outlives its last window; a click on the Dock icon then reopens the
    // windows that were open, as at a start.
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            studio::open_windows(cx);
            cx.activate(true);
        }
    });
    app.run(|cx: &mut App| {
        studio::init(cx);
        studio::open_windows(cx);
        cx.activate(true);
    });
}
