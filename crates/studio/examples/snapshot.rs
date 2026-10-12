//! Renders Studio offscreen and saves a PNG, without showing a window.
//!
//!     OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:8787 OMNIGRAPH_STUDIO_TOKEN=dev-token \
//!     OMNIGRAPH_STUDIO_GRAPH=knowledge \
//!     cargo run -p studio --example snapshot -- out.png \
//!       'studio_graph::OpenType={"type_name":"Person"}' 'studio::ToggleActivity'
//!
//! Each argument after the output path is an action, `name` or `name=<json>`, dispatched after
//! the previous one settles. `SNAPSHOT_WAIT_MS` (default 1500) is how long to let each step's
//! network work finish; `SNAPSHOT_SIZE` is `WIDTHxHEIGHT` (default 1440x900).

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, HeadlessAppContext, px, size};

fn settle(cx: &mut HeadlessAppContext, wait: Duration) {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(25));
    }
    cx.run_until_parked();
}

fn main() {
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
        let (name, data) = match spec.split_once('=') {
            Some((name, json)) => (
                name.to_owned(),
                Some(serde_json::from_str(json).expect("action data is JSON")),
            ),
            None => (spec.clone(), None),
        };
        cx.update_window(handle, |_, window, cx| {
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
