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

use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Global, PathPromptOptions, Window};
use studio_actions::graph::{NewQuery, NewQueryWithText, OpenQueryAt, OpenQueryFile, OpenSchema};
use studio_workspace::{PanelPosition, ToastKind, Workspace};
use ui::IconName;

pub use schema_view::SchemaView;
pub use view::QueryEditor;

/// Bumped whenever a query file is written, so lists of saved queries read the folder again.
#[derive(Default)]
pub struct SavedQueriesChanged(pub usize);

impl Global for SavedQueriesChanged {}

pub(crate) fn saved_queries_changed(cx: &mut App) {
    cx.default_global::<SavedQueriesChanged>().0 += 1;
}

/// Opens `path` in a query tab, or the tab already editing it; the draft comes from `text`
/// when given (a remembered tab), else from the file.
fn open_file(
    workspace: &mut Workspace,
    path: PathBuf,
    text: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(session) = workspace.session().cloned() else {
        return;
    };
    let on_disk = std::fs::read_to_string(&path).ok();
    if on_disk.is_none() && text.is_none() {
        workspace.show_toast(
            ToastKind::Error,
            format!("{} could not be read.", path.display()),
            cx,
        );
        return;
    }
    let number = view::next_number();
    let key = view::file_key(&path);
    workspace.open_item(key, window, cx, move |window, cx| {
        let mut editor = QueryEditor::new(number, session, window, cx);
        editor.set_text(
            text.as_deref().or(on_disk.as_deref()).unwrap_or_default(),
            window,
            cx,
        );
        editor.set_file(path, on_disk, cx);
        editor
    });
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
        let at_commit = data
            .get("at_commit")
            .and_then(|commit| commit.as_str())
            .map(str::to_owned);
        if let Some(path) = data.get("path").and_then(|path| path.as_str()) {
            open_file(workspace, PathBuf::from(path), Some(text), window, cx);
            return;
        }
        let number = view::next_number();
        workspace.open_item(format!("query:{number}"), window, cx, |window, cx| {
            let mut editor = QueryEditor::new(number, session, window, cx);
            editor.set_text(&text, window, cx);
            if let Some(commit) = at_commit {
                editor.pin_to(commit, cx);
            }
            editor
        });
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
            workspace.add_panel(
                PanelPosition::Left,
                queries_panel::QUERIES_PANEL,
                "Queries",
                IconName::FileCode,
                panel.into(),
                cx,
            );
        }
        workspace.register_action(|workspace, action: &OpenQueryFile, window, cx| {
            match &action.path {
                Some(path) => open_file(workspace, PathBuf::from(path), None, window, cx),
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
                let mut editor = QueryEditor::new(number, session, window, cx);
                editor.set_text(&text, window, cx);
                editor
            });
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
