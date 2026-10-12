//! OmniGraph Studio's composition root: every crate is initialised here, in layer order
//! (`docs/architecture.md`). `main.rs` runs it; the snapshot example renders it offscreen.

mod activity;
mod keymaps;
mod menus;
mod open_graph;
mod settings_view;

use gpui::{App, AppContext as _, Entity, Focusable as _, Window, WindowBounds, WindowOptions, point, px, size};
use graph_store::{ConnectionRegistry, ConnectionState, open_session};
use studio_actions::workspace::{NewWindow, OpenGraph, OpenSettings, Quit};
use studio_workspace::Workspace;

pub use open_graph::OpenGraphModal;

/// Sets up settings, themes, the editor, keymaps, the server registry and every feature.
pub fn init(cx: &mut App) {
    studio_theme::init_settings_store(cx);
    studio_settings::init(cx);
    studio_theme::init_themes(cx);
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
    agent_panel::init(cx);

    studio_workspace::register_item_kind(cx, "settings", |_, workspace, window, cx| {
        workspace.open_item("settings", window, cx, settings_view::SettingsView::new);
    });
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &NewWindow, cx| {
        cx.open_window(window_options(cx), build_workspace).ok();
    });
    menus::set(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let handle = cx.entity().downgrade();
        let activity = cx.new(|cx| activity::ActivityLog::new(handle, window, cx));
        workspace.set_activity_view(activity.into(), cx);
        workspace.register_action(|workspace, _: &OpenSettings, window, cx| {
            workspace.open_item("settings", window, cx, settings_view::SettingsView::new);
        });
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
    let bounds = studio_workspace::saved_window_bounds(cx)
        .unwrap_or_else(|| gpui::Bounds::centered(None, size(px(1440.), px(900.)), cx));
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
    // Actions dispatch along the focus path; until something inside the workspace has focus they
    // stop at the window root and never reach the workspace's handlers.
    let focus = workspace.read(cx).focus_handle(cx);
    window.focus(&focus, cx);
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
