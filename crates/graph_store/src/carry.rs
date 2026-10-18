//! What a branch's commits changed, for the views that read it (spec 015, carry-forward): a
//! view pinned to a commit moves to the branch's new head without reading again when nothing
//! it reads changed in between.
//!
//! The session fills one log per branch from the change feed, asked only about the types open
//! views read, and answers for a range of commits from it. Pure; no GPUI.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use gq_language::SchemaModel;
use gq_language::builder::Depends;
use graph_client::api::{ChangeBlockOutput, ChangeFeedOutput, EntityKind};
use serde_json::Value;

/// A node or edge type. The two are separate namespaces: a schema may name both alike.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TypeRef {
    Node(String),
    Edge(String),
}

impl TypeRef {
    pub fn name(&self) -> &str {
        match self {
            Self::Node(name) | Self::Edge(name) => name,
        }
    }
}

/// What a node's page, or a table of a node type, reads: the type, and through each of its
/// relationship groups the edge type and the type at the other end.
pub fn node_reads(schema: &SchemaModel, node_type: &str) -> BTreeSet<TypeRef> {
    let mut types = BTreeSet::from([TypeRef::Node(node_type.to_owned())]);
    for group in schema.relationships_of(node_type) {
        types.insert(TypeRef::Edge(group.edge));
        types.insert(TypeRef::Node(group.other_type));
    }
    types
}

/// What a view of an edge type reads: the edge type and the node types at its ends.
pub fn edge_reads(schema: &SchemaModel, edge_type: &str) -> BTreeSet<TypeRef> {
    let mut types = BTreeSet::from([TypeRef::Edge(edge_type.to_owned())]);
    if let Some(edge) = schema.edge(edge_type) {
        types.insert(TypeRef::Node(edge.from.clone()));
        types.insert(TypeRef::Node(edge.to.clone()));
    }
    types
}

/// The most ids kept for one type over a range; past it every entity of the type counts as
/// changed.
const IDS_PER_TYPE: usize = 1_000;

/// The most commits a branch's log keeps.
const LOG_COMMITS: usize = 500;

/// What commits wrote to one type.
#[derive(Debug, Clone)]
enum Touched {
    /// Too many to list: any entity of the type may have changed.
    All,
    /// The entities written, and for edges the nodes at either end, before and after.
    Ids {
        ids: HashSet<String>,
        ends: HashSet<String>,
    },
}

impl Touched {
    fn none() -> Self {
        Self::Ids {
            ids: HashSet::new(),
            ends: HashSet::new(),
        }
    }

    fn add(&mut self, id: &str, ends: impl IntoIterator<Item = String>) {
        let Self::Ids { ids, ends: at } = self else {
            return;
        };
        ids.insert(id.to_owned());
        at.extend(ends);
        if ids.len() > IDS_PER_TYPE || at.len() > 2 * IDS_PER_TYPE {
            *self = Self::All;
        }
    }

    fn merge(&mut self, other: &Touched) {
        match other {
            Self::All => *self = Self::All,
            Self::Ids { ids, ends } => {
                for id in ids {
                    self.add(id, None);
                }
                if let Self::Ids { ends: at, .. } = self {
                    at.extend(ends.iter().cloned());
                    if at.len() > 2 * IDS_PER_TYPE {
                        *self = Self::All;
                    }
                }
            }
        }
    }
}

/// What one commit wrote, among the type names the feed was asked about.
#[derive(Debug, Clone)]
pub(crate) struct Logged {
    parent: Option<String>,
    /// The type names the feed was asked about: changes to any other type are not known.
    asked: Arc<BTreeSet<String>>,
    touched: HashMap<TypeRef, Touched>,
}

impl Logged {
    fn new(parent: Option<String>, asked: Arc<BTreeSet<String>>) -> Self {
        Self {
            parent,
            asked,
            touched: HashMap::new(),
        }
    }

    fn add(&mut self, block: &ChangeBlockOutput) {
        for change in &block.changes {
            let ty = match change.kind {
                EntityKind::Node => TypeRef::Node(change.entity_type.name.clone()),
                EntityKind::Edge => TypeRef::Edge(change.entity_type.name.clone()),
            };
            let ends = [&change.before, &change.after]
                .into_iter()
                .flatten()
                .filter_map(|image| image.endpoints.as_ref())
                .flat_map(|endpoints| {
                    ["from", "to"]
                        .into_iter()
                        .filter_map(|end| endpoints.get(end).and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                });
            self.touched
                .entry(ty)
                .or_insert_with(Touched::none)
                .add(&change.id, ends);
        }
    }
}

/// What a range of commits wrote to the types the feed was asked about in each of them.
#[derive(Debug, Clone)]
pub struct ChangesSince {
    /// The names asked about in every commit of the range.
    asked: BTreeSet<String>,
    touched: HashMap<TypeRef, Touched>,
}

impl ChangesSince {
    fn asked(&self, ty: &TypeRef) -> bool {
        self.asked.contains(ty.name())
    }

    /// Whether the range may have written any entity of `ty`; true when the feed was not
    /// asked about it.
    pub fn type_changed(&self, ty: &TypeRef) -> bool {
        !self.asked(ty) || self.touched.contains_key(ty)
    }

    /// Whether the range may have written any of `types`.
    pub fn any_changed<'a>(&self, types: impl IntoIterator<Item = &'a TypeRef>) -> bool {
        types.into_iter().any(|ty| self.type_changed(ty))
    }

    /// Whether the range may have written entity `id` of `ty`.
    pub fn entity_changed(&self, ty: &TypeRef, id: &str) -> bool {
        if !self.asked(ty) {
            return true;
        }
        match self.touched.get(ty) {
            None => false,
            Some(Touched::All) => true,
            Some(Touched::Ids { ids, .. }) => ids.contains(id),
        }
    }

    /// Whether the range may have written an edge of `edge_type` with node `id` at either end.
    pub fn edges_at(&self, edge_type: &str, id: &str) -> bool {
        let ty = TypeRef::Edge(edge_type.to_owned());
        if !self.asked(&ty) {
            return true;
        }
        match self.touched.get(&ty) {
            None => false,
            Some(Touched::All) => true,
            Some(Touched::Ids { ends, .. }) => ends.contains(id),
        }
    }
}

/// Whether `since` left alone an answer that depends on `depends` and holds `rows`.
pub fn answer_unchanged(since: &ChangesSince, depends: &Depends, rows: &Value) -> bool {
    match depends {
        Depends::Unknown => false,
        Depends::Types { nodes, edges } => {
            let types = nodes
                .iter()
                .map(|name| TypeRef::Node(name.clone()))
                .chain(edges.iter().map(|name| TypeRef::Edge(name.clone())));
            !since.any_changed(&types.collect::<Vec<_>>())
        }
        Depends::Node { type_name, id } => {
            !since.entity_changed(&TypeRef::Node(type_name.clone()), id)
        }
        Depends::EdgesAt {
            edge,
            nodes,
            listed,
        } => {
            !nodes.iter().any(|node| since.edges_at(edge, node))
                && listed.as_ref().is_none_or(|(type_name, column)| {
                    let ty = TypeRef::Node(type_name.clone());
                    rows.as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|row| row.get(column).and_then(Value::as_str))
                        .all(|id| !since.entity_changed(&ty, id))
                })
        }
    }
}

/// The answer for a range of a branch's commits.
#[derive(Debug, Clone)]
pub enum Since {
    /// The feed is being read for the newest of them; [`crate::SessionEvent::ChangesRead`]
    /// follows.
    Pending,
    /// Some commit in the range is not in the log, or wrote what the log could not keep: the
    /// view reads again.
    Unknown,
    Known(ChangesSince),
}

/// One branch's commits as the feed described them, newest last.
#[derive(Debug, Default)]
pub(crate) struct ChangeLog {
    commits: HashMap<String, Logged>,
    order: VecDeque<String>,
    /// The commit the next feed read starts after: the newest one recorded, or a head seen
    /// while nothing was recorded.
    pub(crate) last: Option<String>,
}

impl ChangeLog {
    pub(crate) fn holds(&self, commit: &str) -> bool {
        self.commits.contains_key(commit)
    }

    pub(crate) fn record(&mut self, commit: String, logged: Logged) {
        if self.commits.insert(commit.clone(), logged).is_none() {
            self.order.push_back(commit.clone());
        }
        while self.order.len() > LOG_COMMITS {
            if let Some(oldest) = self.order.pop_front() {
                self.commits.remove(&oldest);
            }
        }
        self.last = Some(commit);
    }

    /// What the commits after `from` up to and including `to` changed, walking back from `to`
    /// by first parent; `None` when one of them is not in the log.
    pub(crate) fn since(&self, from: &str, to: &str) -> Option<ChangesSince> {
        let mut at = to;
        let mut range: Option<ChangesSince> = None;
        for _ in 0..LOG_COMMITS {
            let logged = self.commits.get(at)?;
            match &mut range {
                None => {
                    range = Some(ChangesSince {
                        asked: (*logged.asked).clone(),
                        touched: logged.touched.clone(),
                    })
                }
                Some(range) => {
                    range.asked.retain(|name| logged.asked.contains(name));
                    for (ty, touched) in &logged.touched {
                        range
                            .touched
                            .entry(ty.clone())
                            .or_insert_with(Touched::none)
                            .merge(touched);
                    }
                }
            }
            match logged.parent.as_deref() {
                Some(parent) if parent == from => return range,
                Some(parent) => at = parent,
                None => return None,
            }
        }
        None
    }
}

/// The complete commits of one feed read, gathered over its pages: a page may end partway
/// through a commit, which the next page continues.
pub(crate) struct FeedFold {
    asked: Arc<BTreeSet<String>>,
    done: Vec<(String, Logged)>,
    open: Option<(String, Logged)>,
}

impl FeedFold {
    pub(crate) fn new(asked: Arc<BTreeSet<String>>) -> Self {
        Self {
            asked,
            done: Vec::new(),
            open: None,
        }
    }

    pub(crate) fn page(&mut self, page: &ChangeFeedOutput) {
        for block in &page.blocks {
            let commit = &block.cause.graph_commit_id;
            if self.open.as_ref().is_none_or(|(open, _)| open != commit) {
                self.done.extend(self.open.take());
                self.open = Some((
                    commit.clone(),
                    Logged::new(block.cause.parent_commit_id.clone(), self.asked.clone()),
                ));
            }
            if let Some((_, logged)) = &mut self.open {
                logged.add(block);
            }
        }
        // A terminal page ends at a commit's end.
        if page.cursor.is_some() {
            self.done.extend(self.open.take());
        }
    }

    /// The commits read whole, oldest first; a commit the read stopped inside is left out.
    pub(crate) fn complete(self) -> Vec<(String, Logged)> {
        self.done
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn page(blocks: Value, cursor: bool) -> ChangeFeedOutput {
        serde_json::from_value(json!({
            "blocks": blocks,
            "cursor": cursor.then_some("c"),
            "next_page_token": (!cursor).then_some("t"),
            "caught_up": cursor.then_some(true),
        }))
        .unwrap()
    }

    pub(crate) fn block(commit: &str, parent: &str, changes: Value) -> Value {
        json!({
            "cause": {
                "graph_commit_id": commit,
                "parent_commit_id": parent,
                "authored_branch": "main",
                "authored_at": 0,
            },
            "changes": changes,
        })
    }

    pub(crate) fn node(ty: &str, id: &str) -> Value {
        json!({
            "kind": "node",
            "type": { "id": ty, "name": ty },
            "id": id,
            "op": "update",
            "before": { "properties": {} },
            "after": { "properties": {} },
        })
    }

    pub(crate) fn edge(ty: &str, id: &str, from: &str, to: &str) -> Value {
        json!({
            "kind": "edge",
            "type": { "id": ty, "name": ty },
            "id": id,
            "op": "insert",
            "after": { "properties": {}, "endpoints": { "from": from, "to": to } },
        })
    }

    fn log(pages: &[ChangeFeedOutput], asked: &[&str]) -> ChangeLog {
        let mut fold = FeedFold::new(Arc::new(asked.iter().map(|s| s.to_string()).collect()));
        for page in pages {
            fold.page(page);
        }
        let mut log = ChangeLog::default();
        for (commit, logged) in fold.complete() {
            log.record(commit, logged);
        }
        log
    }

    #[test]
    fn a_range_knows_the_types_and_ids_its_commits_wrote() {
        let log = log(
            &[page(
                json!([
                    block("c2", "c1", json!([node("Person", "p1")])),
                    block("c3", "c2", json!([])),
                    block("c4", "c3", json!([edge("WorksAt", "e1", "p2", "co1")])),
                ]),
                true,
            )],
            &["Person", "WorksAt", "Company"],
        );
        let since = log.since("c1", "c4").unwrap();
        let person = TypeRef::Node("Person".into());
        assert!(since.type_changed(&person));
        assert!(since.entity_changed(&person, "p1"));
        assert!(!since.entity_changed(&person, "p2"));
        assert!(!since.type_changed(&TypeRef::Node("Company".into())));
        assert!(since.edges_at("WorksAt", "p2"));
        assert!(since.edges_at("WorksAt", "co1"));
        assert!(!since.edges_at("WorksAt", "p1"));
        // A type the feed was not asked about may have changed.
        assert!(since.type_changed(&TypeRef::Node("Project".into())));
        // Only the commits after the start count.
        let later = log.since("c2", "c4").unwrap();
        assert!(!later.type_changed(&person));
        // A start the log cannot reach is not an answer.
        assert!(log.since("c0", "c4").is_none());
    }

    #[test]
    fn a_commit_split_over_pages_counts_once_and_whole() {
        let log = log(
            &[
                page(
                    json!([
                        block("c2", "c1", json!([])),
                        block("c3", "c2", json!([node("Person", "p1")])),
                    ]),
                    false,
                ),
                page(
                    json!([block("c3", "c2", json!([node("Person", "p2")]))]),
                    true,
                ),
            ],
            &["Person"],
        );
        let since = log.since("c1", "c3").unwrap();
        let person = TypeRef::Node("Person".into());
        assert!(since.entity_changed(&person, "p1"));
        assert!(since.entity_changed(&person, "p2"));
    }

    #[test]
    fn a_commit_the_read_stopped_inside_is_left_out() {
        let log = log(
            &[page(
                json!([
                    block("c2", "c1", json!([])),
                    block("c3", "c2", json!([node("Person", "p1")])),
                ]),
                false,
            )],
            &["Person"],
        );
        assert!(log.since("c1", "c2").is_some());
        assert!(log.since("c1", "c3").is_none());
    }

    #[test]
    fn names_asked_about_in_only_some_commits_are_not_known() {
        let mut log = log(
            &[page(json!([block("c2", "c1", json!([]))]), true)],
            &["Person", "Company"],
        );
        let mut fold = FeedFold::new(Arc::new(BTreeSet::from(["Person".to_owned()])));
        fold.page(&page(json!([block("c3", "c2", json!([]))]), true));
        for (commit, logged) in fold.complete() {
            log.record(commit, logged);
        }
        let since = log.since("c1", "c3").unwrap();
        assert!(!since.type_changed(&TypeRef::Node("Person".into())));
        assert!(since.type_changed(&TypeRef::Node("Company".into())));
    }

    #[test]
    fn past_the_id_cap_a_type_counts_as_changed_throughout() {
        let changes: Vec<Value> = (0..=IDS_PER_TYPE)
            .map(|i| node("Person", &format!("p{i}")))
            .collect();
        let log = log(
            &[page(
                json!([block("c2", "c1", Value::Array(changes))]),
                true,
            )],
            &["Person"],
        );
        let since = log.since("c1", "c2").unwrap();
        assert!(since.entity_changed(&TypeRef::Node("Person".into()), "anyone"));
    }
}
