//! Branches, review and merge (spec 008): the branch list, change summary, entity cards by
//! type, fixes bound to the reviewed head, merge outcomes and conflicts.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/008-branches-review-merge/plan.md`.

mod branches;
mod conflicts;
mod edit;
mod view;

use gpui::{App, AppContext as _};
use studio_workspace::{PanelPosition, Workspace};
use ui::IconName;

use branches::BranchList;
use view::open_review;

/// Registers this surface with every new workspace.
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, "review", |data, workspace, window, cx| {
        let field = |name: &str| data.get(name).and_then(|value| value.as_str());
        if let (Some(branch), Some(target)) = (field("branch"), field("target")) {
            open_review(workspace, branch.to_owned(), target.to_owned(), window, cx);
        }
    });
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let handle = cx.entity().downgrade();
        let session = workspace.session().cloned();
        let list = cx.new(|cx| BranchList::new(handle, session, window, cx));
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
        branches::register(workspace);
    })
    .detach();
}
