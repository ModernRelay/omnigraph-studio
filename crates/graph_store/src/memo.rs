//! Answers to reads pinned to a commit (constitution II). A commit never changes, so neither
//! does the answer to a read at it: the answer is kept, and an identical read already under way
//! is shared rather than sent again. The table's peek, the tree, the canvas and the table read
//! the same counts and neighbours; each is sent once.
//!
//! Only successful answers are kept; a failure is forgotten so the next read asks again. An
//! answer is kept only for the read it answers: one that comes back after its key was forgotten
//! or read again (Refresh) changes nothing. The memo holds at most `capacity` entries and
//! forgets the least recently used first. A read nobody waits for any more (every view that
//! asked dropped it) goes before any answer, and a lookup finds nothing for it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::Hash;

/// A read under way, as a memo holds it.
pub(crate) trait Pend {
    /// Whether anyone still waits for its answer. Nothing files the answer of a read nobody
    /// waits for, so held, it would only take a place an answer could have.
    fn wanted(&self) -> bool;
}

/// A memo that is not told when a read is dropped: each is wanted while it is held.
impl Pend for () {
    fn wanted(&self) -> bool {
        true
    }
}

/// What the memo holds for a key: an answer, or the read under way (`P`, a shareable future).
#[derive(Clone)]
pub(crate) enum Held<V, P> {
    Done(V),
    Pending(P),
}

/// How a memo weighs one answer, in bytes.
pub(crate) type Weigh<V> = fn(&V) -> usize;

/// One key's entry: what is held, what its answer weighs (nothing while under way), and when
/// it was last used.
struct Slot<V, P> {
    held: Held<V, P>,
    weight: usize,
    used: u64,
}

pub(crate) struct Memo<K, V, P> {
    entries: HashMap<K, Slot<V, P>>,
    /// Keys by when they were last used, least recently first: using one moves it to the end
    /// in logarithmic time, as many reads a frame do.
    order: BTreeMap<u64, K>,
    /// When each read under way was last used, so finding those nobody waits for goes through
    /// the reads under way alone, not every answer held.
    pending: BTreeSet<u64>,
    clock: u64,
    capacity: usize,
    /// The most the answers held may weigh together, and how one is weighed.
    budget: Option<(usize, Weigh<V>)>,
    bytes: usize,
}

impl<K: Hash + Eq + Clone, V: Clone, P: Clone + Pend> Memo<K, V, P> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            pending: BTreeSet::new(),
            clock: 0,
            capacity: capacity.max(1),
            budget: None,
            bytes: 0,
        }
    }

    /// A memo that also keeps the answers it holds within `budget`, as `weigh` measures them.
    pub(crate) fn weighed(capacity: usize, budget: usize, weigh: Weigh<V>) -> Self {
        Self {
            budget: Some((budget, weigh)),
            ..Self::new(capacity)
        }
    }

    /// What is held for `key`, marking it used. A read under way that nobody waits for is let
    /// go: the next ask sends it again.
    pub(crate) fn get(&mut self, key: &K) -> Option<Held<V, P>> {
        let held = match &self.entries.get(key)?.held {
            Held::Pending(pending) if !pending.wanted() => {
                self.forget(key);
                return None;
            }
            held => held.clone(),
        };
        self.touch(key);
        Some(held)
    }

    /// What is held for `key`, borrowed and without marking it used: telling a read under way
    /// from an answer copies neither. A read under way that nobody waits for is not held.
    pub(crate) fn held(&self, key: &K) -> Option<&Held<V, P>> {
        let held = &self.entries.get(key)?.held;
        match held {
            Held::Pending(pending) if !pending.wanted() => None,
            held => Some(held),
        }
    }

    /// The answer held for `key`, without marking it used.
    pub(crate) fn peek(&self, key: &K) -> Option<&V> {
        match &self.entries.get(key)?.held {
            Held::Done(value) => Some(value),
            Held::Pending(_) => None,
        }
    }

    /// Records a read under way for `key`.
    pub(crate) fn start(&mut self, key: K, pending: P) {
        self.put(key, Held::Pending(pending));
    }

    /// Records how the read under way for `key` ended: an answer is kept, a failure forgotten.
    /// It counts only while `key` still holds that read (`ours` tells it apart): a read whose
    /// key was forgotten, evicted or started again since changes nothing. Whether it counted.
    pub(crate) fn finish<E>(
        &mut self,
        key: &K,
        result: &Result<V, E>,
        ours: impl FnOnce(&P) -> bool,
    ) -> bool {
        match self.entries.get(key).map(|slot| &slot.held) {
            Some(Held::Pending(pending)) if ours(pending) => {}
            _ => return false,
        }
        match result {
            Ok(value) => self.put(key.clone(), Held::Done(value.clone())),
            Err(_) => self.forget(key),
        }
        true
    }

    /// Whether anything is held for `key`, without marking it used: an answer, or a read under
    /// way that someone waits for.
    pub(crate) fn holds(&self, key: &K) -> bool {
        self.held(key).is_some()
    }

    /// Keeps `value` as the answer for `key`.
    pub(crate) fn insert(&mut self, key: K, value: V) {
        self.put(key, Held::Done(value));
    }

    /// Forgets what is held for `key`: the next read asks again.
    pub(crate) fn forget(&mut self, key: &K) {
        if let Some(slot) = self.entries.remove(key) {
            self.bytes -= slot.weight;
            self.order.remove(&slot.used);
            self.pending.remove(&slot.used);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.pending.clear();
        self.bytes = 0;
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    fn put(&mut self, key: K, held: Held<V, P>) {
        let weight = match (&held, self.budget) {
            (Held::Done(value), Some((_, weigh))) => weigh(value),
            _ => 0,
        };
        self.clock += 1;
        let used = self.clock;
        let under_way = matches!(held, Held::Pending(_));
        if let Some(old) = self
            .entries
            .insert(key.clone(), Slot { held, weight, used })
        {
            self.bytes -= old.weight;
            self.order.remove(&old.used);
            self.pending.remove(&old.used);
        }
        self.bytes += weight;
        self.order.insert(used, key);
        if under_way {
            self.pending.insert(used);
        }
        // Over the entry cap, the reads nobody waits for go first: they hold nothing anyone
        // will use. Then the least recently used answers; the entry just kept stays, whatever
        // it weighs. A read someone waits for weighs nothing, so letting it go frees no bytes
        // and loses what it shares: the next identical ask would send it again, and its answer
        // would be kept for nobody. Only the entry cap lets one go, once no answer is left to.
        if self.entries.len() > self.capacity {
            self.drop_unwanted(used);
        }
        let over_budget = |memo: &Self| memo.budget.is_some_and(|(budget, _)| memo.bytes > budget);
        loop {
            let over_count = self.entries.len() > self.capacity;
            if !over_count && !over_budget(self) {
                break;
            }
            let Some(oldest) = self.oldest(used, !over_count) else {
                break;
            };
            if let Some(gone) = self.order.remove(&oldest)
                && let Some(slot) = self.entries.remove(&gone)
            {
                self.bytes -= slot.weight;
                self.pending.remove(&oldest);
            }
        }
    }

    /// Lets go of every read under way that nobody waits for, other than the one used at
    /// `kept`.
    fn drop_unwanted(&mut self, kept: u64) {
        let unwanted: Vec<u64> = self
            .pending
            .iter()
            .copied()
            .filter(|&used| used != kept)
            .filter(|used| {
                self.order
                    .get(used)
                    .and_then(|key| self.entries.get(key))
                    .is_some_and(|slot| matches!(&slot.held, Held::Pending(p) if !p.wanted()))
            })
            .collect();
        for used in unwanted {
            if let Some(key) = self.order.get(&used).cloned() {
                self.forget(&key);
            }
        }
    }

    /// When the least recently used entry other than the one used at `kept` was used: the
    /// oldest answer, else, unless `answers_only`, the oldest read under way.
    fn oldest(&self, kept: u64, answers_only: bool) -> Option<u64> {
        let mut pending = None;
        for (&used, key) in &self.order {
            if used == kept {
                continue;
            }
            match self.entries.get(key).map(|slot| &slot.held) {
                Some(Held::Done(_)) => return Some(used),
                Some(Held::Pending(_)) => {
                    pending.get_or_insert(used);
                }
                None => {}
            }
        }
        pending.filter(|_| !answers_only)
    }

    fn touch(&mut self, key: &K) {
        let Some(slot) = self.entries.get_mut(key) else {
            return;
        };
        self.clock += 1;
        self.order.remove(&slot.used);
        if self.pending.remove(&slot.used) {
            self.pending.insert(self.clock);
        }
        slot.used = self.clock;
        self.order.insert(slot.used, key.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::rc::{Rc, Weak};

    use super::*;

    fn done(memo: &mut Memo<&'static str, u32, ()>, key: &'static str) -> Option<u32> {
        match memo.get(&key) {
            Some(Held::Done(value)) => Some(value),
            _ => None,
        }
    }

    fn ours<P>(_: &P) -> bool {
        true
    }

    #[test]
    fn answers_are_kept_failures_forgotten_and_the_oldest_goes_first() {
        let mut memo: Memo<&'static str, u32, ()> = Memo::new(2);
        memo.start("a", ());
        assert!(matches!(memo.get(&"a"), Some(Held::Pending(()))));
        memo.finish::<()>(&"a", &Ok(1), ours);
        assert_eq!(done(&mut memo, "a"), Some(1));

        memo.start("b", ());
        memo.finish(&"b", &Err::<u32, &str>("refused"), ours);
        assert!(memo.get(&"b").is_none(), "a failure is asked again");

        memo.start("b", ());
        memo.finish::<()>(&"b", &Ok(2), ours);
        // "a" was last used before "b" was added, so it is the least recently used.
        memo.start("c", ());
        memo.finish::<()>(&"c", &Ok(3), ours);
        assert_eq!(memo.len(), 2);
        assert_eq!(
            done(&mut memo, "a"),
            None,
            "the least recently used went first"
        );
        assert_eq!(done(&mut memo, "b"), Some(2));
        assert_eq!(done(&mut memo, "c"), Some(3));

        // Reading "b" makes "c" the oldest.
        assert_eq!(done(&mut memo, "b"), Some(2));
        memo.start("d", ());
        assert_eq!(done(&mut memo, "c"), None);
        assert_eq!(done(&mut memo, "b"), Some(2));
    }

    #[test]
    fn a_weighed_memo_keeps_its_answers_within_the_budget() {
        // Each answer weighs its value; ten bytes at most.
        let mut memo: Memo<&'static str, u32, ()> = Memo::weighed(100, 10, |value| *value as usize);
        memo.insert("a", 4);
        memo.insert("b", 4);
        assert_eq!(memo.bytes(), 8);
        memo.insert("c", 4);
        assert_eq!(done(&mut memo, "a"), None, "the oldest went to make room");
        assert_eq!(memo.bytes(), 8);
        // An answer heavier than the budget is kept alone.
        memo.insert("d", 30);
        assert_eq!(memo.len(), 1);
        assert_eq!(memo.bytes(), 30);
        memo.forget(&"d");
        assert_eq!(memo.bytes(), 0);
        // Under way, an answer weighs nothing; kept, it weighs what it is.
        memo.start("e", ());
        assert_eq!(memo.bytes(), 0);
        memo.finish::<()>(&"e", &Ok(3), ours);
        assert_eq!(memo.bytes(), 3);
    }

    /// A read under way weighs nothing, so the byte budget never lets it go: answers kept
    /// after it, however heavy, push out older answers, and the read stays to be shared and
    /// to keep its answer. The entry cap counts it, letting an answer go first, and it only
    /// once no answer is left to.
    #[test]
    fn the_byte_budget_never_lets_a_read_under_way_go() {
        let mut memo: Memo<&'static str, u32, ()> = Memo::weighed(4, 10, |value| *value as usize);
        memo.insert("old", 2);
        memo.start("slow", ());
        // Heavier together than the budget: the answers before go, the read under way stays.
        memo.insert("a", 6);
        memo.insert("b", 6);
        assert!(matches!(memo.held(&"slow"), Some(Held::Pending(()))));
        assert!(
            !memo.holds(&"old") && !memo.holds(&"a"),
            "the older answers went"
        );
        assert_eq!(memo.bytes(), 6);
        // An answer heavier than the budget alone keeps the read beside it.
        memo.insert("c", 30);
        assert!(matches!(memo.held(&"slow"), Some(Held::Pending(()))));
        assert_eq!(memo.len(), 2);
        assert!(
            memo.finish::<()>(&"slow", &Ok(1), ours),
            "its answer is kept"
        );
        assert_eq!(done(&mut memo, "slow"), Some(1));

        // Four entries at most: an answer goes before a read under way, even a newer answer.
        let mut memo: Memo<&'static str, u32, ()> = Memo::weighed(4, 100, |value| *value as usize);
        memo.start("first", ());
        memo.start("second", ());
        memo.insert("a", 1);
        memo.insert("b", 1);
        memo.insert("c", 1);
        assert_eq!(memo.len(), 4);
        assert!(memo.holds(&"first") && memo.holds(&"second"));
        assert!(!memo.holds(&"a"));
        // With only reads under way to let go, the oldest goes.
        let mut memo: Memo<&'static str, u32, ()> = Memo::new(2);
        memo.start("first", ());
        memo.start("second", ());
        memo.start("third", ());
        assert!(!memo.holds(&"first"));
        assert!(memo.holds(&"second") && memo.holds(&"third"));
    }

    /// A read under way, held by its askers: dropping every one leaves the memo's handle dead,
    /// as dropping every view's task does a session's read.
    #[derive(Clone)]
    struct Asked(Weak<()>);

    impl Pend for Asked {
        fn wanted(&self) -> bool {
            self.0.strong_count() > 0
        }
    }

    /// A ticket alone: wanted while held.
    impl Pend for u64 {
        fn wanted(&self) -> bool {
            true
        }
    }

    fn ask() -> (Rc<()>, Asked) {
        let asker = Rc::new(());
        let asked = Asked(Rc::downgrade(&asker));
        (asker, asked)
    }

    /// A read every asker dropped holds nothing anyone will use: a lookup finds nothing for
    /// it, so the next ask sends it again, and over the entry cap it goes before any answer,
    /// however old. Reads started and left one after another, as prefetching a neighbour and
    /// moving on, scrolling past rows or typing in a search do, never push out an answer.
    #[test]
    fn a_read_nobody_waits_for_goes_before_any_answer() {
        let mut memo: Memo<&'static str, u32, Asked> = Memo::new(3);
        memo.insert("old", 1);
        let (waiting, asked) = ask();
        memo.start("waited", asked);
        let (left, asked) = ask();
        memo.start("left", asked);
        drop(left);
        assert!(!memo.holds(&"left") && memo.held(&"left").is_none());
        // Over the cap: the read left goes, not the oldest answer, nor the read waited for.
        memo.insert("new", 2);
        assert_eq!(memo.len(), 3);
        assert_eq!(memo.peek(&"old"), Some(&1));
        assert_eq!(memo.peek(&"new"), Some(&2));
        assert!(matches!(memo.held(&"waited"), Some(Held::Pending(_))));
        // Left as well, it goes when looked up.
        drop(waiting);
        assert!(memo.get(&"waited").is_none());
        assert_eq!(memo.len(), 2);
        assert!(memo.pending.is_empty());

        let mut memo: Memo<&'static str, u32, Asked> = Memo::new(2);
        memo.insert("answer", 1);
        for key in ["a", "b", "c", "d", "e"] {
            let (_asker, asked) = ask();
            memo.start(key, asked);
        }
        assert_eq!(memo.peek(&"answer"), Some(&1));
        assert_eq!(memo.len(), 2);
        assert_eq!(memo.pending.len(), 1);
        // A read someone waits for still goes after the answers, and only to the entry cap.
        let (_waiting, asked) = ask();
        memo.start("f", asked);
        let (_waiting_too, asked) = ask();
        memo.start("g", asked);
        assert!(!memo.holds(&"answer"));
        assert!(memo.holds(&"f") && memo.holds(&"g"));
        assert_eq!(memo.pending.len(), 2);
    }

    /// An answer is kept for the read it answers only: a key read again, or forgotten, since
    /// keeps what came after.
    #[test]
    fn an_older_read_never_replaces_a_newer_one() {
        let mut memo: Memo<&'static str, u32, u64> = Memo::new(4);
        memo.start("list", 1);
        // Read again (a Refresh) before the first came back.
        memo.forget(&"list");
        memo.start("list", 2);
        assert!(!memo.finish::<()>(&"list", &Ok(10), |ticket| *ticket == 1));
        assert!(matches!(memo.get(&"list"), Some(Held::Pending(2))));
        assert!(memo.finish::<()>(&"list", &Ok(20), |ticket| *ticket == 2));
        assert!(matches!(memo.get(&"list"), Some(Held::Done(20))));
        // Forgotten while under way: the late answer is dropped.
        memo.start("gone", 3);
        memo.forget(&"gone");
        assert!(!memo.finish::<()>(&"gone", &Ok(1), |_| true));
        assert!(memo.get(&"gone").is_none());
    }
}
