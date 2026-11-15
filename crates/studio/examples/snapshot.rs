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
//! both back and forth every 30 frames (`,at=<x>:<y>` puts the pointer there instead, as on the
//! inspector; `,live` lets reads and timers land before each frame, timed and counted with it); `measure:<n>,type` types a letter into the focused
//! field and takes it back, alternately. Each also prints the main thread's CPU time per
//! frame, which a busy machine does not inflate, and its allocations per frame, which are
//! exact on any machine. `record:<dir>[,fps=60][,ms=<n>]` records the rest of the run as a
//! film (`script/demo-video`): from then on the time each step lets pass is drawn and saved
//! every 1/fps s, paced to real time with GPUI's clock moved forward by the same step, so
//! timers and Studio's own animations play out as in the app (motion is not reduced while
//! recording). `ms` is how long each step is given while recording, in place of
//! `SNAPSHOT_WAIT_MS`, and `type:` types a character at a time. A frame the same as the one
//! before is not saved again. `record:stop` ends it; the run's end does too.
//! `<dir>/timeline.json` lists each 1/fps slot's file and each step with the slot it began at,
//! and for a click, hover or drag its point in window coordinates. `mark:<text>` does nothing
//! but appear there, for what draws the film (a zoom, a caption). `reads:<path>`, anywhere in
//! the steps, measures the run's reads (spec 030, `script/measure-frames --reads`): the start,
//! and then each action, `click:` and `head:` step lasts until the session has no read out and has
//! answered none for a second, and prints the step's reads by answer from the session's ledger,
//! with the time to its first answer and to its last; the steps' tallies are written to `path`
//! as JSON. `run:<command>` runs a command and waits for it (another writer's commit);
//! `head:<ms>` waits, at most that long, for the session's branch head to move. `SNAPSHOT_WAIT_MS` (default
//! 1500) is how long to let each step's network work finish; `SNAPSHOT_SIZE` is
//! `WIDTHxHEIGHT` (default 1440x900). `SNAPSHOT_WINDOW_BUTTONS` draws the window's own buttons
//! as another platform's, which this one would leave to the system: `windows`, `linux` (all
//! three at the right) or `linux-left` (close at the left, as some desktops lay them out).
//! Remembered tabs are not restored unless `OMNIGRAPH_STUDIO_NO_RESTORE=0`. The run
//! keeps its state in a fresh temporary directory unless `OMNIGRAPH_STUDIO_DATA` names one, so
//! it neither reads nor writes the app's settings, tabs or conversations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, Focusable as _, HeadlessAppContext, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, ScrollDelta, ScrollEvent, ScrollSource,
    TouchPhase, point, px, size,
};

/// The system allocator, counting what each thread asks of it: a frame's allocations are exact
/// on any machine and in any session, where its CPU time is not. Counted as Zed's
/// `bench_metrics::CountingAllocator` counts (`crates/bench_metrics/src/bench_metrics.rs` at
/// v1.27.0): a reallocation is a new allocation, and the counts are `const` thread-locals with
/// no `Drop`, so counting never allocates and still works while a thread exits.
// Forked from zed-industries/zed crates/bench_metrics/src/bench_metrics.rs @ c84af5b99cbc
struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
}

fn count_allocation(size: usize) {
    // Fails only while the thread exits, when nothing reads its counts.
    ALLOCATIONS.try_with(|n| n.set(n.get() + 1)).ok();
    ALLOCATED_BYTES
        .try_with(|n| n.set(n.get() + size as u64))
        .ok();
}

// SAFETY: every method forwards to `System` with the caller's arguments and returns its result;
// counting never touches the memory.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            count_allocation(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            count_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        let pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !pointer.is_null() {
            count_allocation(new_size);
        }
        pointer
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The calling thread's allocations and allocated bytes so far.
fn thread_allocations() -> (u64, u64) {
    (
        ALLOCATIONS.try_with(Cell::get).unwrap_or(0),
        ALLOCATED_BYTES.try_with(Cell::get).unwrap_or(0),
    )
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

/// A film being recorded by `record:`: the frames saved so far and the steps they show.
struct Recorder {
    dir: std::path::PathBuf,
    frame: Duration,
    /// How long each step is given while recording, in place of `SNAPSHOT_WAIT_MS`.
    step_wait: Option<Duration>,
    started: Instant,
    /// Each 1/fps slot's file name.
    slots: Vec<String>,
    steps: Vec<serde_json::Value>,
    last: Option<image::RgbaImage>,
    saved: usize,
    /// PNGs are encoded on other threads; a full queue makes the recording wait.
    queue: Option<std::sync::mpsc::SyncSender<(std::path::PathBuf, image::RgbaImage)>>,
    encoders: Vec<std::thread::JoinHandle<()>>,
}

impl Recorder {
    fn start(spec: &str) -> Self {
        let mut parts = spec.split(',').map(str::trim);
        let dir = std::path::PathBuf::from(parts.next().expect("record:<dir>"));
        let (mut fps, mut step_wait) = (60., None);
        for part in parts {
            if let Some(n) = part.strip_prefix("fps=") {
                fps = n.parse().expect("fps=<frames a second>");
            } else if let Some(ms) = part.strip_prefix("ms=") {
                step_wait = Some(Duration::from_millis(
                    ms.parse().expect("ms=<milliseconds>"),
                ));
            }
        }
        std::fs::create_dir_all(&dir).expect("create the recording's directory");
        let (queue, frames) =
            std::sync::mpsc::sync_channel::<(std::path::PathBuf, image::RgbaImage)>(8);
        let frames = Arc::new(std::sync::Mutex::new(frames));
        let encoders = (0..std::thread::available_parallelism().map_or(4, |n| n.get().min(8)))
            .map(|_| {
                let frames = frames.clone();
                std::thread::spawn(move || {
                    loop {
                        let next = frames.lock().expect("the frame queue").recv();
                        let Ok((path, image)) = next else { break };
                        save_png(&path, &image);
                    }
                })
            })
            .collect();
        Recorder {
            dir,
            frame: Duration::from_secs_f64(1. / fps),
            step_wait,
            started: Instant::now(),
            slots: Vec::new(),
            steps: Vec::new(),
            last: None,
            saved: 0,
            queue: Some(queue),
            encoders,
        }
    }

    /// Notes that `spec` begins at the next slot, with its point when it has one.
    fn step(&mut self, spec: &str) {
        let numbers = |at: &str| -> Vec<f32> {
            at.split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect()
        };
        let mut step = serde_json::json!({ "step": spec, "frame": self.slots.len() });
        for kind in ["click", "hover", "drag"] {
            if let Some(at) = spec.strip_prefix(kind).and_then(|s| s.strip_prefix(':')) {
                let n = numbers(at);
                step["kind"] = kind.into();
                step["x"] = n[0].into();
                step["y"] = n[1].into();
                if kind == "drag" {
                    step["to"] = serde_json::json!([n[2], n[3]]);
                }
            }
        }
        self.steps.push(step);
    }

    /// Lets `wait` pass a frame at a time, drawing and saving each.
    fn film(&mut self, cx: &mut HeadlessAppContext, handle: AnyWindowHandle, wait: Duration) {
        let frames = (wait.as_secs_f64() / self.frame.as_secs_f64())
            .round()
            .max(1.) as usize;
        for _ in 0..frames {
            // Animations read the real clock, so each frame is drawn when its slot comes.
            let due = self.started + self.frame * self.slots.len() as u32;
            if let Some(early) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(early);
            }
            cx.advance_clock(self.frame);
            cx.run_until_parked();
            let image = cx
                .update_window(handle, |_, window, cx| {
                    // A headless window gets no animation frames, so each is drawn here.
                    window.refresh();
                    window.draw(cx).clear(cx);
                    window.render_to_image()
                })
                .expect("draw a frame")
                .expect("render the window");
            if self.last.as_ref() != Some(&image) {
                self.saved += 1;
                let name = format!("{:05}.png", self.saved);
                self.queue
                    .as_ref()
                    .expect("recording")
                    .send((self.dir.join(&name), image.clone()))
                    .expect("queue a frame");
                self.last = Some(image);
            }
            self.slots.push(format!("{:05}.png", self.saved));
        }
    }

    /// Waits for the last PNGs and writes `timeline.json`.
    fn finish(mut self, width: f32, height: f32) {
        drop(self.queue.take());
        for encoder in self.encoders.drain(..) {
            encoder.join().expect("encode the frames");
        }
        let timeline = serde_json::json!({
            "fps": 1. / self.frame.as_secs_f64(),
            "width": width,
            "height": height,
            "frames": self.slots,
            "steps": self.steps,
        });
        let path = self.dir.join("timeline.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&timeline).expect("timeline JSON"),
        )
        .expect("write timeline.json");
        eprintln!(
            "recorded {} frames ({} saved) in {:.1?}, for {:.1?} of film, to {}",
            timeline["frames"].as_array().map_or(0, Vec::len),
            self.saved,
            self.started.elapsed(),
            self.frame * timeline["frames"].as_array().map_or(0, Vec::len) as u32,
            self.dir.display()
        );
    }
}

/// Saves a PNG with fast compression: a film has thousands, and they are read once.
fn save_png(path: &std::path::Path, image: &image::RgbaImage) {
    use image::ImageEncoder as _;
    let file = std::io::BufWriter::new(std::fs::File::create(path).expect("create a frame"));
    image::codecs::png::PngEncoder::new_with_quality(
        file,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    )
    .expect("write a frame");
}

/// Lets a step's time pass: filmed while recording, settled otherwise.
fn rest(
    cx: &mut HeadlessAppContext,
    handle: AnyWindowHandle,
    recorder: &mut Option<Recorder>,
    wait: Duration,
) {
    match recorder {
        Some(recorder) => recorder.film(cx, handle, wait),
        None => settle(cx, wait),
    }
}

/// How long a recorded `type:` takes over each character.
const TYPING: Duration = Duration::from_millis(70);

/// Dispatches the action `name` with `data` in the window.
fn dispatch(
    cx: &mut HeadlessAppContext,
    handle: AnyWindowHandle,
    name: &str,
    data: Option<serde_json::Value>,
) {
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
            .build_action(name, data)
            .unwrap_or_else(|error| panic!("unknown action {name}: {error:?}"));
        window.dispatch_action(action, cx);
    })
    .expect("dispatch an action");
}

/// The scenario runner's reads (spec 030), on with `reads:<path>`: each step's reads by
/// answer, from the window's session ledger, and the time to its first answer and to settled.
struct Reads {
    path: std::path::PathBuf,
    steps: Vec<serde_json::Value>,
}

/// How long a step must see no read out and none answered to be settled, and how long a step
/// may take at most.
const READS_QUIET: Duration = Duration::from_millis(1000);
const READS_CAP: Duration = Duration::from_secs(120);

/// The window's graph session, once it has one.
fn window_session(
    cx: &mut HeadlessAppContext,
    handle: AnyWindowHandle,
) -> Option<gpui::Entity<graph_store::GraphSession>> {
    cx.update_window(handle, |_, window, cx| {
        studio_workspace::workspace_for(window).and_then(|w| w.read(cx).session().cloned())
    })
    .ok()
    .flatten()
}

impl Reads {
    /// Starts a step: the session's ledger keeps events from now, and forgets what came
    /// before, so its table is the step's alone. Called before the step's action, since an
    /// answer held or seeded is recorded as it is asked.
    fn begin(cx: &mut HeadlessAppContext, handle: AnyWindowHandle) -> Instant {
        if let Some(session) = window_session(cx, handle) {
            cx.update(|cx| {
                session.update(cx, |session, _| {
                    session.ledger_mut().keep_events(true);
                    session.ledger_mut().clear();
                })
            });
        }
        Instant::now()
    }

    /// Lets real time pass from `start`, with GPUI's clock moved by as much, so the ledger's
    /// times are wire times, until the session is `ready` and has had no read out and none
    /// answered for [`READS_QUIET`]; then prints and keeps the step's reads.
    fn step(
        &mut self,
        name: &str,
        start: Instant,
        cx: &mut HeadlessAppContext,
        handle: AnyWindowHandle,
        cap: Duration,
        ready: impl Fn(&graph_store::GraphSession) -> bool,
    ) {
        let mut clock = Instant::now();
        let (mut first, mut last, mut calm_from) = (None, None, Duration::ZERO);
        let timed_out = loop {
            std::thread::sleep(Duration::from_millis(10));
            let now = Instant::now();
            cx.advance_clock(now - clock);
            clock = now;
            cx.run_until_parked();
            let at = start.elapsed();
            let session = window_session(cx, handle);
            let (answered, calm) = session.map_or((0, false), |session| {
                cx.update(|cx| {
                    session.update(cx, |session, _| {
                        // A session opened during the step (the first) keeps from then.
                        let ledger = session.ledger_mut();
                        ledger.keep_events(true);
                        let answered = ledger.take_events().len();
                        (answered, session.reads_in_flight == 0 && ready(session))
                    })
                })
            });
            if answered > 0 {
                first.get_or_insert(at);
                last = Some(at);
            }
            if answered > 0 || !calm {
                calm_from = at;
            }
            if at - calm_from >= READS_QUIET {
                break false;
            }
            if at >= cap {
                break true;
            }
        };
        let ms = |d: Option<Duration>| d.map(|d| d.as_secs_f64() * 1000.);
        let Some(session) = window_session(cx, handle) else {
            eprintln!("reads {name}: no graph session");
            return;
        };
        let (table, mut reads, sent) = cx.update(|cx| {
            let ledger = session.read(cx).ledger();
            let reads: Vec<_> = ledger
                .tallies()
                .map(|((what, asker, answered), tally)| {
                    let mean = tally.took / tally.count.max(1) as u32;
                    serde_json::json!({
                        "what": what.as_ref(),
                        "asker": format!("{asker:?}"),
                        "answered": format!("{answered:?}"),
                        "count": tally.count,
                        "mean_ms": ms(Some(mean)),
                        "slowest_ms": ms(Some(tally.slowest)),
                    })
                })
                .collect();
            (ledger.table(), reads, ledger.sent())
        });
        reads.sort_by_key(|read| {
            (
                std::cmp::Reverse(read["count"].as_u64()),
                read["what"].to_string(),
                read["asker"].to_string(),
                read["answered"].to_string(),
            )
        });
        let asked: u64 = reads.iter().filter_map(|r| r["count"].as_u64()).sum();
        let shown =
            |d: Option<Duration>| d.map_or("-".to_owned(), |d| format!("{} ms", d.as_millis()));
        eprintln!(
            "reads {name}: {asked} asked, {sent} sent | first answer {}, settled {}{}",
            shown(first),
            shown(last),
            if timed_out { " (not settled)" } else { "" }
        );
        eprint!("{table}");
        self.steps.push(serde_json::json!({
            "step": name,
            "first_ms": ms(first),
            "settled_ms": ms(last),
            "timed_out": timed_out,
            "asked": asked,
            "sent": sent,
            "reads": reads,
        }));
    }

    fn write(&self) {
        let latency = std::env::var("STUDIO_READS_LATENCY_MS").ok();
        let json = serde_json::json!({
            "latency_ms": latency.and_then(|ms| ms.parse::<u64>().ok()),
            "steps": self.steps,
        });
        std::fs::write(
            &self.path,
            serde_json::to_string_pretty(&json).expect("reads JSON"),
        )
        .expect("write the reads JSON");
        eprintln!("reads: {}", self.path.display());
    }
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
    let opened = Instant::now();
    let window = cx
        .open_window(size(px(width), px(height)), studio::build_workspace)
        .expect("open the snapshot window");
    let handle: AnyWindowHandle = window.into();
    // The window is the one in front, as Studio's is while someone uses it: views that only work
    // while a window is active (the branch-head poll) run.
    cx.update_window(handle, |_, window, _| window.activate_window())
        .expect("activate the snapshot window");
    if let Ok(platform) = std::env::var("SNAPSHOT_WINDOW_BUTTONS") {
        use gpui::{WindowButton, WindowButtonLayout};
        use studio_ui::window_buttons::{RIGHT_SIDE, WindowButtons};
        let buttons = match platform.as_str() {
            "windows" => WindowButtons::Windows,
            "linux" => WindowButtons::Linux(RIGHT_SIDE),
            "linux-left" => WindowButtons::Linux(WindowButtonLayout {
                left: [Some(WindowButton::Close), None, None],
                right: [
                    Some(WindowButton::Minimize),
                    Some(WindowButton::Maximize),
                    None,
                ],
            }),
            other => panic!("SNAPSHOT_WINDOW_BUTTONS is windows, linux or linux-left: {other}"),
        };
        cx.update_window(handle, |_, window, cx| {
            if let Some(workspace) = studio_workspace::workspace_for(window) {
                workspace.update(cx, |workspace, cx| {
                    workspace.draw_window_buttons_as(buttons, cx)
                });
            }
        })
        .expect("draw the window's buttons");
    }
    // `reads:<path>` measures the whole run, the start (until the schema is read) among it.
    let mut reads = actions
        .iter()
        .find_map(|spec| spec.strip_prefix("reads:"))
        .map(|path| Reads {
            path: path.into(),
            steps: Vec::new(),
        });
    match reads.as_mut() {
        Some(reads) => reads.step("start", opened, &mut cx, handle, READS_CAP, |session| {
            session.schema.loaded().is_some()
        }),
        None => settle(&mut cx, wait),
    }

    let mut recorder: Option<Recorder> = None;
    for spec in actions {
        if spec.starts_with("reads:") {
            continue;
        }
        // `run:<command>` runs a command and waits for it: another writer's commit.
        if let Some(command) = spec.strip_prefix("run:") {
            let status = std::process::Command::new("sh")
                .args(["-c", command])
                .status()
                .expect("run the command");
            assert!(status.success(), "run: the command failed ({status})");
            continue;
        }
        // `head:<ms>` waits for the session's branch head to move, at most that long: another
        // writer's commit, seen by the head poll. Measured as a step while `reads:` is on.
        if let Some(ms) = spec.strip_prefix("head:") {
            let cap = Duration::from_millis(ms.trim().parse().expect("head:<milliseconds>"));
            let Some(session) = window_session(&mut cx, handle) else {
                panic!("head: the window has no graph session");
            };
            let was = cx.update(|cx| {
                let session = session.read(cx);
                session.head(&session.branch).map(str::to_owned)
            });
            let moved = move |session: &graph_store::GraphSession| {
                session.head(&session.branch).map(str::to_owned) != was
            };
            match reads.as_mut() {
                Some(reads) => {
                    let start = Reads::begin(&mut cx, handle);
                    reads.step("head", start, &mut cx, handle, cap, moved);
                }
                None => {
                    let deadline = Instant::now() + cap;
                    while Instant::now() < deadline && !cx.update(|cx| moved(session.read(cx))) {
                        settle(&mut cx, Duration::from_millis(100));
                    }
                }
            }
            continue;
        }
        if let Some(rest) = spec.strip_prefix("record:") {
            if let Some(done) = recorder.take() {
                done.finish(width, height);
            }
            if rest.trim() != "stop" {
                // A film shows Studio's motion, which a still would catch halfway.
                cx.update(|cx| cx.set_reduce_motion(false));
                let mut started = Recorder::start(rest);
                started.film(&mut cx, handle, started.frame);
                recorder = Some(started);
            } else {
                cx.update(|cx| cx.set_reduce_motion(true));
            }
            continue;
        }
        if let Some(recorder) = recorder.as_mut() {
            recorder.step(&spec);
        }
        // A note for the film (a zoom, a caption): the timeline keeps it, the window does not.
        if spec.starts_with("mark:") {
            continue;
        }
        let step_wait = recorder.as_ref().and_then(|r| r.step_wait).unwrap_or(wait);
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
            let mut at: Option<(f32, f32)> = None;
            let mut live = false;
            for part in parts {
                if part == "live" {
                    live = true;
                } else if part == "refresh" {
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
                } else if let Some(point) = part.strip_prefix("at=") {
                    let (x, y) = point.split_once(':').expect("at=<x>:<y>");
                    at = Some((x.parse().expect("at x"), y.parse().expect("at y")));
                }
            }
            // Input goes to the window's centre, or where `at` puts it: each frame first handles
            // one scroll or one move of a held button, as a frame of a real zoom or pan does,
            // then draws.
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
            let mut allocations = Vec::with_capacity(frames);
            let mut allocated = Vec::with_capacity(frames);
            for frame in 0..frames {
                // Back and forth every 30 frames, so the view stays where it can be measured.
                let sign = if (frame / 30) % 2 == 0 { 1. } else { -1. };
                // `live`: a frame's worth of real time passes before each frame, and what landed
                // meanwhile (reads, timers) is handled inside the frame's time and allocations, as
                // the app does between two frames.
                let (mut landed, mut landed_cpu) = (Duration::ZERO, Duration::ZERO);
                let (mut landed_count, mut landed_bytes) = (0, 0);
                if live {
                    std::thread::sleep(Duration::from_millis(8));
                    cx.advance_clock(Duration::from_millis(8));
                    let (start, cpu_start) = (Instant::now(), thread_cpu_time());
                    let (count_start, bytes_start) = thread_allocations();
                    cx.run_until_parked();
                    let (count_end, bytes_end) = thread_allocations();
                    (landed, landed_cpu) = (start.elapsed(), thread_cpu_time() - cpu_start);
                    (landed_count, landed_bytes) =
                        (count_end - count_start, bytes_end - bytes_start);
                }
                cx.update_window(handle, |root, window, cx| {
                    let start = Instant::now() - landed;
                    let cpu_start = thread_cpu_time() - landed_cpu;
                    let (count_start, bytes_start) = thread_allocations();
                    let (count_start, bytes_start) =
                        (count_start - landed_count, bytes_start - landed_bytes);
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
                    let (count_end, bytes_end) = thread_allocations();
                    times.push(start.elapsed());
                    cpu_times.push(thread_cpu_time() - cpu_start);
                    allocations.push(count_end - count_start);
                    allocated.push(bytes_end - bytes_start);
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
            allocations.sort();
            allocated.sort();
            let at = |times: &[Duration], q: f32| times[((times.len() - 1) as f32 * q) as usize];
            let count_at =
                |counts: &[u64], q: f32| counts[((counts.len() - 1) as f32 * q) as usize];
            // A clock that ticks coarsely (10 ms under gVisor) reads most frames as 0 or one
            // tick, so the median says little there; the mean over many frames still does.
            let cpu_mean = cpu_times.iter().sum::<Duration>() / frames.max(1) as u32;
            let kind = match (typing, scroll, drag, refresh) {
                (true, _, _, _) => " (type)",
                (_, Some(_), _, _) => " (scroll)",
                (_, _, Some(_), _) => " (drag)",
                (_, _, _, true) => " (refresh)",
                _ => "",
            };
            // Wall time is what a frame took; the main thread's CPU time is the work in it,
            // which other processes on a busy machine do not inflate; its allocations are
            // exact, the same on any machine for the same frame.
            eprintln!(
                "measure{kind}: {frames} frames, median {:.2?}, p90 {:.2?}, p95 {:.2?}, p99 {:.2?}, max {:.2?} | cpu median {:.2?}, p95 {:.2?}, p99 {:.2?}, mean {:.2?} | allocs median {}, p95 {}, max {}, bytes median {}",
                at(&times, 0.5),
                at(&times, 0.9),
                at(&times, 0.95),
                at(&times, 0.99),
                at(&times, 1.),
                at(&cpu_times, 0.5),
                at(&cpu_times, 0.95),
                at(&cpu_times, 0.99),
                cpu_mean,
                count_at(&allocations, 0.5),
                count_at(&allocations, 0.95),
                count_at(&allocations, 1.),
                count_at(&allocated, 0.5),
            );
            continue;
        }
        if let Some(path) = spec.strip_prefix("shot:") {
            let image = cx.capture_screenshot(handle).expect("render the window");
            image.save(path).expect("write the PNG");
            continue;
        }
        if let Some(ms) = spec.strip_prefix("wait:") {
            rest(
                &mut cx,
                handle,
                &mut recorder,
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
            rest(&mut cx, handle, &mut recorder, step_wait);
            continue;
        }
        if let Some(keys) = spec.strip_prefix("key:") {
            for key in keys.split(' ') {
                cx.update_window(handle, |_, window, cx| {
                    let keystroke = gpui::Keystroke::parse_with_os(key, cx.platform_info().os)
                        .expect("a keystroke");
                    window.dispatch_keystroke(keystroke, cx);
                })
                .expect("press a key");
                cx.run_until_parked();
            }
            rest(&mut cx, handle, &mut recorder, step_wait);
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
            rest(&mut cx, handle, &mut recorder, step_wait);
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
            match reads.as_mut() {
                Some(reads) if recorder.is_none() => {
                    let start = Reads::begin(&mut cx, handle);
                    click(&mut cx, handle, point(px(x), px(y)), modifiers, button);
                    reads.step("click", start, &mut cx, handle, READS_CAP, |_| true);
                }
                _ => {
                    click(&mut cx, handle, point(px(x), px(y)), modifiers, button);
                    rest(&mut cx, handle, &mut recorder, step_wait);
                }
            }
            continue;
        }
        // While recording, text is typed a character at a time, as a person types it.
        if let (Some(text), Some(film)) = (spec.strip_prefix("type:"), recorder.as_mut()) {
            for character in text.replace("\\n", "\n").chars() {
                let data = serde_json::Value::String(character.to_string());
                dispatch(&mut cx, handle, "editor::HandleInput", Some(data));
                film.film(&mut cx, handle, TYPING);
            }
            rest(&mut cx, handle, &mut recorder, step_wait);
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
        match reads.as_mut() {
            // Measured: the step lasts until its reads settle.
            Some(reads) if recorder.is_none() => {
                let start = Reads::begin(&mut cx, handle);
                dispatch(&mut cx, handle, &name, data);
                reads.step(&name, start, &mut cx, handle, READS_CAP, |_| true);
            }
            _ => {
                dispatch(&mut cx, handle, &name, data);
                rest(&mut cx, handle, &mut recorder, step_wait);
            }
        }
    }
    if let Some(done) = recorder.take() {
        done.finish(width, height);
    }
    if let Some(reads) = &reads {
        reads.write();
    }

    let image = cx.capture_screenshot(handle).expect("render the window");
    image.save(&out).expect("write the PNG");
    println!("{out}");
}
