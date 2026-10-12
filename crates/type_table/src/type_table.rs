//! One virtualized table per node or edge type: engine-side filter and count, keyset pages
//! pinned to one commit, peek, value-bound cell edits.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/004-type-tables/plan.md`.

mod dialogs;
pub mod model;
mod peek;
mod table;
mod types_list;

use gpui::{App, AppContext as _, Context, KeyBinding, Window};
use studio_actions::graph::{OpenNode, OpenType};
use studio_actions::table::{
    AddFilter, Cancel, ClearFilters, ConfirmEdit, CopyCell, DeleteRow, EditCell, FilterBySelection,
    InsertRow, MoveDown, MoveLeft, MoveRight, MoveUp, PeekRow, RefreshTable,
};
use studio_workspace::{ToastKind, Workspace};

pub use dialogs::{DeleteRowModal, InsertRowModal};
pub use peek::{NodePeek, PeekSubject};
pub use table::{Cursor, TypeTable};
pub use types_list::TypesList;

/// Navigation keys work while no text field in the table has focus.
const NAVIGATING: &str = "TypeTable && !editing";
const EDITING: &str = "TypeTable && editing";

/// Registers this surface with every new workspace: the sidebar's Types section, `OpenType`,
/// `OpenNode`, and the table's key bindings.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("j", MoveDown, Some(NAVIGATING)),
        KeyBinding::new("down", MoveDown, Some(NAVIGATING)),
        KeyBinding::new("k", MoveUp, Some(NAVIGATING)),
        KeyBinding::new("up", MoveUp, Some(NAVIGATING)),
        KeyBinding::new("left", MoveLeft, Some(NAVIGATING)),
        KeyBinding::new("right", MoveRight, Some(NAVIGATING)),
        KeyBinding::new("enter", EditCell, Some(NAVIGATING)),
        KeyBinding::new("enter", ConfirmEdit, Some(EDITING)),
        KeyBinding::new("space", PeekRow, Some(NAVIGATING)),
        KeyBinding::new("escape", Cancel, Some("TypeTable")),
        KeyBinding::new("secondary-r", RefreshTable, Some("TypeTable")),
        KeyBinding::new("secondary-c", CopyCell, Some(NAVIGATING)),
        KeyBinding::new("secondary-n", InsertRow, Some("TypeTable")),
        KeyBinding::new("secondary-backspace", DeleteRow, Some(NAVIGATING)),
        KeyBinding::new("secondary-f", AddFilter, Some("TypeTable")),
        KeyBinding::new("secondary-alt-f", FilterBySelection, Some(NAVIGATING)),
        KeyBinding::new("secondary-alt-backspace", ClearFilters, Some("TypeTable")),
    ]);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let handle = cx.entity().downgrade();
        let session = workspace.session().cloned();
        let types = cx.new(|cx| TypesList::new(handle, session, window, cx));
        workspace.add_sidebar_section("Types", types.into(), cx);
        workspace.register_action(|workspace, action: &OpenType, window, cx| {
            open_type(workspace, &action.type_name, window, cx)
        });
        workspace.register_action(|workspace, action: &OpenNode, _window, cx| {
            open_node(workspace, action, cx)
        });
    })
    .detach();
}

/// Opens a type's table, or activates it when it is open.
fn open_type(
    workspace: &mut Workspace,
    type_name: &str,
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
        format!("type:{type_name}"),
        window,
        cx,
        move |window, cx| TypeTable::new(handle, session, name, window, cx),
    );
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
