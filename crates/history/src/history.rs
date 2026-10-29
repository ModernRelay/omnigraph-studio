//! Commit history (spec 009): a lane graph of a branch's commits and each commit's changes.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/009-history/plan.md`.

mod commit_view;
mod lanes;
mod merged;
mod panel;
mod view;

use gpui::{App, AppContext as _};
use studio_actions::graph::{OpenCommit, OpenHistory};
use studio_workspace::{PanelPosition, Workspace};
use ui::IconName;

use view::{HistoryView, history_key};

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
        workspace.open_item::<HistoryView>(history_key(&branch), window, cx, |window, cx| {
            HistoryView::new(session, branch, window, cx)
        });
    });
    studio_workspace::register_item_kind(cx, "commit", |data, workspace, window, cx| {
        let (Some(session), Some(id)) = (
            workspace.session().cloned(),
            data.get("commit_id").and_then(|id| id.as_str()),
        ) else {
            return;
        };
        // Read once shown: a tab behind another reads nothing at start.
        let id = id.to_owned();
        workspace.open_item::<commit_view::CommitView>(
            commit_view::commit_key(&id),
            window,
            cx,
            |_, cx| commit_view::CommitView::when_shown(session, id, cx),
        );
    });
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if let Some(window) = window {
            let handle = cx.entity().downgrade();
            let session = workspace.session().cloned();
            let panel = cx.new(|cx| panel::HistoryPanel::new(handle, session, window, cx));
            workspace.add_panel(
                PanelPosition::Bottom,
                "history",
                "History",
                IconName::HistoryRerun,
                panel.clone().into(),
                cx,
            );
            workspace.set_panel_focus("history", move |cx| panel.read(cx).focus(cx), cx);
        }
        workspace.register_action(|workspace, _: &OpenHistory, window, cx| {
            let Some(session) = workspace.session().cloned() else {
                return;
            };
            let branch = session.read(cx).branch.clone();
            workspace.open_item::<HistoryView>(history_key(&branch), window, cx, |window, cx| {
                HistoryView::new(session, branch, window, cx)
            });
        });
        // A commit named by its id alone (a toast's, a parent link's): its tab reads it beside
        // its changes. History opens the commits it lists with the commit as listed.
        workspace.register_action(|workspace, action: &OpenCommit, window, cx| {
            let Some(session) = workspace.session().cloned() else {
                return;
            };
            let id = action.commit_id.clone();
            commit_view::open_in(workspace, session, id, None, window, cx);
        });
    })
    .detach();
}
