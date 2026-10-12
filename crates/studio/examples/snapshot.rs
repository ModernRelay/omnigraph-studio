//! Renders Studio offscreen and saves a PNG, without showing a window.
//!
//!     OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:8787 OMNIGRAPH_STUDIO_TOKEN=dev-token \
//!     OMNIGRAPH_STUDIO_GRAPH=knowledge \
//!     cargo run -p studio --example snapshot -- out.png \
//!       'studio_graph::OpenType={"type_name":"Person"}' 'studio::ToggleActivity'
//!
//! Each argument after the output path is an action, `name` or `name=<json>`, dispatched after
//! the previous one settles. `type:<text>` types text into the focused editor (`\n` is a new
//! line); it is `editor::HandleInput` with the text. `click:<x>,<y>` clicks at a point in
//! window pixels (half the PNG's, which is drawn at 2x). `SNAPSHOT_WAIT_MS` (default 1500) is how
//! long to let each step's network work finish; `SNAPSHOT_SIZE` is `WIDTHxHEIGHT` (default
//! 1440x900). Remembered tabs are not restored unless `OMNIGRAPH_STUDIO_NO_RESTORE=0`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, Focusable as _, HeadlessAppContext, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, point, px, size,
};

/// A left click at `position`, as the platform delivers one: move, press, release.
fn click(
    cx: &mut HeadlessAppContext,
    handle: AnyWindowHandle,
    position: gpui::Point<gpui::Pixels>,
) {
    let events = [
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    ];
    for event in events {
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_event(event, cx);
        })
        .expect("dispatch a mouse event");
        cx.run_until_parked();
    }
}

/// Runs the app for `wait` of real time. The headless dispatcher's clock is simulated, so it is
/// moved forward as real time passes and timers (debounced saves, toasts) fire as in the app.
fn settle(cx: &mut HeadlessAppContext, wait: Duration) {
    let step = Duration::from_millis(25);
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        cx.run_until_parked();
        std::thread::sleep(step);
        cx.advance_clock(step);
    }
    cx.run_until_parked();
}

fn main() {
    // The agent panel starts the running executable as its MCP proxy (spec 011); answer that
    // here as Studio's own binary does.
    let raw: Vec<_> = std::env::args_os().skip(1).collect();
    if raw.first().map(|a| a.as_os_str()) == Some(std::ffi::OsStr::new(agent_panel::PROXY_COMMAND))
    {
        let socket = raw.get(1).expect("mcp-proxy needs a socket path");
        agent_panel::proxy(std::path::Path::new(socket)).expect("proxy the MCP socket");
        return;
    }
    // Start without remembered tabs so a render depends only on its actions; set the variable
    // to 0 to check restoring.
    if std::env::var_os("OMNIGRAPH_STUDIO_NO_RESTORE").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("OMNIGRAPH_STUDIO_NO_RESTORE", "1") };
    }
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "snapshot.png".into());
    let actions: Vec<String> = args.collect();
    let wait = Duration::from_millis(
        std::env::var("SNAPSHOT_WAIT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1500),
    );
    let (width, height) = std::env::var("SNAPSHOT_SIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1440.0_f32, 900.0_f32));

    let text_system = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text_system,
        Arc::new(studio_theme::Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.allow_parking();
    cx.update(studio::init);
    let window = cx
        .open_window(size(px(width), px(height)), studio::build_workspace)
        .expect("open the snapshot window");
    let handle: AnyWindowHandle = window.into();
    settle(&mut cx, wait);

    for spec in actions {
        if let Some((x, y)) = spec
            .strip_prefix("click:")
            .and_then(|at| at.split_once(','))
        {
            let x: f32 = x.trim().parse().expect("click x is a number");
            let y: f32 = y.trim().parse().expect("click y is a number");
            click(&mut cx, handle, point(px(x), px(y)));
            settle(&mut cx, wait);
            continue;
        }
        let (name, data) = match spec.strip_prefix("type:") {
            Some(text) => (
                "editor::HandleInput".to_owned(),
                Some(serde_json::Value::String(text.replace("\\n", "\n"))),
            ),
            None => match spec.split_once('=') {
                Some((name, json)) => (
                    name.to_owned(),
                    Some(serde_json::from_str(json).expect("action data is JSON")),
                ),
                None => (spec.clone(), None),
            },
        };
        cx.update_window(handle, |_, window, cx| {
            // With nothing focused an action reaches only global handlers, so give the
            // workspace focus first, as a click in the window would.
            if window.focused(cx).is_none()
                && let Some(workspace) = studio_workspace::workspace_for(window)
            {
                let focus = workspace.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
            let action = cx
                .build_action(&name, data)
                .unwrap_or_else(|error| panic!("unknown action {name}: {error:?}"));
            window.dispatch_action(action, cx);
        })
        .expect("dispatch an action");
        settle(&mut cx, wait);
    }

    let image = cx.capture_screenshot(handle).expect("render the window");
    image.save(&out).expect("write the PNG");
    println!("{out}");
}
