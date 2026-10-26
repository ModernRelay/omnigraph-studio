//! The activity log (spec 002): every write and its outcome, newest first.

use std::ops::Range;
use std::time::Duration;

use gpui::{Context, Entity, Render, Subscription, Task, WeakEntity, Window, uniform_list};
use graph_store::{
    GraphSession, HeldAs, Loadable, MergeSettled, OutcomeEntry, OutcomeKind, SessionEvent,
};
use studio_actions::graph::OpenHistory;
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;

/// How often the log draws again while it lists writes, for the times they read ("5 min ago"),
/// which change by the minute.
const TIMES_EVERY: Duration = Duration::from_secs(60);

pub struct ActivityLog {
    session: Option<Entity<GraphSession>>,
    /// How many outcomes the session had logged ([`GraphSession::outcomes_logged`]), and how
    /// many merges it had settled ([`GraphSession::merges_settled`]), when the log last drew: a
    /// session event that changed neither (a read starting or ending, a head seen) leaves the
    /// log as it was.
    logged: (u64, u64),
    /// Draws the log again each minute while it lists writes.
    _times: Option<Task<()>>,
    #[cfg(test)]
    renders: usize,
    _subscriptions: Vec<Subscription>,
}

impl ActivityLog {
    /// `session` is the workspace's session now; later changes arrive as `SessionChanged`,
    /// which may have been emitted before this view existed.
    pub fn new(
        workspace: WeakEntity<Workspace>,
        session: Option<Entity<GraphSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            session: None,
            logged: (0, 0),
            _times: None,
            #[cfg(test)]
            renders: 0,
            _subscriptions: Vec::new(),
        };
        if let Some(workspace) = workspace.upgrade() {
            this._subscriptions.push(cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, _window, cx| match event {
                    WorkspaceEvent::SessionChanged => {
                        this.watch(workspace.read(cx).session().cloned(), cx);
                    }
                },
            ));
        }
        this.watch(session, cx);
        this
    }

    fn watch(&mut self, session: Option<Entity<GraphSession>>, cx: &mut Context<Self>) {
        self._subscriptions.truncate(1);
        if let Some(session) = &session {
            // Every write's outcome is logged with this event, which every read sent and
            // answered also raises: only a new outcome, or a merge settled, draws the log. A new
            // branch list draws it while a row offers Open history, which it offers only on a
            // branch the list holds.
            self._subscriptions.push(cx.subscribe(
                session,
                |this, session, event: &SessionEvent, cx| {
                    match event {
                        SessionEvent::ActivityChanged
                            if logged(session.read(cx)) != this.logged =>
                        {
                            cx.notify();
                        }
                        SessionEvent::BranchesChanged
                            if session
                                .read(cx)
                                .outcomes
                                .iter()
                                .any(|entry| history_to_check(entry).is_some()) =>
                        {
                            cx.notify();
                        }
                        _ => {}
                    }
                },
            ));
        }
        self.session = session;
        self._times = None;
        cx.notify();
    }

    /// Draws the log again each minute while it lists writes, for the times they read.
    fn keep_times(&mut self, cx: &mut Context<Self>) {
        if self._times.is_some() {
            return;
        }
        self._times = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TIMES_EVERY).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        }));
    }

    /// Opens History on `branch`, where a write whose outcome nobody knows went: whether it
    /// landed is read there (constitution III). The graph is the one the window shows, whose
    /// writes the log lists; the window moves to the branch first, as a toast's Open history
    /// did before errors folded into the log. Never to a branch the session's list does not
    /// hold ([`history_refusal`]), which the row's button is disabled for.
    fn open_history(&mut self, branch: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        if history_refusal(&branch, &session.read(cx).branches).is_some() {
            return;
        }
        session.update(cx, |session, cx| session.set_branch(branch, cx));
        window.dispatch_action(Box::new(OpenHistory), cx);
    }
}

impl Render for ActivityLog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let (count, logged) = self.session.as_ref().map_or((0, (0, 0)), |session| {
            let session = session.read(cx);
            (session.outcomes.len(), logged(session))
        });
        self.logged = logged;
        if count == 0 {
            return studio_ui::ds::empty_state(IconName::BoltOutlined, "No writes yet")
                .into_any_element();
        }
        self.keep_times(cx);
        // Every row is a feed row, so only the visible ones are built, newest first.
        uniform_list(
            "activity-log",
            count,
            cx.processor(|this, range: Range<usize>, _window, cx| {
                let Some(session) = &this.session else {
                    return Vec::new();
                };
                let session = session.read(cx);
                let outcomes = &session.outcomes;
                range
                    .filter_map(|index| {
                        let entry = outcomes.get(outcomes.len().checked_sub(index + 1)?)?;
                        let row = activity_row(index, entry);
                        Some(match history_to_check(entry) {
                            Some(branch) => {
                                let refusal = history_refusal(branch, &session.branches);
                                row.action(open_history_button(index, branch, refusal, cx))
                            }
                            None => row,
                        })
                    })
                    .collect()
            }),
        )
        .size_full()
        .py(px(studio_ui::ds::space::XS))
        .into_any_element()
    }
}

/// What one write's row says of its outcome: the verb, the commit it names in the line (a held
/// one's, which is on no branch), the branch it went to with its preposition, or none when it is
/// on no branch, and the tooltip.
#[derive(Debug, PartialEq)]
struct Said {
    status: studio_ui::ds::Status,
    verb: &'static str,
    commit: Option<String>,
    place: Option<(&'static str, String)>,
    tooltip: Option<String>,
}

/// What a write's outcome is said to be (constitution III): committed to its branch only when
/// a commit moved it; a proposal held as its commit, which publishes to the branch it was made
/// for; a snapshot saved as its commit, on no branch; refused or unknown with their reason.
fn said(entry: &OutcomeEntry) -> Said {
    let branch = entry.branch.clone();
    match &entry.outcome {
        OutcomeKind::Committed { commit } => Said {
            status: studio_ui::ds::Status::Done,
            verb: "committed",
            commit: None,
            place: Some(("to", branch)),
            tooltip: commit
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(|id| format!("Commit {id}")),
        },
        OutcomeKind::Held {
            commit,
            held: HeldAs::Proposal,
        } => Said {
            status: studio_ui::ds::Status::Done,
            verb: "was held as commit",
            commit: Some(studio_ui::short_commit_id(commit)),
            place: Some(("to publish to", branch.clone())),
            tooltip: Some(format!(
                "Commit {commit}, on no branch: {branch} did not change. `commit publish \
                 \"{commit}\"` puts it there; `commit drop \"{commit}\"` drops it."
            )),
        },
        OutcomeKind::Held {
            commit,
            held: HeldAs::Snapshot,
        } => Said {
            status: studio_ui::ds::Status::Done,
            verb: "was saved as snapshot",
            commit: Some(studio_ui::short_commit_id(commit)),
            place: None,
            tooltip: Some(format!(
                "Snapshot {commit}, on no branch: no branch changed. Read it by its id; it never \
                 publishes."
            )),
        },
        OutcomeKind::Refused { message } => Said {
            status: studio_ui::ds::Status::Failed,
            verb: "was refused",
            commit: None,
            place: Some(("on", branch)),
            tooltip: Some(format!("Nothing was written: {message}")),
        },
        OutcomeKind::Unknown { reason } => Said {
            status: studio_ui::ds::Status::Running,
            verb: "has an unknown outcome",
            commit: None,
            place: Some(("on", branch)),
            tooltip: Some(format!("{reason}. Check History before trying again.")),
        },
    }
}

/// What the log draws again for ([`ActivityLog::logged`]).
fn logged(session: &GraphSession) -> (u64, u64) {
    (session.outcomes_logged, session.merges_settled)
}

/// The branch whose History a write's row opens: an outcome nobody knows is settled by reading
/// the history of the branch it went to (constitution III), while it is unsettled. A merge whose
/// target history has since shown it landed is settled ([`graph_store::MergeEntry::settled`]):
/// its row offers nothing more, as a row whose outcome is known does. One the history has not
/// shown may still land, so its row still offers it.
fn history_to_check(entry: &OutcomeEntry) -> Option<&str> {
    let settled = entry
        .merge
        .as_ref()
        .is_some_and(|merge| merge.settled == Some(MergeSettled::Landed));
    match entry.outcome {
        OutcomeKind::Unknown { .. } if !settled => Some(&entry.branch),
        _ => None,
    }
}

/// Why History cannot be opened on `branch`, when the branch list the session holds does not
/// have it: a log row outlives its toast and the branch it names, which may have been deleted
/// since (a merge's target), or never made (a create whose outcome nobody knows that did not
/// land), and Open history moves the window to that branch first. `None` when the list holds
/// it.
fn history_refusal(branch: &str, branches: &Loadable<Vec<String>>) -> Option<String> {
    match branches.loaded() {
        Some(listed) if listed.iter().any(|listed| listed == branch) => None,
        Some(_) => Some(format!(
            "There is no branch {branch} now: it was deleted, or the write that made it did not \
             land. Its History cannot be opened."
        )),
        None => Some(format!(
            "The branch list is not read yet, so whether {branch} is there is not known."
        )),
    }
}

/// An unknown outcome's row button: History on its branch ([`ActivityLog::open_history`]),
/// disabled with why when the branch is not there to open (`refusal`).
fn open_history_button(
    index: usize,
    branch: &str,
    refusal: Option<String>,
    cx: &Context<ActivityLog>,
) -> impl IntoElement {
    let branch = branch.to_owned();
    let disabled = refusal.is_some();
    div()
        .debug_selector(move || format!("activity-open-history-{index}"))
        .child(
            studio_ui::ds::row_button(("activity-open-history", index), "Open history")
                .variant(studio_ui::ds::Variant::Ghost)
                .disabled(disabled)
                .when_some(refusal, |button, refusal| {
                    button.tooltip(ui::Tooltip::text(refusal))
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_history(branch.clone(), window, cx)
                })),
        )
}

/// One write in the feed, after the reference frames: a status dot, what was written, the
/// commit a held write is (underlined), where it went, and when; the tooltip has the commit in
/// full, or a refusal's or an unknown outcome's reason.
fn activity_row(index: usize, entry: &OutcomeEntry) -> studio_ui::ds::ActivityRow {
    let said = said(entry);
    // As every other time in Studio reads.
    let at = entry
        .at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros() as i64);
    let when = studio_ui::relative_time(at, studio_ui::now_micros());
    let mut row = studio_ui::ds::ActivityRow::new(
        ("activity", index),
        IconName::Pencil,
        entry.description.clone(),
        said.verb,
        when,
    )
    .status(said.status);
    if let Some(commit) = said.commit {
        row = row.object(commit);
    }
    if let Some((preposition, branch)) = said.place {
        row = row.place(preposition, IconName::GitBranch, branch);
    }
    match said.tooltip {
        Some(tooltip) => row.tooltip(tooltip),
        None => row,
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};
    use graph_store::{MergeEntry, MergeSent, OutcomeKind};

    use super::*;

    /// A write held off every branch says what it is, with its commit, and never that it was
    /// committed to the branch it was sent to: a proposal is held to publish to it, a snapshot
    /// is on no branch. A committed write goes to its branch.
    #[test]
    fn a_held_write_says_it_is_on_no_branch() {
        let entry = |outcome: OutcomeKind| OutcomeEntry {
            at: std::time::SystemTime::UNIX_EPOCH,
            branch: "main".into(),
            description: "add".into(),
            outcome,
            follow_up: None,
            merge: None,
        };
        let proposal = said(&entry(OutcomeKind::Held {
            commit: "01JPROPOSALAA".into(),
            held: HeldAs::Proposal,
        }));
        assert_eq!(proposal.verb, "was held as commit");
        assert_eq!(proposal.commit.as_deref(), Some("OPOSALAA"), "its short id");
        assert_eq!(
            proposal.place,
            Some(("to publish to", "main".to_owned())),
            "named as the branch it publishes to, not one it changed"
        );
        let tooltip = proposal.tooltip.unwrap();
        assert!(tooltip.contains("on no branch"), "{tooltip}");
        assert!(
            tooltip.contains("commit publish \"01JPROPOSALAA\""),
            "{tooltip}"
        );

        let snapshot = said(&entry(OutcomeKind::Held {
            commit: "01JSNAPSHOTAA".into(),
            held: HeldAs::Snapshot,
        }));
        assert_eq!(snapshot.verb, "was saved as snapshot");
        assert_eq!(snapshot.commit.as_deref(), Some("APSHOTAA"));
        assert_eq!(snapshot.place, None, "on no branch");

        let committed = said(&entry(OutcomeKind::Committed {
            commit: Some("c2".into()),
        }));
        assert_eq!(committed.verb, "committed");
        assert_eq!(committed.place, Some(("to", "main".to_owned())));
        assert_eq!(committed.commit, None);
    }

    /// The log draws again when a write's outcome is logged, and not when a read starts or
    /// ends: every read the window sends raises the same event.
    #[gpui::test]
    async fn the_log_draws_for_outcomes_not_for_reads(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let (session, _) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n}\n",
            |_| serde_json::json!([]),
            cx,
        );
        cx.run_until_parked();
        let (log, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| ActivityLog::new(WeakEntity::new_invalid(), Some(session), window, cx)
        });
        cx.run_until_parked();
        let renders = |log: &Entity<ActivityLog>, cx: &mut VisualTestContext| {
            log.read_with(cx, |log, _| log.renders)
        };
        let before = renders(&log, cx);
        // A read starting and ending, and a head seen, log no outcome.
        session.update(cx, |_, cx| {
            cx.emit(SessionEvent::ActivityChanged);
            cx.emit(SessionEvent::CountsChanged);
        });
        cx.run_until_parked();
        assert_eq!(renders(&log, cx), before, "a read drew the log");

        let write = session.update(cx, |session, cx| {
            session.external_write("main".into(), "Rename Ada".into(), false, cx)
        });
        write.finish(OutcomeKind::Refused {
            message: "policy denied".into(),
        });
        cx.run_until_parked();
        assert!(renders(&log, cx) > before, "an outcome draws it");
        let after = renders(&log, cx);
        session.update(cx, |_, cx| cx.emit(SessionEvent::ActivityChanged));
        cx.run_until_parked();
        assert_eq!(renders(&log, cx), after);
    }

    /// A write whose outcome nobody knows offers its branch's History in its row, as its toast
    /// did before errors folded into the log: pressed, the window moves to the branch the write
    /// went to and History opens there. A refused write's row offers nothing. Before, the row
    /// only said to check History in its tooltip.
    #[gpui::test]
    async fn an_unknown_outcome_opens_its_branchs_history(cx: &mut TestAppContext) {
        use std::{cell::RefCell, rc::Rc};
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let (session, served) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n}\n",
            |_| serde_json::json!([]),
            cx,
        );
        served.lock().unwrap().branches.push("fix".into());
        cx.run_until_parked();
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        // The graph and branch the window shows each time History is opened.
        let opened: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_session(session.clone(), window, cx);
            let handle = cx.entity().downgrade();
            let log = cx.new(|cx| ActivityLog::new(handle, Some(session.clone()), window, cx));
            workspace.set_activity_view(log.into(), cx);
            let opened = opened.clone();
            workspace.register_action(move |workspace, _: &OpenHistory, _, cx| {
                let shown = workspace.session().unwrap().read(cx);
                let at = (shown.graph_id.clone(), shown.branch.clone());
                opened.borrow_mut().push(at);
            });
        });
        // Two writes to fix while main is shown: one refused, then one nobody knows the
        // outcome of, which the log lists first.
        let outcomes = [
            OutcomeKind::Refused {
                message: "policy denied".into(),
            },
            OutcomeKind::Unknown {
                reason: "the connection closed".into(),
            },
        ];
        for outcome in outcomes {
            let write = session.update(cx, |session, cx| {
                session.external_write("fix".into(), "Rename Ada".into(), false, cx)
            });
            write.finish(outcome);
        }
        cx.dispatch_action(studio_actions::workspace::ActivatePanel {
            id: "activity".into(),
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("activity-open-history-1").is_none(),
            "a refusal offers nothing"
        );
        let button = cx
            .debug_bounds("activity-open-history-0")
            .expect("the unknown outcome's Open history");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let graph = session.read_with(cx, |session, _| session.graph_id.clone());
        assert_eq!(*opened.borrow(), [(graph, "fix".to_owned())]);
    }

    /// Open history is offered while an outcome is unsettled and on a branch the session's
    /// list holds; a branch the list lacks (deleted since, or a create that did not land)
    /// leaves it disabled with why, and a merge whose target history shows it landed offers
    /// nothing. Before, every unknown outcome offered it for as long as its row lived, and it
    /// moved the window to a branch that might be gone.
    #[test]
    fn open_history_is_offered_for_an_unsettled_outcome_on_a_listed_branch() {
        let entry = |branch: &str, settled: Option<MergeSettled>| OutcomeEntry {
            at: std::time::SystemTime::UNIX_EPOCH,
            branch: branch.into(),
            description: "merge".into(),
            outcome: OutcomeKind::Unknown {
                reason: "the connection closed".into(),
            },
            follow_up: None,
            merge: settled.map(|settled| MergeEntry {
                source: "feature".into(),
                sent: MergeSent::default(),
                settled: Some(settled),
            }),
        };
        let listed = Loadable::Loaded(vec!["main".to_owned(), "fix".to_owned()]);
        assert_eq!(history_to_check(&entry("fix", None)), Some("fix"));
        assert_eq!(history_refusal("fix", &listed), None);
        assert_eq!(
            history_to_check(&entry("main", Some(MergeSettled::Landed))),
            None
        );
        assert_eq!(
            history_to_check(&entry("main", Some(MergeSettled::NotShown))),
            Some("main"),
            "a merge the history has not shown may still land"
        );
        let gone = history_refusal("gone", &listed).expect("refused");
        assert!(gone.starts_with("There is no branch gone now"), "{gone}");
        let unread = history_refusal("fix", &Loadable::Loading).expect("refused");
        assert!(unread.contains("not read yet"), "{unread}");
    }

    /// An unknown outcome's row follows the session: its Open history is disabled, and does
    /// nothing, once the branch it names leaves the branch list, and goes once a read of the
    /// merge's target settles it. The log draws for both, though neither logs an outcome.
    #[gpui::test]
    async fn an_unknown_outcomes_row_follows_its_branch_and_settlement(cx: &mut TestAppContext) {
        use std::{cell::RefCell, rc::Rc};
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
        let (session, served) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n}\n",
            |_| serde_json::json!([]),
            cx,
        );
        served.lock().unwrap().branches.push("fix".into());
        cx.run_until_parked();
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let opened: Rc<RefCell<Vec<String>>> = Rc::default();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_session(session.clone(), window, cx);
            let handle = cx.entity().downgrade();
            let log = cx.new(|cx| ActivityLog::new(handle, Some(session.clone()), window, cx));
            workspace.set_activity_view(log.into(), cx);
            let opened = opened.clone();
            workspace.register_action(move |workspace, _: &OpenHistory, _, cx| {
                let branch = workspace.session().unwrap().read(cx).branch.clone();
                opened.borrow_mut().push(branch);
            });
        });
        // A merge into main nobody knows the outcome of, then a write to fix alike.
        let unknown = || OutcomeKind::Unknown {
            reason: "the connection closed".into(),
        };
        session.update(cx, |session, cx| {
            session.outcomes.push(OutcomeEntry {
                at: std::time::SystemTime::now(),
                branch: "main".into(),
                description: "Merge feature into main".into(),
                outcome: unknown(),
                follow_up: None,
                merge: Some(MergeEntry {
                    source: "feature".into(),
                    sent: MergeSent::default(),
                    settled: None,
                }),
            });
            session.outcomes_logged += 1;
            cx.emit(SessionEvent::ActivityChanged);
        });
        let write = session.update(cx, |session, cx| {
            session.external_write("fix".into(), "Rename Ada".into(), false, cx)
        });
        write.finish(unknown());
        cx.dispatch_action(studio_actions::workspace::ActivatePanel {
            id: "activity".into(),
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("activity-open-history-0").is_some(), "fix");
        assert!(cx.debug_bounds("activity-open-history-1").is_some(), "main");

        // fix is deleted elsewhere: its row's button is drawn disabled, and does nothing.
        served
            .lock()
            .unwrap()
            .branches
            .retain(|branch| branch != "fix");
        session.update(cx, |session, cx| session.reload_branches(cx));
        cx.run_until_parked();
        let button = cx
            .debug_bounds("activity-open-history-0")
            .expect("drawn, disabled");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(opened.borrow().is_empty(), "nothing opened");
        assert_eq!(
            session.read_with(cx, |session, _| session.branch.clone()),
            "main"
        );

        // main's history shows the merge landed: its row offers nothing more.
        session.update(cx, |session, cx| {
            let sent = MergeSent::default();
            session.settle_merge("feature", "main", &sent, MergeSettled::Landed, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("activity-open-history-1").is_none(),
            "settled"
        );
    }
}
