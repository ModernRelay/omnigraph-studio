//! Proposals (spec 037): an edit on a branch the graph protects starts a branch of its own,
//! `proposal/<actor>/<MMDD>-<n>`, and lands there; the window moves onto it and says it is
//! proposing, and the proposal reaches its base when it is reviewed and merged.
//!
//! Which branches are protected is the policy's (`protected_branches`), and the access route
//! does not say (E63). Until it does, `main` is protected in a window's session while the
//! graph's `propose_changes` setting is on, which it is unless the person turns it off.

use futures::channel::oneshot;
use gpui::{App, Context, Task};
use graph_client::WriteOutcome;
use graph_client::api::BranchCreateOutput;

use super::{BranchStart, GraphSession};

/// The branch a proposal goes into when it is merged, while the server cannot say which
/// branches its policy protects (E63).
pub const PROTECTED: &str = "main";

/// What every proposal's branch is named under.
pub const PREFIX: &str = "proposal/";

/// The proposal a session is on: its branch, the branch it proposes to, and the changes Studio
/// made on it since the session moved there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub base: String,
    pub branch: String,
    pub changes: u64,
}

/// Where an edit sent to a branch lands (spec 037).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Landing {
    /// On the branch itself.
    Branch(String),
    /// On the proposal the session is on, which proposes to the branch.
    Proposal(String),
    /// On a proposal started from the branch by this edit.
    NewProposal { from: String },
}

impl Landing {
    /// The branch the edit is sent to, once known.
    pub fn branch(&self) -> Option<&str> {
        match self {
            Landing::Branch(branch) | Landing::Proposal(branch) => Some(branch),
            Landing::NewProposal { .. } => None,
        }
    }

    /// Where it lands, in words a write surface ends with ("write · main").
    pub fn words(&self) -> String {
        match self {
            Landing::Branch(branch) | Landing::Proposal(branch) => branch.clone(),
            Landing::NewProposal { from } => format!("a new proposal from {from}"),
        }
    }
}

/// A proposal's branch made for an edit that did not land: the next edit from the same
/// commit takes it rather than starting another.
#[derive(Clone, Debug)]
pub(super) struct Spare {
    base: String,
    branch: String,
    start: String,
}

/// A start under way: the edits sent meanwhile wait for it, and go to its branch.
#[derive(Default)]
pub(super) struct Starting {
    waiters: Vec<oneshot::Sender<Option<String>>>,
}

impl GraphSession {
    /// Whether an edit on `branch` starts a proposal in this session (E63's stand-in): only
    /// `main`, only in a window's session, and only while the graph's setting is on.
    pub fn protects(&self, branch: &str, cx: &App) -> bool {
        self.proposes && branch == PROTECTED && self.options(cx).propose_changes
    }

    /// Turns proposals on for this session as the graph's setting says: what a window's
    /// session does ([`crate::open_session`]). A session made otherwise (a test's, a CLI's)
    /// writes where it is told.
    pub fn follow_proposal_setting(&mut self) {
        self.proposes = true;
    }

    /// The proposal the session is on: its branch is named `proposal/…`, which only Studio's
    /// proposals are, and it proposes to `main` (E63).
    pub fn proposal(&self) -> Option<Proposal> {
        self.branch.starts_with(PREFIX).then(|| Proposal {
            base: PROTECTED.to_owned(),
            branch: self.branch.clone(),
            changes: self.proposal_changes,
        })
    }

    /// Where an edit sent to `branch` lands: there, on the proposal the session is on when
    /// `branch` is protected, or on a new proposal from it.
    pub fn lands_on(&self, branch: &str, cx: &App) -> Landing {
        if !self.protects(branch, cx) {
            return Landing::Branch(branch.to_owned());
        }
        match self.proposal() {
            Some(proposal) if proposal.base == branch => Landing::Proposal(proposal.branch),
            _ => Landing::NewProposal {
                from: branch.to_owned(),
            },
        }
    }

    /// Why an edit on `branch` is not offered, as the access route says: none where the edit
    /// starts a proposal, whose start the server judges when it is sent (a policy that lets the
    /// credential make no branch refuses it then, and the draft stays).
    pub fn edit_refusal(&self, branch: &str, cx: &App) -> Option<String> {
        if self.protects(branch, cx) {
            return None;
        }
        self.refusal(
            graph_client::api::AccessAction::Change,
            &crate::AccessScope::branch(branch),
        )
    }

    /// Where an edit on `branch` lands, in words: what every write surface names (constitution
    /// IV).
    pub fn write_target(&self, branch: &str, cx: &App) -> String {
        self.lands_on(branch, cx).words()
    }

    /// Back on the branch the proposal proposes to; the proposal stays a branch, listed with
    /// the others, to review and merge later.
    pub fn stop_proposing(&mut self, cx: &mut Context<Self>) {
        if let Some(proposal) = self.proposal() {
            self.set_branch(proposal.base, cx);
        }
    }

    /// Sends an edit meant for `branch` where it lands ([`Self::lands_on`]): `send` is given the
    /// branch and the commit it is bound to. On a new proposal it is sent once the proposal's
    /// branch starts at `if_commit` (or `branch`'s head, for an edit bound to no commit), bound
    /// to the same commit, so it means there what it meant on `branch`; once it lands the
    /// session moves onto the proposal. A start refused, or with no answer, sends nothing.
    pub(crate) fn routed<T: Send + 'static>(
        &mut self,
        branch: &str,
        if_commit: Option<String>,
        send: impl FnOnce(
            &mut Self,
            String,
            Option<String>,
            &mut Context<Self>,
        ) -> Task<WriteOutcome<T>>
        + 'static,
        cx: &mut Context<Self>,
    ) -> Task<WriteOutcome<T>> {
        let from = match self.lands_on(branch, cx) {
            Landing::Branch(branch) | Landing::Proposal(branch) => {
                return send(self, branch, if_commit, cx);
            }
            Landing::NewProposal { from } => from,
        };
        let started = self.start_proposal(&from, if_commit.clone(), cx);
        let work = cx.spawn(async move |this, cx| {
            let proposal = match started.await {
                Ok(proposal) => proposal,
                Err(outcome) => return outcome,
            };
            let Ok(write) = this.update(cx, |this, cx| send(this, proposal.clone(), if_commit, cx))
            else {
                return WriteOutcome::NotSent {
                    reason: "the graph was closed".into(),
                };
            };
            let outcome = write.await;
            this.update(cx, |this, cx| {
                this.proposal_wrote(&from, &proposal, &outcome, cx)
            })
            .ok();
            outcome
        });
        Self::to_outcome(work, cx)
    }

    /// The first edit of a proposal ended: landed, or perhaps landed, the session moves onto
    /// it; refused, its branch is kept for the next edit from the same commit.
    fn proposal_wrote<T>(
        &mut self,
        from: &str,
        proposal: &str,
        outcome: &WriteOutcome<T>,
        cx: &mut Context<Self>,
    ) {
        if self
            .opening_proposal
            .as_ref()
            .is_some_and(|(_, opening)| opening == proposal)
        {
            self.opening_proposal = None;
        }
        match outcome {
            WriteOutcome::Committed(_) | WriteOutcome::Unknown { .. } => {
                self.spare_proposal = None;
                if self.branch == from {
                    // The changes made on it while it was opening, counted as they landed.
                    let changes = self.proposal_changes;
                    self.set_branch(proposal.to_owned(), cx);
                    self.proposal_changes = changes;
                    cx.notify();
                }
            }
            WriteOutcome::Refused(_) | WriteOutcome::NotSent { .. } => {
                if let Some(start) = self.head(proposal).map(str::to_owned) {
                    self.spare_proposal = Some(Spare {
                        base: from.to_owned(),
                        branch: proposal.to_owned(),
                        start,
                    });
                }
            }
        }
    }

    /// The branch an edit's proposal goes to: one started at `at` (or `from`'s head), a spare
    /// that starts there, or the one another edit is starting meanwhile. Fails with what the
    /// edit's outcome is then: nothing was sent.
    fn start_proposal<T: Send + 'static>(
        &mut self,
        from: &str,
        at: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<String, WriteOutcome<T>>> {
        // One made whose first edit is still out takes this edit too: the window is not on it
        // yet, and another would split the person's changes.
        if let Some((base, branch)) = &self.opening_proposal
            && base == from
        {
            return Task::ready(Ok(branch.clone()));
        }
        let start_commit = at.clone().or_else(|| self.head(from).map(str::to_owned));
        if let Some(spare) = &self.spare_proposal
            && spare.base == from
            && Some(&spare.start) == start_commit.as_ref()
        {
            return Task::ready(Ok(spare.branch.clone()));
        }
        if let Some(starting) = &mut self.starting_proposal {
            let (tx, rx) = oneshot::channel();
            starting.waiters.push(tx);
            return cx.spawn(async move |_, _| match rx.await {
                Ok(Some(branch)) => Ok(branch),
                _ => Err(WriteOutcome::NotSent {
                    reason: "the proposal it was to go to could not be started".into(),
                }),
            });
        }
        let name = self.proposal_name(cx);
        let start = match at {
            Some(commit) => BranchStart::Commit(commit),
            None => BranchStart::Head(from.to_owned()),
        };
        self.starting_proposal = Some(Starting::default());
        let create = self.create_branch(name.clone(), start, cx);
        let base = from.to_owned();
        cx.spawn(async move |this, cx| {
            let created = create.await;
            let branch = matches!(created, WriteOutcome::Committed(_)).then(|| name.clone());
            this.update(cx, |this, _| {
                if let Some(branch) = &branch {
                    this.opening_proposal = Some((base, branch.clone()));
                }
                for waiter in this
                    .starting_proposal
                    .take()
                    .map(|starting| starting.waiters)
                    .unwrap_or_default()
                {
                    waiter.send(branch.clone()).ok();
                }
            })
            .ok();
            started(created, &name)
        })
    }

    /// A new proposal's name: `proposal/<actor>/<MMDD>-<n>`, with the first `n` no branch
    /// listed holds, the date in UTC; without an actor (a server with no authentication),
    /// `proposal/<MMDD>-<n>`.
    fn proposal_name(&self, _cx: &App) -> String {
        let actor = self.actor().map(branch_word);
        let day = month_day(std::time::SystemTime::now());
        let stem = match actor {
            Some(actor) if !actor.is_empty() => format!("{PREFIX}{actor}/{day}"),
            _ => format!("{PREFIX}{day}"),
        };
        let taken = |name: &str| {
            self.branches
                .loaded()
                .is_some_and(|list| list.iter().any(|branch| branch == name))
                || self
                    .spare_proposal
                    .as_ref()
                    .is_some_and(|spare| spare.branch == name)
        };
        (1..)
            .map(|n| format!("{stem}-{n}"))
            .find(|name| !taken(name))
            .expect("some number is free")
    }
}

/// What a proposal's start means for the edit waiting on it: its branch, or the edit's outcome,
/// which sent nothing.
// Made once per start and matched at once, into the edit's outcome: boxing it saves nothing.
#[expect(clippy::result_large_err)]
fn started<T>(
    created: WriteOutcome<BranchCreateOutput>,
    name: &str,
) -> Result<String, WriteOutcome<T>> {
    match created {
        WriteOutcome::Committed(_) => Ok(name.to_owned()),
        // The server's own refusal, a 403 shown as a denial (constitution IV).
        WriteOutcome::Refused(error) => Err(WriteOutcome::Refused(error)),
        WriteOutcome::NotSent { reason } => Err(WriteOutcome::NotSent { reason }),
        WriteOutcome::Unknown { .. } => Err(WriteOutcome::NotSent {
            reason: format!(
                "Studio did not hear whether the proposal {name} was started; the Branches panel \
                 lists it if it was"
            ),
        }),
    }
}

/// An actor's id as one word of a branch name: letters, digits, `.`, `_` and `-` kept, anything
/// else a `-`.
pub(crate) fn branch_word(actor: &str) -> String {
    actor
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

/// `time`'s month and day in UTC, as `MMDD`.
pub(crate) fn month_day(time: std::time::SystemTime) -> String {
    let days = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() / 86_400)
        .unwrap_or(0) as i64;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{month:02}{day:02}")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::AsyncReadExt as _;
    use gpui::{AppContext as _, TestAppContext};
    use graph_client::api::ChangeRequest;
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};
    use serde_json::{Value, json};

    use super::*;
    use crate::connection::{Connection, ServerEntry, ServerSource};

    /// What the fake server was sent: each request's method, path, body and bound commit.
    type Sent = Arc<Mutex<Vec<(String, String, Value, Option<String>)>>>;

    /// A session on `main` of a server that makes branches (or refuses to, with `refuse`) and
    /// commits every mutation as `n1`, `n2`, … on the branch it names.
    fn proposing_session(
        refuse: bool,
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<GraphSession>, Sent) {
        let sent: Sent = Arc::default();
        let seen = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            let method = request.method().as_str().to_owned();
            let path = request.uri().path().to_owned();
            let bound = request
                .headers()
                .get(graph_client::IF_GRAPH_COMMIT_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let seen = seen.clone();
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let asked: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let mut status = 200;
                let body = if path.ends_with("/access") {
                    json!({ "graph_id": "knowledge", "actor_id": "act-writer", "graph_wide": ["read", "change", "branch_create"], "actions": ["read", "change"] })
                } else if method == "POST" && path.ends_with("/branches") {
                    seen.lock()
                        .unwrap()
                        .push((method, path, asked.clone(), bound));
                    if refuse {
                        status = 403;
                        json!({ "error": "branch_create is not allowed", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } })
                    } else {
                        let at = asked.get("from_commit").cloned().unwrap_or(json!("m9"));
                        json!({ "uri": "u", "from": "main", "name": asked["name"], "from_commit_id": at })
                    }
                } else if path.contains("/mutate") {
                    let mut seen = seen.lock().unwrap();
                    let n = seen
                        .iter()
                        .filter(|(_, path, ..)| path.contains("/mutate"))
                        .count()
                        + 1;
                    let branch = asked["branch"].clone();
                    seen.push((method, path, asked, bound));
                    json!({ "branch": branch, "query_name": "set", "affected_nodes": 1, "affected_edges": 0, "commit": { "graph_commit_id": format!("n{n}"), "graph_manifest_version": n, "kind": "published", "created_at": n } })
                } else if path.ends_with("/schema") {
                    json!({ "schema_source": "node Person {\n  slug: String @key\n}\n" })
                } else if path.ends_with("/query") {
                    json!({ "query_name": "q", "target": { "branch": "main" }, "row_count": 0, "rows": [], "graph_commit_id": "m9" })
                } else {
                    // The branches made so far.
                    let mut branches = vec![json!("main")];
                    for (_, path, body, _) in seen.lock().unwrap().iter() {
                        if path.ends_with("/branches") && !refuse {
                            branches.push(body["name"].clone());
                        }
                    }
                    json!({ "branches": branches })
                };
                Ok(Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session = cx.new(|cx| {
            let mut session = GraphSession::new(connection, "knowledge".into(), "main".into(), cx);
            session.follow_proposal_setting();
            session
        });
        cx.run_until_parked();
        (session, sent)
    }

    fn edit(branch: &str) -> ChangeRequest {
        ChangeRequest {
            query: "query set() { update Person set { slug: \"a\" } }".into(),
            name: Some("set".into()),
            params: None,
            branch: Some(branch.into()),
        }
    }

    /// The first edit on main starts `proposal/<actor>/<MMDD>-1` at the commit the edit was
    /// read at, and lands there bound to that commit; the session moves onto the proposal, and
    /// the next edit, sent to main or to the proposal, goes to it with no second start. Stopping
    /// goes back to main, and the proposal stays a branch.
    #[gpui::test]
    async fn the_first_edit_on_main_starts_a_proposal_and_lands_there(cx: &mut TestAppContext) {
        let (session, sent) = proposing_session(false, cx);
        let name = format!(
            "proposal/act-writer/{}-1",
            month_day(std::time::SystemTime::now())
        );
        session.read_with(cx, |session, cx| {
            assert!(session.protects("main", cx));
            assert_eq!(session.write_target("main", cx), "a new proposal from main");
            assert_eq!(session.write_target("feature", cx), "feature");
            assert_eq!(session.edit_refusal("main", cx), None);
        });
        let write = session.update(cx, |session, cx| {
            session.write_request(edit("main"), Some("c7".into()), "Set slug".into(), cx)
        });
        let outcome = write.await;
        assert!(matches!(outcome, WriteOutcome::Committed(_)), "{outcome:?}");
        cx.run_until_parked();
        session.read_with(cx, |session, cx| {
            assert_eq!(session.branch, name, "the window moved onto the proposal");
            assert_eq!(
                session.proposal(),
                Some(Proposal {
                    base: "main".into(),
                    branch: name.clone(),
                    changes: 1
                })
            );
            assert_eq!(
                session.lands_on("main", cx),
                Landing::Proposal(name.clone())
            );
            assert_eq!(session.write_target("main", cx), name);
        });
        for branch in ["main", name.as_str()] {
            let write = session.update(cx, |session, cx| {
                session.write_request(edit(branch), None, "Set slug".into(), cx)
            });
            assert!(matches!(write.await, WriteOutcome::Committed(_)));
        }
        cx.run_until_parked();
        let sent: Vec<(String, Value, Option<String>)> = sent
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path, body, bound)| {
                let what = if path.ends_with("/branches") {
                    "create"
                } else {
                    "mutate"
                };
                let at = body.get("from_commit").or(body.get("branch")).cloned();
                (what.to_owned(), at.unwrap_or(Value::Null), bound.clone())
            })
            .collect();
        assert_eq!(
            sent,
            [
                ("create".into(), json!("c7"), None),
                ("mutate".into(), json!(name), Some("c7".into())),
                ("mutate".into(), json!(name), None),
                ("mutate".into(), json!(name), None),
            ]
        );
        session.update(cx, |session, cx| {
            assert_eq!(session.proposal().map(|p| p.changes), Some(3));
            session.stop_proposing(cx);
            assert_eq!(session.branch, "main");
            assert_eq!(session.proposal(), None);
        });
    }

    /// Two edits sent on main before either lands go to one proposal: the second waits for the
    /// first's start, and one sent once the branch is made but before the first edit lands goes
    /// there too, not to a second proposal.
    #[gpui::test]
    async fn edits_sent_together_go_to_one_proposal(cx: &mut TestAppContext) {
        let (session, sent) = proposing_session(false, cx);
        let (first, second) = session.update(cx, |session, cx| {
            (
                session.write_request(edit("main"), None, "Set slug".into(), cx),
                session.write_request(edit("main"), None, "Set slug".into(), cx),
            )
        });
        assert!(matches!(first.await, WriteOutcome::Committed(_)));
        assert!(matches!(second.await, WriteOutcome::Committed(_)));
        cx.run_until_parked();
        let creates = sent
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, path, ..)| path.ends_with("/branches"))
            .count();
        assert_eq!(creates, 1, "one proposal");
        session.update(cx, |session, cx| {
            assert_eq!(session.proposal().map(|p| p.changes), Some(2));
            // Made, its first edit not back: an edit now joins it.
            session.set_branch("main".into(), cx);
            session.opening_proposal = Some(("main".into(), "proposal/act-writer/x".into()));
        });
        let third = session.update(cx, |session, cx| {
            session.write_request(edit("main"), None, "Set slug".into(), cx)
        });
        assert!(matches!(third.await, WriteOutcome::Committed(_)));
        let sent = sent.lock().unwrap();
        let (_, path, body, _) = sent.last().unwrap();
        assert!(path.contains("/mutate"));
        assert_eq!(body["branch"], "proposal/act-writer/x");
    }

    /// A start the policy refuses sends no edit: the edit is refused with the server's words,
    /// and the session stays on main.
    #[gpui::test]
    async fn a_refused_start_sends_nothing(cx: &mut TestAppContext) {
        let (session, sent) = proposing_session(true, cx);
        let write = session.update(cx, |session, cx| {
            session.write_request(edit("main"), None, "Set slug".into(), cx)
        });
        assert!(matches!(
            write.await,
            WriteOutcome::Refused(error) if error.reason().contains("branch_create")
        ));
        cx.run_until_parked();
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .all(|(_, path, ..)| path.ends_with("/branches")),
            "no mutation was sent"
        );
        session.read_with(cx, |session, _| assert_eq!(session.branch, "main"));
    }

    /// A session that does not follow the setting (one made outside a window) writes where it
    /// is told, as does one whose graph turned proposals off.
    #[gpui::test]
    async fn without_proposals_an_edit_writes_main(cx: &mut TestAppContext) {
        let (session, sent) = proposing_session(false, cx);
        session.update(cx, |session, _| session.proposes = false);
        let write = session.update(cx, |session, cx| {
            session.write_request(edit("main"), None, "Set slug".into(), cx)
        });
        assert!(matches!(write.await, WriteOutcome::Committed(_)));
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].2["branch"], "main");
    }

    #[test]
    fn a_proposal_is_named_by_its_actor_and_day() {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_795_046_400);
        assert_eq!(month_day(at), "1119", "2026-11-19");
        assert_eq!(month_day(std::time::UNIX_EPOCH), "0101");
        let leap = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_709_164_800);
        assert_eq!(month_day(leap), "0229", "2024-02-29");
        assert_eq!(branch_word("act-writer"), "act-writer");
        assert_eq!(branch_word("user:ada@example.com"), "user-ada-example.com");
    }
}
