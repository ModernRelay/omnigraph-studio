//! The sink: the spans that ended while collecting, and what reads them: a table, a Chrome trace.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::Unit;

static COLLECTING: AtomicBool = AtomicBool::new(false);

/// Whether ended spans are kept ([`collect`]).
#[inline]
pub fn collecting() -> bool {
    COLLECTING.load(Ordering::Relaxed)
}

/// Keeps ended spans from now on ([`take`] reads them), turning measuring on too; or stops.
pub fn collect(on: bool) {
    if on {
        crate::set_on(true);
        Collector::global();
    }
    COLLECTING.store(on, Ordering::Relaxed);
}

/// Spans kept at most; past it a span is counted as dropped, as a trace says.
const MAX_RECORDS: usize = 1 << 20;

/// A span as it ended.
#[derive(Clone, Debug)]
pub struct Record {
    /// What kind of span: `step`, `frame`, `fetch`, `decode`.
    pub cat: &'static str,
    pub name: Cow<'static, str>,
    /// From the collector's start.
    pub start: Duration,
    pub duration: Duration,
    pub thread: u32,
    /// A span of its own track ([`crate::span_async`]).
    pub track: Option<u64>,
    /// What each counter added while a [`crate::step`] was open, leaving out those that added
    /// nothing.
    pub attached: Vec<Attached>,
}

/// What one counter added during a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attached {
    pub group: &'static str,
    pub name: &'static str,
    pub unit: Unit,
    /// On every thread.
    pub all: u64,
    /// On the step's own thread.
    pub here: u64,
}

impl Record {
    /// What `group`/`name` added during the step.
    pub fn get(&self, group: &str, name: &str) -> Option<Attached> {
        self.attached
            .iter()
            .filter(|a| a.group == group && a.name == name)
            .copied()
            .reduce(|a, b| Attached {
                all: a.all + b.all,
                here: a.here + b.here,
                ..a
            })
    }

    /// What the counters of `group` added during the step, summed: on every thread, and here.
    pub fn sum(&self, group: &str) -> (u64, u64) {
        self.attached
            .iter()
            .filter(|a| a.group == group)
            .fold((0, 0), |(all, here), a| (all + a.all, here + a.here))
    }
}

/// The one sink every span ends in.
pub struct Collector {
    epoch: Instant,
    records: Mutex<Vec<Record>>,
    dropped: AtomicU64,
    threads: Mutex<BTreeMap<u32, String>>,
}

static COLLECTOR: OnceLock<Collector> = OnceLock::new();

impl Collector {
    pub fn global() -> &'static Collector {
        COLLECTOR.get_or_init(|| Collector {
            epoch: Instant::now(),
            records: Mutex::new(Vec::new()),
            dropped: AtomicU64::new(0),
            threads: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn epoch(&self) -> Instant {
        self.epoch
    }

    pub(crate) fn name_thread(&self, id: u32, name: String) {
        self.threads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, name);
    }

    pub(crate) fn push(&self, record: Record) {
        let mut records = self.records.lock().unwrap_or_else(|p| p.into_inner());
        if records.len() < MAX_RECORDS {
            records.push(record);
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Everything kept so far, kept still.
    pub fn snapshot(&self) -> Records {
        Records {
            records: self
                .records
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
            threads: self.threads(),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }

    fn threads(&self) -> Vec<(u32, String)> {
        self.threads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, name)| (*id, name.clone()))
            .collect()
    }
}

/// Everything kept so far, taken out of the collector.
pub fn take() -> Records {
    let collector = Collector::global();
    let records = std::mem::take(&mut *collector.records.lock().unwrap_or_else(|p| p.into_inner()));
    Records {
        records,
        threads: collector.threads(),
        dropped: collector.dropped.swap(0, Ordering::Relaxed),
    }
}

/// Ended spans, and what reads them.
pub struct Records {
    pub records: Vec<Record>,
    threads: Vec<(u32, String)>,
    dropped: u64,
}

impl Records {
    /// The spans of one kind.
    pub fn of<'a>(&'a self, cat: &'a str) -> impl Iterator<Item = &'a Record> + 'a {
        self.records.iter().filter(move |r| r.cat == cat)
    }

    /// One line per step: its name, its time, and each counter it carries, on every thread
    /// (and on its own, when that differs).
    pub fn table(&self) -> String {
        let mut out = String::new();
        for record in self.of("step") {
            let _ = write!(
                out,
                "{:<32} {:>9.1} ms",
                record.name,
                record.duration.as_secs_f64() * 1000.
            );
            for a in &record.attached {
                let value = |n: u64| match a.unit {
                    Unit::Count => n.to_string(),
                    Unit::Bytes => format!("{:.1} KB", n as f64 / 1e3),
                    Unit::Nanos => format!("{:.1} ms", n as f64 / 1e6),
                };
                let _ = write!(out, "  {} {} {}", a.group, a.name, value(a.all));
                if a.here != a.all {
                    let _ = write!(out, " ({} here)", value(a.here));
                }
            }
            out.push('\n');
        }
        out
    }

    /// Chrome trace JSON (`ui.perfetto.dev`, `chrome://tracing`): each span a complete event
    /// (`X`) on its thread or an async pair (`b`/`e`) on its own track, a step's counters in its
    /// `args`, and for each step a counter track (`C`) per counter group, at what the step added
    /// while it lasts.
    pub fn chrome_trace(&self) -> String {
        let pid = std::process::id();
        let micros = |d: Duration| d.as_nanos() as f64 / 1000.;
        let mut events: Vec<String> = Vec::with_capacity(self.records.len() + 8);
        for (id, name) in &self.threads {
            events.push(format!(
                "{{\"ph\":\"M\",\"name\":\"thread_name\",\"pid\":{pid},\"tid\":{id},\"args\":{{\"name\":{}}}}}",
                json(name)
            ));
        }
        for r in &self.records {
            let (cat, name) = (json(r.cat), json(&r.name));
            let (ts, end) = (micros(r.start), micros(r.start + r.duration));
            let args = if r.attached.is_empty() {
                String::new()
            } else {
                let fields: Vec<String> = r
                    .attached
                    .iter()
                    .map(|a| format!("{}:{}", json(&format!("{} {}", a.group, a.name)), a.all))
                    .collect();
                format!(",\"args\":{{{}}}", fields.join(","))
            };
            match r.track {
                None => events.push(format!(
                    "{{\"ph\":\"X\",\"cat\":{cat},\"name\":{name},\"pid\":{pid},\"tid\":{},\"ts\":{ts:.3},\"dur\":{:.3}{args}}}",
                    r.thread,
                    micros(r.duration)
                )),
                Some(id) => {
                    events.push(format!(
                        "{{\"ph\":\"b\",\"cat\":{cat},\"name\":{name},\"pid\":{pid},\"tid\":{},\"id\":{id},\"ts\":{ts:.3}{args}}}",
                        r.thread
                    ));
                    events.push(format!(
                        "{{\"ph\":\"e\",\"cat\":{cat},\"name\":{name},\"pid\":{pid},\"tid\":{},\"id\":{id},\"ts\":{end:.3}}}",
                        r.thread
                    ));
                }
            }
            // A step's counters: a counter track per group, one series per counter, at what
            // the step added across the step and back to nothing as it ends. Frames and
            // measures inside a step carry theirs in their `args` only.
            if r.cat == "step" {
                let mut groups: BTreeMap<&str, (Vec<String>, Vec<String>)> = BTreeMap::new();
                for a in &r.attached {
                    let (during, after) = groups.entry(a.group).or_default();
                    during.push(format!("{}:{}", json(a.name), a.all));
                    after.push(format!("{}:0", json(a.name)));
                }
                for (group, (during, after)) in groups {
                    for (at, series) in [(ts, during), (end, after)] {
                        events.push(format!(
                            "{{\"ph\":\"C\",\"name\":{},\"pid\":{pid},\"ts\":{at:.3},\"args\":{{{}}}}}",
                            json(group),
                            series.join(",")
                        ));
                    }
                }
            }
        }
        if self.dropped > 0 {
            events.push(format!(
                "{{\"ph\":\"i\",\"s\":\"g\",\"name\":\"collector full: {} spans dropped\",\"pid\":{pid},\"tid\":0,\"ts\":0}}",
                self.dropped
            ));
        }
        format!(
            "{{\"displayTimeUnit\":\"ms\",\"traceEvents\":[{}]}}",
            events.join(",")
        )
    }
}

fn json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Collects for one test; the collector is the process's, so a test reads its own step's record
/// ([`crate::Span::end`]) rather than everything taken.
#[cfg(test)]
pub(crate) fn collect_for_test() -> crate::On {
    COLLECTING.store(true, Ordering::Relaxed);
    crate::hold_on()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_is_chrome_json_with_counter_tracks() {
        let step = Record {
            cat: "step",
            name: "open \"Person\"".into(),
            start: Duration::from_micros(5),
            duration: Duration::from_micros(7),
            thread: 1,
            track: None,
            attached: vec![Attached {
                group: "GET",
                name: "/graphs/{graph_id}/query",
                unit: Unit::Count,
                all: 3,
                here: 0,
            }],
        };
        let fetch = Record {
            cat: "fetch",
            name: "/graphs/{graph_id}/query".into(),
            start: Duration::from_micros(1),
            duration: Duration::from_micros(20),
            thread: 2,
            track: Some(9),
            attached: Vec::new(),
        };
        let records = Records {
            records: vec![step, fetch],
            threads: vec![(1, "main".into())],
            dropped: 0,
        };
        let text = records.chrome_trace();
        assert!(text.starts_with("{\"displayTimeUnit\":\"ms\",\"traceEvents\":[{\"ph\":\"M\""));
        assert!(text.contains("\"name\":\"open \\\"Person\\\"\""));
        assert!(text.contains("\"ph\":\"X\""));
        assert!(text.contains("\"args\":{\"GET /graphs/{graph_id}/query\":3}"));
        assert!(text.contains("{\"ph\":\"C\",\"name\":\"GET\",\"pid\":"));
        assert!(text.contains("\"args\":{\"/graphs/{graph_id}/query\":0}"));
        assert!(text.contains("\"ph\":\"b\"") && text.contains("\"ph\":\"e\""));
        assert!(text.contains("\"ts\":21.000"));
        assert!(text.ends_with("]}"));
        let table = records.table();
        assert!(table.starts_with("open \"Person\""));
        assert!(table.contains("GET /graphs/{graph_id}/query 3 (0 here)"));
    }
}
