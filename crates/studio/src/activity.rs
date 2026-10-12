//! The activity log (spec 002): every write and its outcome, newest first.

use gpui::{Context, Entity, Render, Subscription, WeakEntity, Window};
use graph_store::{GraphSession, OutcomeKind, SessionEvent};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;

pub struct ActivityLog {
    workspace: WeakEntity<Workspace>,
    session: Option<Entity<GraphSession>>,
    _subscriptions: Vec<Subscription>,
}

impl ActivityLog {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            workspace: workspace.clone(),
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
        let _ = &self.workspace;
        let entries = self
            .session
            .as_ref()
            .map(|s| s.read(cx).outcomes.clone())
            .unwrap_or_default();
        v_flex()
            .id("activity-log")
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .gap_1()
            .when(entries.is_empty(), |d| {
                d.child(
                    Label::new("No writes yet in this window.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(entries.iter().rev().map(|entry| {
                let (color, outcome) = match &entry.outcome {
                    OutcomeKind::Committed { commit } => (
                        Color::Success,
                        match commit.as_deref() {
                            Some(id) if !id.is_empty() => format!("committed {}", short_commit(id)),
                            _ => "committed".into(),
                        },
                    ),
                    OutcomeKind::Refused { message } => {
                        (Color::Error, format!("refused: {message}"))
                    }
                    OutcomeKind::Unknown { reason } => {
                        (Color::Warning, format!("unknown: {reason}"))
                    }
                };
                h_flex()
                    .gap_2()
                    .child(ui::Indicator::dot().color(color))
                    .child(Label::new(entry.description.clone()).size(LabelSize::Small))
                    .child(
                        Label::new(format!("on {}", entry.branch))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(outcome).size(LabelSize::Small).color(color))
            }))
    }
}

/// The last segment of a graph commit id, for display.
pub fn short_commit(id: &str) -> String {
    let tail = id.rsplit('.').next().unwrap_or(id);
    tail.chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}
