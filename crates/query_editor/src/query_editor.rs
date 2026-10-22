//! The `.gq` editor and run loop (spec 007): Zed's editor with highlighting, diagnostics and
//! completion from `gq_language`, then reads, confirmed writes, parameters, results and plans.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/007-query-editor/plan.md`.

mod analysis;
mod completion;
mod hover;
mod queries_panel;
mod results;
mod schema_view;
mod syntax;
mod view;

use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Entity, Global, PathPromptOptions, Window};
use graph_client::api::QueryCatalogEntry;
use serde_json::{Map, Value};
use studio_actions::graph::{
    NewQuery, NewQueryWithText, OpenQueryAt, OpenQueryFile, OpenSchema, OpenStoredQuery,
};
use studio_workspace::{ItemEvent, PanelPosition, ToastKind, Workspace};
use ui::IconName;

use schema_view::SchemaView;
use view::QueryEditor;

/// Bumped whenever a query file is written, so lists of saved queries read the folder again.
#[derive(Default)]
pub struct SavedQueriesChanged(pub usize);

impl Global for SavedQueriesChanged {}

pub(crate) fn saved_queries_changed(cx: &mut App) {
    cx.default_global::<SavedQueriesChanged>().0 += 1;
}

/// Opens `path` in a query tab, or shows the tab already editing it as it is. A new tab opens
/// at once, in its place among the others, and reads the file off the main thread, saying it
/// is reading until the text is in. `draft` is a remembered tab's text and what it was typed
/// over (`QueryEditor::file_read` decides between it and the file); with nothing remembered, a
/// file that cannot be read closes the tab and says so.
fn open_file(
    workspace: &mut Workspace,
    path: PathBuf,
    draft: Option<view::Draft>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<QueryEditor>> {
    let session = workspace.session().cloned()?;
    let number = view::next_number();
    let key = view::file_key(&path);
    let mut opened = false;
    let tab_path = path.clone();
    let editor = workspace.open_item(key, window, cx, |window, cx| {
        opened = true;
        QueryEditor::reading(number, session, tab_path, draft, window, cx)
    })?;
    if !opened {
        return Some(editor);
    }
    let read = {
        let path = path.clone();
        cx.background_spawn(async move { std::fs::read_to_string(&path).ok() })
    };
    let tab = editor.downgrade();
    cx.spawn_in(window, async move |workspace, cx| {
        let on_disk = read.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                let Some(tab) = tab.upgrade() else {
                    return;
                };
                if !tab.update(cx, |tab, cx| tab.file_read(on_disk, window, cx)) {
                    workspace.show_toast(
                        ToastKind::Error,
                        format!("{} could not be read.", path.display()),
                        cx,
                    );
                    tab.update(cx, |_, cx| cx.emit(ItemEvent::Close));
                }
            })
            .ok();
    })
    .detach();
    Some(editor)
}

/// Opens the tab running the stored query `name`, or the one already open (spec 007). `values`,
/// a remembered tab's, fill the parameters. A query the catalog has not been read for yet opens
/// and takes its parameters when it is.
fn open_stored(
    workspace: &mut Workspace,
    name: String,
    values: Option<Map<String, Value>>,
    at_commit: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<QueryEditor>> {
    let Some(session) = workspace.session().cloned() else {
        workspace.show_toast(
            ToastKind::Info,
            "Open a graph first; stored queries belong to one.",
            cx,
        );
        return None;
    };
    session.update(cx, |session, cx| session.want_stored_queries(cx));
    let entry = session
        .read(cx)
        .stored_query(&name)
        .unwrap_or_else(|| QueryCatalogEntry {
            name: name.clone(),
            tool_name: None,
            description: None,
            instruction: None,
            mutation: false,
            params: Vec::new(),
        });
    let number = view::next_number();
    let opened = workspace.open_item(view::stored_key(&name), window, cx, |window, cx| {
        let mut editor = QueryEditor::new_stored(number, session, entry, window, cx);
        if let Some(commit) = at_commit {
            editor.pin_to(commit, cx);
        }
        editor
    });
    let editor = opened?;
    if let Some(values) = values {
        editor.update(cx, |editor, cx| editor.fill_params(&values, window, cx));
    }
    Some(editor)
}

/// The result columns' dragged widths a remembered tab had.
fn saved_widths(data: &Value) -> BTreeMap<String, f32> {
    data.get("widths")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, width)| Some((name.clone(), width.as_f64()? as f32)))
        .collect()
}

/// Opens the schema tab, or activates it; with a type, shows its declaration.
fn open_schema(
    workspace: &mut Workspace,
    type_name: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        workspace.show_toast(
            ToastKind::Info,
            "Open a graph first to read its schema.",
            cx,
        );
        return;
    };
    let view = workspace.open_item(schema_view::SCHEMA_KEY, window, cx, |window, cx| {
        SchemaView::new(session, window, cx)
    });
    if let Some(type_name) = type_name
        && let Some(view) = view
    {
        view.update(cx, |view, cx| view.reveal(type_name, window, cx));
    }
}

/// Registers `NewQuery` with every new workspace. The editor's keys are in `assets/keymaps/`.
pub fn init(cx: &mut App) {
    studio_workspace::register_item_kind(cx, "query", |data, workspace, window, cx| {
        let Some(session) = workspace.session().cloned() else {
            return;
        };
        let text = data
            .get("text")
            .and_then(|text| text.as_str())
            .map(str::to_owned);
        let at_commit = data
            .get("at_commit")
            .and_then(|commit| commit.as_str())
            .map(str::to_owned);
        let widths = saved_widths(data);
        if let Some(path) = data.get("path").and_then(|path| path.as_str()) {
            // A file tab keeps its text with the hash of the file's text it was typed over,
            // and says whether it was the file's (`saved`): that is shown only when the file
            // cannot be read. A build that kept no hash kept its text always, and whether its
            // file changed since is not known.
            let draft = text.map(|text| view::Draft {
                text,
                based_on: data
                    .get("based_on")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                saved: data.get("saved").and_then(Value::as_bool).unwrap_or(false),
            });
            if let Some(editor) = open_file(workspace, PathBuf::from(path), draft, window, cx) {
                editor.update(cx, |editor, cx| {
                    editor.set_column_widths(widths);
                    if let Some(commit) = at_commit {
                        editor.pin_to(commit, cx);
                    }
                });
            }
            return;
        }
        let number = view::next_number();
        let text = text.unwrap_or_default();
        workspace.open_item(format!("query:{number}"), window, cx, |window, cx| {
            // Its text, even emptied, is the tab's: the example does not fill it again.
            let mut editor = QueryEditor::with_text(number, session, &text, window, cx);
            editor.set_column_widths(widths);
            if let Some(commit) = at_commit {
                editor.pin_to(commit, cx);
            }
            editor
        });
    });
    studio_workspace::register_item_kind(cx, "stored_query", |data, workspace, window, cx| {
        let Some(name) = data.get("name").and_then(Value::as_str) else {
            return;
        };
        let text = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_owned);
        let values = data.get("values").and_then(Value::as_object).cloned();
        let opened = open_stored(
            workspace,
            name.to_owned(),
            values,
            text("at_commit"),
            window,
            cx,
        );
        if let Some(editor) = opened {
            let widths = saved_widths(data);
            editor.update(cx, |editor, _| editor.set_column_widths(widths));
        }
    });
    studio_workspace::register_item_kind(
        cx,
        schema_view::SCHEMA_KEY,
        |_, workspace, window, cx| {
            open_schema(workspace, None, window, cx);
        },
    );
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if let Some(window) = window {
            let handle = cx.entity().downgrade();
            let session = workspace.session().cloned();
            let panel = cx.new(|cx| queries_panel::QueriesPanel::new(handle, session, window, cx));
            let actions = cx.new(|cx| queries_panel::QueriesActions::new(&panel, cx));
            workspace.add_panel(
                PanelPosition::Left,
                queries_panel::QUERIES_PANEL,
                "Queries",
                IconName::FileCode,
                panel.clone().into(),
                cx,
            );
            workspace.set_panel_actions(queries_panel::QUERIES_PANEL, actions.into(), cx);
            // Shown from its button or its key, the panel takes the keyboard: ↑ ↓ and ↵.
            workspace.set_panel_focus(
                queries_panel::QUERIES_PANEL,
                move |cx| Some(gpui::Focusable::focus_handle(panel.read(cx), cx)),
                cx,
            );
        }
        workspace.register_action(|workspace, action: &OpenQueryFile, window, cx| {
            match &action.path {
                Some(path) => {
                    open_file(workspace, PathBuf::from(path), None, window, cx);
                }
                None => {
                    let chosen = cx.prompt_for_paths(PathPromptOptions {
                        files: true,
                        directories: false,
                        multiple: false,
                        prompt: Some("Open".into()),
                    });
                    cx.spawn_in(window, async move |workspace, cx| {
                        let Ok(Ok(Some(paths))) = chosen.await else {
                            return;
                        };
                        let Some(path) = paths.into_iter().next() else {
                            return;
                        };
                        workspace
                            .update_in(cx, |workspace, window, cx| {
                                open_file(workspace, path, None, window, cx)
                            })
                            .ok();
                    })
                    .detach();
                }
            }
        });
        workspace.register_action(|workspace, action: &OpenQueryAt, window, cx| {
            let Some(session) = workspace.session().cloned() else {
                return;
            };
            let number = view::next_number();
            let commit = action.commit_id.clone();
            workspace.open_item(format!("query:{number}"), window, cx, |window, cx| {
                let mut editor = QueryEditor::new(number, session, window, cx);
                editor.pin_to(commit, cx);
                editor
            });
        });
        workspace.register_action(|workspace, action: &NewQueryWithText, window, cx| {
            let Some(session) = workspace.session().cloned() else {
                return;
            };
            let number = view::next_number();
            let text = action.text.clone();
            workspace.open_item(format!("query:{number}"), window, cx, |window, cx| {
                QueryEditor::with_text(number, session, &text, window, cx)
            });
        });
        workspace.register_action(|workspace, action: &OpenStoredQuery, window, cx| {
            open_stored(workspace, action.name.clone(), None, None, window, cx);
        });
        workspace.register_action(|workspace, action: &OpenSchema, window, cx| {
            open_schema(workspace, action.type_name.clone(), window, cx);
        });
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
