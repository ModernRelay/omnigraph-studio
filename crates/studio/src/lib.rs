//! OmniGraph Studio's composition root: every crate is initialised here, in layer order
//! (`docs/architecture.md`). `main.rs` runs it; the snapshot example renders it offscreen.

mod activity;
mod keymaps;
mod open_graph;

use gpui::{App, AppContext as _, Entity, Window, WindowBounds, WindowOptions, point, px, size};
use graph_store::{ConnectionRegistry, ConnectionState, open_session};
use studio_actions::workspace::{OpenGraph, Quit};
use studio_workspace::Workspace;

pub use open_graph::OpenGraphModal;

/// Sets up settings, themes, the editor, keymaps, the server registry and every feature.
pub fn init(cx: &mut App) {
    studio_theme::init(cx);
    editor::init(cx);
    graph_store::init(cx);
    keymaps::load(cx);

    command_palette::init(cx);
    type_table::init(cx);
    relationship_tree::init(cx);
    graph_canvas::init(cx);
    query_editor::init(cx);
    review::init(cx);
    history::init(cx);
    inspectors::init(cx);

    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let handle = cx.entity().downgrade();
        let activity = cx.new(|cx| activity::ActivityLog::new(handle, window, cx));
        workspace.set_activity_view(activity.into(), cx);
        workspace.register_action(|workspace, _: &OpenGraph, window, cx| {
            let handle = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, |window, cx| {
                OpenGraphModal::new(handle, window, cx)
            });
        });
    })
    .detach();
}

pub fn window_options(cx: &App) -> WindowOptions {
    let bounds = gpui::Bounds::centered(None, size(px(1440.), px(900.)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("OmniGraph Studio".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(12.), px(12.))),
        }),
        app_id: Some("com.modernrelay.omnigraph-studio".into()),
        ..Default::default()
    }
}

/// Builds the root view of a window.
pub fn build_workspace(window: &mut Window, cx: &mut App) -> Entity<Workspace> {
    let workspace = cx.new(|cx| Workspace::new(window, cx));
    open_initial_graph(&workspace, window, cx);
    workspace
}

/// Opens `OMNIGRAPH_STUDIO_GRAPH` on the environment server when both are set, else shows the
/// "Open a graph" modal.
fn open_initial_graph(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let registry = ConnectionRegistry::global(cx);
    let graph = std::env::var("OMNIGRAPH_STUDIO_GRAPH").ok();
    let environment = registry.read(cx).find("environment", cx);
    match (environment, graph) {
        (Some(connection), Some(graph)) => {
            connection.update(cx, |connection, cx| connection.connect(cx));
            let branch = std::env::var("OMNIGRAPH_STUDIO_BRANCH").unwrap_or_else(|_| "main".into());
            let session = open_session(connection, graph, branch, cx);
            workspace.update(cx, |workspace, cx| {
                workspace.set_session(session, window, cx)
            });
        }
        _ => {
            for connection in registry.read(cx).connections.clone() {
                if connection.read(cx).state == ConnectionState::Idle {
                    connection.update(cx, |connection, cx| connection.connect(cx));
                }
            }
            let handle = workspace.downgrade();
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    OpenGraphModal::new(handle, window, cx)
                });
            });
        }
    }
}
