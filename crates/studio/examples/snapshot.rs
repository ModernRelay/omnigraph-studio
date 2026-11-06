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
//! (for a write from outside to land between steps); `shot:<path>` saves the window as it
//! is to another PNG, so one run can record the frames of a sequence; `measure:<n>[,refresh]` draws n frames
//! after notifying the workspace (or redrawing everything) and prints their times;
//! `measure:<n>,scroll=<dy>` first scrolls at the window's centre each frame (a zoom on the
//! canvas; `scroll=<dx>:<dy>` scrolls sideways too), and `measure:<n>,drag=<dx>:<dy>` drags from the centre by that much a frame (a pan),
//! both back and forth every 30 frames (`,at=<x>:<y>` starts either there instead, a panel's
//! edge for a resize); `measure:<n>,type` types a letter into the focused
//! field and takes it back, alternately. Each also prints the main thread's CPU time per
//! frame, which a busy machine does not inflate. `SNAPSHOT_WAIT_MS` (default 1500) is how
//! long to let each step's network work finish; `SNAPSHOT_SIZE` is `WIDTHxHEIGHT` (default
//! 1440x900). Remembered tabs are not restored unless `OMNIGRAPH_STUDIO_NO_RESTORE=0`. The run
//! keeps its state in a fresh temporary directory unless `OMNIGRAPH_STUDIO_DATA` names one, so
//! it neither reads nor writes the app's settings, tabs or conversations.
//!
//! After each step it prints a `step` line (`report:<name>` names the next step): how long until
//! the requests it started had all ended (`settled`, wall time, indicative), the main thread's
//! CPU time and allocations over the step, the requests by route with their bytes, the views
//! drawn, and the time and allocations of the frame drawn after it. `measure` adds allocations
//! and views drawn per frame, which are exact where times are not. `heap` is what every thread
//! holds allocated after the step, for growth across steps that should give back what they took
//! (a tab opened and closed). With `OMNIGRAPH_STUDIO_TRACE`
//! naming a file, the run's spans (requests, decoding, frames) are written there as Chrome trace
//! JSON (`studio_perf`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, Focusable as _, HeadlessAppContext, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, ScrollDelta, ScrollEvent, ScrollSource,
    TouchPhase, point, px, size,
};

/// Counts the allocations made on each thread into `studio_perf`'s counters, so a step's and a
/// frame's are known exactly, and keeps the heap every thread holds as a gauge (Zed's
/// `bench_metrics::CountingAllocator`; divan's `AllocProfiler`). Counting takes no lock and
/// allocates nothing.
struct CountingAllocator;

static ALLOCATIONS: studio_perf::Counter =
    studio_perf::Counter::new("alloc", "allocations", studio_perf::Unit::Count);
static ALLOCATED: studio_perf::Counter =
    studio_perf::Counter::new("alloc", "bytes", studio_perf::Unit::Bytes);
static HEAP: studio_perf::Gauge = studio_perf::Gauge::new("heap", studio_perf::Unit::Bytes);

fn note_allocation(bytes: usize) {
    ALLOCATIONS.add(1);
    ALLOCATED.add(bytes as u64);
}

// SAFETY: every call is passed to the system allocator unchanged.
unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        note_allocation(layout.size());
        HEAP.add(layout.size() as i64);
        // SAFETY: the caller's contract, passed on.
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        HEAP.add(-(layout.size() as i64));
        // SAFETY: the caller's contract, passed on.
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        note_allocation(layout.size());
        HEAP.add(layout.size() as i64);
        // SAFETY: the caller's contract, passed on.
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        note_allocation(new_size);
        HEAP.add(new_size as i64 - layout.size() as i64);
        // SAFETY: the caller's contract, passed on.
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// A request's counters are kept by its method (`graph_client::route`).
fn is_request(group: &str) -> bool {
    group.chars().all(|c| c.is_ascii_uppercase())
}

/// Requests ended so far, on every route.
fn requests_ended() -> u64 {
    studio_perf::counters()
        .filter(|counter| is_request(counter.group))
        .map(|counter| counter.total())
        .sum()
}

/// What one step did: a `studio_perf` step span, which carries every counter's count from before
/// it to after it settled, and how long its actions waited on their requests.
struct Step {
    span: studio_perf::Span,
    started: Instant,
    /// Each action's time from its start until its last request ended, added up.
    busy: Duration,
    cpu: Duration,
}

impl Step {
    fn begin(name: &str) -> Self {
        Self {
            span: studio_perf::step("step", name.to_owned()),
            started: Instant::now(),
            busy: Duration::ZERO,
            cpu: thread_cpu_time(),
        }
    }

    /// Adds an action that began at `began` and whose last request ended at `last` ([`settle`]).
    fn note(&mut self, began: Instant, last: Option<Instant>) {
        if let Some(last) = last {
            self.busy += last.saturating_duration_since(began);
        }
    }

    /// Ends the step and prints its line, from what the step span carries: `settled` is when
    /// its last request ended, then the frame drawn after it.
    fn report(self, cx: &mut HeadlessAppContext, handle: AnyWindowHandle) {
        let cpu = thread_cpu_time() - self.cpu;
        let Some(step) = self.span.end() else {
            return;
        };
        let frame_start = Instant::now();
        let frame = studio_perf::step("frame", "after step");
        cx.update_window(handle, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .expect("draw the frame after a step");
        let frame = frame.end().expect("measuring is on");
        let frame_time = frame_start.elapsed();
        let named = |group: &str| -> Vec<String> {
            step.attached
                .iter()
                .filter(|a| a.group == group)
                .map(|a| format!("{}×{}", a.name, a.all))
                .collect()
        };
        let requests: Vec<String> = step
            .attached
            .iter()
            .filter(|a| is_request(a.group))
            .map(|a| format!("{} {}×{}", a.group, a.name, a.all))
            .collect();
        let count: u64 = step
            .attached
            .iter()
            .filter(|a| is_request(a.group))
            .map(|a| a.all)
            .sum();
        let here =
            |record: &studio_perf::Record, name| record.get("alloc", name).map_or(0, |a| a.here);
        eprintln!(
            "step\t{}\tsettled {:.1} ms\tcpu {:.1} ms\tallocs {} ({:.1} MB)\trequests {count} ({:.1} KB) [{}]\tqueries [{}]\tdraws [{}]\tframe {:.1} ms, {} views, {} allocs\theap {:.1} MB",
            step.name,
            self.busy.as_secs_f64() * 1000.,
            cpu.as_secs_f64() * 1000.,
            here(&step, "allocations"),
            here(&step, "bytes") as f64 / 1e6,
            step.sum("received").0 as f64 / 1e3,
            requests.join(", "),
            named("query").join(", "),
            named("draw").join(", "),
            frame_time.as_secs_f64() * 1000.,
            frame.sum("draw").0,
            here(&frame, "allocations"),
            HEAP.get() as f64 / 1e6,
        );
        let _ = self.started;
    }
}

/// Ends an action: one outside a named step (`adhoc`) is reported at once, one inside the step
/// `report:` opened adds to it.
fn end_action(
    open: &mut Option<Step>,
    adhoc: Option<Step>,
    began: Instant,
    last: Option<Instant>,
    cx: &mut HeadlessAppContext,
    handle: AnyWindowHandle,
) {
    match adhoc {
        Some(mut step) => {
            step.note(began, last);
            step.report(cx, handle);
        }
        None => {
            if let Some(step) = open {
                step.note(began, last);
            }
        }
    }
}

/// Reports the step `report:` opened, if one is open.
fn close_step(open: &mut Option<Step>, cx: &mut HeadlessAppContext, handle: AnyWindowHandle) {
    if let Some(step) = open.take() {
        step.report(cx, handle);
    }
}

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
///
/// Returns when the last request under way ended, if any ran: a step's network work.
fn settle(cx: &mut HeadlessAppContext, wait: Duration) -> Option<Instant> {
    let step = Duration::from_millis(5);
    let deadline = Instant::now() + wait;
    let requests = requests_ended;
    let mut seen = requests();
    let mut last = None;
    while Instant::now() < deadline {
        cx.run_until_parked();
        let now = requests();
        if studio_perf::REQUESTS_UNDER_WAY.get() > 0 || now != seen {
            seen = now;
            last = Some(Instant::now());
        }
        std::thread::sleep(step);
        cx.advance_clock(step);
    }
    cx.run_until_parked();
    last
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
    // Every span is kept, and every counter counts, from here on.
    studio_perf::init();
    studio_perf::collect(true);
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

    let startup = Step::begin("start");
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
    // Start: from before GPUI starts until the window's first reads have ended.
    let mut startup = startup;
    let began = startup.started;
    startup.note(began, settle(&mut cx, wait));
    startup.report(&mut cx, handle);
    // The step `report:<name>` opened, which the actions after it add to until the next.
    let mut open: Option<Step> = None;

    for spec in actions {
        // `measure:<n>` draws n frames, each after the workspace (the window's root view) is
        // notified, as a toast or a hover in the frame would, and prints the draw times;
        // `measure:<n>,refresh` redraws everything each time, cached views included.
        if let Some(rest) = spec.strip_prefix("measure:") {
            close_step(&mut open, &mut cx, handle);
            let mut parts = rest.split(',').map(str::trim);
            let frames: usize = parts
                .next()
                .and_then(|n| n.parse().ok())
                .expect("measure:<frames>");
            let mut refresh = false;
            let mut typing = false;
            let mut scroll: Option<(f32, f32)> = None;
            let mut drag: Option<(f32, f32)> = None;
            let mut at: Option<(f32, f32)> = None;
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
                } else if let Some(point) = part.strip_prefix("at=") {
                    let (x, y) = point.split_once(':').expect("at=<x>:<y>");
                    at = Some((x.parse().expect("at x"), y.parse().expect("at y")));
                } else if let Some(delta) = part.strip_prefix("drag=") {
                    let (dx, dy) = delta.split_once(':').expect("drag=<dx>:<dy>");
                    drag = Some((dx.parse().expect("drag dx"), dy.parse().expect("drag dy")));
                }
            }
            // Input goes to the window's centre: each frame first handles one scroll or one move
            // of a held button, as a frame of a real zoom or pan does, then draws.
            let center = match at {
                Some((x, y)) => point(px(x), px(y)),
                None => point(px(width / 2.), px(height / 2.)),
            };
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
            let mut frame_allocations = Vec::with_capacity(frames);
            // The measure is a step of its own, each frame one inside it: what each counted
            // (views drawn, allocations, requests) comes back attached to it.
            let measure = studio_perf::step("measure", spec.clone());
            let cpu_total = thread_cpu_time();
            for frame in 0..frames {
                // Back and forth every 30 frames, so the view stays where it can be measured.
                let sign = if (frame / 30) % 2 == 0 { 1. } else { -1. };
                // A frame is one update that delivers the input; as it ends, GPUI (headless, in
                // its test mode) draws what the input changed, once, as an app's next frame
                // would. Timed from outside, so the draw is in it.
                let frame_span = studio_perf::step("frame", "measure");
                let start = Instant::now();
                let cpu_start = thread_cpu_time();
                cx.update_window(handle, |root, window, cx| {
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
                })
                .expect("draw a frame");
                times.push(start.elapsed());
                cpu_times.push(thread_cpu_time() - cpu_start);
                let frame = frame_span.end().expect("measuring is on");
                frame_allocations.push(frame.get("alloc", "allocations").map_or(0, |a| a.here));
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
            let cpu_total = thread_cpu_time() - cpu_total;
            let drawn = measure.end().expect("measuring is on");
            times.sort();
            cpu_times.sort();
            frame_allocations.sort();
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
            // Exact counts, which a busy machine or a coarse thread clock do not blur: the
            // allocations a frame made on the main thread, the views it drew, and the main
            // thread's CPU time over all the frames, which a clock that ticks in 10 ms (gVisor)
            // still measures.
            let views: Vec<String> = drawn
                .attached
                .iter()
                .filter(|a| a.group == "draw")
                .map(|a| format!("{} {:.2}", a.name, a.all as f64 / frames as f64))
                .collect();
            let requests: u64 = drawn
                .attached
                .iter()
                .filter(|a| is_request(a.group))
                .map(|a| a.all)
                .sum();
            let median_allocations = frame_allocations[frames / 2];
            let p95_allocations = frame_allocations[((frames - 1) as f32 * 0.95) as usize];
            eprintln!(
                "measure counts{kind}: cpu mean {:.2?} | allocs median {median_allocations}, p95 {p95_allocations}, max {} | views drawn per frame [{}] | requests {requests}",
                cpu_total / frames as u32,
                frame_allocations[frames - 1],
                views.join(", "),
            );
            continue;
        }
        if let Some(next) = spec.strip_prefix("report:") {
            close_step(&mut open, &mut cx, handle);
            open = Some(Step::begin(next));
            continue;
        }
        let adhoc = open.is_none().then(|| Step::begin(&spec));
        let began = Instant::now();
        if let Some(path) = spec.strip_prefix("shot:") {
            let image = cx.capture_screenshot(handle).expect("render the window");
            image.save(path).expect("write the PNG");
            continue;
        }
        if let Some(ms) = spec.strip_prefix("wait:") {
            let last = settle(
                &mut cx,
                Duration::from_millis(ms.trim().parse().expect("wait is milliseconds")),
            );
            if let Some(step) = &mut open {
                step.note(began, last);
            }
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
            let last = settle(&mut cx, wait);
            end_action(&mut open, adhoc, began, last, &mut cx, handle);
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
            let last = settle(&mut cx, wait);
            end_action(&mut open, adhoc, began, last, &mut cx, handle);
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
            let last = settle(&mut cx, wait);
            end_action(&mut open, adhoc, began, last, &mut cx, handle);
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
            let last = settle(&mut cx, wait);
            end_action(&mut open, adhoc, began, last, &mut cx, handle);
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
        let last = settle(&mut cx, wait);
        end_action(&mut open, adhoc, began, last, &mut cx, handle);
    }

    close_step(&mut open, &mut cx, handle);
    let image = cx.capture_screenshot(handle).expect("render the window");
    image.save(&out).expect("write the PNG");
    if let Some(path) = studio_perf::write_trace().expect("write the trace") {
        eprintln!("trace: {}", path.display());
    }
    println!("{out}");
}
