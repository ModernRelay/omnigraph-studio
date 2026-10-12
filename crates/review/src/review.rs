//! Branches, review and merge (spec 008): the branch list, change summary, entity cards by
//! type, fixes bound to the reviewed head, merge outcomes and conflicts.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/008-branches-review-merge/plan.md`.

mod branches;
mod conflicts;
mod view;

use gpui::{App, AppContext as _, Focusable as _};
use studio_workspace::{PanelPosition, Workspace};
use ui::IconName;

pub use branches::{BranchList, CreateBranchModal, DeleteBranchModal, TARGET};
pub use conflicts::conflict_wording;
pub use view::{ReviewView, open_review, review_key};

/// Registers this surface with every new workspace.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        // Window-level actions reach the workspace only through a focused element, and a fresh
        // window focuses nothing. Until studio_workspace focuses itself, focus it here.
        if window.focused(cx).is_none() {
            let focus = workspace.focus_handle(cx);
            window.focus(&focus, cx);
        }
        let handle = cx.entity().downgrade();
        let session = workspace.session().cloned();
        let list = cx.new(|cx| BranchList::new(handle, session, window, cx));
        workspace.add_panel(
            PanelPosition::Left,
            "branches",
            "Branches",
            IconName::GitBranch,
            list.into(),
            cx,
        );
        branches::register(workspace);
    })
    .detach();
}
