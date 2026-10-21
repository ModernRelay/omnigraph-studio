//! What a control says when the credential may not take its action (spec 002): the reason a
//! disabled control gives, naming what the credential lacks.

use graph_client::api::AccessAction;

use crate::access::{AccessScope, Allowed};
use crate::session::GraphSession;

impl GraphSession {
    /// Why the credential may not take `action` for `scope`, once the access route refused it;
    /// `None` while it is allowed or not known, so the control stays offered and the server
    /// decides. A hash lookup: safe to ask while drawing.
    pub fn refusal(&self, action: AccessAction, scope: &AccessScope) -> Option<String> {
        (self.allows(action, scope) == Allowed::No).then(|| refusal_text(action, scope))
    }
}

/// The words for `action` refused for `scope`: "Your credential may not change fix".
fn refusal_text(action: AccessAction, scope: &AccessScope) -> String {
    let branch = scope.branch.as_deref().unwrap_or("this graph");
    let target = scope.target.as_deref();
    let lacks = match action {
        AccessAction::Read => format!("read {branch}"),
        AccessAction::Export => format!("export {branch}"),
        AccessAction::Change => format!("change {branch}"),
        AccessAction::SchemaApply => "change the schema".to_owned(),
        AccessAction::BranchCreate => match target {
            Some(target) => format!("create {target} from {branch}"),
            None => format!("create a branch from {branch}"),
        },
        AccessAction::BranchDelete => format!("delete {branch}"),
        AccessAction::BranchMerge => match target {
            Some(target) => format!("merge {branch} into {target}"),
            None => format!("merge {branch}"),
        },
        AccessAction::CommitPublish => format!("publish commits on {branch}"),
        AccessAction::CommitDrop => format!("drop commits on {branch}"),
        AccessAction::InvokeQuery => format!("run stored queries on {branch}"),
        AccessAction::Other => format!("do this on {branch}"),
    };
    format!("Your credential may not {lacks}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refusal names the action and what it is taken on: the branch, and the target of a
    /// create or a merge.
    #[test]
    fn a_refusal_names_the_action_and_its_branches() {
        let fix = AccessScope::branch("fix");
        let into = AccessScope::branch_into("fix", "main");
        assert_eq!(
            refusal_text(AccessAction::Change, &fix),
            "Your credential may not change fix"
        );
        assert_eq!(
            refusal_text(AccessAction::BranchDelete, &fix),
            "Your credential may not delete fix"
        );
        assert_eq!(
            refusal_text(AccessAction::BranchMerge, &into),
            "Your credential may not merge fix into main"
        );
        assert_eq!(
            refusal_text(
                AccessAction::BranchCreate,
                &AccessScope::branch_into("main", "x")
            ),
            "Your credential may not create x from main"
        );
    }
}
