use std::io::Read as _;
use std::sync::Mutex as StdMutex;

use gpui::{BorrowAppContext as _, TestAppContext};
use http_client::{FakeHttpClient, Response};

use super::*;

/// The recorder and its switches are process-wide: tests that touch them take turns.
static ONE_AT_A_TIME: StdMutex<()> = StdMutex::new(());

struct Sent {
    bodies: Arc<StdMutex<Vec<Value>>>,
    statuses: Arc<StdMutex<VecDeque<u16>>>,
}

fn http(statuses: &[u16]) -> (Arc<dyn HttpClient>, Sent) {
    let bodies: Arc<StdMutex<Vec<Value>>> = Arc::default();
    let queue: Arc<StdMutex<VecDeque<u16>>> =
        Arc::new(StdMutex::new(statuses.iter().copied().collect()));
    let (seen, answers) = (bodies.clone(), queue.clone());
    let client = FakeHttpClient::create(move |request| {
        let seen = seen.clone();
        let status = answers.lock().unwrap().pop_front().unwrap_or(200);
        async move {
            assert_eq!(
                request.headers().get("Content-Encoding").unwrap(),
                "gzip",
                "a batch is sent gzipped"
            );
            let mut gz = Vec::new();
            request.into_body().read_to_end(&mut gz).await?;
            let mut text = String::new();
            flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut text)?;
            seen.lock().unwrap().push(serde_json::from_str(&text)?);
            Ok(Response::builder()
                .status(status)
                .body(AsyncBody::default())?)
        }
    });
    (
        client,
        Sent {
            bodies,
            statuses: queue,
        },
    )
}

fn context() -> ReportContext {
    ReportContext {
        version: "0.4.4".into(),
        os: "macos",
        os_version: "26".into(),
        arch: "aarch64",
    }
}

fn start(
    cx: &mut TestAppContext,
    blocked: Option<Blocked>,
    http: Arc<dyn HttpClient>,
) -> (tempfile::TempDir, Entity<Telemetry>) {
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        blocked,
        dir: dir.path().join("telemetry"),
        token: Some("phc_test".into()),
        context: context(),
        url: "https://us.i.posthog.com/batch/".into(),
        native_reports: None,
    };
    cx.update(|cx| {
        studio_settings::init_test(cx);
        init_with(options, http, cx);
    });
    (dir, cx.update(|cx| Telemetry::global(cx).unwrap()))
}

fn set(cx: &mut TestAppContext, usage: UsageSharing, crash_reports: CrashReports) {
    cx.update(|cx| {
        cx.update_global::<StudioSettings, _>(|settings, _| {
            settings.studio.telemetry.usage = usage;
            settings.studio.telemetry.crash_reports = crash_reports;
        });
    });
    cx.run_until_parked();
}

fn query() -> Event {
    Event::QueryRan {
        kind: QueryKind::Read,
        outcome: QueryOutcome::Ok,
        duration: DurationBucket::Under100Ms,
        rows: CountBucket::Tens,
    }
}

fn wait_a_minute(cx: &mut TestAppContext) {
    cx.executor().advance_clock(WRITE_EVERY);
    cx.run_until_parked();
}

#[test]
fn the_environment_can_turn_telemetry_off() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    };
    assert_eq!(blocked_by_environment(env(&[])), None);
    assert_eq!(
        blocked_by_environment(env(&[("DO_NOT_TRACK", "1")])),
        Some(Blocked::DoNotTrack)
    );
    assert_eq!(blocked_by_environment(env(&[("DO_NOT_TRACK", "0")])), None);
    assert_eq!(blocked_by_environment(env(&[("DO_NOT_TRACK", "")])), None);
    assert_eq!(
        blocked_by_environment(env(&[("OMNIGRAPH_STUDIO_TELEMETRY", "off")])),
        Some(Blocked::StudioTelemetryOff)
    );
}

/// Until the person chooses, nothing is recorded or sent, and the status bar asks.
#[gpui::test]
fn nothing_is_recorded_until_usage_is_shared(cx: &mut TestAppContext) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (http, sent) = http(&[]);
    let (_dir, telemetry) = start(cx, None, http);
    assert!(cx.update(|cx| telemetry.read(cx).asks_about_usage(cx)));
    record(query());
    wait_a_minute(cx);
    assert!(telemetry.read_with(cx, |t, _| t.store().pending().is_empty()));
    assert!(sent.bodies.lock().unwrap().is_empty());
    assert!(!telemetry.read_with(cx, |t, _| t.store().has_install_id()));
}

/// Shared, an event is written down within a minute and sent as PostHog's batch: anonymous,
/// with no location, under the install ID; once received it moves to the sent log.
#[gpui::test]
fn a_shared_event_is_sent_anonymously_and_logged(cx: &mut TestAppContext) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (http, sent) = http(&[200]);
    let (_dir, telemetry) = start(cx, None, http);
    set(cx, UsageSharing::Share, CrashReports::Ask);
    assert!(!cx.update(|cx| telemetry.read(cx).asks_about_usage(cx)));
    record(query());
    wait_a_minute(cx);
    let bodies = sent.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    assert_eq!(body["api_key"], "phc_test");
    assert!(body["sent_at"].is_string());
    let event = &body["batch"][0];
    assert_eq!(event["event"], "query_ran");
    assert_eq!(event["properties"]["$process_person_profile"], false);
    assert_eq!(event["properties"]["$geoip_disable"], true);
    let install = telemetry.read_with(cx, |t, _| t.store().install_id());
    assert_eq!(event["distinct_id"], install.as_str());
    telemetry.read_with(cx, |t, _| {
        assert!(t.store().pending().is_empty());
        assert_eq!(t.store().sent().len(), 1);
    });
}

/// A 5xx keeps the batch for later; a refusal (a 400) deletes it, since sending it again
/// cannot succeed.
#[gpui::test]
fn a_server_error_is_retried_and_a_refusal_is_not(cx: &mut TestAppContext) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (http, sent) = http(&[503, 400]);
    let (_dir, telemetry) = start(cx, None, http);
    set(cx, UsageSharing::Share, CrashReports::Ask);
    record(query());
    wait_a_minute(cx);
    assert_eq!(sent.bodies.lock().unwrap().len(), 1);
    assert_eq!(telemetry.read_with(cx, |t, _| t.store().pending().len()), 1);
    // Not again within the backoff.
    wait_a_minute(cx);
    assert_eq!(sent.bodies.lock().unwrap().len(), 1);
    cx.executor().advance_clock(SEND_EVERY);
    cx.run_until_parked();
    assert_eq!(sent.bodies.lock().unwrap().len(), 2);
    assert!(sent.statuses.lock().unwrap().is_empty());
    assert!(telemetry.read_with(cx, |t, _| t.store().pending().is_empty()));
    assert!(telemetry.read_with(cx, |t, _| t.store().sent().is_empty()));
}

/// Turning usage off deletes what is pending for it and the install ID at once.
#[gpui::test]
fn turning_usage_off_deletes_its_reports_and_the_id(cx: &mut TestAppContext) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (http, _sent) = http(&[503]);
    let (_dir, telemetry) = start(cx, None, http);
    set(cx, UsageSharing::Share, CrashReports::Ask);
    record(query());
    wait_a_minute(cx);
    assert_eq!(telemetry.read_with(cx, |t, _| t.store().pending().len()), 1);
    set(cx, UsageSharing::DontShare, CrashReports::Ask);
    telemetry.read_with(cx, |t, _| {
        assert!(t.store().pending().is_empty());
        assert!(!t.store().has_install_id());
    });
    record(query());
    wait_a_minute(cx);
    assert!(telemetry.read_with(cx, |t, _| t.store().pending().is_empty()));
}

fn a_crash(store: &Store) {
    let report = CrashReport {
        kind: "panic".into(),
        line: "panic on thread main at crates/x.rs:1: boom".into(),
        thread: "main".into(),
        frames: Vec::new(),
        ran: Some(RunBucket::Under1M),
        version: "0.4.3".into(),
        at: store::now_text(Utc::now()),
    };
    store.keep_crash(&report, "one").unwrap();
}

/// A crash report kept by the last run waits for an answer; sent, it goes as a `$exception`
/// tied to no install, while usage stays unshared.
#[gpui::test]
fn a_crash_report_waits_for_an_answer_then_goes_as_an_exception(cx: &mut TestAppContext) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    a_crash(&Store::new(dir.path().join("telemetry")));
    let (http, sent) = http(&[200]);
    let options = Options {
        blocked: None,
        dir: dir.path().join("telemetry"),
        token: Some("phc_test".into()),
        context: context(),
        url: "https://us.i.posthog.com/batch/".into(),
        native_reports: None,
    };
    cx.update(|cx| {
        studio_settings::init_test(cx);
        init_with(options, http, cx);
    });
    let telemetry = cx.update(|cx| Telemetry::global(cx).unwrap());
    assert_eq!(telemetry.read_with(cx, |t, _| t.waiting_crashes().len()), 1);
    telemetry.update(cx, |t, cx| t.send_crashes(cx));
    cx.run_until_parked();
    let bodies = sent.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1);
    let event = &bodies[0]["batch"][0];
    assert_eq!(event["event"], "$exception");
    assert_eq!(event["properties"]["version"], "0.4.3");
    assert!(!telemetry.read_with(cx, |t, _| t.store().has_install_id()));
    assert!(telemetry.read_with(cx, |t, _| t.store().crashes().is_empty()));
}

/// Never send deletes the crash reports kept, unsent.
#[gpui::test]
fn never_sending_crash_reports_deletes_them(cx: &mut TestAppContext) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (http, sent) = http(&[]);
    let (_dir, telemetry) = start(cx, None, http);
    let store = telemetry.read_with(cx, |t, _| t.store().clone());
    a_crash(&store);
    set(cx, UsageSharing::NotChosen, CrashReports::Never);
    assert!(telemetry.read_with(cx, |t, _| t.store().crashes().is_empty()));
    assert!(sent.bodies.lock().unwrap().is_empty());
}

/// A development build, or one the environment turned off, asks nothing and sends nothing,
/// whatever the settings say.
#[gpui::test]
fn a_blocked_build_sends_nothing(cx: &mut TestAppContext) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (http, sent) = http(&[]);
    let (_dir, telemetry) = start(cx, Some(Blocked::DevelopmentBuild), http);
    assert!(!cx.update(|cx| telemetry.read(cx).asks_about_usage(cx)));
    set(cx, UsageSharing::Share, CrashReports::Always);
    record(query());
    wait_a_minute(cx);
    cx.executor().advance_clock(SEND_EVERY);
    cx.run_until_parked();
    assert!(sent.bodies.lock().unwrap().is_empty());
    assert!(telemetry.read_with(cx, |t, _| t.store().pending().is_empty()));
}
