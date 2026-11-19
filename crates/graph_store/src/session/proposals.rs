//! Proposals (spec 038): an edit asked of a branch the graph treats as protected lands on a
//! branch of its own, `proposal/<actor>/<MMDD>-<n>`, which the window then shows and a review
//! merges. Which branches are protected the policy knows and does not say (E63): until it does,
//! Studio treats `main` as protected unless the person turns proposals off, and any other
//! branch as open unless the person turns them on.

use futures::FutureExt as _;
use gpui::{Context, Task};
use graph_client::WriteOutcome;

use super::{BranchStart, GraphSession, Loadable, SessionEvent};

/// A proposal branch being made: its name once made, or why it was not.
pub(super) type Starting = futures::future::Shared<Task<Result<String, String>>>;

/// What every proposal's name starts with.
pub const PROPOSAL_PREFIX: &str = "proposal/";

/// The branch Studio treats as protected while the server does not say which are (E63).
pub const PROTECTED_WITHOUT_POLICY: &str = "main";

/// Whether edits of a protected branch are proposed unless the person turns proposals off
/// there (spec 038). [`crate::init`] sets it, so Studio does; a test that makes its own session
/// writes where it asks unless it sets this.
pub struct ProposeProtected(pub bool);

impl gpui::Global for ProposeProtected {}

/// Where a write asked of a branch lands ([`GraphSession::write_target`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteTarget {
    /// The branch asked of.
    Branch(String),
    /// The proposal the window is on, which proposes into `base`, the branch asked of.
    Proposal { base: String, branch: String },
    /// A new proposal from `base`, the branch asked of, which the write starts.
    NewProposal { base: String },
}

impl WriteTarget {
    /// The branch the write goes to, when it exists already.
    pub fn branch(&self) -> Option<&str> {
        match self {
            WriteTarget::Branch(branch) | WriteTarget::Proposal { branch, .. } => Some(branch),
            WriteTarget::NewProposal { .. } => None,
        }
    }

    /// Whether the write goes to a proposal rather than to the branch asked of.
    pub fn proposes(&self) -> bool {
        !matches!(self, WriteTarget::Branch(_))
    }

    /// Where the write lands, in words, as an editor's hint and a confirmation name it: the
    /// branch, or "a new proposal from main".
    pub fn label(&self) -> String {
        match self {
            WriteTarget::Branch(branch) | WriteTarget::Proposal { branch, .. } => branch.clone(),
            WriteTarget::NewProposal { base } => format!("a new proposal from {base}"),
        }
    }
}

/// A proposal the window is on: its branch, and the branch it proposes into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub base: String,
    pub branch: String,
}

/// The name of a new proposal by `actor`, made on `month`/`day`: `proposal/<actor>/<MMDD>-<n>`,
/// with `n` the first number no branch in `taken` holds. An actor's name keeps its letters,
/// digits, `.`, `_` and `-`, lower-cased; anything else becomes `-`, and no actor reads `me`.
pub fn proposal_name(actor: Option<&str>, month: u32, day: u32, taken: &[String]) -> String {
    let actor: String = actor
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let actor = actor.trim_matches(|c| matches!(c, '-' | '.'));
    let actor = if actor.is_empty() { "me" } else { actor };
    let stem = format!("{PROPOSAL_PREFIX}{actor}/{month:02}{day:02}");
    (1..)
        .map(|n| format!("{stem}-{n}"))
        .find(|name| !taken.contains(name))
        .unwrap_or(stem)
}

impl GraphSession {
    /// Whether the graph treats `branch` as protected, as far as Studio can tell (spec 038):
    /// `main`, until the server says which branches its policy protects (E63).
    pub fn protects(&self, branch: &str) -> bool {
        branch == PROTECTED_WITHOUT_POLICY
    }

    /// Whether an edit asked of `branch` lands on a proposal: a protected branch unless the
    /// person turned proposals off there, another branch only where they turned them on.
    pub fn proposes_on(&self, branch: &str) -> bool {
        if self.proposes_by_default(branch) {
            !self.propose_off.contains(branch)
        } else {
            self.propose_on.contains(branch)
        }
    }

    /// Whether edits of `branch` are proposed when the person has said nothing.
    fn proposes_by_default(&self, branch: &str) -> bool {
        self.propose_protected && self.protects(branch)
    }

    /// Turns proposals on or off for edits asked of `branch`. Turned off while the window is
    /// on a proposal into it, the window goes back to it: its edits commit there again.
    pub fn set_proposing(&mut self, branch: &str, on: bool, cx: &mut Context<Self>) {
        if self.proposes_by_default(branch) {
            if on {
                self.propose_off.remove(branch);
            } else {
                self.propose_off.insert(branch.to_owned());
            }
        } else if on {
            self.propose_on.insert(branch.to_owned());
        } else {
            self.propose_on.remove(branch);
        }
        if !on
            && self
                .proposal
                .as_ref()
                .is_some_and(|open| open.base == branch)
        {
            self.stop_proposing(cx);
        }
        cx.emit(SessionEvent::ProposingChanged);
        cx.notify();
    }

    /// The branches the person turned proposals off on (protected ones) and on (the others),
    /// as a window saves them.
    pub fn proposing_switches(&self) -> (Vec<String>, Vec<String>) {
        let mut off: Vec<String> = self.propose_off.iter().cloned().collect();
        let mut on: Vec<String> = self.propose_on.iter().cloned().collect();
        off.sort();
        on.sort();
        (off, on)
    }

    /// Takes the switches a window saved ([`Self::proposing_switches`]) and, when it was on a
    /// proposal, the branch that proposal proposes into.
    pub fn restore_proposing(
        &mut self,
        off: Vec<String>,
        on: Vec<String>,
        proposing_into: Option<String>,
    ) {
        self.propose_off = off.into_iter().collect();
        self.propose_on = on.into_iter().collect();
        self.proposal = proposing_into.map(|base| Proposal {
            base,
            branch: self.branch.clone(),
        });
    }

    /// The proposal the window is on, if it is on one.
    pub fn proposal(&self) -> Option<&Proposal> {
        self.proposal
            .as_ref()
            .filter(|open| open.branch == self.branch)
    }

    /// The window leaves its proposal for the branch it proposes into. The proposal's branch
    /// stays, listed with the others, and the next edit there starts another.
    pub fn stop_proposing(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = self.proposal.take() {
            self.set_branch(open.base, cx);
            cx.emit(SessionEvent::ProposingChanged);
            cx.notify();
        }
    }

    /// How many of Studio's own writes this session committed to the proposal the window is
    /// on: what the top bar counts. A window reopened on a proposal counts from then.
    pub fn proposal_edits(&self) -> usize {
        let Some(open) = self.proposal() else {
            return 0;
        };
        self.outcomes
            .iter()
            .filter(|entry| {
                entry.branch == open.branch
                    && entry.merge.is_none()
                    && matches!(
                        entry.outcome,
                        super::OutcomeKind::Committed { commit: Some(_) }
                    )
            })
            .count()
    }

    /// Where a write asked of `branch` lands, in words ([`WriteTarget::label`]): what an
    /// editor's hint names after "write".
    pub fn writes_to(&self, branch: &str) -> String {
        self.write_target(branch).label()
    }

    /// Where a write asked of `branch` lands (spec 038): the proposal the window is on, when it
    /// proposes into `branch`; a new proposal, when edits of `branch` are proposed; else the
    /// branch itself.
    pub fn write_target(&self, branch: &str) -> WriteTarget {
        if let Some(open) = self.proposal()
            && open.base == branch
        {
            return WriteTarget::Proposal {
                base: open.base.clone(),
                branch: open.branch.clone(),
            };
        }
        if self.proposes_on(branch) {
            WriteTarget::NewProposal {
                base: branch.to_owned(),
            }
        } else {
            WriteTarget::Branch(branch.to_owned())
        }
    }

    /// Sends a write asked of `asked` where [`Self::write_target`] says: `send` is given the
    /// branch to write to. A write that starts a proposal makes its branch first, at
    /// `start_at` (the commit the write is bound to, so it holds there) or at the head of
    /// `asked`, then sends the write there once; the window moves to the proposal once the
    /// write went through. A proposal that could not be made sends nothing, so the write is
    /// refused with why, and its draft stays. Writes asked while it is being made wait for it
    /// and go there too.
    pub(super) fn routed_write<T: Send + 'static>(
        &mut self,
        asked: &str,
        start_at: Option<String>,
        send: impl FnOnce(&mut Self, String, &mut Context<Self>) -> Task<WriteOutcome<T>> + 'static,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<T>> {
        let base = match self.write_target(asked) {
            WriteTarget::Branch(branch) | WriteTarget::Proposal { branch, .. } => {
                return send(self, branch, cx);
            }
            WriteTarget::NewProposal { base } => base,
        };
        let start = self.start_proposal(&base, start_at, cx);
        // Runs to its end whether or not the caller waits, as every write does
        // ([`GraphSession::to_outcome`]).
        let work = cx.spawn(async move |this, cx| {
            let branch = match start.await {
                Ok(branch) => branch,
                Err(reason) => return WriteOutcome::NotSent { reason },
            };
            let Ok(write) = this.update(cx, |this, cx| send(this, branch.clone(), cx)) else {
                return WriteOutcome::NotSent {
                    reason: "Studio closed the graph before the edit was sent".into(),
                };
            };
            let outcome = write.await;
            if matches!(outcome, WriteOutcome::Committed(_)) {
                this.update(cx, |this, cx| this.enter_proposal(base, branch, cx))
                    .ok();
            }
            outcome
        });
        Self::to_outcome(work, cx)
    }

    /// Makes the proposal branch a write starts, once for every write asked of `base` while
    /// it is being made: what each of them waits for is its name, or why it was not made.
    fn start_proposal(
        &mut self,
        base: &str,
        start_at: Option<String>,
        cx: &mut Context<Self>,
    ) -> Starting {
        if let Some((starting, task)) = &self.proposal_start
            && starting == base
        {
            return task.clone();
        }
        let mut taken: Vec<String> = match &self.branches {
            Loadable::Loaded(branches) => branches.clone(),
            _ => Vec::new(),
        };
        taken.extend(self.proposals_made.iter().cloned());
        let today = chrono::Local::now();
        let name = {
            use chrono::Datelike as _;
            proposal_name(self.actor(), today.month(), today.day(), &taken)
        };
        self.proposals_made.push(name.clone());
        let start = match start_at {
            Some(commit) => BranchStart::Commit(commit),
            None => BranchStart::Head(base.to_owned()),
        };
        let create = self.create_branch(name.clone(), start, cx);
        let task = cx
            .spawn(async move |this, cx| {
                let outcome = create.await;
                this.update(cx, |this, _| this.proposal_start = None).ok();
                match outcome {
                    WriteOutcome::Committed(_) => Ok(name),
                    WriteOutcome::Refused(error) => Err(format!(
                        "The proposal branch {name} could not be made: {}",
                        studio_ui::clause(error.reason())
                    )),
                    WriteOutcome::NotSent { reason } => Err(format!(
                        "The proposal branch {name} was not made: {}",
                        studio_ui::clause(&reason)
                    )),
                    WriteOutcome::Unknown { reason } => Err(format!(
                        "Whether the proposal branch {name} was made is not known ({}); \
                         the branch list will say",
                        studio_ui::clause(&reason)
                    )),
                }
            })
            .shared();
        self.proposal_start = Some((base.to_owned(), task.clone()));
        task
    }

    /// A write went through on the proposal `branch` it started from `base`: a window still on
    /// `base` moves to it, so what it shows is what was proposed. One that moved elsewhere
    /// meanwhile stays where it is; the outcome names the proposal.
    fn enter_proposal(&mut self, base: String, branch: String, cx: &mut Context<Self>) {
        if self.branch != base {
            return;
        }
        self.set_branch(branch.clone(), cx);
        self.proposal = Some(Proposal { base, branch });
        cx.emit(SessionEvent::ProposingChanged);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::AsyncReadExt as _;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use graph_client::api::ChangeRequest;
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};
    use serde_json::{Value, json};

    use super::*;
    use crate::connection::{Connection, ServerEntry, ServerSource};

    /// Every write request sent: its method, path and body.
    type Sent = Arc<Mutex<Vec<(String, String, Value)>>>;

    /// A session on `main` of a server that makes branches unless `refuse_branches`, and
    /// commits every mutation on the branch it names.
    fn proposing_session(
        refuse_branches: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<GraphSession>, Sent) {
        let sent: Sent = Arc::default();
        let seen = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            let method = request.method().to_string();
            let path = request.uri().path().to_owned();
            let seen = seen.clone();
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let writes =
                    method == "POST" && (path.ends_with("/branches") || path.ends_with("/mutate"));
                if writes {
                    seen.lock()
                        .unwrap()
                        .push((method, path.clone(), body.clone()));
                }
                let (status, answer) = if writes && path.ends_with("/branches") {
                    if refuse_branches {
                        (
                            403,
                            json!({ "error": "policy denied", "code": "forbidden",
                                    "disposition": { "action": "refresh", "effects": "none" } }),
                        )
                    } else {
                        (
                            200,
                            json!({ "uri": "u", "from": body["from"], "name": body["name"],
                                    "from_commit_id": "m1" }),
                        )
                    }
                } else if writes {
                    let n = seen.lock().unwrap().len();
                    (
                        200,
                        json!({ "branch": body["branch"], "query_name": "q",
                                "affected_nodes": 1, "affected_edges": 0,
                                "commit": { "graph_commit_id": format!("c{n}"),
                                            "graph_manifest_version": n, "kind": "published",
                                            "created_at": 0, "parent_commit_id": "m1" } }),
                    )
                } else {
                    (200, json!({ "branches": ["main"] }))
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(answer.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        cx.update(|cx| cx.set_global(ProposeProtected(true)));
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        (session, sent)
    }

    fn edit(branch: &str) -> ChangeRequest {
        ChangeRequest {
            query: "query q() { update Decision set { status: \"decided\" } }".into(),
            name: Some("q".into()),
            params: None,
            branch: Some(branch.into()),
        }
    }

    fn today_proposal() -> String {
        use chrono::Datelike as _;
        let today = chrono::Local::now();
        proposal_name(None, today.month(), today.day(), &[])
    }

    /// The first edit of main makes a proposal from main's head and lands there; one asked
    /// while it is made waits and goes there too, and the window moves to it. Edits after go
    /// to it, and with proposals turned off main takes them again.
    #[gpui::test]
    async fn an_edit_of_main_lands_on_a_proposal_the_window_moves_to(cx: &mut TestAppContext) {
        let (session, sent) = proposing_session(false, cx);
        let proposal = today_proposal();
        session.read_with(cx, |session, _| {
            assert_eq!(
                session.write_target("main"),
                WriteTarget::NewProposal {
                    base: "main".into()
                }
            );
            assert_eq!(
                session.write_target("fix"),
                WriteTarget::Branch("fix".into())
            );
        });
        let first = session.update(cx, |session, cx| {
            session.write_request(edit("main"), None, "Set status".into(), cx)
        });
        let second = session.update(cx, |session, cx| {
            session.write_request(edit("main"), None, "Set name".into(), cx)
        });
        assert!(matches!(first.await, WriteOutcome::Committed(_)));
        assert!(matches!(second.await, WriteOutcome::Committed(_)));
        cx.run_until_parked();
        let third = session.update(cx, |session, cx| {
            session.write_request(edit("main"), None, "Set brief".into(), cx)
        });
        assert!(matches!(third.await, WriteOutcome::Committed(_)));
        cx.run_until_parked();
        {
            let sent = sent.lock().unwrap();
            let sent: Vec<(&str, &Value)> = sent
                .iter()
                .map(|(_, path, body)| (path.rsplit('/').next().unwrap(), body))
                .collect();
            assert_eq!(sent.len(), 4, "{sent:?}");
            assert_eq!(sent[0].0, "branches");
            assert_eq!(sent[0].1["name"], json!(proposal));
            assert_eq!(sent[0].1["from"], json!("main"));
            for (route, body) in &sent[1..] {
                assert_eq!(*route, "mutate");
                assert_eq!(
                    body["branch"],
                    json!(proposal),
                    "every edit on the proposal"
                );
            }
        }
        session.read_with(cx, |session, _| {
            assert_eq!(session.branch, proposal, "the window shows the proposal");
            assert_eq!(
                session.proposal(),
                Some(&Proposal {
                    base: "main".into(),
                    branch: proposal.clone()
                })
            );
            assert_eq!(session.proposal_edits(), 3);
            assert_eq!(session.head(&proposal), Some("c4"));
            assert_eq!(session.head("main"), None, "main never moved");
        });

        session.update(cx, |session, cx| session.set_proposing("main", false, cx));
        session.read_with(cx, |session, _| {
            assert_eq!(session.branch, "main", "back on main");
            assert_eq!(session.proposal(), None);
            assert_eq!(
                session.write_target("main"),
                WriteTarget::Branch("main".into())
            );
        });
        let direct = session.update(cx, |session, cx| {
            session.write_request(edit("main"), None, "Set status".into(), cx)
        });
        assert!(matches!(direct.await, WriteOutcome::Committed(_)));
        let last = sent.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last.2["branch"], json!("main"));
    }

    /// A proposal the server will not make sends no edit: the write is refused with why, and
    /// the window stays on main.
    #[gpui::test]
    async fn a_proposal_that_cannot_be_made_sends_nothing(cx: &mut TestAppContext) {
        let (session, sent) = proposing_session(true, cx);
        let write = session.update(cx, |session, cx| {
            session.write_request(edit("main"), None, "Set status".into(), cx)
        });
        match write.await {
            WriteOutcome::NotSent { reason } => {
                assert!(reason.contains("could not be made"), "{reason}")
            }
            _ => panic!("the edit was sent"),
        }
        cx.run_until_parked();
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "only the branch was asked for");
        assert!(sent[0].1.ends_with("/branches"));
        session.read_with(cx, |session, _| {
            assert_eq!(session.branch, "main");
            assert_eq!(session.proposal(), None);
        });
    }

    #[test]
    fn a_proposal_is_named_by_its_actor_and_day_and_never_twice() {
        let taken = vec![
            "main".to_owned(),
            "proposal/act-writer/1118-1".to_owned(),
            "proposal/act-writer/1118-2".to_owned(),
        ];
        assert_eq!(
            proposal_name(Some("act-writer"), 11, 18, &taken),
            "proposal/act-writer/1118-3"
        );
        assert_eq!(
            proposal_name(Some("act-writer"), 3, 4, &taken),
            "proposal/act-writer/0304-1"
        );
        assert_eq!(
            proposal_name(Some("Ada Lovelace <ada@x.io>"), 11, 18, &[]),
            "proposal/ada-lovelace--ada-x.io/1118-1"
        );
        assert_eq!(proposal_name(None, 11, 18, &[]), "proposal/me/1118-1");
        assert_eq!(proposal_name(Some("::"), 11, 18, &[]), "proposal/me/1118-1");
    }

    #[test]
    fn a_target_names_where_the_write_lands() {
        assert_eq!(WriteTarget::Branch("fix".into()).label(), "fix");
        assert_eq!(
            WriteTarget::NewProposal {
                base: "main".into()
            }
            .label(),
            "a new proposal from main"
        );
        let open = WriteTarget::Proposal {
            base: "main".into(),
            branch: "proposal/me/1118-1".into(),
        };
        assert_eq!(open.label(), "proposal/me/1118-1");
        assert!(open.proposes());
        assert_eq!(open.branch(), Some("proposal/me/1118-1"));
    }
}
