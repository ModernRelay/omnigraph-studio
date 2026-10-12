//! Commit history (spec 009): a lane graph of a branch's commits and each commit's changes.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/009-history/plan.md`.

mod commit_view;
mod lanes;
mod merged;
mod view;

use gpui::{App, KeyBinding};
use studio_actions::graph::{OpenCommit, OpenHistory};
use studio_workspace::Workspace;

pub use commit_view::{CommitCounts, CommitView, commit_key};
pub use lanes::{Graph, GraphCommit, GraphRow, LaneLine, assign_lanes};
pub use merged::{branch_heads, with_merged_commits};
pub use view::{HistoryView, LONG_HISTORY, history_key};

/// Registers this surface with every new workspace.
pub fn init(cx: &mut App) {
    // Up, down and enter come from Zed's `menu` bindings; j and k belong in
    // `assets/keymaps/` and are bound here, in the `History` context only, until they move.
    cx.bind_keys([
        KeyBinding::new("j", menu::SelectNext, Some("History")),
        KeyBinding::new("k", menu::SelectPrevious, Some("History")),
    ]);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenHistory, window, cx| {
            let Some(session) = workspace.session().cloned() else {
                return;
            };
            let branch = session.read(cx).branch.clone();
            workspace.open_item::<HistoryView>(history_key(&branch), window, cx, |_, cx| {
                HistoryView::new(session, branch, cx)
            });
        });
        workspace.register_action(|workspace, action: &OpenCommit, window, cx| {
            let Some(session) = workspace.session().cloned() else {
                return;
            };
            let id = action.commit_id.clone();
            workspace.open_item::<CommitView>(commit_key(&id), window, cx, |_, cx| {
                CommitView::new(session, id, None, cx)
            });
        });
    })
    .detach();
}
