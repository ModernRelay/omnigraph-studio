//! Spans: something with a name and a duration, which the collector keeps while it collects.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use crate::counter::{Counter, counters};
use crate::records::{Attached, Collector, Record, collecting};

static NEXT_TRACK: AtomicU64 = AtomicU64::new(1);
static NEXT_THREAD: AtomicU32 = AtomicU32::new(1);

thread_local! {
    static THREAD: u32 = {
        let id = NEXT_THREAD.fetch_add(1, Ordering::Relaxed);
        let name = std::thread::current().name().unwrap_or("thread").to_owned();
        Collector::global().name_thread(id, name);
        id
    };
}

/// A span under way. Inert while nothing collects ([`crate::collect`]): made and dropped
/// without reading the clock, taking a lock or allocating.
#[must_use = "a span measures until it is dropped"]
pub struct Span(Option<Box<Open>>);

struct Open {
    cat: &'static str,
    name: Cow<'static, str>,
    start: Instant,
    thread: u32,
    track: Option<u64>,
    /// Each counter's count on every thread and on this one as the span began: a [`step`]'s.
    base: Option<Vec<(&'static Counter, u64, u64)>>,
    /// Room for the same as it ends, made as it began, so reading them allocates nothing that
    /// the step would count as its own.
    end: Vec<(&'static Counter, u64, u64)>,
}

/// Counters a step leaves room for beyond those numbered when it began.
const NEW_COUNTERS: usize = 64;

/// Work to show on its thread's track in the trace: decoding an answer, a frame.
#[inline]
pub fn span(cat: &'static str, name: &'static str) -> Span {
    if !collecting() {
        return Span(None);
    }
    Span::begin(cat, Cow::Borrowed(name), false, false)
}

/// [`span`] of work that waits, such as a request held across awaits: on a track of its own, so
/// spans under way at once do not nest by mistake.
#[inline]
pub fn span_async(cat: &'static str, name: &'static str) -> Span {
    if !collecting() {
        return Span(None);
    }
    Span::begin(cat, Cow::Borrowed(name), true, false)
}

/// A measured span: a scenario, an interaction or a frame. When it ends, what each counter
/// added while it was open is attached to it, on every thread and on this one.
pub fn step(cat: &'static str, name: impl Into<Cow<'static, str>>) -> Span {
    if !collecting() {
        return Span(None);
    }
    Span::begin(cat, name.into(), false, true)
}

impl Span {
    fn begin(cat: &'static str, name: Cow<'static, str>, waits: bool, measured: bool) -> Self {
        // Everything the span allocates is allocated before the counters are read, so a step
        // counts none of its own allocations (divan's `AllocProfiler` reads its tallies after
        // its setup for the same reason).
        let room = if measured {
            counters().count() + NEW_COUNTERS
        } else {
            0
        };
        let mut open = Box::new(Open {
            cat,
            name,
            thread: THREAD.with(|id| *id),
            track: waits.then(|| NEXT_TRACK.fetch_add(1, Ordering::Relaxed)),
            base: measured.then(|| Vec::with_capacity(room)),
            end: Vec::with_capacity(room),
            start: Instant::now(),
        });
        if let Some(base) = &mut open.base {
            base.extend(
                counters()
                    .take(room)
                    .map(|counter| (counter, counter.total(), counter.here())),
            );
        }
        // Last, so reading the counters is not in the span's time.
        open.start = Instant::now();
        Span(Some(open))
    }

    /// Ends the span now and returns what it recorded, which the collector keeps too; `None`
    /// for an inert span.
    pub fn end(mut self) -> Option<Record> {
        self.finish()
    }

    fn finish(&mut self) -> Option<Record> {
        let mut open = self.0.take()?;
        let end = Instant::now();
        // Into the room made as the span began: nothing is allocated before every counter is read.
        let room = open.end.capacity();
        open.end.extend(
            counters()
                .take(room)
                .map(|counter| (counter, counter.total(), counter.here())),
        );
        let attached = open
            .base
            .map(|base| {
                open.end
                    .iter()
                    .filter_map(|&(counter, all_now, here_now)| {
                        let (all, here) = base
                            .iter()
                            .find(|(c, ..)| std::ptr::eq(*c, counter))
                            .map_or((0, 0), |(_, all, here)| (*all, *here));
                        let attached = Attached {
                            group: counter.group,
                            name: counter.name,
                            unit: counter.unit,
                            all: all_now - all,
                            here: here_now - here,
                        };
                        (attached.all > 0 || attached.here > 0).then_some(attached)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let collector = Collector::global();
        let record = Record {
            cat: open.cat,
            name: std::mem::take(&mut open.name),
            start: open.start.saturating_duration_since(collector.epoch()),
            duration: end.saturating_duration_since(open.start),
            thread: open.thread,
            track: open.track,
            attached,
        };
        collector.push(record.clone());
        Some(record)
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Unit;

    #[test]
    fn a_step_carries_what_was_counted_while_it_was_open() {
        static REQUESTS: Counter = Counter::new("GET", "/test/route", Unit::Count);
        static BYTES: Counter = Counter::new("bytes", "/test/route", Unit::Bytes);
        let _collect = crate::records::collect_for_test();
        REQUESTS.add(1);
        let step = step("step", "open a thing");
        REQUESTS.add(2);
        BYTES.add(300);
        std::thread::spawn(|| REQUESTS.add(4)).join().unwrap();
        let record = step.end().unwrap();
        let requests = record.get("GET", "/test/route").unwrap();
        assert_eq!((requests.all, requests.here), (6, 2));
        assert_eq!(record.get("bytes", "/test/route").unwrap().all, 300);
        assert_eq!(record.name, "open a thing");
    }

    /// A step's own bookkeeping is not among what it counts: counted as an allocator would, the
    /// counters a step reads see nothing added between its start and end but what the work did.
    #[test]
    fn a_step_counts_nothing_of_its_own() {
        static WORK: Counter = Counter::new("test", "work", Unit::Count);
        let _collect = crate::records::collect_for_test();
        WORK.add(1);
        let step = step("step", "empty");
        let record = step.end().unwrap();
        assert_eq!(record.get("test", "work"), None);
    }
}
