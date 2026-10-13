//! Answers to reads pinned to a commit (constitution II). A commit never changes, so neither
//! does the answer to a read at it: the answer is kept, and an identical read already under way
//! is shared rather than sent again. The table's peek, the tree, the canvas and the table read
//! the same counts and neighbours; each is sent once.
//!
//! Only successful answers are kept; a failure is forgotten so the next read asks again. The
//! memo holds at most `capacity` entries and forgets the least recently used first.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// What the memo holds for a key: an answer, or the read under way (`P`, a shareable future).
#[derive(Clone)]
pub(crate) enum Held<V, P> {
    Done(V),
    Pending(P),
}

pub(crate) struct Memo<K, V, P> {
    entries: HashMap<K, Held<V, P>>,
    /// Keys from least to most recently used.
    order: VecDeque<K>,
    capacity: usize,
}

impl<K: Hash + Eq + Clone, V: Clone, P: Clone> Memo<K, V, P> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// What is held for `key`, marking it used.
    pub(crate) fn get(&mut self, key: &K) -> Option<Held<V, P>> {
        let held = self.entries.get(key)?.clone();
        self.touch(key);
        Some(held)
    }

    /// Records a read under way for `key`.
    pub(crate) fn start(&mut self, key: K, pending: P) {
        self.put(key, Held::Pending(pending));
    }

    /// Records how a read for `key` ended: an answer is kept, a failure forgotten.
    pub(crate) fn finish<E>(&mut self, key: &K, result: &Result<V, E>) {
        match result {
            Ok(value) => {
                if self.entries.contains_key(key) {
                    self.entries.insert(key.clone(), Held::Done(value.clone()));
                } else {
                    // Evicted while under way: keep the answer as newly used.
                    self.put(key.clone(), Held::Done(value.clone()));
                }
            }
            Err(_) => {
                self.entries.remove(key);
                self.order.retain(|k| k != key);
            }
        }
    }

    /// Forgets what is held for `key`: the next read asks again.
    pub(crate) fn forget(&mut self, key: &K) {
        self.entries.remove(key);
        self.order.retain(|k| k != key);
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn put(&mut self, key: K, held: Held<V, P>) {
        self.entries.insert(key.clone(), held);
        self.touch(&key);
        while self.entries.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn touch(&mut self, key: &K) {
        self.order.retain(|k| k != key);
        self.order.push_back(key.clone());
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
}
