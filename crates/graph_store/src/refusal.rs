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

/// A write the server refused because it would leave a node with fewer or more edges than an
/// `@card` bound allows, as the engine words it (`@card violation on edge Works: source 'ada'
/// has 0 edges (min 1)`). Omnigraph 0.15 checks a minimum on every write that inserts or
/// updates a source node, so a node saved without the edges it needs is refused with this.
/// The answer states `effects: unknown` though nothing was written (spec 012, E52), so a
/// write's view still settles it from history; this only says why in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardViolation {
    pub edge: String,
    pub source: String,
    pub count: u32,
    /// `true` for a minimum (`min`), `false` for a maximum.
    pub minimum: bool,
    pub bound: u32,
}

impl CardViolation {
    /// The violation a write's outcome names, if its text has one.
    pub fn find(reason: &str) -> Option<Self> {
        let rest = &reason[reason.find("@card violation on edge ")? + 24..];
        let (edge, rest) = rest.split_once(": source '")?;
        let (source, rest) = rest.split_once("' has ")?;
        let (count, rest) = rest.split_once(" edges (")?;
        let (bound_kind, rest) = rest.split_once(' ')?;
        let bound = rest.split(')').next()?;
        Some(Self {
            edge: edge.to_owned(),
            source: source.to_owned(),
            count: count.parse().ok()?,
            minimum: match bound_kind {
                "min" => true,
                "max" => false,
                _ => return None,
            },
            bound: bound.parse().ok()?,
        })
    }

    /// The violation in words: what the node would have, and what the schema asks.
    pub fn sentence(&self) -> String {
        let needs = if self.minimum {
            format!("at least {}", self.bound)
        } else {
            format!("at most {}", self.bound)
        };
        let has = match self.count {
            1 => "1 edge".to_owned(),
            n => format!("{n} edges"),
        };
        format!(
            "{} would have {has} of {}, and the schema allows each one {needs}",
            self.source, self.edge
        )
    }
}

#[cfg(test)]
mod card_tests {
    use super::CardViolation;

    /// The engine's words for a `@card` refusal, read from a write's outcome as the client words
    /// an unknown one; anything else is not one.
    #[test]
    fn a_card_refusal_is_read_from_its_words() {
        let reason = "the server answered 400 Bad Request and could not say whether the change \
                      was made: @card violation on edge MemberOf: source 'bo' has 0 edges (min 1)";
        let violation = CardViolation::find(reason).expect("a violation");
        assert_eq!(
            violation,
            CardViolation {
                edge: "MemberOf".into(),
                source: "bo".into(),
                count: 0,
                minimum: true,
                bound: 1,
            }
        );
        assert_eq!(
            violation.sentence(),
            "bo would have 0 edges of MemberOf, and the schema allows each one at least 1"
        );
        let most = CardViolation::find(
            "@card violation on edge Knows: source 'Alice' has 4 edges (max 2)",
        )
        .unwrap();
        assert!(!most.minimum);
        assert!(most.sentence().ends_with("at most 2"));
        for other in [
            "__dst 'nobody' not found in Team",
            "@card violation on edge X",
            "",
        ] {
            assert_eq!(CardViolation::find(other), None, "{other}");
        }
    }
}
