//! Branches, review and merge (spec 008): the branch list, change summary, entity cards by
//! type, fixes bound to the reviewed head, merge outcomes and conflicts.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/008-branches-review-merge/plan.md`.

mod branches;
mod conflicts;
mod view;

use gpui::{App, AppContext as _, Focusable as _, KeyBinding};
use studio_actions::review::{AcceptAndNext, Merge, NextChange, PreviousChange, RevertAndNext};
use studio_workspace::Workspace;

pub use branches::{BranchList, CreateBranchModal, DeleteBranchModal, TARGET};
pub use conflicts::conflict_wording;
pub use view::{ReviewView, open_review, review_key};

/// The review's default keys. They belong in `assets/keymaps/`; until they move there they are
/// bound here, in the `Review` key context only.
fn bind_keys(cx: &mut App) {
    #[cfg(target_os = "macos")]
    let (accept, revert, merge) = ("cmd-y", "alt-cmd-z", "cmd-shift-enter");
    #[cfg(not(target_os = "macos"))]
    let (accept, revert, merge) = ("ctrl-y", "alt-ctrl-z", "ctrl-shift-enter");
    let context = Some("Review");
    cx.bind_keys([
        KeyBinding::new("j", NextChange, context),
        KeyBinding::new("down", NextChange, context),
        KeyBinding::new("k", PreviousChange, context),
        KeyBinding::new("up", PreviousChange, context),
        KeyBinding::new(accept, AcceptAndNext, context),
        KeyBinding::new(revert, RevertAndNext, context),
        KeyBinding::new(merge, Merge, context),
    ]);
}

/// Registers this surface with every new workspace.
pub fn init(cx: &mut App) {
    bind_keys(cx);
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
        workspace.add_sidebar_section("Branches", list.into(), cx);
        branches::register(workspace);
    })
    .detach();
}
