//! Measuring Studio: named spans, counters that attach to them, and one sink
//! (`docs/drawing.md`, "Measuring"; the design is in `docs/perf/20261106-report.md`).
//!
//! Layer 0, standard library only, so every crate may count and trace, the ones without GPUI
//! among them.
//!
//! - A [`Counter`] is a `static` at the place that counts ([`count!`] declares one there): a
//!   request on a route, the bytes of its answer, a view drawn, an allocation. Adding to one
//!   costs one relaxed atomic load while measuring is off, and an atomic add and a thread-local
//!   add while it is on: no lock and no allocation.
//! - A [`Span`] is something with a name and a duration. [`span`] marks work for the trace (a
//!   request, decoding an answer). [`step`] measures a scenario, an interaction or a frame: when
//!   it ends, what every counter added while it was open is attached to it, counted on every
//!   thread and on the span's own.
//! - Ended spans go to one sink, the [`Collector`], while one collects ([`collect`]). It prints
//!   a table ([`Records::table`]), writes Chrome trace JSON that `ui.perfetto.dev` opens
//!   ([`Records::chrome_trace`]), and a test reads counters through a [`Capture`].
//!
//! Measuring is off until something turns it on: [`init`] when `OMNIGRAPH_STUDIO_PERF` or
//! `OMNIGRAPH_STUDIO_TRACE` is set, the snapshot tool, or a test's [`capture`]. Every name is
//! from Studio's code, never anything a person typed or a server sent: a route's template, never
//! its path; a query's name only when Studio built the query. Nothing here holds a token, query
//! text or row data.

mod counter;
mod records;
mod span;

pub use counter::{Capture, Counter, Gauge, Unit, capture, counters};
pub use records::{Attached, Collector, Record, Records, collect, collecting, take};
pub use span::{Span, span, span_async, step};

use std::sync::atomic::{AtomicU64, Ordering};

/// Requests to a server under way (`graph_client`): the snapshot tool waits for it to fall to
/// none before it says an interaction settled.
pub static REQUESTS_UNDER_WAY: Gauge = Gauge::new("requests under way", Unit::Count);

/// Whether measuring is on, and how many hold it on: the top bit is the process's own setting
/// ([`set_on`]), the rest count the [`On`] guards alive. One relaxed load decides every counter
/// and span while it is off. After GPUI's `TRACE_STATE` (`gpui/src/profiler.rs`, Zed v1.26.2).
static STATE: AtomicU64 = AtomicU64::new(0);
const SETTING: u64 = 1 << 63;

/// Whether counters count and spans measure.
#[inline]
pub fn on() -> bool {
    STATE.load(Ordering::Relaxed) != 0
}

/// Turns measuring on or off for the process, apart from any [`On`] guard.
pub fn set_on(on: bool) {
    if on {
        STATE.fetch_or(SETTING, Ordering::Relaxed);
    } else {
        STATE.fetch_and(!SETTING, Ordering::Relaxed);
    }
}

/// Keeps measuring on while it lives, whatever the process's setting.
#[must_use = "measuring stays on only while the guard lives"]
pub struct On(());

/// Turns measuring on until the guard is dropped.
pub fn hold_on() -> On {
    STATE.fetch_add(1, Ordering::Relaxed);
    On(())
}

impl Drop for On {
    fn drop(&mut self) {
        STATE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The environment variable that turns measuring on for a run of Studio.
pub const PERF_ENV: &str = "OMNIGRAPH_STUDIO_PERF";
/// The environment variable that names a trace file: measuring is on, every span is collected,
/// and Studio writes them there as Chrome trace JSON when it quits ([`write_trace`]).
pub const TRACE_ENV: &str = "OMNIGRAPH_STUDIO_TRACE";

/// Reads [`PERF_ENV`] and [`TRACE_ENV`] once, when the process starts.
pub fn init() {
    let set = |name| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    if set(PERF_ENV) || set(TRACE_ENV) {
        set_on(true);
    }
    if set(TRACE_ENV) {
        collect(true);
    }
}

/// Writes the spans collected so far to the file [`TRACE_ENV`] names, as Chrome trace JSON,
/// and returns its path; `None` when it names none.
pub fn write_trace() -> std::io::Result<Option<std::path::PathBuf>> {
    let Some(path) = std::env::var_os(TRACE_ENV).filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(path);
    let records = Collector::global().snapshot();
    std::fs::write(&path, records.chrome_trace())?;
    Ok(Some(path))
}

/// Declares a [`Counter`] where it counts, as a `static`, and adds to it: `count!("draw",
/// "TableGrid")` adds one, `count!("bytes", "answers", Unit::Bytes, n)` adds `n`.
#[macro_export]
macro_rules! count {
    ($group:literal, $name:literal) => {
        $crate::count!($group, $name, $crate::Unit::Count, 1)
    };
    ($group:literal, $name:literal, $unit:expr, $n:expr) => {{
        static COUNTER: $crate::Counter = $crate::Counter::new($group, $name, $unit);
        COUNTER.add($n as u64);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_holds_measuring_on() {
        let before = on();
        {
            let _on = hold_on();
            assert!(on());
        }
        assert_eq!(on(), before);
    }
}
