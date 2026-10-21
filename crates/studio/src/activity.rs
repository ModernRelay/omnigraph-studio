//! The activity log (spec 002): every write and its outcome, newest first.

use std::ops::Range;
use std::time::Duration;

use gpui::{Context, Entity, Render, Subscription, Task, WeakEntity, Window, uniform_list};
use graph_store::{GraphSession, OutcomeEntry, OutcomeKind, SessionEvent};
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

/// One write in the feed, after the reference frames: a status dot, what was written
/// (underlined), where it went, and when; refusals and unknown outcomes keep their reason in a
/// tooltip.
fn activity_row(index: usize, entry: &OutcomeEntry) -> studio_ui::ds::ActivityRow {
    let (status, verb, tooltip) = match &entry.outcome {
        OutcomeKind::Committed { commit } => (
            studio_ui::ds::Status::Done,
            "committed",
            commit
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(|id| format!("Commit {id}")),
        ),
        OutcomeKind::Refused { message } => (
            studio_ui::ds::Status::Failed,
            "was refused",
            Some(format!("Nothing was written: {message}")),
        ),
        OutcomeKind::Unknown { reason } => (
            studio_ui::ds::Status::Running,
            "has an unknown outcome",
            Some(format!("{reason}. Check History before trying again.")),
        ),
    };
    // As every other time in Studio reads.
    let at = entry
        .at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros() as i64);
    let when = studio_ui::relative_time(at, studio_ui::now_micros());
    let row = studio_ui::ds::ActivityRow::new(
        ("activity", index),
        IconName::Pencil,
        entry.description.clone(),
        verb,
        when,
    )
    .status(status)
    .place(
        if matches!(entry.outcome, OutcomeKind::Committed { .. }) {
            "to"
        } else {
            "on"
        },
        IconName::GitBranch,
        entry.branch.clone(),
    );
    match tooltip {
        Some(tooltip) => row.tooltip(tooltip),
        None => row,
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};
    use graph_store::OutcomeKind;

    use super::*;

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
