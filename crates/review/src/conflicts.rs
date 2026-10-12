//! Merge conflicts in words (spec 008). OmniGraph 0.13 reports each conflict's kind; Studio
//! says what happened, naming the side when the review knows it.

use graph_client::api::MergeConflictKind;
use graph_store::NetChange;

/// The sentence for a conflict of `kind`. `here` is what the reviewed branch did to the entity,
/// when the review saw it; `target` is the branch being merged into.
pub fn conflict_wording(kind: MergeConflictKind, here: Option<NetChange>, target: &str) -> String {
    match kind {
        MergeConflictKind::DivergentInsert => "Added on both branches with different values".into(),
        MergeConflictKind::DivergentUpdate => "Changed on both branches".into(),
        MergeConflictKind::DeleteVsUpdate => match here {
            Some(NetChange::Removed) => format!("Deleted here, changed on {target}"),
            Some(NetChange::Changed | NetChange::Added) => {
                format!("Deleted on {target}, changed here")
            }
            None => "Deleted on one branch, changed on the other".into(),
        },
        MergeConflictKind::OrphanEdge => "Relationship points to a deleted node".into(),
        MergeConflictKind::UniqueViolation => "Two rows share a unique value".into(),
        MergeConflictKind::CardinalityViolation => {
            "More relationships than the schema allows".into()
        }
        MergeConflictKind::ValueConstraintViolation => {
            "A value breaks a constraint in the schema".into()
        }
    }
}

/// What to do about conflicts, after the list.
pub fn resolution_hint(source: &str, target: &str) -> String {
    format!(
        "Nothing was merged. Fix each entity on {source} with ordinary edits so it agrees with \
         {target}, then merge again."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_words() {
        let kinds = [
            MergeConflictKind::DivergentInsert,
            MergeConflictKind::DivergentUpdate,
            MergeConflictKind::DeleteVsUpdate,
            MergeConflictKind::OrphanEdge,
            MergeConflictKind::UniqueViolation,
            MergeConflictKind::CardinalityViolation,
            MergeConflictKind::ValueConstraintViolation,
        ];
        let mut seen = std::collections::HashSet::new();
        for kind in kinds {
            let words = conflict_wording(kind, None, "main");
            assert!(!words.contains('_'), "{words}");
            assert!(seen.insert(words), "two kinds read the same");
        }
    }

    #[test]
    fn delete_versus_update_names_the_side() {
        assert_eq!(
            conflict_wording(
                MergeConflictKind::DeleteVsUpdate,
                Some(NetChange::Changed),
                "main"
            ),
            "Deleted on main, changed here"
        );
        assert_eq!(
            conflict_wording(
                MergeConflictKind::DeleteVsUpdate,
                Some(NetChange::Removed),
                "main"
            ),
            "Deleted here, changed on main"
        );
        assert_eq!(
            conflict_wording(MergeConflictKind::DivergentUpdate, None, "main"),
            "Changed on both branches"
        );
        assert_eq!(
            conflict_wording(MergeConflictKind::OrphanEdge, None, "main"),
            "Relationship points to a deleted node"
        );
    }
}
