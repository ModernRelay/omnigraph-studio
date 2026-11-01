//! One virtualized table per node or edge type: engine-side filter and count, keyset pages
//! pinned to one commit, peek, value-bound cell edits.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/004-type-tables/plan.md`.

mod blob;
mod blob_view;
mod blob_write;
mod dialogs;
mod fill;
mod grouping;
mod model;
mod node_page;
mod offers;
mod pdf;
mod peek;
mod suggest;
mod table;
#[cfg(test)]
mod test_support;
mod text_editor;
mod types_list;
mod words;
mod write;

use gpui::{App, AppContext as _, Context, Entity, Window};
use gq_language::{Filter, FilterOp};
use graph_store::GraphSession;
use serde_json::Value;
use studio_actions::graph::{
    OpenBlob, OpenNode, OpenNodePage, OpenTextEditor, OpenType, PeekNode, UndoDelete,
};
use studio_workspace::{PanelPosition, ToastKind, Workspace};
use ui::IconName;

use blob_view::BlobView;
use node_page::NodePage;
use peek::{NodePeek, PeekSubject};
use table::{Opening, TypeTable};
use text_editor::TextEditor;
use types_list::TypesList;

/// Registers this surface: the tabs it restores (a type's table, a node's page, a Blob, a long
/// text), and with every new workspace the left dock's Graph panel and the actions that open what it shows (`OpenType`, `OpenNode`, `OpenNodePage`,
/// `PeekNode`, `OpenBlob`, `OpenTextEditor`, `UndoDelete`). Its keys are in `assets/keymaps/`
/// (`TypeTable`, `TypeTable && editing`).
pub fn init(cx: &mut App) {
    // A Blob tab comes back with the value it showed, at the commit it read it at, writing to
    // the branch it wrote to.
    studio_workspace::register_item_kind(cx, "blob", |data, workspace, window, cx| {
        if let Some(saved) = saved_blob(data) {
            open_blob(workspace, &saved.open, saved.branch, window, cx);
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
            // Named as it was until it is shown: a page behind another tab reads nothing.
            open_node_page(workspace, &action, text("label"), window, cx);
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
            // Its first read is the one its order and grouping make.
            let opening = Opening {
                filters: Vec::new(),
                sort,
                group_by,
                layout,
            };
            open_type(workspace, type_name, at_commit, opening, false, window, cx);
        }
    });
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let handle = cx.entity().downgrade();
        let session = workspace.session().cloned();
        let types = cx.new(|cx| TypesList::new(handle, session, window, cx));
        let types: gpui::AnyView = types.into();
        workspace.add_panel(
            PanelPosition::Left,
            "types",
            "Data",
            IconName::Blocks,
            types.clone(),
            cx,
        );
        // A schema can hold hundreds of types: the list draws those in view.
        workspace.set_panel_scrolls_itself(&types, cx);
        workspace.register_action(|workspace, action: &UndoDelete, _window, cx| {
            undo_delete(workspace, action, cx);
        });
        workspace.register_action(|workspace, action: &OpenBlob, window, cx| {
            open_blob(workspace, action, None, window, cx);
        });
        workspace.register_action(|workspace, action: &OpenType, window, cx| {
            let filter = action.filter_id.clone().map(|id| match &action.via {
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
            });
            let opening = Opening {
                filters: filter.clone().into_iter().collect(),
                ..Opening::default()
            };
            let table = open_type(
                workspace,
                &action.type_name,
                action.at_commit.clone(),
                opening,
                action.preview,
                window,
                cx,
            );
            // A tab open already shows only it now; a new one opened with it.
            if let (Some(table), Some(filter)) = (table, filter) {
                table.update(cx, |table, cx| table.show_only(filter, cx));
            }
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
            open_node_page(workspace, action, None, window, cx);
        });
        workspace.register_action(|workspace, action: &OpenTextEditor, window, cx| {
            open_text_editor(workspace, action, window, cx);
        });
    })
    .detach();
}

/// Opens a type's table, at the branch head or read-only at a commit, showing what `opening`
/// says, or activates it when it is open.
fn open_type(
    workspace: &mut Workspace,
    type_name: &str,
    at_commit: Option<String>,
    opening: Opening,
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
        move |window, cx| TypeTable::opening(handle, session, name, at_commit, opening, window, cx),
    )
}

/// What every type table's tab key starts with ([`table_key`]).
pub(crate) const TABLE_KEY_PREFIX: &str = "type:";

/// One tab per type at the head, and one per type and commit.
pub(crate) fn table_key(type_name: &str, at_commit: Option<&str>) -> String {
    match at_commit {
        Some(commit) => format!("{TABLE_KEY_PREFIX}{type_name}@{commit}"),
        None => format!("{TABLE_KEY_PREFIX}{type_name}"),
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
    let peek: gpui::AnyView = peek.into();
    workspace.set_inspector(Some(peek.clone()), cx);
    // The peek is a list that draws, and reads, only the relationship groups in view.
    workspace.set_panel_scrolls_itself(&peek, cx);
}

/// Opens a text property in its editor, or shows the tab it is in.
fn open_text_editor(
    workspace: &mut Workspace,
    action: &OpenTextEditor,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
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
    });
}

/// Opens a node's page in a tab, or shows the tab it is in.
fn open_node_page(
    workspace: &mut Workspace,
    action: &OpenNodePage,
    saved_label: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    let key = NodePage::key(&action.type_name, &action.id, action.at_commit.as_deref());
    let (type_name, id, at_commit) = (
        action.type_name.clone(),
        action.id.clone(),
        action.at_commit.clone(),
    );
    workspace.open_item(key, window, cx, move |_window, cx| {
        NodePage::new(session, type_name, id, at_commit, saved_label, cx)
    });
}

/// A Blob tab as saved ([`BlobView`]'s `serialize`): the value it opens, and the branch it
/// writes to.
struct SavedBlob {
    open: OpenBlob,
    /// `None` only for a tab held at a commit, which writes nowhere.
    branch: Option<String>,
}

/// The Blob tab a saved one names: its value, the commit it read at, whether it holds that
/// commit, and the branch it writes to. One saved without saying whether it holds its commit
/// does not come back, nor one that writes without saying where: it would write to the branch
/// the window shows, which the commit it reads may not be on.
fn saved_blob(data: &Value) -> Option<SavedBlob> {
    let text = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_owned);
    let open = OpenBlob {
        type_name: text("type_name")?,
        id: text("id")?,
        property: text("property")?,
        edge: data.get("edge").and_then(Value::as_bool).unwrap_or(false),
        at_commit: text("at_commit"),
        held: data.get("held")?.as_bool()?,
    };
    let held = open.held && open.at_commit.is_some();
    let branch = text("branch");
    if branch.is_none() && !held {
        return None;
    }
    Some(SavedBlob { open, branch })
}

/// The value `action` opens.
fn blob_cell(action: &OpenBlob) -> graph_client::BlobCell {
    graph_client::BlobCell {
        edge: action.edge,
        type_name: action.type_name.clone(),
        id: action.id.clone(),
        property: action.property.clone(),
    }
}

/// The tab `action` opens, on `branch` when it is given (one restored on the branch it was
/// saved on), else on the session's.
fn blob_view(
    session: Entity<GraphSession>,
    action: &OpenBlob,
    branch: Option<String>,
    window: &mut Window,
    cx: &mut Context<BlobView>,
) -> BlobView {
    let view = BlobView::new(
        session,
        blob_cell(action),
        action.at_commit.clone(),
        action.held,
        window,
        cx,
    );
    match branch {
        Some(branch) => view.on_branch(branch),
        None => view,
    }
}

/// Opens a Blob value in a tab of its own, or shows the tab it is in: the one for its commit
/// and for the branch it writes to, `branch` or the session's.
fn open_blob(
    workspace: &mut Workspace,
    action: &OpenBlob,
    branch: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    let writes_to = branch
        .clone()
        .unwrap_or_else(|| session.read(cx).branch.clone());
    let at = action.at_commit.as_deref();
    let key = BlobView::key(&blob_cell(action), at, action.held, &writes_to);
    let action = action.clone();
    workspace.open_item(key, window, cx, move |window, cx| {
        blob_view(session, &action, branch, window, cx)
    });
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
    // To the delete's branch, though the window moves elsewhere while the rows are read.
    let branch = action.branch.clone();
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
            let description = format!("Undo: {what}");
            session.write_on(&branch, &query, Some(commit), description, None, cx)
        });
        write.await;
    })
    .detach();
}
