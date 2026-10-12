//! Commit history (spec 009): a lane graph of a branch's commits and each commit's changes.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/009-history/plan.md`.

mod commit_view;
mod lanes;
mod merged;
mod view;

use gpui::App;
use studio_actions::graph::{OpenCommit, OpenHistory};
use studio_workspace::Workspace;

pub use commit_view::{CommitCounts, CommitView, commit_key};
pub use lanes::{Graph, GraphCommit, GraphRow, LaneLine, assign_lanes};
pub use merged::{branch_heads, with_merged_commits};
pub use view::{HistoryView, LONG_HISTORY, history_key};

/// Registers this surface with every new workspace.
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, "history", |data, workspace, window, cx| {
        let (Some(session), Some(branch)) = (
            workspace.session().cloned(),
            data.get("branch").and_then(|branch| branch.as_str()),
        ) else {
            return;
        };
        let branch = branch.to_owned();
        workspace.open_item::<HistoryView>(history_key(&branch), window, cx, |_, cx| {
            HistoryView::new(session, branch, cx)
        });
    });
    studio_workspace::register_item_kind(cx, "commit", |data, workspace, window, cx| {
        let (Some(session), Some(id)) = (
            workspace.session().cloned(),
            data.get("commit_id").and_then(|id| id.as_str()),
        ) else {
            return;
        };
        let id = id.to_owned();
        workspace.open_item::<CommitView>(commit_key(&id), window, cx, |_, cx| {
            CommitView::new(session, id, None, cx)
        });
    });
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
