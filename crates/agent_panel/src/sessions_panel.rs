//! The Conversations panel of the left sidebar (spec 011): the graph's conversations with
//! agents, as Delta's sidebar lists threads and Zed's lists its agent threads. Pinned ones
//! first, then by last activity; each row is the conversation's title with what it is doing
//! (running, waiting for the person, failed, finished unseen) or when it was last active, and
//! pin and archive on hover. Archived ones fold away at the end. A row opens the conversation
//! in its own tab, or shows the tab it is in.

use chrono::{DateTime, Datelike as _, Utc};
use gpui::{
    App, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    WeakEntity, Window, div,
};
use studio_actions::agent::{NewConversation, OpenConversation};
use studio_ui::ds;
use studio_workspace::Workspace;
use ui::prelude::*;
use ui::{CommonAnimationExt as _, IconButton, Indicator, Tooltip};

use crate::sessions::{AgentSessions, Live, SessionRecord};

pub const PANEL_ID: &str = "agent-conversations";

pub struct SessionsPanel {
    workspace: WeakEntity<Workspace>,
    sessions: Entity<AgentSessions>,
    show_archived: bool,
    _subscriptions: Vec<Subscription>,
}

impl SessionsPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sessions = AgentSessions::global(cx);
        let mut subscriptions = vec![cx.observe(&sessions, |_, _, cx| cx.notify())];
        // The shown tab marks its conversation; the graph decides which are listed.
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(cx.observe(&workspace, |_, _, cx| cx.notify()));
        }
        Self {
            workspace,
            sessions,
            show_archived: false,
            _subscriptions: subscriptions,
        }
    }

    fn graph(&self, cx: &App) -> Option<String> {
        let workspace = self.workspace.upgrade()?;
        let session = workspace.read(cx).session()?.clone();
        Some(session.read(cx).settings_key(cx))
    }

    /// The conversation in the shown tab.
    fn shown(&self, cx: &App) -> Option<String> {
        let workspace = self.workspace.upgrade()?;
        let key = workspace.read(cx).active_item()?.dedupe_key(cx)?;
        let id = key.strip_prefix("agent:")?;
        (!id.starts_with("new:")).then(|| id.to_owned())
    }

    fn row(
        &self,
        record: &SessionRecord,
        shown: bool,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sessions = self.sessions.read(cx);
        let id = record.session.clone();
        let (end, state): (AnyElement, Option<&str>) = match sessions.live(&id) {
            Some(Live::Running) => (
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Accent)
                    .with_rotate_animation(2)
                    .into_any_element(),
                Some("Running"),
            ),
            Some(Live::Waiting) => (
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning)
                    .into_any_element(),
                Some("Waiting for your answer"),
            ),
            Some(Live::Failed) => (
                Icon::new(IconName::XCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Error)
                    .into_any_element(),
                Some("The agent stopped"),
            ),
            None if sessions.is_unread(&id) => (
                Indicator::dot().color(Color::Accent).into_any_element(),
                Some("Finished while you were away"),
            ),
            None => (
                ds::sidebar_count(relative_time(record.updated_at, now)).into_any_element(),
                None,
            ),
        };
        let mut about = format!(
            "{} · {} · last active {}",
            record.shown_title(),
            agent_name(&record.agent),
            record
                .updated_at
                .with_timezone(&chrono::Local)
                .format("%-d %b %H:%M"),
        );
        if let Some(state) = state {
            about = format!("{state} · {about}");
        }
        let pin = {
            let (id, pinned) = (id.clone(), record.pinned);
            let sessions = self.sessions.clone();
            IconButton::new(
                SharedString::from(format!("pin-{id}")),
                if pinned {
                    IconName::Unpin
                } else {
                    IconName::Pin
                },
            )
            .icon_size(IconSize::XSmall)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text(if pinned { "Unpin" } else { "Pin" }))
            .on_click(move |_, _, cx| {
                sessions.update(cx, |sessions, cx| sessions.set_pinned(&id, !pinned, cx));
            })
        };
        let archive = {
            let (id, archived) = (id.clone(), record.archived);
            let sessions = self.sessions.clone();
            IconButton::new(
                SharedString::from(format!("archive-{id}")),
                if archived {
                    IconName::Undo
                } else {
                    IconName::Archive
                },
            )
            .icon_size(IconSize::XSmall)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text(if archived {
                "Restore to the list"
            } else {
                "Archive (the agent keeps the conversation)"
            }))
            .on_click(move |_, _, cx| {
                sessions.update(cx, |sessions, cx| sessions.set_archived(&id, !archived, cx));
            })
        };
        ds::SidebarRow::new(
            SharedString::from(format!("conversation-{id}")),
            record.shown_title().to_owned(),
        )
        .selected(shown)
        .muted(record.archived)
        .end(end)
        .actions(
            h_flex()
                .gap(px(ds::space::XXS))
                .when(!record.archived, |row| row.child(pin))
                .child(archive),
        )
        .tooltip(about)
        .on_click(move |_, window, cx| {
            window.dispatch_action(
                Box::new(OpenConversation {
                    session: id.clone(),
                }),
                cx,
            )
        })
        .into_any_element()
    }
}

impl Render for SessionsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(graph) = self.graph(cx) else {
            return v_flex().into_any_element();
        };
        let shown = self.shown(cx);
        let now = Utc::now();
        let (listed, archived): (Vec<SessionRecord>, Vec<SessionRecord>) = {
            let sessions = self.sessions.read(cx);
            (
                sessions.for_graph(&graph).into_iter().cloned().collect(),
                sessions.archived(&graph).into_iter().cloned().collect(),
            )
        };
        let (pinned, recent): (Vec<_>, Vec<_>) = listed.into_iter().partition(|r| r.pinned);
        let is_shown = |record: &SessionRecord| shown.as_deref() == Some(record.session.as_str());

        let mut list = v_flex().w_full().child(
            ds::SidebarRow::new("new-conversation", "New conversation")
                .start(
                    Icon::new(IconName::Plus)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .muted(true)
                .on_click(|_, window, cx| window.dispatch_action(Box::new(NewConversation), cx)),
        );
        if pinned.is_empty() && recent.is_empty() {
            list = list.child(ds::sidebar_placeholder(
                "no-conversations",
                IconName::ZedAssistant,
                "No conversations yet",
                "Ask the agent from the Agent tab (⌘⇧A); each conversation is listed here",
            ));
        }
        if !pinned.is_empty() {
            list = list.child(ds::section_label("Pinned"));
            for record in &pinned {
                let row = self.row(record, is_shown(record), now, cx);
                list = list.child(row);
            }
            if !recent.is_empty() {
                list = list.child(ds::section_label("Recent"));
            }
        }
        for record in &recent {
            let row = self.row(record, is_shown(record), now, cx);
            list = list.child(row);
        }
        if !archived.is_empty() {
            let open = self.show_archived;
            list = list.child(
                ds::SidebarRow::new("archived", "Archived")
                    .muted(true)
                    .start(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .end(ds::sidebar_count(archived.len().to_string()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_archived = !this.show_archived;
                        cx.notify();
                    })),
            );
            if open {
                for record in &archived {
                    let row = self.row(record, is_shown(record), now, cx);
                    list = list.child(row);
                }
            }
        }
        list.child(div().h(px(ds::space::SM))).into_any_element()
    }
}

/// When a conversation was last active, as a sidebar fits it: "now", "5m", "3h", "2d", "12 Oct",
/// and "Oct 2025" from another year.
fn relative_time(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let elapsed = now.signed_duration_since(at);
    if elapsed.num_minutes() < 1 {
        "now".into()
    } else if elapsed.num_minutes() < 60 {
        format!("{}m", elapsed.num_minutes())
    } else if elapsed.num_hours() < 24 {
        format!("{}h", elapsed.num_hours())
    } else if elapsed.num_days() < 7 {
        format!("{}d", elapsed.num_days())
    } else if at.year() == now.year() {
        at.with_timezone(&chrono::Local)
            .format("%-d %b")
            .to_string()
    } else {
        at.with_timezone(&chrono::Local).format("%b %Y").to_string()
    }
}

/// The agent's name from its command: the package or program, without a version or path.
fn agent_name(command: &str) -> String {
    let program = command
        .split_whitespace()
        .rfind(|part| !part.starts_with('-'))
        .unwrap_or(command);
    let base = program.rsplit('/').next().unwrap_or(program);
    base.split('@')
        .find(|part| !part.is_empty())
        .unwrap_or(base)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_short() {
        let now = Utc::now();
        let ago = |minutes: i64| now - chrono::Duration::minutes(minutes);
        assert_eq!(relative_time(now, now), "now");
        assert_eq!(relative_time(ago(5), now), "5m");
        assert_eq!(relative_time(ago(3 * 60), now), "3h");
        assert_eq!(relative_time(ago(2 * 24 * 60), now), "2d");
        assert!(!relative_time(ago(30 * 24 * 60), now).ends_with('d'));
    }

    #[test]
    fn agents_are_named_by_their_package() {
        assert_eq!(
            agent_name("npx -y @agentclientprotocol/claude-agent-acp@latest"),
            "claude-agent-acp"
        );
        assert_eq!(agent_name("/usr/local/bin/my-agent --acp"), "my-agent");
    }
}
