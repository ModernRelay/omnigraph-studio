//! What a view has read at the commit it shows (spec 015). Every read a view makes for what it
//! shows goes through its [`ReadScope`], which keeps what each answer depends on
//! (`builder::Depends`) and, for a list, the nodes it listed. From that one record the session
//! asks the change feed about the types the view reads, and decides whether a head move changed
//! anything the view shows; views keep no read set of their own.

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

/// One answer the view shows: what it depends on, and the nodes it listed once it came in
/// (`None` while it is read, or when the answer lists none).
struct Entry {
    depends: Depends,
    listed: Option<Vec<String>>,
}

pub(crate) struct Scope {
    pub(crate) branch: String,
    entries: Vec<Entry>,
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
            generation: 0,
            alive: Rc::downgrade(&alive),
        };
        (scope, ReadScope { id, _alive: alive })
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.strong_count() > 0
    }

    /// A read the view sends: it counts from now, before its answer is in.
    pub(crate) fn add(&mut self, depends: Depends) -> EntryAt {
        self.entries.push(Entry {
            depends,
            listed: None,
        });
        EntryAt {
            generation: self.generation,
            index: self.entries.len() - 1,
        }
    }

    /// An answer the view shows that was read elsewhere (a table's row and relationships, given
    /// to its peek), with the nodes it lists.
    pub(crate) fn add_known(&mut self, depends: Depends, listed: Option<Vec<String>>) {
        self.entries.push(Entry { depends, listed });
    }

    /// The read at `at` came in with `rows`.
    pub(crate) fn answered(&mut self, at: EntryAt, rows: &Value) {
        if at.generation != self.generation {
            return;
        }
        if let Some(entry) = self.entries.get_mut(at.index) {
            entry.listed = listed_ids(&entry.depends, rows);
        }
    }

    /// The view shows none of what it read: it reads again (Refresh, another order).
    pub(crate) fn reset(&mut self, branch: String) {
        self.entries.clear();
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
            .all(|entry| depends_unchanged(since, &entry.depends, entry.listed.as_deref()))
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
        let node_read = scope.add(Depends::Node {
            type_name: "Person".into(),
            id: "p1".into(),
        });
        let list = scope.add(neighbours_of("p1"));
        scope.answered(node_read, &json!([]));
        scope.answered(list, &json!([{ "id": "p2" }]));
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
        scope.add(neighbours_of("p1"));
        let asked = ["Person", "Knows"];
        assert!(
            !scope.unchanged(&since(json!([node("Person", "p9")]), &asked)),
            "any person may be listed"
        );
        assert!(scope.unchanged(&since(json!([]), &asked)));
    }

    #[test]
    fn a_reset_scope_forgets_what_it_read_and_late_answers() {
        let (mut scope, _handle) = Scope::new(1, "main".into());
        let late = scope.add(neighbours_of("p1"));
        scope.reset("main".into());
        scope.answered(late, &json!([{ "id": "p2" }]));
        assert_eq!(scope.names().count(), 0);
        let asked = ["Person", "Knows"];
        assert!(scope.unchanged(&since(json!([node("Person", "p2")]), &asked)));
    }

    #[test]
    fn an_answer_whose_dependence_is_unknown_always_changes() {
        let (mut scope, _handle) = Scope::new(1, "main".into());
        scope.add(Depends::Unknown);
        assert!(!scope.unchanged(&since(json!([]), &["Person"])));
    }
}
