//! The activity log (spec 002): every write and its outcome, newest first.

use std::ops::Range;
use std::time::Duration;

use gpui::{Context, Entity, Render, Subscription, Task, WeakEntity, Window, uniform_list};
use graph_store::{GraphSession, HeldAs, OutcomeEntry, OutcomeKind, SessionEvent};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;

/// How often the log draws again while it lists writes, for the times they read ("5 min ago"),
/// which change by the minute.
const TIMES_EVERY: Duration = Duration::from_secs(60);

pub struct ActivityLog {
    session: Option<Entity<GraphSession>>,
    /// How many outcomes the session had logged ([`GraphSession::outcomes_logged`]) when the
    /// log last drew: a session event that logged none (a read starting or ending, a head
    /// seen) leaves the log as it was.
    logged: u64,
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
            logged: 0,
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
            // answered also raises: only a new outcome draws the log.
            self._subscriptions.push(cx.subscribe(
                session,
                |this, session, event: &SessionEvent, cx| {
                    if let SessionEvent::ActivityChanged = event
                        && session.read(cx).outcomes_logged != this.logged
                    {
                        cx.notify();
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
}

impl Render for ActivityLog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let (count, logged) = self.session.as_ref().map_or((0, 0), |session| {
            let session = session.read(cx);
            (session.outcomes.len(), session.outcomes_logged)
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
                let outcomes = &session.read(cx).outcomes;
                range
                    .filter_map(|index| {
                        let entry = outcomes.get(outcomes.len().checked_sub(index + 1)?)?;
                        Some(activity_row(index, entry))
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
    use graph_store::OutcomeKind;

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
}
