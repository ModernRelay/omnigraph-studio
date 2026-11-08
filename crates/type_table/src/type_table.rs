//! One virtualized table per node or edge type: engine-side filter and count, keyset pages
//! pinned to one commit, peek, value-bound cell edits.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/004-type-tables/plan.md`.

mod blob;
mod blob_view;
mod blob_write;
mod branch_diff;
mod cell_select;
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
use filter_words as words;
mod write;

use gpui::{App, AppContext as _, Context, Entity, WeakEntity, Window};
use gq_language::{Filter, FilterOp};
use graph_store::GraphSession;
use serde_json::Value;
use studio_actions::graph::{
    OpenBlob, OpenNode, OpenNodePage, OpenSavedView, OpenTextEditor, OpenType, PeekNode,
    RestoreSavedView, UndoDelete,
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
        if let Some((type_name, at_commit, opening)) = saved_table(data) {
            open_type(workspace, &type_name, at_commit, opening, false, window, cx);
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
        workspace.register_action(|workspace, action: &OpenSavedView, window, cx| {
            open_saved_view(workspace, action, window, cx);
        });
        workspace.register_action(|workspace, action: &RestoreSavedView, _window, cx| {
            restore_saved_view(workspace, action, cx);
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
            // Filters that come with the action: a captured set's table, reopened (spec 020).
            let captured: Vec<Filter> = action
                .filters
                .clone()
                .and_then(|filters| serde_json::from_value(filters).ok())
                .unwrap_or_default();
            let opening = Opening {
                filters: filter.clone().into_iter().chain(captured).collect(),
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
                action.snapshot.clone(),
                cx,
            );
            // A peek asked for shows the inspector, even when its dock was hidden.
            workspace.reveal_inspector(cx);
        });
        // A node opens in the center, as its page (spec 020); a peek is `PeekNode`.
        workspace.register_action(|workspace, action: &OpenNode, window, cx| {
            let page = OpenNodePage {
                type_name: action.type_name.clone(),
                id: action.id.clone(),
                at_commit: None,
            };
            open_node_page(workspace, &page, None, window, cx);
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
    let key = match &opening.view {
        Some(view) => view_key(view, at_commit.as_deref()),
        None => table_key(type_name, at_commit.as_deref()),
    };
    workspace.open_item_with::<TypeTable>(key, preview, window, cx, move |window, cx| {
        TypeTable::opening(handle, session, name, at_commit, opening, window, cx)
    })
}

/// What every type table's tab key starts with ([`table_key`]).
/// A type table tab as `TypeTable::serialize` saved it: its type, the commit it is held at, and
/// how it opens again.
pub(crate) fn saved_table(data: &Value) -> Option<(String, Option<String>, Opening)> {
    let type_name = data.get("type_name")?.as_str()?.to_owned();
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
    let filters = data
        .get("filters")
        .cloned()
        .and_then(|filters| {
            serde_json::from_value::<Vec<studio_settings::SavedFilter>>(filters).ok()
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|filter| {
            Some(gq_language::Filter {
                property: filter.property,
                op: gq_language::FilterOp::from_key(&filter.op)?,
                value: filter.value,
            })
        })
        .collect();
    // Its first read is the one its filters, order and grouping make.
    let opening = Opening {
        filters,
        sort,
        group_by,
        layout,
        view: data.get("view").and_then(Value::as_str).map(str::to_owned),
    };
    Some((type_name, at_commit, opening))
}

/// What a saved view's tab first shows: its filters, order, grouping and columns. Those the
/// schema no longer has are left out when it is read, as a restored tab's are.
pub(crate) fn view_opening(view: &studio_settings::SavedView) -> Opening {
    Opening {
        filters: view
            .filters
            .iter()
            .filter_map(|filter| {
                Some(Filter {
                    property: filter.property.clone(),
                    op: FilterOp::from_key(&filter.op)?,
                    value: filter.value.clone(),
                })
            })
            .collect(),
        sort: view.sort.as_ref().map(|sort| gq_language::Sort {
            property: sort.property.clone(),
            descending: sort.descending,
        }),
        group_by: view.group_by.clone(),
        layout: model::ColumnLayout::new(view.hidden.clone(), view.order.clone()),
        view: Some(view.key()),
    }
}

/// A node type's view as a stored query (spec 024), ready to paste into a `.gq` file in the
/// cluster's `queries/` folder: the query, then the values the view used for its parameters,
/// in comments above it. `Err` says why it cannot be one.
pub(crate) fn stored_query_text(
    schema: &gq_language::SchemaModel,
    view: &studio_settings::SavedView,
    filters: &[Filter],
) -> Result<(String, String), String> {
    if schema.node(&view.type_name).is_none() {
        return Err(format!(
            "Only a view of a node type can be a stored query, and {} is not one.",
            view.type_name
        ));
    }
    if filters.iter().any(|filter| filter.op.is_date_preset()) {
        return Err(
            "A filter for today, this week or the last 30 days moves every day, and a stored \
             query's parameter is a fixed date. Use an on-or-after filter, then copy it."
                .to_owned(),
        );
    }
    let sort = view.sort.as_ref().map(|sort| gq_language::Sort {
        property: sort.property.clone(),
        descending: sort.descending,
    });
    let name = if view.name.is_empty() {
        &view.type_name
    } else {
        &view.name
    };
    let query = gq_language::builder::stored_view_query(
        schema,
        &view.type_name,
        filters,
        sort.as_ref(),
        name,
    )
    .map_err(|error| format!("Studio could not write it as a query: {error:?}"))?;
    let mut text = format!(
        "// {} as a stored query. Save it in your cluster's queries/ folder, then run\n\
         // `omnigraph cluster apply`.\n",
        if view.name.is_empty() {
            format!("The {} table", view.type_name)
        } else {
            format!("The view “{}” of {}", view.name, view.type_name)
        }
    );
    if !query.params.is_empty() {
        text.push_str(&format!(
            "// The values the view used: {}\n",
            Value::Object(query.params.clone())
        ));
    }
    text.push_str(&query.text);
    Ok((query.name, text))
}

/// Copies a node type's view as a stored query (spec 024) and says so, or why it could not.
pub(crate) fn copy_as_stored_query(
    workspace: &WeakEntity<Workspace>,
    schema: Option<&gq_language::SchemaModel>,
    view: &studio_settings::SavedView,
    filters: &[Filter],
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let outcome = match schema {
        Some(schema) => stored_query_text(schema, view, filters),
        None => Err("The schema is still being read; try again in a moment.".to_owned()),
    };
    match outcome {
        Ok((name, text)) => {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    ToastKind::Info,
                    format!(
                        "Copied the stored query {name}. Save it in your cluster's queries/ \
                         folder and apply the cluster."
                    ),
                    cx,
                )
            });
        }
        Err(why) => workspace.update(cx, |workspace, cx| {
            workspace.show_toast(ToastKind::Error, why, cx)
        }),
    }
}

/// Opens a saved view's tab (spec 024), or shows the one it is in. Step 1 opens every view as
/// a table.
fn open_saved_view(
    workspace: &mut Workspace,
    action: &OpenSavedView,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<TypeTable>> {
    let session = workspace.session().cloned()?;
    let graph = session.read(cx).overrides_key();
    let Some(view) = studio_settings::StudioSettings::get(cx)
        .view(&graph, &action.id)
        .cloned()
    else {
        workspace.show_toast(ToastKind::Error, "That view was deleted.", cx);
        return None;
    };
    // A view of a stored query opens in the query editor's tab for it (spec 024).
    if let Some(query) = &view.query {
        let gone = session
            .read(cx)
            .stored_queries
            .loaded()
            .is_some_and(|queries| !queries.iter().any(|q| q.name == query.stored));
        if gone {
            workspace.show_toast(
                ToastKind::Error,
                format!(
                    "“{}” runs the stored query {}, which this graph no longer serves.",
                    view.name, query.stored
                ),
                cx,
            );
        } else {
            window.dispatch_action(
                Box::new(studio_actions::graph::OpenStoredQuery {
                    name: query.stored.clone(),
                    view: Some(view.key()),
                    preview: action.preview,
                }),
                cx,
            );
        }
        return None;
    }
    if let Some(schema) = session.read(cx).schema_model()
        && schema.node(&view.type_name).is_none()
        && schema.edge(&view.type_name).is_none()
    {
        workspace.show_toast(
            ToastKind::Error,
            format!(
                "“{}” shows {}, which this branch's schema does not have.",
                view.name, view.type_name
            ),
            cx,
        );
        return None;
    }
    let handle = cx.entity().downgrade();
    let opening = view_opening(&view);
    workspace.open_item_with::<TypeTable>(
        view_key(&view.key(), None),
        action.preview,
        window,
        cx,
        move |window, cx| {
            TypeTable::opening(handle, session, view.type_name, None, opening, window, cx)
        },
    )
}

/// Puts back a deleted view where it was (the Undo on its toast).
fn restore_saved_view(
    workspace: &mut Workspace,
    action: &RestoreSavedView,
    cx: &mut Context<Workspace>,
) {
    let Ok(view) = serde_json::from_value::<studio_settings::SavedView>(action.view.clone()) else {
        return;
    };
    let name = view.name.clone();
    let graph = action.graph.clone();
    let at = action.at;
    let taken = studio_settings::StudioSettings::get(cx).view_name_taken(&graph, &name, None);
    if taken {
        workspace.show_toast(
            ToastKind::Error,
            format!("“{name}” was not restored: another view has its name now."),
            cx,
        );
        return;
    }
    studio_settings::update(cx, move |settings| {
        settings.restore_view(&graph, at, view);
    });
}

/// What a saved view's tab key starts with ([`view_key`]).
pub(crate) const VIEW_KEY_PREFIX: &str = "view:";

/// One tab per saved view at the head, and one per view and commit.
pub(crate) fn view_key(id: &str, at_commit: Option<&str>) -> String {
    match at_commit {
        Some(commit) => format!("{VIEW_KEY_PREFIX}{id}@{commit}"),
        None => format!("{VIEW_KEY_PREFIX}{id}"),
    }
}

pub(crate) const TABLE_KEY_PREFIX: &str = "type:";

/// One tab per type at the head, and one per type and commit.
pub(crate) fn table_key(type_name: &str, at_commit: Option<&str>) -> String {
    match at_commit {
        Some(commit) => format!("{TABLE_KEY_PREFIX}{type_name}@{commit}"),
        None => format!("{TABLE_KEY_PREFIX}{type_name}"),
    }
}

/// Shows one node in the inspector, held at `at_commit`, or else read at `snapshot`, the
/// commit the view it comes from shows, or at the branch head.
fn peek_node(
    workspace: &mut Workspace,
    type_name: &str,
    id: &str,
    at_commit: Option<String>,
    snapshot: Option<String>,
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
    let snapshot = at_commit.or(snapshot);
    let peek = cx.new(|cx| {
        let mut peek = NodePeek::new(
            session,
            subject,
            None,
            snapshot,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A node type's view copies as a stored query with its values in a comment above it; an
    /// edge type's, or one filtered by a moving date, says why it cannot (spec 024).
    #[test]
    fn a_view_copies_as_a_stored_query_or_says_why_not() {
        let schema =
            gq_language::SchemaModel::parse(include_str!("../../../dev/cluster/knowledge.pg"))
                .unwrap();
        let view = studio_settings::SavedView {
            name: "Berlin companies".into(),
            type_name: "Company".into(),
            ..studio_settings::SavedView::default()
        };
        let filters = [Filter {
            property: "city".into(),
            op: FilterOp::Eq,
            value: serde_json::json!("Berlin"),
        }];
        let (name, text) = stored_query_text(&schema, &view, &filters).unwrap();
        assert_eq!(name, "berlin_companies");
        assert!(
            text.starts_with("// The view “Berlin companies” of Company as a stored query."),
            "{text}"
        );
        assert!(
            text.contains(r#"// The values the view used: {"city":"Berlin"}"#),
            "{text}"
        );
        assert!(
            text.contains("query berlin_companies($city: String)"),
            "{text}"
        );
        assert!(
            gq_language::check::check(&schema, &text)
                .diagnostics
                .is_empty(),
            "it type-checks with its comments: {text}"
        );

        let edge = schema.edges.first().unwrap().name.clone();
        let of_edge = studio_settings::SavedView {
            type_name: edge,
            ..view.clone()
        };
        assert!(stored_query_text(&schema, &of_edge, &[]).is_err());
        let today = [Filter {
            property: "created_at".into(),
            op: FilterOp::Today,
            value: Value::Null,
        }];
        let why = stored_query_text(&schema, &view, &today).unwrap_err();
        assert!(why.contains("fixed date"), "{why}");
    }
}
