//! Counters: `static`s at the place that counts, each given a number once, and a thread-local
//! copy of every count, so a span or a test can say what was counted on its own thread.

use std::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::on;

/// What a counter counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Count,
    Bytes,
    Nanos,
}

/// The counters a process can number. A counter past it is counted, but on no thread's tally
/// and in no span's.
pub(crate) const MAX_COUNTERS: usize = 512;
const UNNUMBERED: u32 = u32::MAX;
const FULL: u32 = u32::MAX - 1;

/// Every counter numbered so far, in the order they were first added to: a fixed table claimed
/// slot by slot, so numbering one takes no lock and allocates nothing, and an allocator may
/// count. After tracing-core's `DefaultCallsite::register` and puffin's `ScopeId`.
static NUMBERED: [AtomicPtr<Counter>; MAX_COUNTERS] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; MAX_COUNTERS];
static NEXT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's count of each numbered counter. `const` and without `Drop`, so reading it
    /// never allocates and works while the thread's other thread-locals are torn down (Zed's
    /// `bench_metrics` allocation counts; divan's `ThreadAllocInfo`).
    static HERE: [Cell<u64>; MAX_COUNTERS] = const { [const { Cell::new(0) }; MAX_COUNTERS] };
}

/// A count, kept as a `static` where it is counted: [`crate::count!`] declares one, or a crate
/// keeps a table of them (a route's requests).
pub struct Counter {
    pub group: &'static str,
    pub name: &'static str,
    pub unit: Unit,
    id: AtomicU32,
    total: AtomicU64,
}

impl Counter {
    pub const fn new(group: &'static str, name: &'static str, unit: Unit) -> Self {
        Self {
            group,
            name,
            unit,
            id: AtomicU32::new(UNNUMBERED),
            total: AtomicU64::new(0),
        }
    }

    /// Adds `n` while measuring is on; otherwise one relaxed load and nothing else.
    #[inline]
    pub fn add(&'static self, n: u64) {
        if on() {
            self.add_on(n);
        }
    }

    /// Adds a duration, in nanoseconds, to a counter of [`Unit::Nanos`].
    #[inline]
    pub fn add_time(&'static self, time: std::time::Duration) {
        if on() {
            self.add_on(time.as_nanos() as u64);
        }
    }

    fn add_on(&'static self, n: u64) {
        self.total.fetch_add(n, Ordering::Relaxed);
        let id = self.id();
        if (id as usize) < MAX_COUNTERS {
            // Fails only while the thread exits, when nothing reads its counts.
            HERE.try_with(|here| here[id as usize].set(here[id as usize].get() + n))
                .ok();
        }
    }

    /// Its number, given the first time it is added to.
    fn id(&'static self) -> u32 {
        let id = self.id.load(Ordering::Relaxed);
        if id != UNNUMBERED {
            return id;
        }
        self.number()
    }

    #[cold]
    fn number(&'static self) -> u32 {
        // Two threads may race to number it: the one whose claim lands first keeps its slot,
        // and the other's slot is left empty, which readers skip.
        let slot = NEXT.fetch_add(1, Ordering::Relaxed);
        let id = if slot < MAX_COUNTERS {
            slot as u32
        } else {
            FULL
        };
        match self
            .id
            .compare_exchange(UNNUMBERED, id, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                if id != FULL {
                    NUMBERED[slot].store(self as *const Counter as *mut Counter, Ordering::Release);
                }
                id
            }
            Err(won) => won,
        }
    }

    /// Everything added on every thread since the process started.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Everything added on this thread.
    pub fn here(&self) -> u64 {
        let id = self.id.load(Ordering::Relaxed) as usize;
        if id >= MAX_COUNTERS {
            return 0;
        }
        HERE.try_with(|here| here[id].get()).unwrap_or(0)
    }
}

/// Every counter added to so far.
pub fn counters() -> impl Iterator<Item = &'static Counter> {
    let len = NEXT.load(Ordering::Acquire).min(MAX_COUNTERS);
    NUMBERED[..len].iter().filter_map(|slot| {
        let counter = slot.load(Ordering::Acquire);
        // SAFETY: only `&'static Counter`s are stored, and never removed.
        (!counter.is_null()).then(|| unsafe { &*counter })
    })
}

/// A level rather than a count (requests under way, the heap): it goes up and down, and a span
/// reports where it ends, not what was added. Moves only while measuring is on.
pub struct Gauge {
    pub name: &'static str,
    pub unit: Unit,
    value: AtomicI64,
}

impl Gauge {
    pub const fn new(name: &'static str, unit: Unit) -> Self {
        Self {
            name,
            unit,
            value: AtomicI64::new(0),
        }
    }

    #[inline]
    pub fn add(&self, n: i64) {
        if on() {
            self.value.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// What a test counts: every counter's count on this thread from when it was made, with
/// measuring held on meanwhile. A `#[gpui::test]` runs its foreground and background work on its
/// own thread, so tests running beside it in the same process add nothing to it.
pub struct Capture {
    _on: crate::On,
    base: Vec<(&'static Counter, u64)>,
}

/// Starts a [`Capture`] on this thread.
pub fn capture() -> Capture {
    let on = crate::hold_on();
    Capture {
        _on: on,
        base: counters()
            .map(|counter| (counter, counter.here()))
            .collect(),
    }
}

impl Capture {
    /// What `group`/`name` counted on this thread since the capture began.
    pub fn count(&self, group: &str, name: &str) -> u64 {
        counters()
            .filter(|counter| counter.group == group && counter.name == name)
            .map(|counter| counter.here() - self.since(counter))
            .sum()
    }

    /// What every counter of `group` counted on this thread since the capture began, by name,
    /// leaving out those that counted nothing.
    pub fn group(&self, group: &str) -> Vec<(&'static str, u64)> {
        let mut counted: Vec<(&'static str, u64)> = counters()
            .filter(|counter| counter.group == group)
            .map(|counter| (counter.name, counter.here() - self.since(counter)))
            .filter(|(_, n)| *n > 0)
            .collect();
        counted.sort();
        counted
    }

    fn since(&self, counter: &'static Counter) -> u64 {
        self.base
            .iter()
            .find(|(base, _)| std::ptr::eq(*base, counter))
            .map_or(0, |(_, n)| *n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static A: Counter = Counter::new("test", "a", Unit::Count);
    static B: Counter = Counter::new("test", "b", Unit::Bytes);

    #[test]
    fn a_capture_counts_its_thread_only() {
        let capture = capture();
        A.add(2);
        B.add(10);
        std::thread::spawn(|| A.add(100)).join().unwrap();
        assert_eq!(capture.count("test", "a"), 2);
        assert_eq!(capture.group("test"), [("a", 2), ("b", 10)]);
        assert!(A.total() >= 102);
    }

    #[test]
    fn a_counter_is_numbered_once() {
        let _on = crate::hold_on();
        static C: Counter = Counter::new("test", "numbered", Unit::Count);
        C.add(1);
        C.add(1);
        assert_eq!(
            counters()
                .filter(|counter| counter.name == "numbered")
                .count(),
            1
        );
    }
}
