//! Every report Studio may send (spec 018, "What a report may hold"): a closed set of typed
//! events whose fields are enums, `bool`s, buckets and counts, so no field can hold a name, a
//! query, a value or an address. `docs/telemetry.md` is generated from this file
//! ([`catalog_markdown`]), and a test refuses any free-text field.

use schemars::JsonSchema;
use serde::Serialize;

/// How long something took, said as a range rather than a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub enum DurationBucket {
    #[serde(rename = "under_10ms")]
    Under10Ms,
    #[serde(rename = "under_30ms")]
    Under30Ms,
    #[serde(rename = "under_100ms")]
    Under100Ms,
    #[serde(rename = "under_300ms")]
    Under300Ms,
    #[serde(rename = "under_1s")]
    Under1S,
    #[serde(rename = "under_3s")]
    Under3S,
    #[serde(rename = "under_10s")]
    Under10S,
    #[serde(rename = "under_30s")]
    Under30S,
    #[serde(rename = "30s_or_more")]
    Over30S,
}

impl DurationBucket {
    pub const ALL: [Self; 9] = [
        Self::Under10Ms,
        Self::Under30Ms,
        Self::Under100Ms,
        Self::Under300Ms,
        Self::Under1S,
        Self::Under3S,
        Self::Under10S,
        Self::Under30S,
        Self::Over30S,
    ];
    const BOUNDS_MS: [u128; 8] = [10, 30, 100, 300, 1_000, 3_000, 10_000, 30_000];

    pub fn of(duration: std::time::Duration) -> Self {
        let ms = duration.as_millis();
        let at = Self::BOUNDS_MS
            .iter()
            .position(|bound| ms < *bound)
            .unwrap_or(Self::BOUNDS_MS.len());
        Self::ALL[at]
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|b| *b == self).unwrap_or(0)
    }
}

/// How many (rows, tool calls, windows), said as a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub enum CountBucket {
    #[serde(rename = "0")]
    None,
    #[serde(rename = "1")]
    One,
    #[serde(rename = "2_to_10")]
    Few,
    #[serde(rename = "11_to_100")]
    Tens,
    #[serde(rename = "101_to_1000")]
    Hundreds,
    #[serde(rename = "1001_to_10000")]
    Thousands,
    #[serde(rename = "more_than_10000")]
    More,
}

impl CountBucket {
    pub fn of(count: u64) -> Self {
        match count {
            0 => Self::None,
            1 => Self::One,
            2..=10 => Self::Few,
            11..=100 => Self::Tens,
            101..=1_000 => Self::Hundreds,
            1_001..=10_000 => Self::Thousands,
            _ => Self::More,
        }
    }
}

/// How much memory Studio used, said as a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
pub enum MemoryBucket {
    #[serde(rename = "under_256mb")]
    Under256Mb,
    #[serde(rename = "under_512mb")]
    Under512Mb,
    #[serde(rename = "under_1gb")]
    Under1Gb,
    #[serde(rename = "under_2gb")]
    Under2Gb,
    #[serde(rename = "under_4gb")]
    Under4Gb,
    #[serde(rename = "4gb_or_more")]
    Over4Gb,
}

impl MemoryBucket {
    pub fn of(bytes: u64) -> Self {
        const MB: u64 = 1024 * 1024;
        match bytes / MB {
            0..256 => Self::Under256Mb,
            256..512 => Self::Under512Mb,
            512..1024 => Self::Under1Gb,
            1024..2048 => Self::Under2Gb,
            2048..4096 => Self::Under4Gb,
            _ => Self::Over4Gb,
        }
    }
}

/// How long a run of Studio lasted, said as a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, JsonSchema)]
pub enum RunBucket {
    #[serde(rename = "under_1m")]
    Under1M,
    #[serde(rename = "under_10m")]
    Under10M,
    #[serde(rename = "under_1h")]
    Under1H,
    #[serde(rename = "under_4h")]
    Under4H,
    #[serde(rename = "under_12h")]
    Under12H,
    #[serde(rename = "12h_or_more")]
    Over12H,
}

impl RunBucket {
    pub fn of(duration: std::time::Duration) -> Self {
        match duration.as_secs() / 60 {
            0 => Self::Under1M,
            1..10 => Self::Under10M,
            10..60 => Self::Under1H,
            60..240 => Self::Under4H,
            240..720 => Self::Under12H,
            _ => Self::Over12H,
        }
    }
}

/// Where a graph's server is: on this machine, elsewhere, or a managed cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServerClass {
    Local,
    Remote,
    Managed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryKind {
    Read,
    Mutation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryOutcome {
    Ok,
    Refused,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriteKind {
    /// A mutation: a cell, a bulk edit, drafts, an insert or a delete.
    Mutation,
    /// A stored query that writes.
    StoredQuery,
    Blob,
    /// A branch created or deleted.
    Branch,
    Merge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriteOutcome {
    Published,
    /// Landed as a proposal or a snapshot, which moved no branch.
    Held,
    Refused,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Ready,
    Unauthorized,
    Unreachable,
    ContractMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Done,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStep {
    Found,
    Downloaded,
    Installed,
    Failed,
}

/// A usage event (spec 018, "Usage events"). Its tag is the PostHog event's name; its fields
/// are the event's properties. Adding one is a code change, and `docs/telemetry.md` says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Studio started.
    AppStarted {
        /// Whether this is the first start of this version.
        first_start_of_version: bool,
        /// How many windows it restored.
        windows_restored: CountBucket,
    },
    /// A graph was opened in a window.
    GraphOpened {
        server: ServerClass,
        /// Whether the server speaks the HTTP contract Studio was built for.
        contract_matches: bool,
    },
    /// A query from the query editor finished.
    QueryRan {
        kind: QueryKind,
        outcome: QueryOutcome,
        duration: DurationBucket,
        rows: CountBucket,
    },
    /// A write finished.
    WriteFinished {
        kind: WriteKind,
        outcome: WriteOutcome,
    },
    /// A server's connection changed state.
    ConnectionChanged {
        state: ConnectionState,
        round_trip: Option<DurationBucket>,
    },
    /// An agent's turn ended.
    AgentTurnFinished {
        outcome: TurnOutcome,
        duration: DurationBucket,
        tool_calls: CountBucket,
    },
    /// The updater moved a step.
    Update { step: UpdateStep },
}

impl Event {
    /// The PostHog event name: the tag.
    pub fn name(&self) -> &'static str {
        match self {
            Self::AppStarted { .. } => "app_started",
            Self::GraphOpened { .. } => "graph_opened",
            Self::QueryRan { .. } => "query_ran",
            Self::WriteFinished { .. } => "write_finished",
            Self::ConnectionChanged { .. } => "connection_changed",
            Self::AgentTurnFinished { .. } => "agent_turn_finished",
            Self::Update { .. } => "update",
        }
    }
}

/// The day's performance summary (spec 018, "Performance summaries"): histograms as counts per
/// [`DurationBucket`], in its order, and what went wrong how often.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct DailySummary {
    /// Query durations that succeeded, one count per duration bucket in order.
    pub queries_ok: [u32; 9],
    /// Query durations that failed or were refused, per duration bucket.
    pub queries_failed: [u32; 9],
    /// Health-check round trips, per duration bucket.
    pub round_trips: [u32; 9],
    /// The most memory Studio used while its window was in front.
    pub peak_memory: Option<MemoryBucket>,
    /// Panics Studio caught.
    pub panics: u32,
    /// Crashes macOS reported for the previous run.
    pub native_crashes: u32,
    /// Reports dropped because the queue was full.
    pub dropped: u32,
    /// How long Studio has run since its start.
    pub ran: Option<RunBucket>,
}

impl DailySummary {
    pub fn add_query(&mut self, ok: bool, duration: DurationBucket) {
        let histogram = if ok {
            &mut self.queries_ok
        } else {
            &mut self.queries_failed
        };
        histogram[duration.index()] += 1;
    }

    pub fn add_round_trip(&mut self, duration: DurationBucket) {
        self.round_trips[duration.index()] += 1;
    }

    pub fn add_memory(&mut self, bytes: u64) {
        let bucket = MemoryBucket::of(bytes);
        self.peak_memory = Some(self.peak_memory.map_or(bucket, |peak| peak.max(bucket)));
    }

    /// Whether there is anything worth sending.
    pub fn is_empty(&self) -> bool {
        let Self {
            queries_ok,
            queries_failed,
            round_trips,
            peak_memory,
            panics,
            native_crashes,
            dropped,
            ran: _,
        } = self;
        queries_ok
            .iter()
            .chain(queries_failed)
            .chain(round_trips)
            .all(|n| *n == 0)
            && peak_memory.is_none()
            && *panics == 0
            && *native_crashes == 0
            && *dropped == 0
    }
}

/// `docs/telemetry.md`: every report, field, value and bucket, made from the catalog's schema.
pub fn catalog_markdown() -> String {
    let events = serde_json::to_value(schemars::schema_for!(Event)).expect("schema is JSON");
    let summary =
        serde_json::to_value(schemars::schema_for!(DailySummary)).expect("schema is JSON");
    let mut out = String::from(
        "<!-- Generated from crates/studio_telemetry/src/catalog.rs by its test \
         `docs_telemetry_md_matches_the_catalog`. Do not edit by hand. -->\n\n\
         # What Omnigraph Studio sends\n\n\
         Studio asks before it sends anonymous usage data (spec 018), and sends none until you \
         answer: which features are used and how fast they are, tied to one run of Studio and \
         nothing else. You can choose \
         to be counted across runs with a random install ID. Crash reports are shown to you and \
         sent only when you say so. Nothing is sent from a development build, or while \
         `DO_NOT_TRACK` or `OMNIGRAPH_STUDIO_TELEMETRY=0` is set. Change either choice under \
         Settings, Privacy, or from the status bar's notice.\n\n\
         No report holds a graph, type, property, branch, query or file name, a server address, \
         an account or token, query text, a value, a row, an error message or your IP address. \
         Reports go to PostHog US Cloud, which is told to discard the IP address a request comes \
         from and to add no location.\n\n\
         Who receives this data, how long it is kept and your rights over it are in the \
         [privacy notice](privacy.md).\n\n\
         ## Sent with every report\n\n\
         | Property | Value |\n|---|---|\n\
         | `version` | Studio's version |\n\
         | `os` | `macos`, `linux` or `windows` |\n\
         | `os_version` | the system's major version, or `unknown` |\n\
         | `arch` | `aarch64` or `x86_64` |\n\n\
         Usage events go under a random ID made at each start and never written down, or, if \
         you chose to be counted across runs, under a random install ID, deleted when you turn \
         that off and replaced by Reset ID. They also carry a random session ID. Crash reports \
         carry neither: each has a random ID of its own.\n\n\
         ## Usage events\n\n",
    );
    let defs = events.get("$defs").cloned().unwrap_or_default();
    for variant in events["oneOf"].as_array().into_iter().flatten() {
        let props = &variant["properties"];
        let name = props["event"]["const"].as_str().unwrap_or_default();
        let about = variant["description"].as_str().unwrap_or_default();
        out.push_str(&format!("### `{name}`\n\n{about}\n\n"));
        let mut rows = Vec::new();
        for (key, schema) in props.as_object().into_iter().flatten() {
            if key == "event" {
                continue;
            }
            rows.push(format!("| `{key}` | {} |", values_of(schema, &defs)));
        }
        if rows.is_empty() {
            out.push_str("No properties.\n\n");
        } else {
            out.push_str("| Property | Values |\n|---|---|\n");
            out.push_str(&rows.join("\n"));
            out.push_str("\n\n");
        }
    }
    out.push_str(
        "## `daily_summary`\n\n\
         Once a day while usage is on: how often things took how long, and what went wrong how \
         often. Each histogram is a list of counts, one per duration bucket in this order: \
         under 10 ms, 30 ms, 100 ms, 300 ms, 1 s, 3 s, 10 s, 30 s, and 30 s or more.\n\n\
         | Property | Values |\n|---|---|\n",
    );
    let summary_defs = summary.get("$defs").cloned().unwrap_or_default();
    for (key, schema) in summary["properties"].as_object().into_iter().flatten() {
        let about = schema["description"].as_str().unwrap_or_default();
        out.push_str(&format!(
            "| `{key}` | {about} ({}) |\n",
            values_of(schema, &summary_defs)
        ));
    }
    out.push_str(
        "\n## Crash reports\n\n\
         A `$exception` event, the shape PostHog's error tracking reads: the kind (`panic` or the \
         signal's name), the panic's one line with every quoted part replaced by `…` and paths \
         under your home folder shortened to `~`, the names of the functions on the crashed \
         thread (at most 64), the thread's name and how long Studio had run, as a range. A crash \
         report is shown to you before it is sent, unless you chose to always send them.\n",
    );
    out
}

/// What a property may hold, for the table.
fn values_of(schema: &serde_json::Value, defs: &serde_json::Value) -> String {
    let resolved = |schema: &serde_json::Value| -> serde_json::Value {
        match schema.get("$ref").and_then(|r| r.as_str()) {
            Some(reference) => {
                let name = reference.rsplit('/').next().unwrap_or_default();
                defs.get(name).cloned().unwrap_or_default()
            }
            None => schema.clone(),
        }
    };
    let schema = resolved(schema);
    if let Some(alternatives) = schema.get("anyOf").and_then(|a| a.as_array()) {
        let parts: Vec<String> = alternatives
            .iter()
            .map(|one| {
                if one.get("type").and_then(|t| t.as_str()) == Some("null") {
                    "none".to_owned()
                } else {
                    values_of(one, defs)
                }
            })
            .collect();
        return parts.join(", or ");
    }
    if let Some(values) = schema.get("enum").and_then(|e| e.as_array()) {
        return values
            .iter()
            .filter_map(|v| v.as_str())
            .map(|v| format!("`{v}`"))
            .collect::<Vec<_>>()
            .join(", ");
    }
    if let Some(values) = schema.get("oneOf").and_then(|e| e.as_array()) {
        return values
            .iter()
            .filter_map(|v| v.get("const").and_then(|c| c.as_str()))
            .map(|v| format!("`{v}`"))
            .collect::<Vec<_>>()
            .join(", ");
    }
    match schema.get("type").and_then(|t| t.as_str()) {
        Some("boolean") => "`true` or `false`".into(),
        Some("integer") => "a count".into(),
        Some("array") => "a list of counts".into(),
        other => other.unwrap_or("?").to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Every string a report's schema allows is one of a fixed set: no field can hold free
    /// text, so none can carry a name, a query, a value or a message (spec 018).
    #[test]
    fn no_report_field_holds_free_text() {
        fn walk(schema: &serde_json::Value, at: &str, bad: &mut Vec<String>) {
            match schema {
                serde_json::Value::Object(map) => {
                    let is_string = match map.get("type") {
                        Some(serde_json::Value::String(t)) => t == "string",
                        Some(serde_json::Value::Array(ts)) => ts.iter().any(|t| t == "string"),
                        _ => false,
                    };
                    if is_string && !map.contains_key("enum") && !map.contains_key("const") {
                        bad.push(at.to_owned());
                    }
                    for (key, value) in map {
                        walk(value, &format!("{at}/{key}"), bad);
                    }
                }
                serde_json::Value::Array(items) => {
                    for (i, value) in items.iter().enumerate() {
                        walk(value, &format!("{at}/{i}"), bad);
                    }
                }
                _ => {}
            }
        }
        let mut bad = Vec::new();
        walk(
            &serde_json::to_value(schemars::schema_for!(Event)).unwrap(),
            "Event",
            &mut bad,
        );
        walk(
            &serde_json::to_value(schemars::schema_for!(DailySummary)).unwrap(),
            "DailySummary",
            &mut bad,
        );
        assert!(bad.is_empty(), "free-text fields: {bad:?}");
    }

    #[test]
    fn numbers_are_sent_as_ranges() {
        assert_eq!(
            DurationBucket::of(Duration::from_millis(9)),
            DurationBucket::Under10Ms
        );
        assert_eq!(
            DurationBucket::of(Duration::from_millis(10)),
            DurationBucket::Under30Ms
        );
        assert_eq!(
            DurationBucket::of(Duration::from_secs(29)),
            DurationBucket::Under30S
        );
        assert_eq!(
            DurationBucket::of(Duration::from_secs(31)),
            DurationBucket::Over30S
        );
        assert_eq!(CountBucket::of(0), CountBucket::None);
        assert_eq!(CountBucket::of(10), CountBucket::Few);
        assert_eq!(CountBucket::of(10_001), CountBucket::More);
        assert_eq!(
            MemoryBucket::of(300 * 1024 * 1024),
            MemoryBucket::Under512Mb
        );
        assert_eq!(
            RunBucket::of(Duration::from_secs(90 * 60)),
            RunBucket::Under4H
        );
    }

    #[test]
    fn an_event_serializes_as_its_name_and_enum_values() {
        let event = Event::QueryRan {
            kind: QueryKind::Read,
            outcome: QueryOutcome::Ok,
            duration: DurationBucket::Under100Ms,
            rows: CountBucket::Tens,
        };
        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["event"], event.name());
        assert_eq!(json["duration"], "under_100ms");
        assert_eq!(json["rows"], "11_to_100");
    }

    /// `docs/telemetry.md` says what the catalog holds; run with `UPDATE_TELEMETRY_DOC=1` to
    /// write it again after changing the catalog.
    #[test]
    fn docs_telemetry_md_matches_the_catalog() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/telemetry.md");
        let made = catalog_markdown();
        if std::env::var_os("UPDATE_TELEMETRY_DOC").is_some() {
            std::fs::write(path, &made).unwrap();
        }
        let checked_in = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            checked_in, made,
            "docs/telemetry.md is out of date: run the test with UPDATE_TELEMETRY_DOC=1"
        );
    }
}
