//! OmniGraph Studio's composition root: every crate is initialised here, in layer order
//! (`docs/architecture.md`). `main.rs` runs it; the snapshot example renders it offscreen.

mod activity;
pub mod deep_link;
mod design_gallery;
mod keymaps;
pub mod logging;
mod menus;
mod open_graph;
mod settings_view;

use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, Window, WindowBounds, WindowOptions,
    point, px, size,
};
use graph_store::{ConnectionRegistry, ConnectionState, open_session};
use studio_actions::workspace::{NewWindow, OpenGraph, OpenLink, OpenSettings, Quit, RevealLog};
use studio_workspace::{SavedWindow, ToastKind, Workspace};

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

    studio_workspace::register_item_kind(cx, "design-system", |_, workspace, window, cx| {
        workspace.open_item(
            "design-system",
            window,
            cx,
            design_gallery::DesignGallery::new,
        );
    });
    studio_workspace::register_item_kind(cx, "settings", |_, workspace, window, cx| {
        open_settings(workspace, window, cx);
    });
    cx.on_action(|_: &Quit, cx| cx.quit());
    // The file when it exists; a run from a terminal logs to its output, so then the folder.
    cx.on_action(|_: &RevealLog, cx| {
        let file = logging::log_file();
        if file.exists() {
            cx.reveal_path(&file);
        } else if let Some(dir) = file.parent()
            && std::fs::create_dir_all(dir).is_ok()
        {
            cx.open_with_system(dir);
        }
    });
    // Deferred: an action reaches here while its window is held for dispatch, and opening the
    // link updates that window.
    cx.on_action(|action: &OpenLink, cx| {
        let url = action.url.clone();
        cx.defer(move |cx| open_link(&url, cx));
    });
    cx.on_action(|_: &NewWindow, cx| {
        cx.open_window(window_options(None, cx), |window, cx| {
            let workspace = new_workspace(window, cx);
            ask_for_graph(&workspace, window, cx);
            workspace
        })
        .ok();
    });
    menus::set(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let handle = cx.entity().downgrade();
        let session = workspace.session().cloned();
        let activity = cx.new(|cx| activity::ActivityLog::new(handle, session, window, cx));
        workspace.set_activity_view(activity.into(), cx);
        workspace.register_action(
            |workspace, _: &studio_actions::workspace::OpenDesignSystem, window, cx| {
                workspace.open_item(
                    "design-system",
                    window,
                    cx,
                    design_gallery::DesignGallery::new,
                );
            },
        );
        workspace.register_action(|workspace, _: &OpenSettings, window, cx| {
            open_settings(workspace, window, cx);
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

/// Opens the settings tab, offering the workspace's graph as a scope.
fn open_settings(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let graph = workspace.session().map(|session| {
        let session = session.read(cx);
        (session.settings_key(cx), session.graph_id.clone())
    });
    workspace.open_item("settings", window, cx, move |window, cx| {
        settings_view::SettingsView::new(graph, window, cx)
    });
}

/// A window's options, at `bounds` or else where the last moved window was.
pub fn window_options(bounds: Option<gpui::Bounds<gpui::Pixels>>, cx: &App) -> WindowOptions {
    let bounds = bounds
        .or_else(|| studio_workspace::saved_window_bounds(cx))
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

/// Opens Studio's windows: one for `OMNIGRAPH_STUDIO_GRAPH` when it is set, else every window
/// the last run had open with a graph on a server still known, each where it was, else one
/// window asking for a graph.
pub fn open_windows(cx: &mut App) {
    let saved = if environment_graph(cx).is_some() {
        Vec::new()
    } else {
        reopenable_windows(cx)
    };
    if saved.is_empty() {
        cx.open_window(window_options(None, cx), build_workspace)
            .ok();
        return;
    }
    for saved in saved {
        let bounds = saved.bounds(cx);
        cx.open_window(window_options(bounds, cx), move |window, cx| {
            let workspace = new_workspace(window, cx);
            reopen_window(&workspace, &saved, window, cx);
            workspace
        })
        .ok();
    }
}

/// Builds the root view of a window and opens its first graph: the environment's, else the
/// first window the last run had open, else none, asking for one.
pub fn build_workspace(window: &mut Window, cx: &mut App) -> Entity<Workspace> {
    let workspace = new_workspace(window, cx);
    open_initial_graph(&workspace, window, cx);
    workspace
}

fn new_workspace(window: &mut Window, cx: &mut App) -> Entity<Workspace> {
    let workspace = cx.new(|cx| Workspace::new(window, cx));
    // Actions dispatch along the focus path; until something inside the workspace has focus they
    // stop at the window root and never reach the workspace's handlers.
    let focus = workspace.read(cx).focus_handle(cx);
    window.focus(&focus, cx);
    workspace
}

/// The saved windows that still have a graph on a known server, with only those graphs.
fn reopenable_windows(cx: &App) -> Vec<SavedWindow> {
    let registry = ConnectionRegistry::global(cx);
    let known = |url: &str| {
        registry
            .read(cx)
            .connections
            .iter()
            .any(|connection| connection.read(cx).server.url == url)
    };
    studio_workspace::load_windows()
        .into_iter()
        .filter_map(|mut window| {
            let shown = window.session.graphs.get(window.session.shown).cloned();
            window
                .session
                .graphs
                .retain(|graph| known(&graph.server_url));
            window.session.shown = shown
                .and_then(|shown| window.session.graphs.iter().position(|g| *g == shown))
                .unwrap_or(0);
            (!window.session.graphs.is_empty()).then_some(window)
        })
        .collect()
}

/// Reopens a saved window's graphs, showing the one it showed.
fn reopen_window(
    workspace: &Entity<Workspace>,
    saved: &SavedWindow,
    window: &mut Window,
    cx: &mut App,
) {
    let registry = ConnectionRegistry::global(cx);
    let connections = registry.read(cx).connections.clone();
    let mut shown = None;
    for (index, graph) in saved.session.graphs.iter().enumerate() {
        let Some(connection) = connections
            .iter()
            .find(|c| c.read(cx).server.url == graph.server_url)
            .cloned()
        else {
            continue;
        };
        if connection.read(cx).state == ConnectionState::Idle {
            connection.update(cx, |connection, cx| connection.connect(cx));
        }
        let session = open_session(connection, graph.graph.clone(), graph.branch.clone(), cx);
        if index == saved.session.shown {
            shown = Some(session.entity_id());
        }
        workspace.update(cx, |workspace, cx| {
            workspace.set_session(session, window, cx)
        });
    }
    if let Some(id) = shown {
        workspace.update(cx, |workspace, cx| workspace.show_graph(id, window, cx));
    }
}

/// The environment server and `OMNIGRAPH_STUDIO_GRAPH`, when both are set.
fn environment_graph(cx: &App) -> Option<(Entity<graph_store::Connection>, String)> {
    let graph = std::env::var("OMNIGRAPH_STUDIO_GRAPH").ok()?;
    let connection = ConnectionRegistry::global(cx)
        .read(cx)
        .find("environment", cx)?;
    Some((connection, graph))
}

/// Connects to every known server and shows "Open a graph".
fn ask_for_graph(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let registry = ConnectionRegistry::global(cx);
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

/// Opens `OMNIGRAPH_STUDIO_GRAPH` on the environment server when both are set, else the first
/// window the last run had open, else shows "Open a graph".
fn open_initial_graph(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
    if let Some((connection, graph)) = environment_graph(cx) {
        connection.update(cx, |connection, cx| connection.connect(cx));
        let branch = std::env::var("OMNIGRAPH_STUDIO_BRANCH").unwrap_or_else(|_| "main".into());
        let session = open_session(connection, graph, branch, cx);
        workspace.update(cx, |workspace, cx| {
            workspace.set_session(session, window, cx)
        });
    } else if let Some(saved) = reopenable_windows(cx).first() {
        reopen_window(workspace, saved, window, cx);
    } else {
        ask_for_graph(workspace, window, cx);
    }
}

/// Opens an `omnigraph-studio://` link (`deep_link`): in the window that already has its graph
/// open, else in the active window, else in a new one. What the link names is shown, never run;
/// a server Studio does not know is refused rather than added.
pub fn open_link(link: &str, cx: &mut App) {
    let link = match deep_link::parse(link) {
        Ok(link) => link,
        Err(message) => return tell(cx, ToastKind::Error, message),
    };
    let Some(target) = link.target().cloned() else {
        return tell(
            cx,
            ToastKind::Info,
            "This link comes from `omnigraph studio`, which needs a later OmniGraph. Open the graph \
             from “Open a graph” instead."
                .into(),
        );
    };
    let wanted = graph_client::server_root(&target.server).unwrap_or(target.server.clone());
    let connection = ConnectionRegistry::global(cx)
        .read(cx)
        .connections
        .iter()
        .find(|connection| {
            let url = &connection.read(cx).server.url;
            graph_client::server_root(url).as_deref().unwrap_or(url) == wanted
        })
        .cloned();
    let Some(connection) = connection else {
        return tell(
            cx,
            ToastKind::Error,
            format!(
                "Studio does not know the server {}. Add it under “Open a graph” first; a link \
                 never adds a server.",
                target.server
            ),
        );
    };
    let branch = target.branch.clone().unwrap_or_else(|| "main".into());
    let open_in = move |workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App| {
        // The graph on this branch, if the window has it open; else open it here.
        let open = workspace
            .read(cx)
            .open_graphs()
            .into_iter()
            .find(|(session, _)| {
                let session = session.read(cx);
                session.graph_id == target.graph
                    && session.branch == branch
                    && session.connection == connection
            });
        match open {
            Some((session, _)) => workspace.update(cx, |workspace, cx| {
                workspace.show_graph(session.entity_id(), window, cx)
            }),
            None => {
                if connection.read(cx).state == ConnectionState::Idle {
                    connection.update(cx, |connection, cx| connection.connect(cx));
                }
                let session =
                    open_session(connection.clone(), target.graph.clone(), branch.clone(), cx);
                workspace.update(cx, |workspace, cx| {
                    workspace.set_session(session, window, cx)
                });
            }
        }
        let focus = workspace.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        match &link {
            DeepLink::Node { type_name, id, .. } => window.dispatch_action(
                Box::new(studio_actions::graph::OpenNode {
                    type_name: type_name.clone(),
                    id: id.clone(),
                }),
                cx,
            ),
            DeepLink::Query { text, .. } => window.dispatch_action(
                Box::new(studio_actions::graph::NewQueryWithText { text: text.clone() }),
                cx,
            ),
            DeepLink::Graph(_) | DeepLink::Connect => {}
        }
        window.activate_window();
    };
    match link_window(cx) {
        // Through the untyped handle, so the workspace is not held while it is opened into.
        Some(handle) => {
            cx.update_window(handle, |root, window, cx| {
                if let Ok(workspace) = root.downcast::<Workspace>() {
                    open_in(&workspace, window, cx);
                }
            })
            .ok();
        }
        None => {
            cx.open_window(window_options(None, cx), move |window, cx| {
                let workspace = new_workspace(window, cx);
                open_in(&workspace, window, cx);
                workspace
            })
            .ok();
        }
    }
    cx.activate(true);
}

use deep_link::DeepLink;

/// The workspace window a link opens in: the active one, else any.
fn link_window(cx: &App) -> Option<gpui::AnyWindowHandle> {
    let is_workspace = |window: &gpui::AnyWindowHandle| window.downcast::<Workspace>().is_some();
    cx.active_window()
        .filter(is_workspace)
        .or_else(|| cx.windows().into_iter().find(is_workspace))
}

/// Says why a link did not open, in a window if there is one.
fn tell(cx: &mut App, kind: ToastKind, message: String) {
    match link_window(cx) {
        Some(handle) => {
            cx.update_window(handle, |root, _, cx| {
                if let Ok(workspace) = root.downcast::<Workspace>() {
                    workspace.update(cx, |workspace, cx| workspace.show_toast(kind, message, cx));
                }
            })
            .ok();
        }
        None => {
            cx.open_window(window_options(None, cx), move |window, cx| {
                let workspace = new_workspace(window, cx);
                workspace.update(cx, |workspace, cx| workspace.show_toast(kind, message, cx));
                workspace
            })
            .ok();
        }
    }
}
