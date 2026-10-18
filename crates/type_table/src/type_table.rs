//! One virtualized table per node or edge type: engine-side filter and count, keyset pages
//! pinned to one commit, peek, value-bound cell edits.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/004-type-tables/plan.md`.

mod blob;
mod blob_view;
mod dialogs;
mod grouping;
pub mod model;
mod node_page;
mod nodes_panel;
mod offers;
mod pdf;
mod peek;
mod suggest;
mod table;
#[cfg(test)]
mod test_support;
mod text_editor;
mod types_list;
mod write;

use gpui::{App, AppContext as _, Context, Entity, Window};
use gq_language::{Filter, FilterOp};
use serde_json::Value;
use studio_actions::graph::{
    BrowseType, OpenBlob, OpenNode, OpenNodePage, OpenTextEditor, OpenType, PeekNode, UndoDelete,
};
use studio_workspace::{PanelPosition, ToastKind, Workspace};
use ui::IconName;

pub use blob_view::BlobView;
pub use dialogs::{DeleteRowModal, InsertRowModal};
pub use node_page::NodePage;
pub use peek::{NodePeek, PeekSubject};
pub use table::{Cursor, TypeTable};
pub use text_editor::TextEditor;
pub use types_list::TypesList;

/// Registers this surface with every new workspace: the left dock's Types panel, `OpenType`,
/// and `OpenNode`. Its keys are in `assets/keymaps/` (`TypeTable`, `TypeTable && editing`).
pub fn init(cx: &mut App) {
    // A Blob tab comes back with the value it showed, at the same commit.
    studio_workspace::register_item_kind(cx, "blob", |data, workspace, window, cx| {
        let text = |key: &str| data.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        if let (Some(type_name), Some(id), Some(property)) =
            (text("type_name"), text("id"), text("property"))
        {
            let action = OpenBlob {
                type_name,
                id,
                property,
                edge: data.get("edge").and_then(|v| v.as_bool()).unwrap_or(false),
                at_commit: text("at_commit"),
            };
            open_blob(workspace, &action, window, cx);
        }
    });
    // A node's page comes back at the head, or at the commit it held.
    studio_workspace::register_item_kind(cx, "node", |data, workspace, window, cx| {
        let text = |key: &str| data.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        if let (Some(type_name), Some(id)) = (text("type_name"), text("id")) {
            let action = OpenNodePage {
                type_name,
                id,
                at_commit: text("at_commit"),
            };
            open_node_page(workspace, &action, window, cx);
        }
    });
    // A text property's editor comes back reading the node at the head.
    studio_workspace::register_item_kind(cx, "text", |data, workspace, window, cx| {
        let text = |key: &str| data.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        if let (Some(type_name), Some(id), Some(property)) =
            (text("type_name"), text("id"), text("property"))
        {
            let action = OpenTextEditor {
                type_name,
                id,
                property,
                read_at: None,
                text: None,
            };
            open_text_editor(workspace, &action, window, cx);
        }
    });
    studio_workspace::register_item_kind(cx, "type_table", |data, workspace, window, cx| {
        if let Some(type_name) = data.get("type_name").and_then(|name| name.as_str()) {
            let at_commit = data
                .get("at_commit")
                .and_then(|commit| commit.as_str())
                .map(str::to_owned);
            let sort = data.get("sort").and_then(|sort| {
                Some(gq_language::Sort {
                    property: sort.get("property")?.as_str()?.to_owned(),
                    descending: sort.get("descending")?.as_bool()?,
                })
            });
            let group_by = data
                .get("group_by")
                .and_then(|property| property.as_str())
                .map(str::to_owned);
            let keys = |name: &str| -> Vec<String> {
                data.get(name)
                    .and_then(Value::as_array)
                    .map(|keys| {
                        keys.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let mut layout = model::ColumnLayout::new(keys("hidden"), keys("order"));
            if let Some(widths) = data.get("widths").and_then(Value::as_object) {
                for (key, width) in widths {
                    if let Some(width) = width.as_f64() {
                        layout.set_width(key, Some(width as f32));
                    }
                }
            }
            let table = open_type(workspace, type_name, at_commit, false, window, cx);
            if let Some(table) = table {
                table.update(cx, |table, cx| {
                    if layout != model::ColumnLayout::default() {
                        table.set_layout(layout, cx);
                    }
                    if sort.is_some() {
                        table.set_sort(sort, cx);
                    }
                    if group_by.is_some() {
                        table.set_grouping(group_by, cx);
                    }
                });
            }
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
            "Graph",
            IconName::Blocks,
            types.into(),
            cx,
        );
        workspace.register_action(|workspace, action: &UndoDelete, _window, cx| {
            undo_delete(workspace, action, cx);
        });
        workspace.register_action(|workspace, action: &OpenBlob, window, cx| {
            open_blob(workspace, action, window, cx);
        });
        workspace.register_action(|workspace, action: &OpenType, window, cx| {
            let table = open_type(
                workspace,
                &action.type_name,
                action.at_commit.clone(),
                action.preview,
                window,
                cx,
            );
            if let (Some(table), Some(id)) = (table, action.filter_id.clone()) {
                let filter = match &action.via {
                    Some(via) => Filter {
                        property: via.clone(),
                        op: FilterOp::RelatedTo,
                        value: Value::String(id),
                    },
                    None => Filter {
                        property: gq_language::builder::ID_FILTER.into(),
                        op: FilterOp::Eq,
                        value: Value::String(id),
                    },
                };
                table.update(cx, |table, cx| table.show_only(filter, cx));
            }
        });
        // The Nodes panel in the bottom dock, and browsing a type in it.
        let workspace_handle = cx.entity().downgrade();
        let current = workspace.session().cloned();
        let nodes =
            cx.new(|cx| nodes_panel::NodesPanel::new(workspace_handle, current, window, cx));
        workspace.add_panel(
            PanelPosition::Bottom,
            nodes_panel::NODES_PANEL,
            "Nodes",
            IconName::Table,
            nodes.clone().into(),
            cx,
        );
        let focused = nodes.clone();
        workspace.set_panel_focus(
            nodes_panel::NODES_PANEL,
            move |cx| focused.read(cx).focus(cx),
            cx,
        );
        workspace.register_action(move |workspace, action: &BrowseType, window, cx| {
            nodes.update(cx, |panel, cx| panel.browse(&action.type_name, window, cx));
            workspace.activate_panel(nodes_panel::NODES_PANEL, cx);
        });
        workspace.register_action(|workspace, action: &PeekNode, _window, cx| {
            peek_node(
                workspace,
                &action.type_name,
                &action.id,
                action.at_commit.clone(),
                cx,
            );
            // A peek asked for shows the inspector, even when its dock was hidden.
            workspace.reveal_inspector(cx);
        });
        workspace.register_action(|workspace, action: &OpenNode, _window, cx| {
            peek_node(workspace, &action.type_name, &action.id, None, cx);
            // The relationship tree reveals the node too (spec 005).
            cx.propagate();
        });
        workspace.register_action(|workspace, action: &OpenNodePage, window, cx| {
            open_node_page(workspace, action, window, cx);
        });
        workspace.register_action(|workspace, action: &OpenTextEditor, window, cx| {
            open_text_editor(workspace, action, window, cx);
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
    preview: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<TypeTable>> {
    let session = workspace.session().cloned()?;
    if let Some(schema) = session.read(cx).schema_model()
        && schema.node(type_name).is_none()
        && schema.edge(type_name).is_none()
    {
        workspace.show_toast(
            ToastKind::Error,
            format!("The schema has no type {type_name}."),
            cx,
        );
        return None;
    }
    let handle = cx.entity().downgrade();
    let name = type_name.to_owned();
    workspace.open_item_with::<TypeTable>(
        table_key(type_name, at_commit.as_deref()),
        preview,
        window,
        cx,
        move |window, cx| TypeTable::new(handle, session, name, at_commit, window, cx),
    )
}

/// One tab per type at the head, and one per type and commit.
pub(crate) fn table_key(type_name: &str, at_commit: Option<&str>) -> String {
    match at_commit {
        Some(commit) => format!("type:{type_name}@{commit}"),
        None => format!("type:{type_name}"),
    }
}

/// Shows one node in the inspector, read at the branch head or held at `at_commit`.
fn peek_node(
    workspace: &mut Workspace,
    type_name: &str,
    id: &str,
    at_commit: Option<String>,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    let subject = PeekSubject::Node {
        type_name: type_name.to_owned(),
        id: id.to_owned(),
    };
    let holds = at_commit.is_some();
    let peek = cx.new(|cx| {
        let mut peek = NodePeek::new(
            session,
            subject,
            None,
            at_commit,
            Default::default(),
            false,
            cx,
        );
        if holds {
            peek.hold_commit();
        }
        peek
    });
    workspace.set_inspector(Some(peek.into()), cx);
}

/// Opens a text property in its editor, or shows the tab it is in.
pub fn open_text_editor(
    workspace: &mut Workspace,
    action: &OpenTextEditor,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<TextEditor>> {
    let session = workspace.session().cloned()?;
    let key = TextEditor::key(&action.type_name, &action.id, &action.property);
    let action = action.clone();
    workspace.open_item(key, window, cx, move |window, cx| {
        TextEditor::new(
            session,
            action.type_name,
            action.id,
            action.property,
            action.read_at,
            action.text,
            window,
            cx,
        )
    })
}

/// Opens a node's page in a tab, or shows the tab it is in.
pub fn open_node_page(
    workspace: &mut Workspace,
    action: &OpenNodePage,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<NodePage>> {
    let session = workspace.session().cloned()?;
    let key = NodePage::key(&action.type_name, &action.id, action.at_commit.as_deref());
    let (type_name, id, at_commit) = (
        action.type_name.clone(),
        action.id.clone(),
        action.at_commit.clone(),
    );
    workspace.open_item(key, window, cx, move |_window, cx| {
        NodePage::new(session, type_name, id, at_commit, cx)
    })
}

/// Opens a Blob value in a tab of its own, or shows the tab it is in.
pub fn open_blob(
    workspace: &mut Workspace,
    action: &OpenBlob,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<BlobView>> {
    let session = workspace.session()?.clone();
    let cell = graph_client::BlobCell {
        edge: action.edge,
        type_name: action.type_name.clone(),
        id: action.id.clone(),
        property: action.property.clone(),
    };
    let snapshot = action.at_commit.clone();
    let key = BlobView::key(&cell, snapshot.as_deref());
    workspace.open_item(key, window, cx, move |window, cx| {
        BlobView::new(session, cell, snapshot, window, cx)
    })
}

/// Undo on a delete's toast (spec 004): reads what the delete commit removed and inserts it
/// all again in one commit, bound to that commit, so it applies only while the branch still
/// stands where the delete left it. Nothing is retried. The write's outcome toasts as every
/// write's does; this says only why nothing was sent.
fn undo_delete(workspace: &mut Workspace, action: &UndoDelete, cx: &mut Context<Workspace>) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    let (branch, schema) = {
        let session = session.read(cx);
        (session.branch.clone(), session.schema_model())
    };
    if branch != action.branch {
        workspace.show_toast(
            ToastKind::Error,
            format!(
                "Nothing was restored: the delete was on {}. Switch to it to undo.",
                action.branch
            ),
            cx,
        );
        return;
    }
    let Some(schema) = schema else {
        return;
    };
    let query =
        graph_store::deletes::undo_query(session.clone(), schema, action.commit.clone(), cx);
    let (commit, what) = (action.commit.clone(), action.what.clone());
    cx.spawn(async move |workspace, cx| {
        let toast = |kind: ToastKind, message: String, cx: &mut gpui::AsyncApp| {
            workspace
                .update(cx, |workspace, cx| workspace.show_toast(kind, message, cx))
                .ok();
        };
        let query = match query.await {
            Ok(query) => query,
            Err(reason) => {
                toast(
                    ToastKind::Error,
                    format!("Nothing was restored. {reason}"),
                    cx,
                );
                return;
            }
        };
        let write = session.update(cx, |session, cx| {
            session.write(&query, Some(commit), format!("Undo: {what}"), cx)
        });
        write.await;
    })
    .detach();
}
