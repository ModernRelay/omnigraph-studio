//! The agent panel (spec 011): an external agent over the Agent Client Protocol, which reaches
//! the graph with the omnigraph CLI through a gateway Studio runs for each conversation
//! (spec 013). The agent is never given the person's token, and its changes land only on a
//! branch of its own, each shown as a card; with "Ask before writes" on, each waits until the
//! person allows it.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/011-agent-panel/plan.md`.

mod cli_home;
mod codex;
mod confined;
mod connection;
mod gateway;
mod gateway_rules;
mod gateway_tls;
mod host;
#[cfg(test)]
mod json_schema;
mod launch;
mod panel;
mod requester;
mod sessions;
mod sessions_panel;
mod skill;

pub use launch::launch_agent;
pub use panel::PRESETS;

use gpui::{App, AppContext as _};
use studio_actions::agent::{Ask, NewConversation, OpenAgent, OpenConversation, ShowConversations};
use studio_workspace::{PanelPosition, Workspace};
use ui::IconName;

/// The argument that starts Studio's executable as the agent's launcher, which runs the agent
/// without Omnigraph's credentials in its environment where there is no `env -u` (spec 013).
pub const LAUNCH_COMMAND: &str = "launch-agent";

pub fn init(cx: &mut App) {
    // A restored tab comes back with its conversation.
    studio_workspace::register_item_kind(cx, "agent", |data, workspace, window, cx| {
        match data.get("session").and_then(|id| id.as_str()) {
            Some(id) => panel::open_session(workspace, id.to_owned(), window, cx),
            None => panel::new_conversation(workspace, window, cx),
        };
    });
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if let Some(window) = window {
            let handle = cx.entity().downgrade();
            let list = cx.new(|cx| sessions_panel::SessionsPanel::new(handle, window, cx));
            let actions = cx.new(|cx| sessions_panel::SessionsActions::new(&list, cx));
            workspace.add_panel(
                PanelPosition::Left,
                sessions_panel::PANEL_ID,
                "Conversations",
                IconName::ZedAssistant,
                list.into(),
                cx,
            );
            workspace.set_panel_actions(sessions_panel::PANEL_ID, actions.into(), cx);
        }
        workspace.register_action(|workspace, _: &OpenAgent, window, cx| {
            panel::open(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &NewConversation, window, cx| {
            panel::new_conversation(workspace, window, cx);
        });
        workspace.register_action(|workspace, action: &OpenConversation, window, cx| {
            panel::open_session(workspace, action.session.clone(), window, cx);
        });
        workspace.register_action(|workspace, _: &ShowConversations, _window, cx| {
            workspace.activate_panel(sessions_panel::PANEL_ID, cx);
        });
        workspace.register_action(|workspace, action: &Ask, window, cx| {
            if let Some(panel) = panel::open(workspace, window, cx) {
                let text = action.text.clone();
                panel.update(cx, |panel, cx| panel.send_text(text, window, cx));
            }
        });
    })
    .detach();
}
