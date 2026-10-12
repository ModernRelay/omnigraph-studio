//! The agent panel (spec 011): an external agent over the Agent Client Protocol, with graph
//! tools Studio serves and runs itself. The agent never holds the person's token, and its
//! changes land only on a branch of its own after the person approves each one.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/011-agent-panel/plan.md`.

mod host;
mod mcp;
mod panel;
mod socket;
mod tools;

pub use panel::{AgentPanel, PRESETS};
pub use socket::proxy;

use gpui::App;
use studio_actions::agent::{Ask, OpenAgent};
use studio_workspace::Workspace;

/// The argument that starts Studio's executable as the agent's stdio MCP server.
pub const PROXY_COMMAND: &str = "mcp-proxy";

pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, "agent", |_, workspace, window, cx| {
        panel::open(workspace, window, cx);
    });
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenAgent, window, cx| {
            panel::open(workspace, window, cx);
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
