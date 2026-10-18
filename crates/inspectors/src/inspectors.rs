//! Inspectors for physical state (spec 010): server health and readiness, graph states, the
//! snapshot's manifest version and datasets.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/010-inspectors/plan.md`.

mod storage;

use gpui::App;
use studio_actions::graph::OpenInspector;
use studio_workspace::Workspace;

use storage::{STORAGE_KEY, StorageInspector};

/// Registers this surface with every new workspace.
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, "storage", |_, workspace, window, cx| {
        let Some(session) = workspace.session().cloned() else {
            return;
        };
        workspace.open_item::<StorageInspector>(STORAGE_KEY, window, cx, |_, cx| {
            StorageInspector::new(session, cx)
        });
    });
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenInspector, window, cx| {
            let Some(session) = workspace.session().cloned() else {
                return;
            };
            // An inspector already open, in any pane, reads again; a new one reads as it is made.
            let mut made = false;
            let view = workspace.open_item::<StorageInspector>(STORAGE_KEY, window, cx, |_, cx| {
                made = true;
                StorageInspector::new(session, cx)
            });
            if !made && let Some(view) = view {
                view.update(cx, |view, cx| view.reload(cx));
            }
        });
    })
    .detach();
}
