//! One virtualized table per node or edge type: engine-side filter and count, keyset pages
//! pinned to one commit, peek, value-bound cell edits.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/004-type-tables/plan.md`.

mod dialogs;
pub mod model;
mod peek;
mod table;
mod types_list;

use gpui::{App, AppContext as _, Context, Window};
use studio_actions::graph::{OpenNode, OpenType};
use studio_workspace::{PanelPosition, ToastKind, Workspace};
use ui::IconName;

pub use dialogs::{DeleteRowModal, InsertRowModal};
pub use peek::{NodePeek, PeekSubject};
pub use table::{Cursor, TypeTable};
pub use types_list::TypesList;

/// Registers this surface with every new workspace: the left dock's Types panel, `OpenType`,
/// and `OpenNode`. Its keys are in `assets/keymaps/` (`TypeTable`, `TypeTable && editing`).
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, "type_table", |data, workspace, window, cx| {
        if let Some(type_name) = data.get("type_name").and_then(|name| name.as_str()) {
            let at_commit = data
                .get("at_commit")
                .and_then(|commit| commit.as_str())
                .map(str::to_owned);
            open_type(workspace, type_name, at_commit, window, cx);
        }
    });
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let handle = cx.entity().downgrade();
        let session = workspace.session().cloned();
        let types = cx.new(|cx| TypesList::new(handle, session, window, cx));
        workspace.add_panel(
            PanelPosition::Left,
            "types",
            "Types",
            IconName::Table,
            types.into(),
            cx,
        );
        workspace.register_action(|workspace, action: &OpenType, window, cx| {
            open_type(
                workspace,
                &action.type_name,
                action.at_commit.clone(),
                window,
                cx,
            )
        });
        workspace.register_action(|workspace, action: &OpenNode, _window, cx| {
            open_node(workspace, action, cx)
        });
    })
    .detach();
}

/// Opens a type's table, or activates it when it is open.
/// Opens a type's table, at the branch head or read-only at a commit.
fn open_type(
    workspace: &mut Workspace,
    type_name: &str,
    at_commit: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    if let Some(schema) = session.read(cx).schema_model()
        && schema.node(type_name).is_none()
        && schema.edge(type_name).is_none()
    {
        workspace.show_toast(
            ToastKind::Error,
            format!("The schema has no type {type_name}."),
            cx,
        );
        return;
    }
    let handle = cx.entity().downgrade();
    let name = type_name.to_owned();
    workspace.open_item::<TypeTable>(
        table_key(type_name, at_commit.as_deref()),
        window,
        cx,
        move |window, cx| TypeTable::new(handle, session, name, at_commit, window, cx),
    );
}

/// One tab per type at the head, and one per type and commit.
pub(crate) fn table_key(type_name: &str, at_commit: Option<&str>) -> String {
    match at_commit {
        Some(commit) => format!("type:{type_name}@{commit}"),
        None => format!("type:{type_name}"),
    }
}

/// Shows one node in the inspector, read at the branch head.
fn open_node(workspace: &mut Workspace, action: &OpenNode, cx: &mut Context<Workspace>) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    let subject = PeekSubject::Node {
        type_name: action.type_name.clone(),
        id: action.id.clone(),
    };
    let peek = cx.new(|cx| NodePeek::new(session, subject, None, None, false, cx));
    workspace.set_inspector(Some(peek.into()), cx);
}
