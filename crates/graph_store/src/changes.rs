//! Change folding shared by review and history (spec 008, 009): the fork point of a branch, the
//! per-entity fold of change-feed entries (first before, last after), per-type counts and the
//! summary sentence. Pure functions; no GPUI.
//!
//! OmniGraph 0.13 has no branch-to-branch diff (E6) or merge base (E7) route, so review folds
//! the branch's change feed from the fork point found by walking `GET /commits`.

use std::collections::{BTreeSet, HashMap, HashSet};

use gq_language::SchemaModel;
use graph_client::api::{
    ChangeBlockOutput, ChangeCauseOutput, ChangeImageOutput, CommitOutput, EntityChangeOutput,
    EntityKind,
};
use serde_json::Value;

/// The most changes a review folds before it says "Partial" (spec 008).
pub const REVIEW_CHANGE_CAP: usize = 20_000;

/// The newest commit of `branch` that `target` already contains. Both lists are newest first,
/// as `GET /commits?branch=` returns them. `None` when the histories share nothing.
///
/// A target's list holds only its own first-parent chain: a merged branch's commits appear
/// there as some commit's `merged_parent_commit_id` (a fast-forward too publishes a new commit
/// with the branch head as merged parent). A branch list is a first-parent chain, so once one
/// of its commits is in the target, every older one is.
pub fn fork_point<'a>(
    branch: &'a [CommitOutput],
    target: &[CommitOutput],
) -> Option<&'a CommitOutput> {
    let reachable: HashSet<&str> = target
        .iter()
        .flat_map(|c| {
            std::iter::once(c.graph_commit_id.as_str()).chain(c.merged_parent_commit_id.as_deref())
        })
        .collect();
    branch
        .iter()
        .find(|commit| reachable.contains(commit.graph_commit_id.as_str()))
}

/// Whether `target`'s history shows `head` merged: listed itself, or the merged parent of one
/// of its commits.
pub fn contains_merge_of(target: &[CommitOutput], head: &str) -> bool {
    target
        .iter()
        .any(|c| c.graph_commit_id == head || c.merged_parent_commit_id.as_deref() == Some(head))
}

/// The commits of `branch` newer than `fork`, newest first. Every commit when `fork` is `None`.
pub fn commits_since<'a>(branch: &'a [CommitOutput], fork: Option<&str>) -> &'a [CommitOutput] {
    match fork {
        Some(fork) => {
            let end = branch
                .iter()
                .position(|c| c.graph_commit_id == fork)
                .unwrap_or(branch.len());
            &branch[..end]
        }
        None => branch,
    }
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

fn endpoints_of(value: &Value) -> Option<(String, String)> {
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
    commits: Vec<String>,
    actors: BTreeSet<String>,
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

    pub fn add_block(&mut self, block: ChangeBlockOutput) {
        let ChangeBlockOutput { cause, changes } = block;
        for change in changes {
            self.add(&cause, change);
        }
        self.note_commit(&cause);
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
        self.note_commit(&cause);
        complete
    }

    fn note_commit(&mut self, cause: &ChangeCauseOutput) {
        if self.commits.last() != Some(&cause.graph_commit_id) {
            self.commits.push(cause.graph_commit_id.clone());
        }
        if let Some(actor) = &cause.actor_id {
            self.actors.insert(actor.clone());
        }
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
            commits: self.commits,
            actors: self.actors.into_iter().collect(),
            raw_changes: self.raw_changes,
        }
    }
}

/// The result of a fold, in display order.
#[derive(Debug, Clone, Default)]
pub struct FoldedChanges {
    pub changes: Vec<FoldedChange>,
    /// Commits whose changes were folded, in feed order.
    pub commits: Vec<String>,
    pub actors: Vec<String>,
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

impl TypeCounts {
    pub fn total(&self) -> usize {
        self.added + self.changed + self.removed
    }
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

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{} {}", grouped(count), if count == 1 { one } else { many })
}

/// `20000` → `20,000`.
pub fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The review header's sentence: "17 entities and 22 relationships changed in 3 commits by
/// act-dev".
pub fn summary_sentence(
    entities: usize,
    relationships: usize,
    commits: usize,
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
        let fork = fork_point(&branch, &main).unwrap();
        assert_eq!(fork.graph_commit_id, "m2");
        let since = commits_since(&branch, Some("m2"));
        assert_eq!(since.len(), 2);
        assert_eq!(since[0].graph_commit_id, "b2");

        // A branch with no commits of its own forks at its head.
        let fresh = vec![commit("m2", Some("m1")), commit("m1", None)];
        assert_eq!(fork_point(&fresh, &main).unwrap().graph_commit_id, "m2");
        assert!(commits_since(&fresh, Some("m2")).is_empty());

        // Unrelated histories share nothing.
        assert!(fork_point(&[commit("x", None)], &main).is_none());
        assert_eq!(commits_since(&branch, None).len(), 4);
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
        assert_eq!(fork_point(&branch, &main).unwrap().graph_commit_id, "b2");
        assert_eq!(commits_since(&branch, Some("b2")).len(), 1);
        assert!(contains_merge_of(&main, "b2"));
        assert!(contains_merge_of(&main, "m2"));
        assert!(!contains_merge_of(&main, "b3"));
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
        assert_eq!(fork_point(&branch, &main).unwrap().graph_commit_id, "b1");
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
        let folded = fold.finish();
        assert_eq!(folded.commits, vec!["c1"]);
        assert_eq!(folded.actors, vec!["act-a"]);
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
            summary_sentence(17, 22, 3, &dev),
            "17 entities and 22 relationships changed in 3 commits by act-dev"
        );
        assert_eq!(
            summary_sentence(1, 0, 1, &dev),
            "1 entity changed in 1 commit by act-dev"
        );
        assert_eq!(
            summary_sentence(0, 2, 1, &["a".into(), "b".into()]),
            "2 relationships changed in 1 commit by a and b"
        );
        assert_eq!(
            summary_sentence(3, 0, 4, &["a".into(), "b".into(), "c".into()]),
            "3 entities changed in 4 commits by 3 people"
        );
        assert_eq!(summary_sentence(0, 0, 2, &dev), "Nothing changed");
        assert_eq!(
            summary_sentence(2, 0, 2, &[]),
            "2 entities changed in 2 commits"
        );
        assert_eq!(
            summary_sentence(20_000, 0, 3, &dev),
            "20,000 entities changed in 3 commits by act-dev"
        );
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }
}
