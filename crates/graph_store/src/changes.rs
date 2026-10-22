//! Change folding shared by review and history (spec 008, 009): the fork of a branch, the
//! per-entity fold of change-feed entries (first before, last after), per-type counts and the
//! summary sentence. Pure functions; no GPUI.
//!
//! Review folds the branch's change feed after the merge base (E7). When the feed refuses the
//! base as a start, because the target was merged into the branch, review folds the net diff
//! from the merge base instead, and finds the branch's commits since its fork from both
//! branches' `GET /commits`, read a page at a time ([`ForkSearch`]).

use std::collections::{BTreeSet, HashMap, HashSet};

use gq_language::SchemaModel;
use graph_client::api::{
    ChangeBlockOutput, ChangeCauseOutput, ChangeImageOutput, CommitOutput, EntityChangeOutput,
    EntityKind,
};
use serde_json::Value;

/// The most changes a review folds before it says "Partial" (spec 008).
pub const REVIEW_CHANGE_CAP: usize = 20_000;

/// Where a branch forked from its target, found while both histories are read a page at a
/// time, newest first, as `GET /commits?branch=` lists them: the newest commit of the branch's
/// first-parent chain that the target's history holds, or a commit known to be in it
/// ([`Self::hold`], the merge base the server names).
///
/// A target's listing holds only its own first-parent chain: a merged branch's commits appear
/// there as some commit's `merged_parent_commit_id` (a fast-forward too publishes a new commit
/// with the branch head as merged parent), so a merged parent counts as held. A branch's chain
/// is a first-parent chain, so once one of its commits is in the target, every older one is.
/// The search keeps each side's ids as they are read, so a page costs only the commits on it.
#[derive(Debug, Default)]
pub struct ForkSearch {
    /// The branch's chain as far as it was read, newest first.
    chain: Vec<CommitOutput>,
    /// Each commit of `chain` by its place there.
    places: HashMap<String, usize>,
    /// The commits the target's pages list, the merged parents they name, and those held.
    held: HashSet<String>,
    /// The place in `chain` of its newest commit `held` holds.
    fork: Option<usize>,
}

impl ForkSearch {
    /// Takes the branch's next page, below the commits taken before.
    pub fn branch_page(&mut self, page: impl IntoIterator<Item = CommitOutput>) {
        for commit in page {
            let at = self.chain.len();
            if self.fork.is_none() && self.held.contains(&commit.graph_commit_id) {
                self.fork = Some(at);
            }
            self.places
                .entry(commit.graph_commit_id.clone())
                .or_insert(at);
            self.chain.push(commit);
        }
    }

    /// Takes the target's next page.
    pub fn target_page(&mut self, page: &[CommitOutput]) {
        for commit in page {
            self.hold(&commit.graph_commit_id);
            if let Some(merged) = &commit.merged_parent_commit_id {
                self.hold(merged);
            }
        }
    }

    /// Takes `id` as a commit the target's history holds.
    pub fn hold(&mut self, id: &str) {
        if let Some(&at) = self.places.get(id) {
            self.fork = Some(self.fork.map_or(at, |fork| fork.min(at)));
        }
        self.held.insert(id.to_owned());
    }

    /// The fork, once the pages read reach it.
    pub fn fork(&self) -> Option<&CommitOutput> {
        self.fork.map(|at| &self.chain[at])
    }

    /// The branch's commits newer than the fork, newest first: every commit read until the
    /// pages reach it.
    pub fn since(&self) -> &[CommitOutput] {
        &self.chain[..self.fork.unwrap_or(self.chain.len())]
    }
}

/// Whether `target`'s history shows `head` merged: listed itself, or the merged parent of one
/// of its commits.
pub fn contains_merge_of(target: &[CommitOutput], head: &str) -> bool {
    target
        .iter()
        .any(|c| c.graph_commit_id == head || c.merged_parent_commit_id.as_deref() == Some(head))
}

/// What an entity's folded change amounts to. The order is the display order: changes to
/// existing data first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NetChange {
    Changed,
    Removed,
    Added,
}

/// One property's value on each side of a folded change. `None` means the side has no value:
/// the entity did not exist there, or the property was outside that commit's schema.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyDiff {
    pub name: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

/// Every change to one entity, folded: the image before its first change and after its last.
#[derive(Debug, Clone)]
pub struct FoldedChange {
    pub kind: EntityKind,
    /// The type's stable id; names can change across renames.
    pub type_id: String,
    pub type_name: String,
    pub id: String,
    pub before: Option<ChangeImageOutput>,
    pub after: Option<ChangeImageOutput>,
    /// The commits that touched the entity, in feed order.
    pub commits: Vec<String>,
}

impl FoldedChange {
    pub fn is_edge(&self) -> bool {
        matches!(self.kind, EntityKind::Edge)
    }

    /// `None` when the entity ends where it began (inserted then deleted, or changed back).
    pub fn net(&self) -> Option<NetChange> {
        match (&self.before, &self.after) {
            (None, None) => None,
            (None, Some(_)) => Some(NetChange::Added),
            (Some(_), None) => Some(NetChange::Removed),
            (Some(before), Some(after)) => {
                if before.properties == after.properties && before.endpoints == after.endpoints {
                    None
                } else {
                    Some(NetChange::Changed)
                }
            }
        }
    }

    /// The properties a card shows: for a changed entity only those whose value differs; for
    /// an added or removed one every property with a value. Ordered by `order` (the schema's
    /// property order), then by name.
    pub fn property_diffs(&self, order: &[String]) -> Vec<PropertyDiff> {
        let empty = serde_json::Map::new();
        let before = self.before.as_ref().map(|i| &i.properties);
        let after = self.after.as_ref().map(|i| &i.properties);
        let mut names: BTreeSet<&String> = BTreeSet::new();
        names.extend(before.unwrap_or(&empty).keys());
        names.extend(after.unwrap_or(&empty).keys());
        let net = self.net();
        let mut diffs: Vec<PropertyDiff> = names
            .into_iter()
            .filter_map(|name| {
                let b = before.and_then(|p| p.get(name)).cloned();
                let a = after.and_then(|p| p.get(name)).cloned();
                let keep = match net {
                    Some(NetChange::Changed) => b != a,
                    Some(NetChange::Added) => a.as_ref().is_some_and(|v| !v.is_null()),
                    Some(NetChange::Removed) => b.as_ref().is_some_and(|v| !v.is_null()),
                    None => false,
                };
                keep.then(|| PropertyDiff {
                    name: name.clone(),
                    before: b,
                    after: a,
                })
            })
            .collect();
        let rank = |name: &str| order.iter().position(|n| n == name).unwrap_or(usize::MAX);
        diffs.sort_by(|x, y| rank(&x.name).cmp(&rank(&y.name)).then(x.name.cmp(&y.name)));
        diffs
    }

    /// The rows a change card shows: [`Self::property_diffs`] in the schema's property order,
    /// without a node's key, which the card's header already names.
    pub fn card_diffs(&self, schema: Option<&SchemaModel>) -> Vec<PropertyDiff> {
        let (order, key): (Vec<String>, Option<&str>) = match schema {
            Some(schema) if self.is_edge() => (
                schema
                    .edge(&self.type_name)
                    .map(|e| e.properties.iter().map(|p| p.name.clone()).collect())
                    .unwrap_or_default(),
                None,
            ),
            Some(schema) => match schema.node(&self.type_name) {
                Some(node) => (
                    node.properties.iter().map(|p| p.name.clone()).collect(),
                    node.key.as_deref(),
                ),
                None => (Vec::new(), None),
            },
            None => (Vec::new(), None),
        };
        let mut diffs = self.property_diffs(&order);
        diffs.retain(|diff| Some(diff.name.as_str()) != key);
        diffs
    }

    /// An edge's endpoints, `(from, to)`, from its latest image.
    pub fn endpoints(&self) -> Option<(String, String)> {
        let image = self.after.as_ref().or(self.before.as_ref())?;
        endpoints_of(image.endpoints.as_ref()?)
    }

    /// An edge whose ends differ between its first before image and its last after image,
    /// `(before, after)`, each `(from, to)`: a load that upserts an edge by its id with another
    /// `src` or `dst` moves it, and the change feed's two images name the two ends. Such an
    /// edge is changed ([`Self::net`]) even when no property is, so its card names the ends.
    pub fn moved_ends(&self) -> Option<((String, String), (String, String))> {
        if !self.is_edge() {
            return None;
        }
        let before = endpoints_of(self.before.as_ref()?.endpoints.as_ref()?)?;
        let after = endpoints_of(self.after.as_ref()?.endpoints.as_ref()?)?;
        (before != after).then_some((before, after))
    }

    /// A key that identifies the entity across refreshes of the same review.
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}",
            if self.is_edge() { "edge" } else { "node" },
            self.type_id,
            self.id
        )
    }
}

/// `(from, to)` from a change image's endpoints.
pub(crate) fn endpoints_of(value: &Value) -> Option<(String, String)> {
    let text = |key: &str| match value.get(key)? {
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    };
    Some((text("from")?, text("to")?))
}

/// Folds change-feed entries per entity as they arrive.
#[derive(Debug, Clone, Default)]
pub struct ChangeFold {
    entries: Vec<FoldedChange>,
    index: HashMap<(bool, String, String), usize>,
    raw_changes: usize,
}

impl ChangeFold {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many feed entries have been folded.
    pub fn raw_changes(&self) -> usize {
        self.raw_changes
    }

    /// Adds every change in `block`.
    pub fn add_block(&mut self, block: ChangeBlockOutput) {
        self.add_block_capped(block, usize::MAX);
    }

    /// Adds every change in `block` until `cap` feed entries have been folded. Returns `false`
    /// when the cap cut the block short.
    pub fn add_block_capped(&mut self, block: ChangeBlockOutput, cap: usize) -> bool {
        let ChangeBlockOutput { cause, changes } = block;
        let mut complete = true;
        for change in changes {
            if self.raw_changes >= cap {
                complete = false;
                break;
            }
            self.add(&cause, change);
        }
        complete
    }

    pub fn add(&mut self, cause: &ChangeCauseOutput, change: EntityChangeOutput) {
        self.raw_changes += 1;
        let EntityChangeOutput {
            kind,
            entity_type,
            id,
            op: _,
            before,
            after,
        } = change;
        let key = (
            matches!(kind, EntityKind::Edge),
            entity_type.id.clone(),
            id.clone(),
        );
        match self.index.get(&key) {
            Some(&ix) => {
                let entry = &mut self.entries[ix];
                entry.after = after;
                entry.type_name = entity_type.name;
                if entry.commits.last() != Some(&cause.graph_commit_id) {
                    entry.commits.push(cause.graph_commit_id.clone());
                }
            }
            None => {
                self.index.insert(key, self.entries.len());
                self.entries.push(FoldedChange {
                    kind,
                    type_id: entity_type.id,
                    type_name: entity_type.name,
                    id,
                    before,
                    after,
                    commits: vec![cause.graph_commit_id.clone()],
                });
            }
        }
    }

    /// The net changes in display order: grouped by type, node types first, groups with
    /// changes to existing data before groups of additions only; inside a group changed, then
    /// removed, then added, each in feed order.
    pub fn finish(self) -> FoldedChanges {
        let mut changes: Vec<(NetChange, FoldedChange)> = self
            .entries
            .into_iter()
            .filter_map(|change| change.net().map(|net| (net, change)))
            .collect();
        // Rank each type by the most "existing data" change it holds.
        let mut type_rank: HashMap<(bool, String), (NetChange, usize)> = HashMap::new();
        for (position, (net, change)) in changes.iter().enumerate() {
            let rank = type_rank
                .entry((change.is_edge(), change.type_id.clone()))
                .or_insert((*net, position));
            rank.0 = rank.0.min(*net);
        }
        changes.sort_by_key(|(net, change)| {
            let (best, first_seen) = type_rank[&(change.is_edge(), change.type_id.clone())];
            (change.is_edge(), best == NetChange::Added, first_seen, *net)
        });
        FoldedChanges {
            changes: changes.into_iter().map(|(_, c)| c).collect(),
            raw_changes: self.raw_changes,
        }
    }
}

/// The result of a fold, in display order.
#[derive(Debug, Clone, Default)]
pub struct FoldedChanges {
    pub changes: Vec<FoldedChange>,
    pub raw_changes: usize,
}

/// Added, changed and removed entities of one type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeCounts {
    pub is_edge: bool,
    pub type_name: String,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
}

impl FoldedChanges {
    /// Counts per type, in the order the cards appear.
    pub fn counts(&self) -> Vec<TypeCounts> {
        let mut counts: Vec<TypeCounts> = Vec::new();
        let mut index: HashMap<(bool, &str), usize> = HashMap::new();
        for change in &self.changes {
            let key = (change.is_edge(), change.type_id.as_str());
            let ix = *index.entry(key).or_insert_with(|| {
                counts.push(TypeCounts {
                    is_edge: change.is_edge(),
                    type_name: change.type_name.clone(),
                    added: 0,
                    changed: 0,
                    removed: 0,
                });
                counts.len() - 1
            });
            match change.net() {
                Some(NetChange::Added) => counts[ix].added += 1,
                Some(NetChange::Changed) => counts[ix].changed += 1,
                Some(NetChange::Removed) => counts[ix].removed += 1,
                None => {}
            }
        }
        counts
    }

    pub fn entity_count(&self) -> usize {
        self.changes.iter().filter(|c| !c.is_edge()).count()
    }

    pub fn relationship_count(&self) -> usize {
        self.changes.iter().filter(|c| c.is_edge()).count()
    }
}

/// "1 draft", "3 drafts", "1,200 rows": a count, its digits grouped, and the word for it, as
/// Studio writes every count.
pub fn plural(count: usize, one: &str, many: &str) -> String {
    let word = if count == 1 { one } else { many };
    format!("{} {word}", studio_ui::group_digits(count))
}

/// The review header's sentence: "17 entities and 22 relationships changed in 3 commits by
/// act-dev".
///
/// `commits` is `None` when the commits are not all known (a history read only so far): the
/// sentence then names neither a count nor who made them.
pub fn summary_sentence(
    entities: usize,
    relationships: usize,
    commits: Option<usize>,
    actors: &[String],
) -> String {
    let what = match (entities, relationships) {
        (0, 0) => return "Nothing changed".into(),
        (e, 0) => plural(e, "entity", "entities"),
        (0, r) => plural(r, "relationship", "relationships"),
        (e, r) => format!(
            "{} and {}",
            plural(e, "entity", "entities"),
            plural(r, "relationship", "relationships")
        ),
    };
    let Some(commits) = commits else {
        return format!("{what} changed");
    };
    let mut sentence = format!("{what} changed in {}", plural(commits, "commit", "commits"));
    match actors {
        [] => {}
        [one] => sentence.push_str(&format!(" by {one}")),
        [one, two] => sentence.push_str(&format!(" by {one} and {two}")),
        many => sentence.push_str(&format!(" by {} people", many.len())),
    }
    sentence
}

#[cfg(test)]
mod tests {
    use graph_client::api::{ChangeOp, ChangeTypeOutput};
    use serde_json::json;

    use super::*;

    fn commit(id: &str, parent: Option<&str>) -> CommitOutput {
        CommitOutput {
            graph_commit_id: id.into(),
            graph_manifest_version: 0,
            created_at: 0,
            graph_branch: None,
            parent_commit_id: parent.map(Into::into),
            merged_parent_commit_id: None,
            actor_id: None,
            kind: graph_client::api::CommitKind::Published,
        }
    }

    fn cause(id: &str, actor: &str) -> ChangeCauseOutput {
        ChangeCauseOutput {
            graph_commit_id: id.into(),
            authored_branch: "feature".into(),
            authored_at: 0,
            parent_commit_id: None,
            merged_parent_commit_id: None,
            actor_id: Some(actor.into()),
        }
    }

    fn image(properties: Value) -> Option<ChangeImageOutput> {
        Some(ChangeImageOutput {
            properties: properties.as_object().unwrap().clone(),
            endpoints: None,
        })
    }

    fn node(
        type_name: &str,
        id: &str,
        op: ChangeOp,
        before: Option<ChangeImageOutput>,
        after: Option<ChangeImageOutput>,
    ) -> EntityChangeOutput {
        EntityChangeOutput {
            kind: EntityKind::Node,
            entity_type: ChangeTypeOutput {
                id: format!("t-{type_name}"),
                name: type_name.into(),
            },
            id: id.into(),
            op,
            before,
            after,
        }
    }

    /// The fork of a branch from the commit lists read in pages of `page`: each side's ids are
    /// kept as its pages arrive, the two read in turn.
    fn fork_of(branch: &[CommitOutput], target: &[CommitOutput], page: usize) -> ForkSearch {
        let mut search = ForkSearch::default();
        let (mut branch, mut target) = (branch.chunks(page), target.chunks(page));
        loop {
            let (b, t) = (branch.next(), target.next());
            if b.is_none() && t.is_none() {
                return search;
            }
            if let Some(b) = b {
                search.branch_page(b.iter().cloned());
            }
            if let Some(t) = t {
                search.target_page(t);
            }
        }
    }

    fn ids(commits: &[CommitOutput]) -> Vec<&str> {
        commits.iter().map(|c| c.graph_commit_id.as_str()).collect()
    }

    #[test]
    fn the_fork_is_the_newest_branch_commit_in_the_target() {
        let main = vec![
            commit("m3", Some("m2")),
            commit("m2", Some("m1")),
            commit("m1", None),
        ];
        let branch = vec![
            commit("b2", Some("b1")),
            commit("b1", Some("m2")),
            commit("m2", Some("m1")),
            commit("m1", None),
        ];
        for page in [1, 2, 10] {
            let search = fork_of(&branch, &main, page);
            assert_eq!(
                search.fork().unwrap().graph_commit_id,
                "m2",
                "pages of {page}"
            );
            assert_eq!(ids(search.since()), ["b2", "b1"]);
        }

        // A branch with no commits of its own forks at its head.
        let fresh = vec![commit("m2", Some("m1")), commit("m1", None)];
        let search = fork_of(&fresh, &main, 1);
        assert_eq!(search.fork().unwrap().graph_commit_id, "m2");
        assert!(search.since().is_empty());

        // Unrelated histories share nothing: every commit read is the branch's.
        let search = fork_of(&[commit("x", None)], &main, 1);
        assert!(search.fork().is_none());
        assert_eq!(ids(search.since()), ["x"]);

        // Before the target's pages reach it, the fork is not known.
        let mut search = ForkSearch::default();
        search.branch_page(branch.clone());
        search.target_page(&main[..1]);
        assert!(search.fork().is_none());
        assert_eq!(search.since().len(), 4);
        search.target_page(&main[1..]);
        assert_eq!(search.fork().unwrap().graph_commit_id, "m2");
    }

    /// A commit known to be in the target, the merge base, is the fork once the branch's chain
    /// reaches it, with no page of the target read.
    #[test]
    fn a_held_merge_base_is_the_fork_the_chain_reaches() {
        let branch = [
            commit("b2", Some("b1")),
            commit("b1", Some("m2")),
            commit("m2", Some("m1")),
            commit("m1", None),
        ];
        let mut search = ForkSearch::default();
        search.hold("m2");
        search.branch_page(branch[..2].iter().cloned());
        assert!(search.fork().is_none(), "not reached yet");
        search.branch_page(branch[2..].iter().cloned());
        assert_eq!(search.fork().unwrap().graph_commit_id, "m2");
        assert_eq!(ids(search.since()), ["b2", "b1"]);
    }

    #[test]
    fn a_branch_merged_into_the_target_forks_at_its_merged_head() {
        // main published a merge (or fast-forward) commit whose merged parent is b2; the
        // branch commits themselves are not in main's list.
        let mut merge = commit("m3", Some("m2"));
        merge.merged_parent_commit_id = Some("b2".into());
        let main = vec![merge, commit("m2", Some("m1")), commit("m1", None)];
        let branch = vec![
            commit("b3", Some("b2")),
            commit("b2", Some("b1")),
            commit("b1", Some("m1")),
            commit("m1", None),
        ];
        for page in [1, 3] {
            let search = fork_of(&branch, &main, page);
            assert_eq!(search.fork().unwrap().graph_commit_id, "b2");
            assert_eq!(ids(search.since()), ["b3"]);
        }
        assert!(contains_merge_of(&main, "b2"));
        assert!(contains_merge_of(&main, "m2"));
        assert!(!contains_merge_of(&main, "b3"));
    }

    /// The target merged into the branch: the merge commit and the branch's commits before it
    /// are the branch's own, down to where it began.
    #[test]
    fn a_branch_the_target_was_merged_into_forks_where_it_began() {
        let main = vec![commit("m2", Some("m1")), commit("m1", None)];
        let mut merge = commit("fm", Some("f1"));
        merge.merged_parent_commit_id = Some("m2".into());
        let branch = vec![
            commit("f2", Some("fm")),
            merge,
            commit("f1", Some("m1")),
            commit("m1", None),
        ];
        let search = fork_of(&branch, &main, 1);
        assert_eq!(search.fork().unwrap().graph_commit_id, "m1");
        assert_eq!(ids(search.since()), ["f2", "fm", "f1"]);
    }

    #[test]
    fn a_merged_branch_forks_at_its_own_head() {
        // main merged the branch: the branch head is in main's history.
        let main = vec![
            commit("merge", Some("m2")),
            commit("b1", Some("m1")),
            commit("m2", Some("m1")),
            commit("m1", None),
        ];
        let branch = vec![commit("b1", Some("m1")), commit("m1", None)];
        assert_eq!(
            fork_of(&branch, &main, 1).fork().unwrap().graph_commit_id,
            "b1"
        );
    }

    #[test]
    fn folding_keeps_the_first_before_and_the_last_after() {
        let mut fold = ChangeFold::new();
        fold.add(
            &cause("c1", "act-a"),
            node(
                "Person",
                "p1",
                ChangeOp::Update,
                image(json!({"slug": "p1", "title": "A"})),
                image(json!({"slug": "p1", "title": "B"})),
            ),
        );
        fold.add(
            &cause("c2", "act-b"),
            node(
                "Person",
                "p1",
                ChangeOp::Update,
                image(json!({"slug": "p1", "title": "B"})),
                image(json!({"slug": "p1", "title": "C"})),
            ),
        );
        let folded = fold.finish();
        assert_eq!(folded.changes.len(), 1);
        let change = &folded.changes[0];
        assert_eq!(change.net(), Some(NetChange::Changed));
        assert_eq!(change.commits, vec!["c1", "c2"]);
        let diffs = change.property_diffs(&[]);
        assert_eq!(
            diffs,
            vec![PropertyDiff {
                name: "title".into(),
                before: Some(json!("A")),
                after: Some(json!("C")),
            }]
        );
    }

    #[test]
    fn entities_that_end_where_they_began_are_dropped() {
        let mut fold = ChangeFold::new();
        let c = cause("c1", "act-a");
        // Inserted then deleted.
        fold.add(
            &c,
            node(
                "Person",
                "p1",
                ChangeOp::Insert,
                None,
                image(json!({"slug": "p1"})),
            ),
        );
        fold.add(
            &c,
            node(
                "Person",
                "p1",
                ChangeOp::Delete,
                image(json!({"slug": "p1"})),
                None,
            ),
        );
        // Changed and changed back.
        fold.add(
            &c,
            node(
                "Person",
                "p2",
                ChangeOp::Update,
                image(json!({"title": "A"})),
                image(json!({"title": "B"})),
            ),
        );
        fold.add(
            &c,
            node(
                "Person",
                "p2",
                ChangeOp::Update,
                image(json!({"title": "B"})),
                image(json!({"title": "A"})),
            ),
        );
        // Deleted then inserted with other values: a change.
        fold.add(
            &c,
            node(
                "Person",
                "p3",
                ChangeOp::Delete,
                image(json!({"title": "A"})),
                None,
            ),
        );
        fold.add(
            &c,
            node(
                "Person",
                "p3",
                ChangeOp::Insert,
                None,
                image(json!({"title": "Z"})),
            ),
        );
        let folded = fold.finish();
        assert_eq!(folded.raw_changes, 6);
        let ids: Vec<_> = folded.changes.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["p3"]);
        assert_eq!(folded.changes[0].net(), Some(NetChange::Changed));
    }

    #[test]
    fn existing_data_comes_first_and_counts_follow_the_cards() {
        let mut fold = ChangeFold::new();
        let c = cause("c1", "act-a");
        fold.add(
            &c,
            node(
                "Company",
                "c-new",
                ChangeOp::Insert,
                None,
                image(json!({"name": "N"})),
            ),
        );
        fold.add(
            &c,
            node(
                "Person",
                "p-new",
                ChangeOp::Insert,
                None,
                image(json!({"name": "P"})),
            ),
        );
        fold.add(
            &c,
            node(
                "Person",
                "p1",
                ChangeOp::Update,
                image(json!({"title": "A"})),
                image(json!({"title": "B"})),
            ),
        );
        fold.add(
            &c,
            node(
                "Person",
                "p2",
                ChangeOp::Delete,
                image(json!({"name": "Q"})),
                None,
            ),
        );
        fold.add(
            &c,
            EntityChangeOutput {
                kind: EntityKind::Edge,
                entity_type: ChangeTypeOutput {
                    id: "t-WorksAt".into(),
                    name: "WorksAt".into(),
                },
                id: "e1".into(),
                op: ChangeOp::Insert,
                before: None,
                after: Some(ChangeImageOutput {
                    properties: serde_json::Map::new(),
                    endpoints: Some(json!({"from": "p-new", "to": "c-new"})),
                }),
            },
        );
        let folded = fold.finish();
        let order: Vec<_> = folded.changes.iter().map(|c| c.id.as_str()).collect();
        // Person holds changes to existing data, so it leads; Company only adds; edges last.
        assert_eq!(order, vec!["p1", "p2", "p-new", "c-new", "e1"]);
        assert_eq!(
            folded.changes[4].endpoints(),
            Some(("p-new".into(), "c-new".into()))
        );
        let counts = folded.counts();
        assert_eq!(counts[0].type_name, "Person");
        assert_eq!(
            (counts[0].added, counts[0].changed, counts[0].removed),
            (1, 1, 1)
        );
        assert_eq!(counts[1].type_name, "Company");
        assert_eq!(counts[2].type_name, "WorksAt");
        assert_eq!(folded.entity_count(), 4);
        assert_eq!(folded.relationship_count(), 1);
    }

    /// An edge upserted with another end is changed though no property is, and says which ends
    /// it had before and has now; one whose ends stayed, or that only one image names, has none.
    #[test]
    fn an_edge_with_another_end_is_changed_and_names_both() {
        let knows = |id: &str, before: Option<(&str, &str)>, after: Option<(&str, &str)>| {
            let image = |ends: Option<(&str, &str)>| {
                ends.map(|(from, to)| ChangeImageOutput {
                    properties: json!({"since": 1}).as_object().unwrap().clone(),
                    endpoints: Some(json!({"from": from, "to": to})),
                })
            };
            EntityChangeOutput {
                kind: EntityKind::Edge,
                entity_type: ChangeTypeOutput {
                    id: "t-Knows".into(),
                    name: "Knows".into(),
                },
                id: id.into(),
                op: ChangeOp::Update,
                before: image(before),
                after: image(after),
            }
        };
        let mut fold = ChangeFold::new();
        let c = cause("c1", "act-a");
        fold.add(
            &c,
            knows("moved", Some(("ana", "maya")), Some(("bob", "maya"))),
        );
        fold.add(
            &c,
            knows("kept", Some(("ana", "maya")), Some(("ana", "maya"))),
        );
        fold.add(&c, knows("new", None, Some(("ana", "theo"))));
        let folded = fold.finish();
        let moved = &folded.changes[0];
        assert_eq!(moved.id, "moved");
        assert_eq!(moved.net(), Some(NetChange::Changed));
        assert!(moved.property_diffs(&[]).is_empty(), "no property changed");
        assert_eq!(
            moved.moved_ends(),
            Some((
                ("ana".to_owned(), "maya".to_owned()),
                ("bob".to_owned(), "maya".to_owned())
            ))
        );
        let ids: Vec<&str> = folded.changes.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["moved", "new"], "an edge that stayed is dropped");
        assert_eq!(folded.changes[1].moved_ends(), None);
    }

    #[test]
    fn added_and_removed_cards_show_every_value_in_schema_order() {
        let mut fold = ChangeFold::new();
        let c = cause("c1", "act-a");
        fold.add(
            &c,
            node(
                "Person",
                "p1",
                ChangeOp::Insert,
                None,
                image(json!({"title": "T", "name": "N", "email": null, "slug": "p1"})),
            ),
        );
        let folded = fold.finish();
        let order = vec!["slug".to_owned(), "name".to_owned(), "title".to_owned()];
        let names: Vec<_> = folded.changes[0]
            .property_diffs(&order)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["slug", "name", "title"]);
    }

    #[test]
    fn card_rows_leave_out_the_key_and_follow_the_schema() {
        let schema = SchemaModel::parse(include_str!("../../../dev/cluster/knowledge.pg"))
            .expect("dev schema compiles");
        let mut fold = ChangeFold::new();
        fold.add(
            &cause("c1", "act-a"),
            node(
                "Person",
                "p1",
                ChangeOp::Insert,
                None,
                image(json!({"title": "T", "name": "N", "slug": "p1"})),
            ),
        );
        let folded = fold.finish();
        let names: Vec<_> = folded.changes[0]
            .card_diffs(Some(&schema))
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names.len(), 2);
        assert!(!names.contains(&"slug".to_owned()));
    }

    #[test]
    fn the_cap_stops_folding_mid_block() {
        let mut fold = ChangeFold::new();
        let block = ChangeBlockOutput {
            cause: cause("c1", "act-a"),
            changes: (0..5)
                .map(|i| {
                    node(
                        "Person",
                        &format!("p{i}"),
                        ChangeOp::Insert,
                        None,
                        image(json!({"slug": i})),
                    )
                })
                .collect(),
        };
        assert!(!fold.add_block_capped(block, 3));
        assert_eq!(fold.raw_changes(), 3);
        assert_eq!(fold.finish().changes.len(), 3);
    }

    /// Spec 008's budget: folding a capped review stays under a second off the main thread.
    /// Run with `cargo test -p graph_store fold_instrument -- --ignored --nocapture`.
    #[test]
    #[ignore = "instrument: prints the time to fold REVIEW_CHANGE_CAP changes"]
    fn fold_instrument() {
        let blocks: Vec<ChangeBlockOutput> = (0..20)
            .map(|commit| ChangeBlockOutput {
                cause: cause(&format!("c{commit}"), "act-dev"),
                changes: (0..REVIEW_CHANGE_CAP / 20)
                    .map(|i| {
                        // Half the entities change twice, so the fold merges as well as inserts.
                        let id = format!("p{}", (commit * 1_000 + i) % 15_000);
                        node(
                            "Person",
                            &id,
                            ChangeOp::Update,
                            image(json!({"slug": id, "title": "A", "name": "N", "tags": ["x"]})),
                            image(json!({"slug": id, "title": format!("T{commit}"), "name": "N", "tags": ["y"]})),
                        )
                    })
                    .collect(),
            })
            .collect();
        let start = std::time::Instant::now();
        let mut fold = ChangeFold::new();
        for block in blocks {
            fold.add_block_capped(block, REVIEW_CHANGE_CAP);
        }
        let folded = fold.finish();
        let counts = folded.counts();
        let elapsed = start.elapsed();
        println!(
            "folded {} changes into {} entities ({} types) in {elapsed:?}",
            folded.raw_changes,
            folded.changes.len(),
            counts.len()
        );
        assert_eq!(folded.raw_changes, REVIEW_CHANGE_CAP);
    }

    #[test]
    fn the_summary_reads_as_a_sentence() {
        let dev = vec!["act-dev".to_owned()];
        assert_eq!(
            summary_sentence(17, 22, Some(3), &dev),
            "17 entities and 22 relationships changed in 3 commits by act-dev"
        );
        assert_eq!(
            summary_sentence(1, 0, Some(1), &dev),
            "1 entity changed in 1 commit by act-dev"
        );
        assert_eq!(
            summary_sentence(0, 2, Some(1), &["a".into(), "b".into()]),
            "2 relationships changed in 1 commit by a and b"
        );
        assert_eq!(
            summary_sentence(3, 0, Some(4), &["a".into(), "b".into(), "c".into()]),
            "3 entities changed in 4 commits by 3 people"
        );
        assert_eq!(summary_sentence(0, 0, Some(2), &dev), "Nothing changed");
        assert_eq!(
            summary_sentence(2, 0, Some(2), &[]),
            "2 entities changed in 2 commits"
        );
        assert_eq!(
            summary_sentence(20_000, 0, Some(3), &dev),
            "20,000 entities changed in 3 commits by act-dev"
        );
        // A history read only so far names no count, and nobody.
        assert_eq!(
            summary_sentence(17, 22, None, &dev),
            "17 entities and 22 relationships changed"
        );
        assert_eq!(plural(999, "commit", "commits"), "999 commits");
        assert_eq!(plural(1_234_567, "commit", "commits"), "1,234,567 commits");
    }
}
