//! Telemetry's files in the data directory's `telemetry/` folder (spec 018, "Recording, storing
//! and sending"): the reports waiting to go, the ones sent in the last 7 days, crash reports
//! waiting for an answer, and the install ID. Each file has a cap; old lines go first.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::Value;

use crate::crash::CrashReport;

/// The most the pending and sent files may hold.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// How long a report is kept unsent, or listed once sent.
pub const KEEP_DAYS: i64 = 7;

pub fn now_text(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The folder and its files.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn pending_path(&self) -> PathBuf {
        self.dir.join("pending.jsonl")
    }

    fn sent_path(&self) -> PathBuf {
        self.dir.join("sent.jsonl")
    }

    fn crashes_dir(&self) -> PathBuf {
        self.dir.join("crashes")
    }

    fn install_id_path(&self) -> PathBuf {
        self.dir.join("install-id")
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The reports waiting to go, oldest first.
    pub fn pending(&self) -> Vec<Value> {
        read_lines(&self.pending_path())
    }

    /// Adds reports to the end of the pending file, dropping the oldest past its cap.
    pub fn add_pending(&self, events: &[Value]) {
        if events.is_empty() {
            return;
        }
        let mut all = self.pending();
        all.extend(events.iter().cloned());
        write_lines(&self.pending_path(), &all);
    }

    /// Keeps only the pending reports `keep` says yes to.
    pub fn retain_pending(&self, keep: impl Fn(&Value) -> bool) {
        let all = self.pending();
        let kept: Vec<Value> = all.iter().filter(|event| keep(event)).cloned().collect();
        if kept.len() != all.len() {
            write_lines(&self.pending_path(), &kept);
        }
    }

    /// Removes the first `count` pending reports: a batch that was sent or refused.
    pub fn drop_first_pending(&self, count: usize) {
        let all = self.pending();
        write_lines(&self.pending_path(), &all[count.min(all.len())..]);
    }

    /// Records reports as sent at `now`, for the telemetry log.
    pub fn log_sent(&self, events: &[Value], now: DateTime<Utc>) {
        let mut all = read_lines(&self.sent_path());
        let at = now_text(now);
        all.extend(
            events
                .iter()
                .map(|event| serde_json::json!({ "sent_at": at, "report": event })),
        );
        write_lines(&self.sent_path(), &all);
    }

    /// The reports sent in the last [`KEEP_DAYS`], oldest first, each with when it went.
    pub fn sent(&self) -> Vec<Value> {
        read_lines(&self.sent_path())
    }

    /// Deletes what is older than [`KEEP_DAYS`]: pending reports, the sent log's lines and crash
    /// reports nobody answered.
    pub fn expire(&self, now: DateTime<Utc>) {
        let cutoff = now - Duration::days(KEEP_DAYS);
        let fresh = |text: Option<&str>| {
            text.and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .is_none_or(|at| at.with_timezone(&Utc) >= cutoff)
        };
        self.retain_pending(|event| fresh(event.get("timestamp").and_then(Value::as_str)));
        let sent = read_lines(&self.sent_path());
        let kept: Vec<Value> = sent
            .iter()
            .filter(|line| fresh(line.get("sent_at").and_then(Value::as_str)))
            .cloned()
            .collect();
        if kept.len() != sent.len() {
            write_lines(&self.sent_path(), &kept);
        }
        for (path, report) in self.crashes() {
            if !fresh(Some(&report.at)) {
                std::fs::remove_file(path).ok();
            }
        }
    }

    /// Keeps a crash report until it is answered. Synchronous, for the panic hook.
    pub fn keep_crash(&self, report: &CrashReport, id: &str) -> std::io::Result<()> {
        let dir = self.crashes_dir();
        std::fs::create_dir_all(&dir)?;
        let text = serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?;
        std::fs::write(dir.join(format!("{id}.json")), text)
    }

    /// The crash reports waiting for an answer, oldest first.
    pub fn crashes(&self) -> Vec<(PathBuf, CrashReport)> {
        let Ok(entries) = std::fs::read_dir(self.crashes_dir()) else {
            return Vec::new();
        };
        let mut all: Vec<(PathBuf, CrashReport)> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "json"))
            .filter_map(|path| {
                let report = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
                Some((path, report))
            })
            .collect();
        all.sort_by(|a, b| a.1.at.cmp(&b.1.at));
        all
    }

    pub fn delete_crashes(&self) {
        std::fs::remove_dir_all(self.crashes_dir()).ok();
    }

    /// The install ID, made when first asked for.
    pub fn install_id(&self) -> String {
        if let Ok(id) = std::fs::read_to_string(self.install_id_path()) {
            let id = id.trim();
            if uuid::Uuid::parse_str(id).is_ok() {
                return id.to_owned();
            }
        }
        self.reset_install_id()
    }

    /// A new install ID in place of the old.
    pub fn reset_install_id(&self) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(&self.dir).ok();
        std::fs::write(self.install_id_path(), &id).ok();
        id
    }

    pub fn has_install_id(&self) -> bool {
        self.install_id_path().is_file()
    }

    pub fn delete_install_id(&self) {
        std::fs::remove_file(self.install_id_path()).ok();
    }

    /// A small text value kept between runs (the last start, the last version, the last
    /// summary).
    pub fn read_marker(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.marker(name))
            .ok()
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    }

    pub fn remove_marker(&self, name: &str) {
        std::fs::remove_file(self.marker(name)).ok();
    }

    pub fn write_marker(&self, name: &str, value: &str) {
        std::fs::create_dir_all(&self.dir).ok();
        std::fs::write(self.marker(name), value).ok();
    }
}

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Writes `lines`, dropping the oldest until the file is under [`MAX_FILE_BYTES`].
fn write_lines(path: &Path, lines: &[Value]) {
    let texts: Vec<String> = lines.iter().map(Value::to_string).collect();
    let mut total: usize = texts.iter().map(|t| t.len() + 1).sum();
    let mut start = 0;
    while total > MAX_FILE_BYTES && start < texts.len() {
        total -= texts[start].len() + 1;
        start += 1;
    }
    let mut out = String::with_capacity(total);
    for text in &texts[start..] {
        out.push_str(text);
        out.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if out.is_empty() {
        std::fs::remove_file(path).ok();
    } else if let Err(error) =
        studio_settings::write_atomic(path, out.as_bytes(), studio_settings::Access::Private)
    {
        log::warn!("telemetry could not write its file: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("telemetry"));
        (dir, store)
    }

    #[test]
    fn the_pending_file_keeps_its_cap_by_dropping_the_oldest() {
        let (_dir, store) = store();
        let big = "x".repeat(100_000);
        let events: Vec<Value> = (0..20).map(|i| json!({ "n": i, "pad": big })).collect();
        store.add_pending(&events);
        let kept = store.pending();
        assert!(kept.len() < 20 && !kept.is_empty());
        assert_eq!(kept.last().unwrap()["n"], 19);
    }

    #[test]
    fn reports_older_than_a_week_are_deleted_unsent() {
        let (_dir, store) = store();
        let now = Utc::now();
        let old = now_text(now - Duration::days(8));
        let new = now_text(now - Duration::days(1));
        store.add_pending(&[json!({ "timestamp": old }), json!({ "timestamp": new })]);
        store.expire(now);
        assert_eq!(store.pending(), vec![json!({ "timestamp": new })]);
    }

    #[test]
    fn the_install_id_is_kept_reset_and_deleted() {
        let (_dir, store) = store();
        assert!(!store.has_install_id());
        let id = store.install_id();
        assert_eq!(store.install_id(), id);
        let new = store.reset_install_id();
        assert_ne!(new, id);
        store.delete_install_id();
        assert!(!store.has_install_id());
    }
}
