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
//! window pixels (half the PNG's, which is drawn at 2x), with `,cmd` or `,shift` held when
//! added and `,right` for the secondary button; `hover:<x>,<y>` moves the pointer there and rests; `key:<keystroke>` presses keys as the keymap sees them (`key:tab`, `key:cmd-4`);
//! `drag:<x1>,<y1>,<x2>,<y2>` presses at
//! the first point, moves to the second in steps, and releases (`,shift` or `,cmd` held); `wait:<ms>` lets that long pass
//! (for a write from outside to land between steps); `measure:<n>[,refresh]` draws n frames
//! after notifying the workspace (or redrawing everything) and prints their times;
//! `measure:<n>,scroll=<dy>` first scrolls at the window's centre each frame (a zoom on the
//! canvas; `scroll=<dx>:<dy>` scrolls sideways too), and `measure:<n>,drag=<dx>:<dy>` drags from the centre by that much a frame (a pan),
//! both back and forth every 30 frames; `measure:<n>,type` types a letter into the focused
//! field and takes it back, alternately. Each also prints the main thread's CPU time per
//! frame, which a busy machine does not inflate. `SNAPSHOT_WAIT_MS` (default 1500) is how
//! long to let each step's network work finish; `SNAPSHOT_SIZE` is `WIDTHxHEIGHT` (default
//! 1440x900). Remembered tabs are not restored unless `OMNIGRAPH_STUDIO_NO_RESTORE=0`. The run
//! keeps its state in a fresh temporary directory unless `OMNIGRAPH_STUDIO_DATA` names one, so
//! it neither reads nor writes the app's settings, tabs or conversations.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, Focusable as _, HeadlessAppContext, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, ScrollDelta, ScrollEvent, ScrollSource,
    TouchPhase, point, px, size,
};

/// The calling thread's CPU time so far.
#[cfg(unix)]
fn thread_cpu_time() -> Duration {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec for the call to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

/// Elsewhere the wall clock stands in, so CPU times equal wall times.
#[cfg(not(unix))]
fn thread_cpu_time() -> Duration {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// A left-button drag from `from` to `to`, in ten moves.
fn drag(
    cx: &mut HeadlessAppContext,
    handle: AnyWindowHandle,
    from: gpui::Point<gpui::Pixels>,
    to: gpui::Point<gpui::Pixels>,
    modifiers: Modifiers,
) {
    let mut events = vec![
        PlatformInput::MouseMove(MouseMoveEvent {
            position: from,
            pressed_button: None,
            modifiers,
        }),
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: from,
            modifiers,
            click_count: 1,
            first_mouse: false,
        }),
    ];
    for step in 1..=10 {
        let t = step as f32 / 10.;
        events.push(PlatformInput::MouseMove(MouseMoveEvent {
            position: point(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t),
            pressed_button: Some(MouseButton::Left),
            modifiers,
        }));
    }
    events.push(PlatformInput::MouseUp(MouseUpEvent {
        button: MouseButton::Left,
        position: to,
        modifiers,
        click_count: 1,
    }));
    for event in events {
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_event(event, cx);
        })
        .expect("dispatch a mouse event");
        cx.run_until_parked();
    }
}

/// A left click at `position`, as the platform delivers one: move, press, release.
fn click(
    cx: &mut HeadlessAppContext,
    handle: AnyWindowHandle,
    position: gpui::Point<gpui::Pixels>,
    modifiers: Modifiers,
    button: MouseButton,
) {
    let events = [
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
        PlatformInput::MouseDown(MouseDownEvent {
            button,
            position,
            modifiers,
            click_count: 1,
            first_mouse: false,
        }),
        PlatformInput::MouseUp(MouseUpEvent {
            button,
            position,
            modifiers,
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
    // On Windows the agent panel starts the running executable as the agent's launcher (spec
    // 013); answer that here as Studio's own binary does.
    let raw: Vec<_> = std::env::args_os().skip(1).collect();
    if raw.first().map(|a| a.as_os_str()) == Some(std::ffi::OsStr::new(agent_panel::LAUNCH_COMMAND))
    {
        std::process::exit(agent_panel::launch_agent(raw.into_iter().skip(1)));
    }
    // Start without remembered tabs so a render depends only on its actions; set the variable
    // to 0 to check restoring.
    if std::env::var_os("OMNIGRAPH_STUDIO_NO_RESTORE").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("OMNIGRAPH_STUDIO_NO_RESTORE", "1") };
    }
    if std::env::var_os("OMNIGRAPH_STUDIO_DATA").is_none_or(|dir| dir.is_empty()) {
        let dir =
            std::env::temp_dir().join(format!("omnigraph-studio-snapshot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the snapshot's data directory");
        eprintln!("snapshot data: {}", dir.display());
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("OMNIGRAPH_STUDIO_DATA", &dir) };
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
    // A headless window gets no animation frames, so a glide would be captured at its first
    // frame: snapshots are stills, taken with motion reduced.
    cx.update(|cx| cx.set_reduce_motion(true));
    let window = cx
        .open_window(size(px(width), px(height)), studio::build_workspace)
        .expect("open the snapshot window");
    let handle: AnyWindowHandle = window.into();
    // The window is the one in front, as Studio's is while someone uses it: views that only work
    // while a window is active (the branch-head poll) run.
    cx.update_window(handle, |_, window, _| window.activate_window())
        .expect("activate the snapshot window");
    settle(&mut cx, wait);

    for spec in actions {
        // `measure:<n>` draws n frames, each after the workspace (the window's root view) is
        // notified, as a toast or a hover in the frame would, and prints the draw times;
        // `measure:<n>,refresh` redraws everything each time, cached views included.
        if let Some(rest) = spec.strip_prefix("measure:") {
            let mut parts = rest.split(',').map(str::trim);
            let frames: usize = parts
                .next()
                .and_then(|n| n.parse().ok())
                .expect("measure:<frames>");
            let mut refresh = false;
            let mut typing = false;
            let mut scroll: Option<(f32, f32)> = None;
            let mut drag: Option<(f32, f32)> = None;
            for part in parts {
                if part == "refresh" {
                    refresh = true;
                } else if part == "type" {
                    typing = true;
                } else if let Some(delta) = part.strip_prefix("scroll=") {
                    scroll = Some(match delta.split_once(':') {
                        Some((dx, dy)) => (
                            dx.parse().expect("scroll=<dx>:<dy>"),
                            dy.parse().expect("scroll=<dx>:<dy>"),
                        ),
                        None => (0., delta.parse().expect("scroll=<pixels>")),
                    });
                } else if let Some(delta) = part.strip_prefix("drag=") {
                    let (dx, dy) = delta.split_once(':').expect("drag=<dx>:<dy>");
                    drag = Some((dx.parse().expect("drag dx"), dy.parse().expect("drag dy")));
                }
            }
            // Input goes to the window's centre: each frame first handles one scroll or one move
            // of a held button, as a frame of a real zoom or pan does, then draws.
            let center = point(px(width / 2.), px(height / 2.));
            if drag.is_some() {
                cx.update_window(handle, |_, window, cx| {
                    window.dispatch_event(
                        PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Left,
                            position: center,
                            modifiers: Modifiers::default(),
                            click_count: 1,
                            first_mouse: false,
                        }),
                        cx,
                    );
                })
                .expect("press for the drag");
            }
            let mut times = Vec::with_capacity(frames);
            let mut cpu_times = Vec::with_capacity(frames);
            for frame in 0..frames {
                // Back and forth every 30 frames, so the view stays where it can be measured.
                let sign = if (frame / 30) % 2 == 0 { 1. } else { -1. };
                cx.update_window(handle, |root, window, cx| {
                    let start = Instant::now();
                    let cpu_start = thread_cpu_time();
                    if typing {
                        // Typing into the focused field: a letter, then taking it back.
                        let key = if frame % 2 == 0 { "a" } else { "backspace" };
                        window.dispatch_keystroke(
                            gpui::Keystroke::parse(key).expect("a keystroke"),
                            cx,
                        );
                    } else if let Some((dx, dy)) = scroll {
                        window.dispatch_event(
                            PlatformInput::Scroll(ScrollEvent {
                                position: center,
                                delta: ScrollDelta::Pixels(point(px(dx * sign), px(dy * sign))),
                                modifiers: Modifiers::default(),
                                touch_phase: TouchPhase::Moved,
                                source: ScrollSource::Pointer,
                            }),
                            cx,
                        );
                    } else if let Some((dx, dy)) = drag {
                        let step = (frame % 30) as f32 + 1.;
                        let along = if sign > 0. { step } else { 30. - step };
                        window.dispatch_event(
                            PlatformInput::MouseMove(MouseMoveEvent {
                                position: point(
                                    center.x + px(dx * along),
                                    center.y + px(dy * along),
                                ),
                                pressed_button: Some(MouseButton::Left),
                                modifiers: Modifiers::default(),
                            }),
                            cx,
                        );
                    } else if refresh {
                        window.refresh();
                    } else {
                        cx.notify(root.entity_id());
                    }
                    window.draw(cx).clear(cx);
                    times.push(start.elapsed());
                    cpu_times.push(thread_cpu_time() - cpu_start);
                })
                .expect("draw a frame");
            }
            if drag.is_some() {
                cx.update_window(handle, |_, window, cx| {
                    window.dispatch_event(
                        PlatformInput::MouseUp(MouseUpEvent {
                            button: MouseButton::Left,
                            position: center,
                            modifiers: Modifiers::default(),
                            click_count: 1,
                        }),
                        cx,
                    );
                })
                .expect("release the drag");
            }
            times.sort();
            cpu_times.sort();
            let at = |times: &[Duration], q: f32| times[((times.len() - 1) as f32 * q) as usize];
            let kind = match (typing, scroll, drag, refresh) {
                (true, _, _, _) => " (type)",
                (_, Some(_), _, _) => " (scroll)",
                (_, _, Some(_), _) => " (drag)",
                (_, _, _, true) => " (refresh)",
                _ => "",
            };
            // Wall time is what a frame took; the main thread's CPU time is the work in it,
            // which other processes on a busy machine do not inflate.
            eprintln!(
                "measure{kind}: {frames} frames, median {:.2?}, p90 {:.2?}, p95 {:.2?}, p99 {:.2?}, max {:.2?} | cpu median {:.2?}, p95 {:.2?}, p99 {:.2?}",
                at(&times, 0.5),
                at(&times, 0.9),
                at(&times, 0.95),
                at(&times, 0.99),
                at(&times, 1.),
                at(&cpu_times, 0.5),
                at(&cpu_times, 0.95),
                at(&cpu_times, 0.99),
            );
            continue;
        }
        if let Some(ms) = spec.strip_prefix("wait:") {
            settle(
                &mut cx,
                Duration::from_millis(ms.trim().parse().expect("wait is milliseconds")),
            );
            continue;
        }
        if let Some((x, y)) = spec
            .strip_prefix("hover:")
            .and_then(|at| at.split_once(','))
        {
            let position = point(
                px(x.trim().parse().expect("hover x is a number")),
                px(y.trim().parse().expect("hover y is a number")),
            );
            cx.update_window(handle, |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: None,
                        modifiers: Modifiers::default(),
                    }),
                    cx,
                );
            })
            .expect("move the pointer");
            settle(&mut cx, wait);
            continue;
        }
        if let Some(keys) = spec.strip_prefix("key:") {
            for key in keys.split(' ') {
                let keystroke = gpui::Keystroke::parse(key).expect("a keystroke");
                cx.update_window(handle, |_, window, cx| {
                    window.dispatch_keystroke(keystroke, cx);
                })
                .expect("press a key");
                cx.run_until_parked();
            }
            settle(&mut cx, wait);
            continue;
        }
        if let Some(points) = spec.strip_prefix("drag:") {
            let parts: Vec<&str> = points.split(',').map(str::trim).collect();
            let n: Vec<f32> = parts.iter().filter_map(|v| v.parse().ok()).collect();
            let [x1, y1, x2, y2] = n[..] else {
                panic!("drag:<x1>,<y1>,<x2>,<y2>[,shift]")
            };
            let modifiers = Modifiers {
                shift: parts.contains(&"shift"),
                platform: parts.contains(&"cmd"),
                ..Modifiers::default()
            };
            drag(
                &mut cx,
                handle,
                point(px(x1), px(y1)),
                point(px(x2), px(y2)),
                modifiers,
            );
            settle(&mut cx, wait);
            continue;
        }
        if let Some(at) = spec.strip_prefix("click:") {
            let parts: Vec<&str> = at.split(',').map(str::trim).collect();
            let x: f32 = parts[0].parse().expect("click x is a number");
            let y: f32 = parts[1].parse().expect("click y is a number");
            let modifiers = Modifiers {
                platform: parts.contains(&"cmd"),
                control: parts.contains(&"ctrl"),
                shift: parts.contains(&"shift"),
                ..Modifiers::default()
            };
            // `right` clicks with the secondary button, for context menus.
            let button = if parts.contains(&"right") {
                MouseButton::Right
            } else {
                MouseButton::Left
            };
            click(&mut cx, handle, point(px(x), px(y)), modifiers, button);
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
