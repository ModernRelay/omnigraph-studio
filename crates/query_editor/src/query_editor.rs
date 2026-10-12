//! The `.gq` editor and run loop (spec 007): Zed's editor with highlighting, diagnostics and
//! completion from `gq_language`, then reads, confirmed writes, parameters, results and plans.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/007-query-editor/plan.md`.

mod analysis;
mod completion;
mod results;
mod view;

use gpui::{App, KeyBinding};
use studio_actions::graph::NewQuery;
use studio_actions::query::{CopyCell, CopyRow, Explain, Run};
use studio_workspace::{ToastKind, Workspace};

pub use view::QueryEditor;

/// Registers `NewQuery` with every new workspace and binds the editor's keys.
pub fn init(cx: &mut App) {
    bind_keys(cx);
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

/// The query editor's bindings. They are added after Zed's keymap, so at the editor's depth
/// they win over Zed's `cmd-enter` (newline below) in a query.
fn bind_keys(cx: &mut App) {
    let m = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    cx.bind_keys([
        KeyBinding::new(&format!("{m}-enter"), Run, Some("QueryEditor > Editor")),
        KeyBinding::new(
            &format!("{m}-shift-enter"),
            Explain,
            Some("QueryEditor > Editor"),
        ),
        KeyBinding::new(&format!("{m}-enter"), Run, Some("QueryEditor")),
        KeyBinding::new(&format!("{m}-shift-enter"), Explain, Some("QueryEditor")),
        // The confirm bar names the keys it answers to; make them the plain ones.
        KeyBinding::new("enter", menu::Confirm, Some("QueryConfirm")),
        KeyBinding::new("escape", menu::Cancel, Some("QueryConfirm")),
        KeyBinding::new(&format!("{m}-c"), CopyCell, Some("QueryResults")),
        KeyBinding::new(&format!("{m}-shift-c"), CopyRow, Some("QueryResults")),
        // Zed's keymap binds this key, with no context, to `workspace::NewFile`, whose global
        // handler reads Zed's `AppState` and panics in Studio. Bound later at the same depth,
        // this wins everywhere; in Studio a new tab is a new query.
        KeyBinding::new(&format!("{m}-n"), NewQuery, None),
    ]);
}
