//! The commits History draws (spec 009).
//!
//! OmniGraph's `GET /commits?branch=` lists one branch's first-parent chain: a merged
//! branch's commits appear only as a `merged_parent_commit_id`. Until a lineage route exists
//! (E9, E10), History reads the other live branches' lists too and adds every commit reachable
//! through a merged parent, so merges draw as lanes. Commits of deleted branches stay unknown.

use std::collections::{HashMap, HashSet};

use graph_client::api::CommitOutput;

/// `own` plus every commit of `others` reachable from `own` through merged parents, newest
/// first by creation time.
pub fn with_merged_commits(
    own: Vec<CommitOutput>,
    others: &[Vec<CommitOutput>],
) -> Vec<CommitOutput> {
    let mut by_id: HashMap<&str, &CommitOutput> = HashMap::new();
    for list in others {
        for commit in list {
            by_id
                .entry(commit.graph_commit_id.as_str())
                .or_insert(commit);
        }
    }
    let mut seen: HashSet<String> = own.iter().map(|c| c.graph_commit_id.clone()).collect();
    let mut stack: Vec<String> = own
        .iter()
        .filter_map(|c| c.merged_parent_commit_id.clone())
        .collect();
    let mut extra = Vec::new();
    while let Some(id) = stack.pop() {
        if seen.contains(&id) {
            continue;
        }
        let Some(commit) = by_id.get(id.as_str()) else {
            continue;
        };
        seen.insert(id);
        stack.extend(commit.parent_commit_id.clone());
        stack.extend(commit.merged_parent_commit_id.clone());
        extra.push((*commit).clone());
    }
    let mut all = own;
    all.extend(extra);
    all.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then(b.graph_manifest_version.cmp(&a.graph_manifest_version))
            .then(b.graph_commit_id.cmp(&a.graph_commit_id))
    });
    all
}

/// Branch names by the commit each points at, from each branch's list (newest first).
pub fn branch_heads<'a>(
    lists: impl IntoIterator<Item = (&'a str, &'a [CommitOutput])>,
) -> HashMap<String, Vec<String>> {
    let mut heads: HashMap<String, Vec<String>> = HashMap::new();
    for (branch, commits) in lists {
        if let Some(head) = commits.first() {
            heads
                .entry(head.graph_commit_id.clone())
                .or_default()
                .push(branch.to_owned());
        }
    }
    for names in heads.values_mut() {
        names.sort_by(|a, b| (a != "main").cmp(&(b != "main")).then(a.cmp(b)));
    }
    heads
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str, at: i64, parent: Option<&str>, merged: Option<&str>) -> CommitOutput {
        CommitOutput {
            graph_commit_id: id.into(),
            graph_manifest_version: 0,
            created_at: at,
            graph_branch: None,
            parent_commit_id: parent.map(Into::into),
            merged_parent_commit_id: merged.map(Into::into),
            actor_id: None,
            kind: Default::default(),
        }
    }

    #[test]
    fn merged_branches_join_the_history() {
        let main = vec![
            commit("m3", 30, Some("m2"), Some("b2")),
            commit("m2", 20, Some("m1"), None),
            commit("m1", 10, None, None),
        ];
        let feature = vec![
            commit("b3", 35, Some("b2"), None),
            commit("b2", 25, Some("b1"), None),
            commit("b1", 15, Some("m1"), None),
            commit("m1", 10, None, None),
        ];
        let unrelated = vec![
            commit("x1", 40, Some("m1"), None),
            commit("m1", 10, None, None),
        ];
        let all = with_merged_commits(main, &[feature.clone(), unrelated.clone()]);
        let ids: Vec<_> = all.iter().map(|c| c.graph_commit_id.as_str()).collect();
        // b3 came after the merge and x1 was never merged: neither is in main's history.
        assert_eq!(ids, vec!["m3", "b2", "m2", "b1", "m1"]);

        let heads = branch_heads([
            ("feature", feature.as_slice()),
            ("main", all.as_slice()),
            ("other", unrelated.as_slice()),
        ]);
        assert_eq!(heads["m3"], vec!["main"]);
        assert_eq!(heads["b3"], vec!["feature"]);
    }

    #[test]
    fn a_merge_from_a_deleted_branch_stays_a_stub() {
        let main = vec![
            commit("m2", 20, Some("m1"), Some("gone")),
            commit("m1", 10, None, None),
        ];
        let all = with_merged_commits(main, &[]);
        assert_eq!(all.len(), 2);
    }
}
