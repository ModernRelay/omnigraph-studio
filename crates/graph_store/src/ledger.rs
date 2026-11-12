//! The read ledger (spec 030): how each read a view asked of the session was answered, so the
//! cache's cost can be measured and kept within budgets. It records the name Studio gave a
//! query or the route it reads, who asked, and how the answer came: held, carried from an
//! earlier commit, seeded by a page's rows, joined to a read under way, batched with others, or
//! sent. Never a query's text, its parameters, ids or rows.
//!
//! Tallies are always kept, in a table bounded by the names Studio gives its reads. Events, with
//! their times, are kept only while asked for ([`ReadLedger::keep_events`]), at most
//! [`EVENTS`] of them.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use gpui::SharedString;

/// How many events the ledger keeps while keeping them; the oldest go first.
pub const EVENTS: usize = 4096;

/// The view a scope reads for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadView {
    Table,
    Peek,
    Page,
    /// The relationship tree and the graph canvas.
    Exploration,
}

/// Who asked for a read: a view through its scope, or a caller the ledger cannot name yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Asker {
    View(ReadView),
    Unnamed,
}

/// How a kept answer first came to be. A carried answer is kept among answers read, so a later
/// hit on it says only where it was found, not that a server gave it at that commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    Read,
    Seeded,
    Carried,
}

/// Where a sent read was asked to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// A branch's head, asked for with no commit: the server answers at its newest, and the
    /// answer is kept at the commit it names when what it depends on is known.
    Branch,
    /// A commit that is its branch's head, sent as a read of the branch, which the server
    /// answers faster; its answer is kept at the commit.
    HeadCommit,
    /// A commit.
    Pinned,
    /// A route that names neither (a schema, a commit list, a page of changes).
    Route,
}

/// What came of a sent read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Ok,
    /// Refused with this HTTP status.
    Refused(u16),
    /// No answer from the server: the connection, a timeout, a body that did not decode.
    Failed,
    /// Every asker let go before it answered.
    Dropped,
}

/// How a read was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Answered {
    /// An answer kept at this commit.
    Held(Origin),
    /// An answer at an earlier commit that nothing since changed, itself of this origin.
    Carried(Origin),
    /// Another answer at this commit gave it: a page's row, or a count of 0 for a group's
    /// first neighbours (`builder::EmptyWithout`).
    Seeded,
    /// It shared a read already under way.
    Joined,
    /// A count asked with others in one read.
    Batched,
    /// A request to the server.
    Sent(Target, Status),
}

impl Answered {
    /// Whether the server was asked.
    pub fn sent(self) -> bool {
        matches!(self, Answered::Sent(..))
    }
}

/// One read the session answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadEvent {
    pub what: SharedString,
    pub asker: Asker,
    pub answered: Answered,
    /// How long a sent read took, from asking for a slot to its answer; zero otherwise.
    pub took: Duration,
}

/// The session's ledger of reads.
#[derive(Default)]
pub struct ReadLedger {
    tallies: HashMap<(SharedString, Asker, Answered), Tally>,
    events: Option<VecDeque<ReadEvent>>,
}

/// How many reads of one kind were answered one way, and how long the sent ones took.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub count: u64,
    pub took: Duration,
    pub slowest: Duration,
}

impl ReadLedger {
    pub(crate) fn record(&mut self, event: ReadEvent) {
        let tally = self
            .tallies
            .entry((event.what.clone(), event.asker, event.answered))
            .or_default();
        tally.count += 1;
        tally.took += event.took;
        tally.slowest = tally.slowest.max(event.took);
        if let Some(events) = &mut self.events {
            if events.len() == EVENTS {
                events.pop_front();
            }
            events.push_back(event);
        }
    }

    /// Starts or stops keeping events; stopping lets go of those kept.
    pub fn keep_events(&mut self, keep: bool) {
        match (keep, self.events.is_some()) {
            (true, false) => self.events = Some(VecDeque::new()),
            (false, true) => self.events = None,
            _ => {}
        }
    }

    /// The events kept since the last take, oldest first.
    pub fn take_events(&mut self) -> Vec<ReadEvent> {
        self.events
            .as_mut()
            .map(|events| events.drain(..).collect())
            .unwrap_or_default()
    }

    /// Every tally, by what was read, who asked and how it was answered.
    pub fn tallies(&self) -> impl Iterator<Item = (&(SharedString, Asker, Answered), &Tally)> {
        self.tallies.iter()
    }

    /// How many reads of `what` were answered so that `answered` says yes, whoever asked.
    pub fn count(&self, what: &str, answered: impl Fn(Answered) -> bool) -> u64 {
        self.tallies
            .iter()
            .filter(|((name, _, how), _)| name.as_ref() == what && answered(*how))
            .map(|(_, tally)| tally.count)
            .sum()
    }

    /// How many requests went to the server, of any kind.
    pub fn sent(&self) -> u64 {
        self.tallies
            .iter()
            .filter(|((_, _, how), _)| how.sent())
            .map(|(_, tally)| tally.count)
            .sum()
    }

    /// Forgets every tally and event, as a test does between steps.
    pub fn clear(&mut self) {
        self.tallies.clear();
        if let Some(events) = &mut self.events {
            events.clear();
        }
    }

    /// The tallies as a table, most requests first: one line for each name, who asked and how.
    pub fn table(&self) -> String {
        let mut rows: Vec<_> = self.tallies.iter().collect();
        rows.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.0.cmp(&b.0.0)));
        let mut out = String::new();
        for ((what, asker, answered), tally) in rows {
            let mean = tally
                .took
                .checked_div(tally.count as u32)
                .unwrap_or_default();
            out.push_str(&format!(
                "{:>6}  {:<32} {:<20} {:<28} mean {:>7.1} ms  slowest {:>7.1} ms\n",
                tally.count,
                what,
                format!("{asker:?}"),
                format!("{answered:?}"),
                mean.as_secs_f64() * 1000.0,
                tally.slowest.as_secs_f64() * 1000.0,
            ));
        }
        out
    }
}

/// A sent read still under way: recorded as dropped unless it is finished first.
pub(crate) struct Sending {
    pub what: SharedString,
    pub asker: Asker,
    pub target: Target,
    pub started: Instant,
}

impl Sending {
    pub(crate) fn finish(self, status: Status, now: Instant) -> ReadEvent {
        ReadEvent {
            what: self.what,
            asker: self.asker,
            answered: Answered::Sent(self.target, status),
            took: now.saturating_duration_since(self.started),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(what: &str, answered: Answered, ms: u64) -> ReadEvent {
        ReadEvent {
            what: what.to_owned().into(),
            asker: Asker::Unnamed,
            answered,
            took: Duration::from_millis(ms),
        }
    }

    #[test]
    fn tallies_count_and_time_and_events_are_kept_only_when_asked() {
        let mut ledger = ReadLedger::default();
        ledger.record(event("studio_node", Answered::Seeded, 0));
        assert!(ledger.take_events().is_empty());
        ledger.keep_events(true);
        let sent = Answered::Sent(Target::Pinned, Status::Ok);
        ledger.record(event("studio_node", sent, 30));
        ledger.record(event("studio_node", sent, 10));
        assert_eq!(ledger.count("studio_node", |a| a.sent()), 2);
        assert_eq!(ledger.count("studio_node", |a| a == Answered::Seeded), 1);
        assert_eq!(ledger.sent(), 2);
        let tally = ledger
            .tallies()
            .find(|((_, _, a), _)| *a == sent)
            .map(|(_, t)| *t)
            .unwrap();
        assert_eq!(tally.took, Duration::from_millis(40));
        assert_eq!(tally.slowest, Duration::from_millis(30));
        assert_eq!(ledger.take_events().len(), 2);
        assert!(ledger.table().contains("studio_node"));
    }

    #[test]
    fn the_events_kept_are_bounded() {
        let mut ledger = ReadLedger::default();
        ledger.keep_events(true);
        for _ in 0..EVENTS + 5 {
            ledger.record(event("studio_page", Answered::Joined, 0));
        }
        assert_eq!(ledger.take_events().len(), EVENTS);
        assert_eq!(ledger.count("studio_page", |_| true), (EVENTS + 5) as u64);
    }
}
