//! The `.gq` editor and run loop (spec 007): Zed's editor with highlighting, diagnostics and
//! completion from `gq_language`, then reads, confirmed writes, parameters, results and plans.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/007-query-editor/plan.md`.

mod analysis;
mod completion;
mod results;
mod view;

use gpui::App;
use studio_actions::graph::NewQuery;
use studio_workspace::{ToastKind, Workspace};

pub use view::QueryEditor;

/// Registers `NewQuery` with every new workspace and binds the editor's keys.
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, "query", |data, workspace, window, cx| {
        let Some(session) = workspace.session().cloned() else {
            return;
        };
        let text = data
            .get("text")
            .and_then(|text| text.as_str())
            .unwrap_or_default()
            .to_owned();
        let number = view::next_number();
        workspace.open_item(format!("query:{number}"), window, cx, |window, cx| {
            let mut editor = QueryEditor::new(number, session, window, cx);
            editor.set_text(&text, window, cx);
            editor
        });
    });
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &NewQuery, window, cx| {
            let Some(session) = workspace.session().cloned() else {
                workspace.show_toast(
                    ToastKind::Info,
                    "Open a graph first; queries run against its branch.",
                    cx,
                );
                return;
            };
            let number = view::next_number();
            workspace.open_item(format!("query:{number}"), window, cx, |window, cx| {
                QueryEditor::new(number, session, window, cx)
            });
        });
    })
    .detach();
}
