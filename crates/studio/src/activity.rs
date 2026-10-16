//! The activity log (spec 002): every write and its outcome, newest first.

use gpui::{Context, Entity, Render, Subscription, WeakEntity, Window};
use graph_store::{GraphSession, OutcomeKind, SessionEvent};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;

pub struct ActivityLog {
    session: Option<Entity<GraphSession>>,
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
            self._subscriptions
                .push(cx.subscribe(session, |_, _, _: &SessionEvent, cx| cx.notify()));
        }
        self.session = session;
        cx.notify();
    }
}

impl Render for ActivityLog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self
            .session
            .as_ref()
            .map(|s| s.read(cx).outcomes.clone())
            .unwrap_or_default();
        // The feed, after the reference frames: a status dot, what was written (underlined),
        // where it went, and when; refusals and unknown outcomes keep their reason in a tooltip.
        v_flex()
            .id("activity-log")
            .size_full()
            .overflow_y_scroll()
            .py(px(studio_ui::ds::space::XS))
            .when(entries.is_empty(), |d| {
                d.child(studio_ui::ds::empty_state(
                    IconName::BoltOutlined,
                    "No writes yet",
                ))
            })
            .children(entries.iter().rev().enumerate().map(|(index, entry)| {
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
                let when = entry
                    .at
                    .elapsed()
                    .map(|elapsed| relative(elapsed.as_secs()))
                    .unwrap_or_else(|_| "now".into());
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
            }))
    }
}

/// "just now", "3m ago", "2h ago", "4d ago".
fn relative(seconds: u64) -> String {
    match seconds {
        0..=44 => "just now".into(),
        45..=3599 => format!("{}m ago", (seconds + 30) / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}
