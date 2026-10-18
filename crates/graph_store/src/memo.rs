//! Answers to reads pinned to a commit (constitution II). A commit never changes, so neither
//! does the answer to a read at it: the answer is kept, and an identical read already under way
//! is shared rather than sent again. The table's peek, the tree, the canvas and the table read
//! the same counts and neighbours; each is sent once.
//!
//! Only successful answers are kept; a failure is forgotten so the next read asks again. The
//! memo holds at most `capacity` entries and forgets the least recently used first.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

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
    clock: u64,
    capacity: usize,
    /// The most the answers held may weigh together, and how one is weighed.
    budget: Option<(usize, Weigh<V>)>,
    bytes: usize,
}

impl<K: Hash + Eq + Clone, V: Clone, P: Clone> Memo<K, V, P> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
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

    /// What is held for `key`, marking it used.
    pub(crate) fn get(&mut self, key: &K) -> Option<Held<V, P>> {
        let held = self.entries.get(key)?.held.clone();
        self.touch(key);
        Some(held)
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

    /// Records how a read for `key` ended: an answer is kept, a failure forgotten.
    pub(crate) fn finish<E>(&mut self, key: &K, result: &Result<V, E>) {
        match result {
            // Evicted while under way, the answer is kept as newly used.
            Ok(value) => self.put(key.clone(), Held::Done(value.clone())),
            Err(_) => self.forget(key),
        }
    }

    /// Whether anything is held for `key`, without marking it used.
    pub(crate) fn holds(&self, key: &K) -> bool {
        self.entries.contains_key(key)
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
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
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
        if let Some(old) = self
            .entries
            .insert(key.clone(), Slot { held, weight, used })
        {
            self.bytes -= old.weight;
            self.order.remove(&old.used);
        }
        self.bytes += weight;
        self.order.insert(used, key);
        // The least recently used go first; the answer just kept stays, whatever it weighs.
        let over_budget = |memo: &Self| memo.budget.is_some_and(|(budget, _)| memo.bytes > budget);
        while self.entries.len() > self.capacity || over_budget(self) {
            let Some((&oldest, _)) = self.order.first_key_value() else {
                break;
            };
            if oldest == used {
                break;
            }
            if let Some(gone) = self.order.remove(&oldest)
                && let Some(slot) = self.entries.remove(&gone)
            {
                self.bytes -= slot.weight;
            }
        }
    }

    fn touch(&mut self, key: &K) {
        let Some(slot) = self.entries.get_mut(key) else {
            return;
        };
        self.clock += 1;
        self.order.remove(&slot.used);
        slot.used = self.clock;
        self.order.insert(slot.used, key.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(memo: &mut Memo<&'static str, u32, ()>, key: &'static str) -> Option<u32> {
        match memo.get(&key) {
            Some(Held::Done(value)) => Some(value),
            _ => None,
        }
    }

    #[test]
    fn answers_are_kept_failures_forgotten_and_the_oldest_goes_first() {
        let mut memo: Memo<&'static str, u32, ()> = Memo::new(2);
        memo.start("a", ());
        assert!(matches!(memo.get(&"a"), Some(Held::Pending(()))));
        memo.finish::<()>(&"a", &Ok(1));
        assert_eq!(done(&mut memo, "a"), Some(1));

        memo.start("b", ());
        memo.finish(&"b", &Err::<u32, &str>("refused"));
        assert!(memo.get(&"b").is_none(), "a failure is asked again");

        memo.start("b", ());
        memo.finish::<()>(&"b", &Ok(2));
        // "a" was last used before "b" was added, so it is the least recently used.
        memo.start("c", ());
        memo.finish::<()>(&"c", &Ok(3));
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
        memo.finish::<()>(&"e", &Ok(3));
        assert_eq!(memo.bytes(), 3);
    }
}
