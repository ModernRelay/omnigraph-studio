//! What the session's credential may do on its graph, as `GET /graphs/{id}/access` answers it
//! (spec 002): kept per branch, and target, asked about, and judged one action at a time. The
//! server stays the authority; this only decides which actions Studio offers.

use std::collections::{HashMap, HashSet};

use graph_client::api::{AccessAction, AccessOutput};

/// What one answer is about: a branch, and with it the branch a create or a merge targets.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct AccessScope {
    pub branch: Option<String>,
    pub target: Option<String>,
}

impl AccessScope {
    /// The graph alone.
    pub fn graph() -> Self {
        Self::default()
    }

    /// One branch.
    pub fn branch(branch: &str) -> Self {
        Self {
            branch: Some(branch.to_owned()),
            target: None,
        }
    }

    /// A branch, and one a create starts from it or a merge goes into.
    pub fn branch_into(branch: &str, target: &str) -> Self {
        Self {
            branch: Some(branch.to_owned()),
            target: Some(target.to_owned()),
        }
    }
}

/// Whether the credential may take an action, as far as Studio knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allowed {
    Yes,
    No,
    /// Not read yet, or not knowable from what was asked: Studio offers the action, and the
    /// server decides.
    Unknown,
}

impl Allowed {
    /// Whether Studio offers the action: everything but a known refusal.
    pub fn offered(self) -> bool {
        self != Allowed::No
    }
}

/// The answers read so far, and the reads out.
#[derive(Default)]
pub(crate) struct Access {
    answers: HashMap<AccessScope, AccessOutput>,
    asked: HashSet<AccessScope>,
    /// Moves on [`Access::forget`], so a read sent before lands nowhere.
    generation: u64,
    /// The client's authority count the answers were read under.
    authority: u64,
}

impl Access {
    /// Forgets every answer when the client's authority count moved since they were read (a
    /// new token, or a 403). Whether it forgot.
    pub(crate) fn sync(&mut self, authority: u64) -> bool {
        if authority == self.authority {
            return false;
        }
        self.authority = authority;
        let held = !self.answers.is_empty() || !self.asked.is_empty();
        self.forget();
        held
    }

    /// Judges `action` for `scope`: allowed when an answer allows it on every branch or on the
    /// scope's, refused when the scope's answer leaves it out. An answer without a branch
    /// cannot refuse a branch's action (rules may allow it branch by branch), and a create or
    /// a merge is judged only with its target.
    pub(crate) fn allows(&self, action: AccessAction, scope: &AccessScope) -> Allowed {
        if self
            .answers
            .values()
            .any(|answer| answer.graph_wide.contains(&action))
        {
            return Allowed::Yes;
        }
        let targeted = matches!(
            action,
            AccessAction::BranchCreate | AccessAction::BranchMerge
        );
        if scope.branch.is_none() || (targeted && scope.target.is_none()) {
            return Allowed::Unknown;
        }
        match self.answers.get(scope) {
            Some(answer) if answer.actions.contains(&action) => Allowed::Yes,
            Some(_) => Allowed::No,
            None => Allowed::Unknown,
        }
    }

    /// The actor the credential resolves to, from any answer; none before one, or on a server
    /// without authentication.
    pub(crate) fn actor(&self) -> Option<&str> {
        self.answers
            .values()
            .find_map(|answer| answer.actor.as_deref())
    }

    /// Whether `scope` is to be read: neither answered nor asked. When it is, it counts as
    /// asked, and the generation to land its answer with is returned.
    pub(crate) fn wants(&mut self, scope: &AccessScope) -> Option<u64> {
        if self.answers.contains_key(scope) || !self.asked.insert(scope.clone()) {
            return None;
        }
        Some(self.generation)
    }

    /// Lands a read of `scope` sent at `generation`: kept when nothing was forgotten since. A
    /// failed read is asked again the next time. Returns whether anything changed.
    pub(crate) fn landed(
        &mut self,
        scope: AccessScope,
        generation: u64,
        answer: Option<AccessOutput>,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.asked.remove(&scope);
        match answer {
            Some(answer) => {
                self.answers.insert(scope, answer);
                true
            }
            None => false,
        }
    }

    /// Forgets every answer: the credential, or the graph's policy, may have changed (a 403,
    /// a new token). Reads out land nowhere.
    pub(crate) fn forget(&mut self) {
        self.answers.clear();
        self.asked.clear();
        self.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(
        branch: Option<&str>,
        graph_wide: &[AccessAction],
        actions: &[AccessAction],
    ) -> AccessOutput {
        AccessOutput {
            graph_id: "g".into(),
            actor: Some("act-ada".into()),
            graph_wide: graph_wide.to_vec(),
            branch: branch.map(str::to_owned),
            target_branch: None,
            actions: actions.to_vec(),
        }
    }

    /// An action is allowed on every branch by the graph-wide list, on one by its own answer,
    /// and refused only by an answer about that branch; a create or a merge needs its target.
    #[test]
    fn an_action_is_refused_only_by_an_answer_about_its_branch() {
        let mut access = Access::default();
        let main = AccessScope::branch("main");
        assert_eq!(access.allows(AccessAction::Change, &main), Allowed::Unknown);
        let generation = access.wants(&main).unwrap();
        assert_eq!(access.wants(&main), None, "asked once");
        assert!(access.landed(
            main.clone(),
            generation,
            Some(answer(
                Some("main"),
                &[AccessAction::Read],
                &[AccessAction::Read]
            ))
        ));
        assert_eq!(
            access.allows(AccessAction::Read, &AccessScope::branch("fix")),
            Allowed::Yes
        );
        assert_eq!(access.allows(AccessAction::Change, &main), Allowed::No);
        assert!(!access.allows(AccessAction::Change, &main).offered());
        assert_eq!(
            access.allows(AccessAction::Change, &AccessScope::graph()),
            Allowed::Unknown
        );
        assert_eq!(
            access.allows(AccessAction::Change, &AccessScope::branch("fix")),
            Allowed::Unknown,
            "fix was not asked about"
        );
        assert_eq!(
            access.allows(AccessAction::BranchMerge, &main),
            Allowed::Unknown
        );
        assert_eq!(access.actor(), Some("act-ada"));

        let into = AccessScope::branch_into("fix", "main");
        let generation = access.wants(&into).unwrap();
        access.landed(
            into.clone(),
            generation,
            Some(answer(
                Some("fix"),
                &[AccessAction::Read],
                &[AccessAction::Change],
            )),
        );
        assert_eq!(access.allows(AccessAction::BranchMerge, &into), Allowed::No);
        assert_eq!(access.allows(AccessAction::Change, &into), Allowed::Yes);
    }

    /// Forgetting drops every answer, and a read sent before lands nowhere; a failed read is
    /// asked again.
    #[test]
    fn a_forgotten_answer_is_read_again() {
        let mut access = Access::default();
        let main = AccessScope::branch("main");
        let before = access.wants(&main).unwrap();
        access.forget();
        assert!(!access.landed(main.clone(), before, Some(answer(Some("main"), &[], &[]))));
        assert_eq!(access.allows(AccessAction::Change, &main), Allowed::Unknown);
        let now = access.wants(&main).unwrap();
        assert!(!access.landed(main.clone(), now, None));
        assert!(
            access.wants(&main).is_some(),
            "a failed read is asked again"
        );
    }
}
