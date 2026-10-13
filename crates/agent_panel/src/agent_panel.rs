//! The agent panel (spec 011): an external agent over the Agent Client Protocol, with graph
//! tools Studio serves and runs itself. The agent never holds the person's token, and its
//! changes land only on a branch of its own after the person approves each one.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/011-agent-panel/plan.md`.

mod host;
mod mcp;
mod panel;
mod sessions;
mod sessions_panel;
mod socket;
mod tools;

pub use panel::{AgentPanel, PRESETS};
pub use socket::proxy;

use gpui::{App, AppContext as _};
use studio_actions::agent::{Ask, NewConversation, OpenAgent, OpenConversation, ShowConversations};
use studio_workspace::{PanelPosition, Workspace};
use ui::IconName;

/// The argument that starts Studio's executable as the agent's stdio MCP server.
pub const PROXY_COMMAND: &str = "mcp-proxy";

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
            workspace.add_panel(
                PanelPosition::Left,
                sessions_panel::PANEL_ID,
                "Conversations",
                IconName::ZedAssistant,
                list.into(),
                cx,
            );
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
