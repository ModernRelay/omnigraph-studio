//! What a branch's commits changed, for the views that read it (spec 015, carry-forward): a
//! view pinned to a commit moves to the branch's new head without reading again when nothing
//! it reads changed in between.
//!
//! The session fills one log per branch from the change feed, asked only about the types open
//! views read, and answers for a range of commits from it. Pure; no GPUI.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

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
                })
                .collect::<Vec<_>>();
            let touched = self.touched.entry(ty).or_insert_with(Touched::none);
            // An edge whose ends the images do not say could be at any node.
            if matches!(change.kind, EntityKind::Edge) && ends.is_empty() {
                *touched = Touched::All;
            } else {
                touched.add(&change.id, ends);
            }
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
    depends_unchanged(since, depends, listed_ids(depends, rows).as_deref())
}

/// The nodes an answer lists, for a dependence that names a column of them; `None`, which
/// counts any change to the listed type, when a row does not name its node.
pub(crate) fn listed_ids(depends: &Depends, rows: &Value) -> Option<Vec<String>> {
    let Depends::EdgesAt {
        listed: Some((_, column)),
        ..
    } = depends
    else {
        return None;
    };
    rows.as_array()?
        .iter()
        .map(|row| row.get(column).and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// Whether `since` left alone an answer that depends on `depends`. `listed` holds the nodes a
/// list's answer named; while it is not known (the list is being read), any change to the
/// listed type counts.
pub(crate) fn depends_unchanged(
    since: &ChangesSince,
    depends: &Depends,
    listed: Option<&[String]>,
) -> bool {
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
            listed: listed_type,
        } => {
            !nodes.iter().any(|node| since.edges_at(edge, node))
                && listed_type.as_ref().is_none_or(|(type_name, _)| {
                    let ty = TypeRef::Node(type_name.clone());
                    match listed {
                        Some(ids) => ids.iter().all(|id| !since.entity_changed(&ty, id)),
                        None => !since.type_changed(&ty),
                    }
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
    /// Where each commit a walk back can meet stands, older lower: every commit recorded, and
    /// the parent of each when nothing ranked it before (the commit a read started after). A
    /// walk back by first parent meets them in falling rank, so it can stop once past the
    /// oldest it looks for.
    ranks: HashMap<String, u64>,
    /// The commits `ranks` holds, lowest first, and the rank the next one takes.
    ranked: VecDeque<String>,
    next_rank: u64,
    /// The commit the next feed read starts after: the newest one recorded, or a head seen
    /// while nothing was recorded.
    pub(crate) last: Option<String>,
}

impl ChangeLog {
    /// A log holding nothing yet, whose next read starts after `commit`.
    pub(crate) fn starting_at(commit: String) -> Self {
        Self {
            last: Some(commit),
            ..Self::default()
        }
    }

    pub(crate) fn holds(&self, commit: &str) -> bool {
        self.commits.contains_key(commit)
    }

    pub(crate) fn record(&mut self, commit: String, logged: Logged) {
        if let Some(parent) = &logged.parent {
            self.rank_next(parent);
        }
        self.rank_next(&commit);
        if self.commits.insert(commit.clone(), logged).is_none() {
            self.order.push_back(commit.clone());
        }
        while self.order.len() > LOG_COMMITS {
            if let Some(oldest) = self.order.pop_front() {
                self.commits.remove(&oldest);
                self.forget_ranks_below(&oldest);
            }
        }
        self.last = Some(commit);
    }

    /// Where `commit` stands among the commits a walk back through the log can meet, older
    /// lower; `None` for one no walk meets.
    pub(crate) fn rank(&self, commit: &str) -> Option<u64> {
        self.ranks.get(commit).copied()
    }

    /// Ranks `commit` above every commit ranked, unless it has a rank.
    fn rank_next(&mut self, commit: &str) {
        if self.ranks.contains_key(commit) {
            return;
        }
        self.ranks.insert(commit.to_owned(), self.next_rank);
        self.ranked.push_back(commit.to_owned());
        self.next_rank += 1;
    }

    /// Forgets the ranks below `gone`'s, a commit the log let go: a walk back ends at it, the
    /// parent of the oldest commit kept, and meets nothing older.
    fn forget_ranks_below(&mut self, gone: &str) {
        let Some(floor) = self.rank(gone) else {
            return;
        };
        while self
            .ranked
            .front()
            .and_then(|front| self.rank(front))
            .is_some_and(|rank| rank < floor)
        {
            if let Some(front) = self.ranked.pop_front() {
                self.ranks.remove(&front);
            }
        }
    }

    /// What the commits after `from` up to and including `to` changed, walking back from `to`
    /// by first parent; `None` when one of them is not in the log.
    pub(crate) fn since(&self, from: &str, to: &str) -> Option<ChangesSince> {
        let from_rank = self.rank(from)?;
        let mut found = None;
        self.since_each(to, |start, range| {
            if start == from {
                found = Some(range.clone());
                return true;
            }
            // Below `from`'s rank, the walk is past it.
            self.rank(start).is_some_and(|rank| rank < from_rank)
        });
        found
    }

    /// One walk back from `to` by first parent, merging each commit once: `visit` hears each
    /// commit the walk reaches, newest first, with what the commits after it up to `to`
    /// changed, and ends the walk by answering `true`. Reading the ranges from several earlier
    /// commits costs one walk, not one each.
    pub(crate) fn since_each(&self, to: &str, mut visit: impl FnMut(&str, &ChangesSince) -> bool) {
        let mut at = to;
        let mut range: Option<ChangesSince> = None;
        for _ in 0..LOG_COMMITS {
            let Some(logged) = self.commits.get(at) else {
                return;
            };
            let range = match &mut range {
                None => range.insert(ChangesSince {
                    asked: (*logged.asked).clone(),
                    touched: logged.touched.clone(),
                }),
                Some(range) => {
                    range.asked.retain(|name| logged.asked.contains(name));
                    for (ty, touched) in &logged.touched {
                        range
                            .touched
                            .entry(ty.clone())
                            .or_insert_with(Touched::none)
                            .merge(touched);
                    }
                    range
                }
            };
            let Some(parent) = logged.parent.as_deref() else {
                return;
            };
            if visit(parent, range) {
                return;
            }
            at = parent;
        }
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

    /// What cannot be told is taken as changed: an edge whose images name no ends could be at
    /// any node, and an answer with a row that does not name its node lists unknown nodes.
    #[test]
    fn what_a_change_or_an_answer_does_not_say_counts_as_changed() {
        let ends_unknown = json!({
            "kind": "edge",
            "type": { "id": "WorksAt", "name": "WorksAt" },
            "id": "e1",
            "op": "delete",
            "before": { "properties": {} },
        });
        let log = log(
            &[page(
                json!([block("c2", "c1", json!([ends_unknown]))]),
                true,
            )],
            &["WorksAt"],
        );
        assert!(log.since("c1", "c2").unwrap().edges_at("WorksAt", "anyone"));

        let depends = Depends::EdgesAt {
            edge: "WorksAt".into(),
            nodes: vec!["p1".into()],
            listed: Some(("Company".into(), "id".into())),
        };
        assert_eq!(
            listed_ids(&depends, &json!([{ "id": "co1" }, { "id": "co2" }])),
            Some(vec!["co1".to_owned(), "co2".to_owned()])
        );
        assert_eq!(
            listed_ids(&depends, &json!([{ "id": "co1" }, { "name": "x" }])),
            None
        );
        assert_eq!(listed_ids(&depends, &json!({ "id": "co1" })), None);
    }

    /// One walk back gives, for each commit it passes, the same range `since` gives from it.
    #[test]
    fn one_walk_reads_the_range_since_each_earlier_commit() {
        let log = log(
            &[page(
                json!([
                    block("c2", "c1", json!([node("Person", "p1")])),
                    block("c3", "c2", json!([])),
                    block("c4", "c3", json!([node("Person", "p2")])),
                ]),
                true,
            )],
            &["Person"],
        );
        let person = TypeRef::Node("Person".into());
        let mut seen = Vec::new();
        log.since_each("c4", |start, range| {
            let alone = log.since(start, "c4").unwrap();
            for id in ["p1", "p2"] {
                assert_eq!(
                    range.entity_changed(&person, id),
                    alone.entity_changed(&person, id)
                );
            }
            seen.push(start.to_owned());
            start == "c2"
        });
        assert_eq!(
            seen,
            ["c3", "c2"],
            "newest first, until the visit ends the walk"
        );
    }

    /// A walk back meets commits in falling rank: each commit recorded ranks above its parent,
    /// the commit the log started after included, and one no walk meets has none. A log that
    /// lets its oldest commits go keeps the rank of the last one gone, the parent of the
    /// oldest kept, and forgets those below it.
    #[test]
    fn commits_rank_in_the_order_a_walk_back_meets_them() {
        let short = log(
            &[page(
                json!([
                    block("c2", "c1", json!([])),
                    block("c3", "c2", json!([])),
                    block("c9", "c7", json!([])),
                ]),
                true,
            )],
            &["Person"],
        );
        let ranks: Vec<u64> = ["c1", "c2", "c3", "c7", "c9"]
            .iter()
            .map(|commit| short.rank(commit).unwrap())
            .collect();
        assert!(ranks.windows(2).all(|pair| pair[0] < pair[1]), "{ranks:?}");
        assert_eq!(short.rank("c0"), None);
        // A start ranked above the commit asked for is not on the walk back from it.
        assert!(short.since("c3", "c2").is_none());

        // One commit more than the log keeps: c2 goes.
        let blocks: Vec<Value> = (2..=LOG_COMMITS + 2)
            .map(|n| block(&format!("c{n}"), &format!("c{}", n - 1), json!([])))
            .collect();
        let long = log(&[page(Value::Array(blocks), true)], &["Person"]);
        assert_eq!(long.rank("c1"), None, "below the commit gone");
        assert!(long.rank("c2").is_some(), "the parent of the oldest kept");
        assert!(long.since("c2", &format!("c{}", LOG_COMMITS + 2)).is_some());
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
