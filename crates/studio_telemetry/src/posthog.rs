//! PostHog's capture API, as spec 018 uses it: US Cloud's batch endpoint, anonymous events with
//! no location, `$exception` events for crash reports, and which answers are worth retrying.

use serde_json::{Map, Value, json};

use crate::catalog::{DailySummary, Event};
use crate::crash::CrashReport;

/// PostHog US Cloud's batch endpoint (spec 018, decided 2026-11-11).
pub const BATCH_URL: &str = "https://us.i.posthog.com/batch/";

/// What every report says about the Studio that sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub version: String,
    pub os: &'static str,
    pub os_version: String,
    pub arch: &'static str,
}

impl Context {
    pub fn this_studio() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            os: match std::env::consts::OS {
                "macos" => "macos",
                "windows" => "windows",
                _ => "linux",
            },
            os_version: os_major_version().unwrap_or_else(|| "unknown".into()),
            arch: match std::env::consts::ARCH {
                "aarch64" => "aarch64",
                _ => "x86_64",
            },
        }
    }

    fn put(&self, properties: &mut Map<String, Value>) {
        properties.insert("version".into(), self.version.clone().into());
        properties.insert("os".into(), self.os.into());
        properties.insert("os_version".into(), self.os_version.clone().into());
        properties.insert("arch".into(), self.arch.into());
        // An anonymous event: no person profile is made or updated, and no location is added.
        properties.insert("$process_person_profile".into(), false.into());
        properties.insert("$geoip_disable".into(), true.into());
    }
}

/// The system's major version: macOS's from its version file; elsewhere none, so `unknown`.
fn os_major_version() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    let after = plist.split("<key>ProductVersion</key>").nth(1)?;
    let version = after.split("<string>").nth(1)?.split("</string>").next()?;
    version.split('.').next().map(str::to_owned)
}

/// A usage event as PostHog takes it: its tag as `event`, the install ID as `distinct_id`, its
/// fields and the context as `properties`, and a UUID so a batch sent twice counts once.
pub fn usage_event(
    event: &Event,
    context: &Context,
    install_id: &str,
    session_id: &str,
    uuid: &str,
    timestamp: &str,
) -> Value {
    let mut properties = match serde_json::to_value(event) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    properties.remove("event");
    context.put(&mut properties);
    properties.insert("$session_id".into(), session_id.into());
    json!({
        "event": event.name(),
        "distinct_id": install_id,
        "uuid": uuid,
        "timestamp": timestamp,
        "properties": properties,
    })
}

/// The day's summary as one `daily_summary` event.
pub fn summary_event(
    summary: &DailySummary,
    context: &Context,
    install_id: &str,
    session_id: &str,
    uuid: &str,
    timestamp: &str,
) -> Value {
    let mut properties = match serde_json::to_value(summary) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    context.put(&mut properties);
    properties.insert("$session_id".into(), session_id.into());
    json!({
        "event": "daily_summary",
        "distinct_id": install_id,
        "uuid": uuid,
        "timestamp": timestamp,
        "properties": properties,
    })
}

/// A crash report as a `$exception` event, which PostHog's error tracking reads. Its
/// `distinct_id` is a new random ID for this report alone, so it is tied to no install.
pub fn exception_event(
    report: &CrashReport,
    context: &Context,
    distinct_id: &str,
    uuid: &str,
) -> Value {
    let frames: Vec<Value> = report
        .frames
        .iter()
        .map(|frame| {
            json!({
                "platform": "custom",
                "lang": "rust",
                "function": frame.function,
                "in_app": frame.in_app,
            })
        })
        .collect();
    let mut properties = Map::new();
    context.put(&mut properties);
    // The version it crashed in, which may be older than the one sending it.
    properties.insert("version".into(), report.version.clone().into());
    properties.insert(
        "$exception_list".into(),
        json!([{
            "type": report.kind,
            "value": report.line,
            "mechanism": { "handled": false, "synthetic": false },
            "stacktrace": { "type": "raw", "frames": frames },
        }]),
    );
    properties.insert("$exception_level".into(), "fatal".into());
    properties.insert("thread".into(), report.thread.clone().into());
    if let Some(ran) = report.ran {
        properties.insert("ran".into(), serde_json::to_value(ran).unwrap_or_default());
    }
    json!({
        "event": "$exception",
        "distinct_id": distinct_id,
        "uuid": uuid,
        "timestamp": report.at,
        "properties": properties,
    })
}

/// The body of one batch request.
pub fn batch_body(token: &str, events: &[Value], sent_at: &str) -> Value {
    json!({ "api_key": token, "batch": events, "sent_at": sent_at })
}

/// What to do with a batch after PostHog answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Received: delete it (a 2xx says received, not that every event was valid).
    Sent,
    /// Worth sending again later: a timeout, no connection, a 429 or a 5xx.
    Retry,
    /// Refused for good (a body or token PostHog will not take, a billing quota): delete it.
    Refused,
}

pub fn answer_for(status: Option<u16>) -> Answer {
    match status {
        Some(200..=299) => Answer::Sent,
        None | Some(429) | Some(500..=599) => Answer::Retry,
        Some(_) => Answer::Refused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CountBucket, DurationBucket, QueryKind, QueryOutcome};
    use crate::crash::Frame;

    fn context() -> Context {
        Context {
            version: "0.4.4".into(),
            os: "macos",
            os_version: "26".into(),
            arch: "aarch64",
        }
    }

    #[test]
    fn a_usage_event_is_anonymous_and_has_no_location() {
        let event = Event::QueryRan {
            kind: QueryKind::Read,
            outcome: QueryOutcome::Ok,
            duration: DurationBucket::Under1S,
            rows: CountBucket::One,
        };
        let json = usage_event(
            &event,
            &context(),
            "install",
            "session",
            "u",
            "2026-11-11T00:00:00Z",
        );
        assert_eq!(json["event"], "query_ran");
        assert_eq!(json["distinct_id"], "install");
        let p = &json["properties"];
        assert_eq!(p["$process_person_profile"], false);
        assert_eq!(p["$geoip_disable"], true);
        assert_eq!(p["$session_id"], "session");
        assert_eq!(p["duration"], "under_1s");
        assert_eq!(p["version"], "0.4.4");
        assert!(p.get("event").is_none());
    }

    #[test]
    fn a_crash_report_is_an_exception_with_raw_frames() {
        let report = CrashReport {
            kind: "panic".into(),
            line: "panic on thread main at crates/x.rs:1: `…`".into(),
            thread: "main".into(),
            frames: vec![Frame {
                function: "type_table::render".into(),
                in_app: true,
            }],
            ran: None,
            version: "0.4.3".into(),
            at: "2026-11-11T00:00:00Z".into(),
        };
        let json = exception_event(&report, &context(), "one-off", "u");
        assert_eq!(json["event"], "$exception");
        assert_eq!(json["distinct_id"], "one-off");
        let p = &json["properties"];
        assert_eq!(p["version"], "0.4.3");
        assert_eq!(p["$exception_level"], "fatal");
        let exception = &p["$exception_list"][0];
        assert_eq!(exception["type"], "panic");
        assert_eq!(exception["mechanism"]["handled"], false);
        assert_eq!(exception["stacktrace"]["frames"][0]["platform"], "custom");
        assert_eq!(exception["stacktrace"]["frames"][0]["lang"], "rust");
    }

    #[test]
    fn only_a_timeout_a_429_or_a_5xx_is_retried() {
        assert_eq!(answer_for(Some(200)), Answer::Sent);
        assert_eq!(answer_for(None), Answer::Retry);
        assert_eq!(answer_for(Some(429)), Answer::Retry);
        assert_eq!(answer_for(Some(503)), Answer::Retry);
        assert_eq!(answer_for(Some(400)), Answer::Refused);
        assert_eq!(answer_for(Some(401)), Answer::Refused);
        assert_eq!(answer_for(Some(402)), Answer::Refused);
    }
}
