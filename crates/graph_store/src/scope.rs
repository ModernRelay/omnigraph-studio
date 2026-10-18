//! What a view has read at the commit it shows (spec 015). Every read a view makes for what it
//! shows goes through its [`ReadScope`], which keeps what each answer depends on
//! (`builder::Depends`) and, for a list, the nodes it listed. From that one record the session
//! asks the change feed about the types the view reads, and decides whether a head move changed
//! anything the view shows; views keep no read set of their own.

use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use gq_language::builder::Depends;
use serde_json::Value;

use crate::carry::{ChangesSince, TypeRef, depends_unchanged, listed_ids};

/// A view's scope: held by the view, told to the session with each read. Clones are the same
/// scope, which lives while any is held.
#[derive(Clone)]
pub struct ReadScope {
    pub(crate) id: u64,
    _alive: Rc<()>,
}

/// Whether a head move changed what a scope's view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeSince {
    /// The feed is being read for the move; [`crate::SessionEvent::ChangesRead`] follows.
    Pending,
    /// Nothing the view shows changed: it can move to the new commit as it is.
    Unchanged,
    /// Something it shows changed, or the log cannot say: it offers Refresh.
    Changed,
}

/// What the view shows of one dependence: the nodes its answers listed, and its reads still
/// out. While one is, what it will list is not known, so the whole listed type counts.
struct Entry {
    depends: Depends,
    listed: Vec<String>,
    /// `listed` as a set: a table scrolled through thousands of rows lists each page against
    /// them all.
    listed_set: HashSet<String>,
    /// An answer came in (or failed, listing nothing), or a caller gave one with its nodes.
    known: bool,
    /// A caller gave an answer without its nodes: the whole listed type always counts.
    unlisted: bool,
    /// The reads out, each alive while its [`ReadToken`] is: a read that finishes, fails or is
    /// dropped by its view stops counting by itself.
    out: Vec<Weak<()>>,
}

/// Held by a read a scope counts as out, until it finishes or is dropped.
pub(crate) struct ReadToken(#[allow(dead_code)] Rc<()>);

impl Entry {
    /// The nodes listed, when they are all known.
    fn listed(&self) -> Option<&[String]> {
        let settled = self.out.iter().all(|read| read.strong_count() == 0);
        (self.known && !self.unlisted && settled).then_some(self.listed.as_slice())
    }

    fn list(&mut self, ids: Vec<String>) {
        for id in ids {
            if self.listed_set.insert(id.clone()) {
                self.listed.push(id);
            }
        }
    }
}

pub(crate) struct Scope {
    pub(crate) branch: String,
    entries: Vec<Entry>,
    /// Where each dependence is among the entries: the same read made again (a Blob cell
    /// scrolled back into view, a page read again) is one entry, so a scope grows with what the
    /// view shows, not with how often it reads it.
    index: HashMap<Depends, usize>,
    /// Moves when the scope is reset, so a read that comes back afterwards changes nothing.
    generation: u64,
    alive: Weak<()>,
}

/// Where a read's entry is, to fill in its answer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EntryAt {
    generation: u64,
    index: usize,
}

impl Scope {
    pub(crate) fn new(id: u64, branch: String) -> (Self, ReadScope) {
        let alive = Rc::new(());
        let scope = Self {
            branch,
            entries: Vec::new(),
            index: HashMap::new(),
            generation: 0,
            alive: Rc::downgrade(&alive),
        };
        (scope, ReadScope { id, _alive: alive })
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.strong_count() > 0
    }

    /// A read the view sends: it counts from now, before its answer is in, while the token is
    /// held.
    pub(crate) fn add(&mut self, depends: Depends) -> (EntryAt, ReadToken) {
        let index = self.entry(depends);
        let token = Rc::new(());
        let entry = &mut self.entries[index];
        entry.out.retain(|read| read.strong_count() > 0);
        entry.out.push(Rc::downgrade(&token));
        let at = EntryAt {
            generation: self.generation,
            index,
        };
        (at, ReadToken(token))
    }

    /// An answer the view shows that was read elsewhere (a table's row and relationships, given
    /// to its peek), with the nodes it lists (`None` when not known: the whole listed type
    /// counts).
    pub(crate) fn add_known(&mut self, depends: Depends, listed: Option<Vec<String>>) {
        let index = self.entry(depends);
        let entry = &mut self.entries[index];
        match listed {
            Some(ids) => {
                entry.known = true;
                entry.list(ids);
            }
            None => entry.unlisted = true,
        }
    }

    /// The entry of `depends`, made when the view first shows it.
    fn entry(&mut self, depends: Depends) -> usize {
        if let Some(&index) = self.index.get(&depends) {
            return index;
        }
        let index = self.entries.len();
        self.index.insert(depends.clone(), index);
        self.entries.push(Entry {
            depends,
            listed: Vec::new(),
            listed_set: HashSet::new(),
            known: false,
            unlisted: false,
            out: Vec::new(),
        });
        index
    }

    /// The read at `at` came in with `rows`.
    pub(crate) fn answered(&mut self, at: EntryAt, rows: &Value) {
        if at.generation != self.generation {
            return;
        }
        let Some(entry) = self.entries.get_mut(at.index) else {
            return;
        };
        let listed = listed_ids(&entry.depends, rows).unwrap_or_default();
        entry.known = true;
        entry.list(listed);
    }

    /// The read at `at` failed: the view shows nothing it would have listed.
    pub(crate) fn failed(&mut self, at: EntryAt) {
        if at.generation != self.generation {
            return;
        }
        if let Some(entry) = self.entries.get_mut(at.index) {
            entry.known = true;
        }
    }

    /// The view shows none of what it read: it reads again (Refresh, another order).
    pub(crate) fn reset(&mut self, branch: String) {
        self.entries.clear();
        self.index.clear();
        self.generation += 1;
        self.branch = branch;
    }

    /// The type names what the view shows depends on.
    pub(crate) fn names(&self) -> impl Iterator<Item = String> + '_ {
        self.entries
            .iter()
            .flat_map(|entry| depends_types(&entry.depends))
            .map(|ty| ty.name().to_owned())
    }

    /// Whether `since` left everything the view shows as it is.
    pub(crate) fn unchanged(&self, since: &ChangesSince) -> bool {
        self.entries
            .iter()
            .all(|entry| depends_unchanged(since, &entry.depends, entry.listed()))
    }
}

/// The node and edge types an answer depends on.
fn depends_types(depends: &Depends) -> Vec<TypeRef> {
    match depends {
        Depends::Unknown => Vec::new(),
        Depends::Types { nodes, edges } => nodes
            .iter()
            .map(|name| TypeRef::Node(name.clone()))
            .chain(edges.iter().map(|name| TypeRef::Edge(name.clone())))
            .collect(),
        Depends::Node { type_name, .. } => vec![TypeRef::Node(type_name.clone())],
        Depends::EdgesAt { edge, listed, .. } => std::iter::once(TypeRef::Edge(edge.clone()))
            .chain(
                listed
                    .iter()
                    .map(|(type_name, _)| TypeRef::Node(type_name.clone())),
            )
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carry::tests::{block, edge, node, page};
    use crate::carry::{ChangeLog, FeedFold};
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    fn since(changes: Value, asked: &[&str]) -> ChangesSince {
        let mut fold = FeedFold::new(Arc::new(
            asked.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        ));
        fold.page(&page(json!([block("c2", "c1", changes)]), true));
        let mut log = ChangeLog::default();
        for (commit, logged) in fold.complete() {
            log.record(commit, logged);
        }
        log.since("c1", "c2").unwrap()
    }

    fn neighbours_of(id: &str) -> Depends {
        Depends::EdgesAt {
            edge: "Knows".into(),
            nodes: vec![id.into()],
            listed: Some(("Person".into(), "id".into())),
        }
    }

    #[test]
    fn a_scope_is_unchanged_while_nothing_it_shows_changed() {
        let (mut scope, _handle) = Scope::new(1, "main".into());
        let (node_read, node_token) = scope.add(Depends::Node {
            type_name: "Person".into(),
            id: "p1".into(),
        });
        let (list, list_token) = scope.add(neighbours_of("p1"));
        scope.answered(node_read, &json!([]));
        scope.answered(list, &json!([{ "id": "p2" }]));
        drop((node_token, list_token));
        let names: BTreeSet<String> = scope.names().collect();
        assert_eq!(names, BTreeSet::from(["Person".into(), "Knows".into()]));

        let asked = ["Person", "Knows"];
        assert!(scope.unchanged(&since(json!([node("Person", "p9")]), &asked)));
        assert!(
            !scope.unchanged(&since(json!([node("Person", "p1")]), &asked)),
            "the node"
        );
        assert!(
            !scope.unchanged(&since(json!([node("Person", "p2")]), &asked)),
            "a listed neighbour"
        );
        assert!(
            !scope.unchanged(&since(json!([edge("Knows", "e9", "p3", "p1")]), &asked)),
            "an edge at the node"
        );
        assert!(
            !scope.unchanged(&since(json!([]), &["Person"])),
            "a type the feed was not asked about"
        );
    }

    #[test]
    fn a_list_still_being_read_counts_its_whole_type() {
        let (mut scope, _handle) = Scope::new(1, "main".into());
        let (_, token) = scope.add(neighbours_of("p1"));
        let asked = ["Person", "Knows"];
        assert!(
            !scope.unchanged(&since(json!([node("Person", "p9")]), &asked)),
            "any person may be listed"
        );
        assert!(scope.unchanged(&since(json!([]), &asked)));
        drop(token);
    }

    /// A read that failed, or that its view dropped, is not out any more: one hub that timed
    /// out does not make every change to its type count for the view.
    #[test]
    fn a_failed_or_dropped_read_stops_counting_as_out() {
        let (mut scope, _handle) = Scope::new(1, "main".into());
        let asked = ["Person", "Knows"];
        let p9 = || since(json!([node("Person", "p9")]), &asked);
        let (failed, token) = scope.add(neighbours_of("p1"));
        scope.failed(failed);
        drop(token);
        assert!(scope.unchanged(&p9()), "it listed nothing");
        // A second read, answered, then a third dropped before its answer.
        let (read, token) = scope.add(neighbours_of("p1"));
        scope.answered(read, &json!([{ "id": "p2" }]));
        drop(token);
        let (_, dropped) = scope.add(neighbours_of("p1"));
        assert!(
            !scope.unchanged(&p9()),
            "while it is out, any person may be listed"
        );
        drop(dropped);
        assert!(scope.unchanged(&p9()));
        assert!(!scope.unchanged(&since(json!([node("Person", "p2")]), &asked)));
    }

    #[test]
    fn a_list_read_twice_is_one_entry_listing_both_answers() {
        let (mut scope, _handle) = Scope::new(1, "main".into());
        let (first, token) = scope.add(neighbours_of("p1"));
        scope.answered(first, &json!([{ "id": "p2" }]));
        drop(token);
        let asked = ["Person", "Knows"];
        // The next page is out: what it lists is not known yet.
        let (next, token) = scope.add(neighbours_of("p1"));
        assert_eq!(scope.entries.len(), 1);
        assert!(!scope.unchanged(&since(json!([node("Person", "p9")]), &asked)));
        scope.answered(next, &json!([{ "id": "p3" }]));
        drop(token);
        assert!(scope.unchanged(&since(json!([node("Person", "p9")]), &asked)));
        assert!(!scope.unchanged(&since(json!([node("Person", "p2")]), &asked)));
        assert!(!scope.unchanged(&since(json!([node("Person", "p3")]), &asked)));
    }

    #[test]
    fn a_reset_scope_forgets_what_it_read_and_late_answers() {
        let (mut scope, _handle) = Scope::new(1, "main".into());
        let (late, _token) = scope.add(neighbours_of("p1"));
        scope.reset("main".into());
        scope.answered(late, &json!([{ "id": "p2" }]));
        assert_eq!(scope.names().count(), 0);
        let asked = ["Person", "Knows"];
        assert!(scope.unchanged(&since(json!([node("Person", "p2")]), &asked)));
    }

    #[test]
    fn an_answer_whose_dependence_is_unknown_always_changes() {
        let (mut scope, _handle) = Scope::new(1, "main".into());
        let _read = scope.add(Depends::Unknown);
        assert!(!scope.unchanged(&since(json!([]), &["Person"])));
    }
}
