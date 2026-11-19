//! Branches, review and merge (spec 008): the branch list, change summary, entity cards by
//! type, fixes bound to the reviewed head, merge outcomes and conflicts.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/008-branches-review-merge/plan.md`.

mod branches;
mod compare;
mod conflicts;
mod edit;
mod scope_menu;
mod summary;
mod view;

use gpui::{App, AppContext as _};
use studio_workspace::{PanelPosition, Workspace};
use ui::IconName;

use branches::{BranchList, BranchesActions};
use view::{open_compare, open_review};

/// Registers this surface with every new workspace.
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, "review", |data, workspace, window, cx| {
        let field = |name: &str| data.get(name).and_then(|value| value.as_str());
        if let (Some(branch), Some(target)) = (field("branch"), field("target")) {
            open_review(workspace, branch.to_owned(), target.to_owned(), window, cx);
        }
    });
    // A comparison of two branches or commits (spec 043), opened again with the window.
    studio_workspace::register_item_kind(cx, "compare", |data, workspace, window, cx| {
        let field = |name: &str| data.get(name).and_then(|value| value.as_str());
        if let (Some(from), Some(to)) = (field("from"), field("to")) {
            open_compare(workspace, from.to_owned(), to.to_owned(), false, window, cx);
        }
    });
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let handle = cx.entity().downgrade();
        let session = workspace.session().cloned();
        let list = cx.new(|cx| BranchList::new(handle, session, window, cx));
        let actions = cx.new(|cx| BranchesActions::new(&list, cx));
        let view: gpui::AnyView = list.into();
        workspace.add_panel(
            PanelPosition::Left,
            "branches",
            "Branches",
            IconName::GitBranch,
            view.clone(),
            cx,
        );
        // The list draws only the rows in view, sized from whichever dock it is in.
        workspace.set_panel_scrolls_itself(&view, cx);
        workspace.set_panel_actions("branches", actions.into(), cx);
        branches::register(workspace);
    })
    .detach();
}
