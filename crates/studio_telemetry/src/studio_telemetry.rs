//! Telemetry, with consent (spec 018): crash reports, usage events and a daily performance
//! summary, each off until the person says otherwise, sent to PostHog US Cloud through Studio's
//! one HTTP client.
//!
//! Recording ([`record`]) is a push onto a bounded queue behind two atomics: no I/O, and nothing
//! at all while usage is off. A background task writes the queue to `telemetry/pending.jsonl`
//! every minute and sends what is pending once an hour. Nothing is sent from a development
//! build, while `DO_NOT_TRACK` or `OMNIGRAPH_STUDIO_TELEMETRY=0` is set, or from a build made
//! without PostHog's project token.

pub mod catalog;
pub mod crash;
pub mod posthog;
mod store;

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Request};
use serde_json::Value;
use studio_settings::{CrashReports, StudioSettings, UsageSharing};

pub use catalog::*;
pub use crash::CrashReport;
use posthog::{Answer, Context as ReportContext};
pub use store::Store;

/// PostHog's project token, compiled into release builds by the release workflow; a build made
/// without it sends nothing. PostHog's project tokens are meant to be public.
const TOKEN: Option<&str> = option_env!("OMNIGRAPH_STUDIO_POSTHOG_TOKEN");

/// Reports held in memory before the oldest are dropped.
pub const MAX_QUEUED: usize = 1_000;
/// Most reports in one batch, and most bytes before gzip.
pub const MAX_BATCH: usize = 200;
pub const MAX_BATCH_BYTES: usize = 256 * 1024;
/// How often the queue is written down, and how often what is pending is sent.
pub const WRITE_EVERY: Duration = Duration::from_secs(60);
pub const SEND_EVERY: Duration = Duration::from_secs(60 * 60);
/// The longest wait before a batch is tried again.
pub const MAX_BACKOFF: Duration = Duration::from_secs(24 * 60 * 60);
/// How long one request may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// A session ID is renewed after this long, as PostHog counts one only for 24 hours.
const SESSION_LIFE: Duration = Duration::from_secs(23 * 60 * 60);

/// Why nothing is sent, whatever the settings say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    DevelopmentBuild,
    DoNotTrack,
    StudioTelemetryOff,
    NoToken,
}

impl Blocked {
    /// What Settings and the log say about it.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::DevelopmentBuild => "Telemetry is off in a development build.",
            Self::DoNotTrack => "Turned off by DO_NOT_TRACK.",
            Self::StudioTelemetryOff => "Turned off by OMNIGRAPH_STUDIO_TELEMETRY.",
            Self::NoToken => {
                "This build was made without a telemetry project, so it sends nothing."
            }
        }
    }
}

/// What the environment says, read through `var`: `OMNIGRAPH_STUDIO_TELEMETRY` at `0`, `off`
/// or `false`, or `DO_NOT_TRACK` set to anything but empty or `0`.
pub fn blocked_by_environment(var: impl Fn(&str) -> Option<String>) -> Option<Blocked> {
    if var("OMNIGRAPH_STUDIO_TELEMETRY")
        .is_some_and(|v| matches!(v.trim().to_lowercase().as_str(), "0" | "off" | "false"))
    {
        return Some(Blocked::StudioTelemetryOff);
    }
    if var("DO_NOT_TRACK").is_some_and(|v| !v.trim().is_empty() && v.trim() != "0") {
        return Some(Blocked::DoNotTrack);
    }
    None
}

// ---- recording, from any thread ----

static USAGE_ON: AtomicBool = AtomicBool::new(false);
const CRASH_OFF: u8 = 0;
const CRASH_ASK: u8 = 1;
const CRASH_ALWAYS: u8 = 2;
static CRASH_MODE: AtomicU8 = AtomicU8::new(CRASH_OFF);

#[derive(Default)]
struct Recorded {
    events: VecDeque<(Event, String, String)>,
    summary: DailySummary,
}

static RECORDED: Mutex<Option<Recorded>> = Mutex::new(None);
/// What the panic hook needs, set once telemetry may keep crash reports.
static CRASH_PLACE: OnceLock<(Store, String, Instant)> = OnceLock::new();

fn with_recorded(f: impl FnOnce(&mut Recorded)) {
    if let Ok(mut recorded) = RECORDED.lock() {
        f(recorded.get_or_insert_with(Recorded::default));
    }
}

/// Records a usage event, while usage is shared; otherwise does nothing. Never blocks on I/O.
pub fn record(event: Event) {
    if !USAGE_ON.load(Ordering::Relaxed) {
        return;
    }
    let at = store::now_text(Utc::now());
    let uuid = uuid::Uuid::now_v7().to_string();
    with_recorded(|recorded| {
        if let Event::QueryRan {
            outcome, duration, ..
        } = event
        {
            recorded
                .summary
                .add_query(matches!(outcome, QueryOutcome::Ok), duration);
        }
        if recorded.events.len() == MAX_QUEUED {
            recorded.events.pop_front();
            recorded.summary.dropped += 1;
        }
        recorded.events.push_back((event, at, uuid));
    });
}

/// Counts a health check's round trip into the day's summary, while usage is shared.
pub fn note_round_trip(duration: Duration) {
    if USAGE_ON.load(Ordering::Relaxed) {
        with_recorded(|recorded| {
            recorded
                .summary
                .add_round_trip(DurationBucket::of(duration))
        });
    }
}

/// Counts Studio's memory use into the day's summary, while usage is shared.
pub fn note_memory(bytes: u64) {
    if USAGE_ON.load(Ordering::Relaxed) {
        with_recorded(|recorded| recorded.summary.add_memory(bytes));
    }
}

/// Keeps a report of a panic, from the panic hook: its line (quotes already removed), the
/// backtrace's function names and how long Studio had run. Does nothing unless crash reports
/// may be kept. Never takes a lock the panicking thread may hold.
pub fn keep_panic(line: &str) {
    if CRASH_MODE.load(Ordering::Relaxed) == CRASH_OFF {
        return;
    }
    let Some((store, version, started)) = CRASH_PLACE.get() else {
        return;
    };
    let home = std::env::home_dir().unwrap_or_default();
    let printed = std::backtrace::Backtrace::force_capture().to_string();
    let thread = std::thread::current();
    let report = CrashReport {
        kind: "panic".into(),
        line: crash::without_home(line, &home),
        thread: thread.name().unwrap_or("unnamed").to_owned(),
        frames: crash::frames_of_backtrace(&crash::without_home(&printed, &home)),
        ran: Some(RunBucket::of(started.elapsed())),
        version: version.clone(),
        at: store::now_text(Utc::now()),
    };
    if let Err(error) = store.keep_crash(&report, &uuid::Uuid::new_v4().to_string()) {
        writeln!(
            std::io::stderr(),
            "Studio could not keep a crash report: {error}"
        )
        .ok();
    }
    if let Ok(mut recorded) = RECORDED.try_lock() {
        recorded
            .get_or_insert_with(Recorded::default)
            .summary
            .panics += 1;
    }
}

// ---- the entity ----

/// How telemetry starts: what blocks it, where it keeps its files, where it sends them.
pub struct Options {
    pub blocked: Option<Blocked>,
    pub dir: PathBuf,
    pub token: Option<String>,
    pub context: ReportContext,
    pub url: String,
    /// Where macOS keeps its crash reports; `None` reads none.
    pub native_reports: Option<PathBuf>,
}

/// Telemetry's state for the status bar, Settings and the log.
pub struct Telemetry {
    blocked: Option<Blocked>,
    store: Store,
    token: Option<String>,
    url: String,
    context: ReportContext,
    http: Arc<dyn HttpClient>,
    session: (String, Instant),
    /// Crash reports waiting for the person's answer (crash reports set to ask).
    waiting: Vec<(PathBuf, CrashReport)>,
    first_start_of_version: bool,
    started: Instant,
    last_sent: Option<Instant>,
    retry_at: Option<Instant>,
    backoff: Duration,
    sending: bool,
    _loop: Option<Task<()>>,
}

struct GlobalTelemetry(Entity<Telemetry>);

impl Global for GlobalTelemetry {}

impl Telemetry {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTelemetry>().map(|g| g.0.clone())
    }

    pub fn blocked(&self) -> Option<Blocked> {
        self.blocked
    }

    /// Whether the person still has to choose about usage data: the status bar asks.
    pub fn asks_about_usage(&self, cx: &App) -> bool {
        self.blocked.is_none() && settings(cx).usage == UsageSharing::NotChosen
    }

    /// The crash reports waiting for an answer.
    pub fn waiting_crashes(&self) -> &[(PathBuf, CrashReport)] {
        &self.waiting
    }

    pub fn first_start_of_version(&self) -> bool {
        self.first_start_of_version
    }

    /// The crash report as it would be sent.
    pub fn crash_as_sent(&self, report: &CrashReport) -> Value {
        posthog::exception_event(report, &self.context, "<a random ID for this report>", "")
    }

    /// The reports held and the ones sent in the last 7 days, as JSON, for the log.
    pub fn log(&self) -> Value {
        serde_json::json!({
            "blocked": self.blocked.map(Blocked::explanation),
            "waiting_for_your_answer": self.waiting.iter().map(|(_, r)| self.crash_as_sent(r)).collect::<Vec<_>>(),
            "pending": self.store.pending(),
            "sent_in_the_last_7_days": self.store.sent(),
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Sends the waiting crash reports.
    pub fn send_crashes(&mut self, cx: &mut Context<Self>) {
        let waiting = std::mem::take(&mut self.waiting);
        let events: Vec<Value> = waiting
            .into_iter()
            .map(|(path, report)| {
                std::fs::remove_file(path).ok();
                self.exception(&report)
            })
            .collect();
        self.store.add_pending(&events);
        self.send_soon(cx);
        cx.notify();
    }

    /// Deletes the waiting crash reports unsent.
    pub fn discard_crashes(&mut self, cx: &mut Context<Self>) {
        for (path, _) in self.waiting.drain(..) {
            std::fs::remove_file(path).ok();
        }
        cx.notify();
    }

    /// A new install ID; the old one is never sent again.
    pub fn reset_id(&mut self, cx: &mut Context<Self>) {
        if self.store.has_install_id() {
            self.store.reset_install_id();
        }
        cx.notify();
    }

    fn exception(&self, report: &CrashReport) -> Value {
        posthog::exception_event(
            report,
            &self.context,
            &uuid::Uuid::new_v4().to_string(),
            &uuid::Uuid::now_v7().to_string(),
        )
    }

    fn session_id(&mut self) -> String {
        if self.session.1.elapsed() > SESSION_LIFE {
            self.session = (uuid::Uuid::now_v7().to_string(), Instant::now());
        }
        self.session.0.clone()
    }

    /// Follows the settings: the switches, and what a switch turned off deletes at once.
    fn follow_settings(&mut self, cx: &mut Context<Self>) {
        let options = settings(cx);
        let usage = self.blocked.is_none() && options.usage == UsageSharing::Share;
        let was = USAGE_ON.swap(usage, Ordering::SeqCst);
        if usage && !was {
            self.store.install_id();
            if self.store.read_marker("summary-sent").is_none() {
                self.store
                    .write_marker("summary-sent", &store::now_text(Utc::now()));
            }
        }
        if !usage {
            with_recorded(|recorded| *recorded = Recorded::default());
            self.store.retain_pending(is_exception);
            self.store.delete_install_id();
            self.store.remove_marker("summary-sent");
        }
        let crash = match (self.blocked, options.crash_reports) {
            (Some(_), _) | (None, CrashReports::Never) => CRASH_OFF,
            (None, CrashReports::Ask) => CRASH_ASK,
            (None, CrashReports::Always) => CRASH_ALWAYS,
        };
        CRASH_MODE.store(crash, Ordering::SeqCst);
        match crash {
            CRASH_OFF if self.blocked.is_none() => {
                self.waiting.clear();
                self.store.delete_crashes();
                self.store.retain_pending(|event| !is_exception(event));
            }
            CRASH_ALWAYS if !self.waiting.is_empty() => self.send_crashes(cx),
            _ => {}
        }
        cx.notify();
    }

    /// Writes down what was recorded: the queue, and the day's summary once a day.
    fn write_down(&mut self) {
        if self.blocked.is_some() {
            return;
        }
        let mut taken = Recorded::default();
        with_recorded(|recorded| {
            taken.events = std::mem::take(&mut recorded.events);
            let due = self
                .store
                .read_marker("summary-sent")
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(&at).ok())
                .is_some_and(|at| Utc::now() - at.with_timezone(&Utc) >= chrono::Duration::days(1));
            if due {
                taken.summary = std::mem::take(&mut recorded.summary);
            }
        });
        if !USAGE_ON.load(Ordering::SeqCst) {
            return;
        }
        let install = self.store.install_id();
        let session = self.session_id();
        let mut events: Vec<Value> = taken
            .events
            .iter()
            .map(|(event, at, uuid)| {
                posthog::usage_event(event, &self.context, &install, &session, uuid, at)
            })
            .collect();
        let now = store::now_text(Utc::now());
        if taken.summary != DailySummary::default() {
            let mut summary = taken.summary;
            summary.ran = Some(RunBucket::of(self.started.elapsed()));
            events.push(posthog::summary_event(
                &summary,
                &self.context,
                &install,
                &session,
                &uuid::Uuid::now_v7().to_string(),
                &now,
            ));
            self.store.write_marker("summary-sent", &now);
        }
        self.store.add_pending(&events);
    }

    fn send_soon(&mut self, cx: &mut Context<Self>) {
        self.last_sent = None;
        self.retry_at = None;
        self.tick(cx);
    }

    /// Writes down and, when it is time, sends one batch.
    fn tick(&mut self, cx: &mut Context<Self>) {
        self.write_down();
        // The executor's clock, which a test can move.
        let now = cx.background_executor().now();
        let due = self
            .last_sent
            .is_none_or(|at| now.duration_since(at) >= SEND_EVERY)
            && self.retry_at.is_none_or(|at| now >= at);
        if due && !self.sending {
            self.send(cx);
        }
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let (None, Some(token)) = (self.blocked, self.token.clone()) else {
            return;
        };
        let pending = self.store.pending();
        if pending.is_empty() {
            return;
        }
        let mut batch = Vec::new();
        let mut bytes = 0;
        for event in pending {
            let size = event.to_string().len();
            if !batch.is_empty() && (batch.len() == MAX_BATCH || bytes + size > MAX_BATCH_BYTES) {
                break;
            }
            bytes += size;
            batch.push(event);
        }
        let body = posthog::batch_body(&token, &batch, &store::now_text(Utc::now()));
        let http = self.http.clone();
        let url = self.url.clone();
        self.sending = true;
        cx.spawn(async move |this, cx| {
            // The request carries its own timeout, which answers as no answer.
            let status = post(http, &url, &body).await;
            this.update(cx, |this, cx| this.sent(batch, status, cx))
                .ok();
        })
        .detach();
    }

    fn sent(&mut self, batch: Vec<Value>, status: Option<u16>, cx: &mut Context<Self>) {
        self.sending = false;
        match posthog::answer_for(status) {
            Answer::Sent => {
                self.store.drop_first_pending(batch.len());
                self.store.log_sent(&batch, Utc::now());
                self.last_sent = Some(cx.background_executor().now());
                self.retry_at = None;
                self.backoff = SEND_EVERY;
            }
            Answer::Retry => {
                self.retry_at = Some(cx.background_executor().now() + self.backoff);
                self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
            }
            Answer::Refused => {
                log::warn!(
                    "telemetry: PostHog refused a batch ({}); it is deleted",
                    status.unwrap_or_default()
                );
                self.store.drop_first_pending(batch.len());
                self.last_sent = Some(cx.background_executor().now());
            }
        }
        cx.notify();
    }
}

fn is_exception(event: &Value) -> bool {
    event.get("event").and_then(Value::as_str) == Some("$exception")
}

fn settings(cx: &App) -> studio_settings::TelemetryOptions {
    cx.try_global::<StudioSettings>()
        .map(|settings| settings.studio.telemetry)
        .unwrap_or_default()
}

/// Sends one batch, gzipped; the answer's status, or `None` when no answer came.
async fn post(http: Arc<dyn HttpClient>, url: &str, body: &Value) -> Option<u16> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(body.to_string().as_bytes()).ok()?;
    let bytes = gz.finish().ok()?;
    let request = Request::post(url)
        .header("Content-Type", "application/json")
        .header("Content-Encoding", "gzip")
        .timeout(REQUEST_TIMEOUT)
        .body(AsyncBody::from(bytes))
        .ok()?;
    let mut response = http.send(request).await.ok()?;
    // Read and drop the answer, so the connection can be used again.
    let mut sink = Vec::new();
    response
        .body_mut()
        .take(64 * 1024)
        .read_to_end(&mut sink)
        .await
        .ok();
    Some(response.status().as_u16())
}

/// Starts telemetry for this Studio: after `studio_settings::init` and `graph_store::init`,
/// whose HTTP client it sends through, and before any window, so a crash report found at start
/// is offered at once.
pub fn init(cx: &mut App) {
    let release = std::env::current_exe()
        .ok()
        .is_some_and(|exe| studio_settings::is_release_build(&exe));
    let blocked = if !release {
        Some(Blocked::DevelopmentBuild)
    } else if let Some(blocked) = blocked_by_environment(|name| std::env::var(name).ok()) {
        Some(blocked)
    } else if TOKEN.is_none_or(str::is_empty) {
        Some(Blocked::NoToken)
    } else {
        None
    };
    let native_reports = cfg!(target_os = "macos")
        .then(|| std::env::home_dir().map(|home| home.join("Library/Logs/DiagnosticReports")))
        .flatten();
    let options = Options {
        blocked,
        dir: studio_settings::data_dir().join("telemetry"),
        token: TOKEN.map(str::to_owned),
        context: ReportContext::this_studio(),
        url: posthog::BATCH_URL.to_owned(),
        native_reports,
    };
    let http = cx.http_client();
    init_with(options, http, cx);
}

pub fn init_with(options: Options, http: Arc<dyn HttpClient>, cx: &mut App) {
    let store = Store::new(options.dir);
    let now = Utc::now();
    let mut first_start_of_version = false;
    let mut native_crashes = 0;
    if options.blocked.is_none() {
        store.expire(now);
        let version = options.context.version.clone();
        first_start_of_version = store.read_marker("last-version").as_deref() != Some(&version);
        store.write_marker("last-version", &version);
        // macOS's reports of crashes since the previous start, which the panic hook cannot see.
        if let (Some(dir), Some(since)) = (&options.native_reports, store.read_marker("last-start"))
            && let Ok(since) = chrono::DateTime::parse_from_rfc3339(&since)
        {
            native_crashes = keep_native_crashes(&store, dir, since.with_timezone(&Utc), &version);
        }
        store.write_marker("last-start", &store::now_text(now));
        CRASH_PLACE
            .set((store.clone(), version, Instant::now()))
            .ok();
    }
    let telemetry = cx.new(|cx| {
        let mut telemetry = Telemetry {
            blocked: options.blocked,
            waiting: if options.blocked.is_none() {
                store.crashes()
            } else {
                Vec::new()
            },
            store,
            token: options.token,
            url: options.url,
            context: options.context,
            http,
            session: (uuid::Uuid::now_v7().to_string(), Instant::now()),
            first_start_of_version,
            started: Instant::now(),
            last_sent: None,
            retry_at: None,
            backoff: SEND_EVERY,
            sending: false,
            _loop: None,
        };
        telemetry.follow_settings(cx);
        if options.blocked.is_none() {
            telemetry._loop = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(WRITE_EVERY).await;
                    if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                        break;
                    }
                }
            }));
        }
        telemetry
    });
    if native_crashes > 0 {
        with_recorded(|recorded| recorded.summary.native_crashes += native_crashes);
    }
    cx.set_global(GlobalTelemetry(telemetry.clone()));
    let observed = telemetry.clone();
    cx.observe_global::<StudioSettings>(move |cx| {
        observed.update(cx, |telemetry, cx| telemetry.follow_settings(cx));
    })
    .detach();
    // What was recorded is written down at quit, so the last minute is not lost.
    let quitting = telemetry.downgrade();
    cx.on_app_quit(move |cx| {
        if let Some(telemetry) = quitting.upgrade() {
            telemetry.update(cx, |telemetry, _| telemetry.write_down());
        }
        async {}
    })
    .detach();
}

/// Keeps macOS's reports of Studio's crashes written after `since`, cut down
/// ([`crash::from_ips`]); returns how many.
fn keep_native_crashes(
    store: &Store,
    dir: &std::path::Path,
    since: chrono::DateTime<Utc>,
    version: &str,
) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut kept = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("omnigraph-studio") || !name.ends_with(".ips") {
            continue;
        }
        let Some(modified) = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(chrono::DateTime::<Utc>::from)
        else {
            continue;
        };
        if modified <= since {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(report) = crash::from_ips(&text, version, store::now_text(modified))
            && store
                .keep_crash(&report, &uuid::Uuid::new_v4().to_string())
                .is_ok()
        {
            kept += 1;
        }
    }
    kept
}

#[cfg(test)]
mod tests;
