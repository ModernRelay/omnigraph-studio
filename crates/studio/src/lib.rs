//! Omnigraph Studio's composition root: every crate is initialised here, in layer order
//! (`docs/architecture.md`). `main.rs` runs it; the snapshot example renders it offscreen.

mod about;
mod activity;
pub mod deep_link;
#[cfg(debug_assertions)]
mod design_gallery;
mod keymaps;
pub mod logging;
mod menus;
mod open_graph;
mod settings_view;
mod update_status;

use std::collections::HashSet;

use deep_link::{CliLink, DeepLink};
use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, Global, Window, WindowBounds,
    WindowHandle, WindowOptions, px, size,
};
use graph_store::{ConnectionRegistry, ServerSource, open_session, open_session_behind};
use studio_actions::offered;
use studio_actions::update::{CheckForUpdates, DownloadUpdate, RestartToUpdate, ViewReleaseNotes};
use studio_actions::workspace::{
    About, NewWindow, OpenGraph, OpenLink, OpenSettings, Quit, RevealLog,
};
use studio_update::{UpdateState, Updater};
use studio_workspace::{SavedGraph, SavedWindow, ToastKind, Workspace};

pub use open_graph::OpenGraphModal;

/// Sets up settings, themes, the editor, keymaps, the server registry and every feature.
pub fn init(cx: &mut App) {
    studio_theme::init_settings_store(cx);
    studio_settings::init(cx);
    studio_theme::init_themes(cx);
    editor::init(cx);
    graph_store::init(cx);
    // After `graph_store`, whose HTTP client it sends through (spec 019).
    studio_workspace::models::Models::init(cx);
    // After `graph_store`, whose HTTP client it sends through (spec 017).
    studio_update::init(cx);
    keymaps::load(cx);

    command_palette::init(cx);
    type_table::init(cx);
    relationship_tree::init(cx);
    graph_canvas::init(cx);
    query_editor::init(cx);
    review::init(cx);
    history::init(cx);
    agent_panel::init(cx);

    #[cfg(debug_assertions)]
    studio_workspace::register_item_kind(cx, "design-system", |_, workspace, window, cx| {
        open_design_system(workspace, window, cx);
    });
    studio_workspace::register_item_kind(cx, "settings", |_, workspace, window, cx| {
        open_settings(workspace, window, cx);
    });
    // A write still running keeps Studio open: quitting would lose its outcome (spec 002), and
    // unsaved tabs are asked about first. Deferred, as `OpenLink` is below: the window the
    // action came from is held for dispatch, and could not be asked.
    cx.on_action(|_: &Quit, cx| cx.defer(|cx| studio_workspace::quit(false, cx)));
    // The file when it exists; a run from a terminal logs to its output, so then the folder.
    // A folder that cannot be made is said, with its path and why: the menu item would
    // otherwise do nothing at all. Looked for and made off the main thread; the window the
    // action came from is no longer held for dispatch when the answer comes.
    cx.on_action(|_: &RevealLog, cx| {
        let file = logging::log_file();
        cx.spawn(async move |cx| {
            let place = cx
                .background_spawn(async move { log_to_reveal(&file) })
                .await;
            cx.update(|cx| match place {
                Ok(LogPlace::File(file)) => cx.reveal_path(&file),
                Ok(LogPlace::Folder(dir)) => cx.open_with_system(&dir),
                Err(message) => tell(cx, ToastKind::Error, message),
            });
        })
        .detach();
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
    cx.on_action(|_: &About, cx| about::open_about(cx));
    // App-wide, so the app menu reaches them with no Studio window in front, and each offered
    // only where it applies (spec 017, "What the person sees"; constitution VIII).
    cx.on_action(|_: &CheckForUpdates, cx| {
        if let Some(updater) = Updater::global(cx) {
            updater.update(cx, |updater, cx| updater.check(cx));
        }
    });
    offered::when::<CheckForUpdates>(
        |cx| {
            update_is(cx, |state| {
                matches!(
                    state,
                    UpdateState::Idle
                        | UpdateState::UpToDate
                        | UpdateState::Failed { .. }
                        | UpdateState::Off
                )
            })
        },
        cx,
    );
    // Deferred, as Quit is: the window the action came from is held while it dispatches, and
    // restarting may ask in it.
    cx.on_action(|_: &RestartToUpdate, cx| {
        if update_is(cx, |state| matches!(state, UpdateState::Ready { .. })) {
            cx.defer(|cx| studio_workspace::restart(false, cx));
        }
    });
    offered::when::<RestartToUpdate>(
        |cx| update_is(cx, |state| matches!(state, UpdateState::Ready { .. })),
        cx,
    );
    cx.on_action(|_: &DownloadUpdate, cx| {
        if let Some(updater) = Updater::global(cx) {
            let url = updater.read(cx).download_url();
            cx.open_url(&url);
        }
    });
    offered::when::<DownloadUpdate>(
        |cx| update_is(cx, |state| matches!(state, UpdateState::Available { .. })),
        cx,
    );
    cx.on_action(|_: &ViewReleaseNotes, cx| {
        if let Some(updater) = Updater::global(cx) {
            let url = updater.read(cx).release_notes_url();
            cx.open_url(&url);
        }
    });
    menus::set(cx);
    // The app menu offers Restart to Update once an update is ready.
    if let Some(updater) = Updater::global(cx) {
        let mut ready = false;
        cx.observe(&updater, move |updater, cx| {
            let now = matches!(updater.read(cx).state(), UpdateState::Ready { .. });
            if now != ready {
                ready = now;
                menus::set(cx);
            }
        })
        .detach();
    }
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let handle = cx.entity().downgrade();
        let session = workspace.session().cloned();
        let activity = cx.new(|cx| activity::ActivityLog::new(handle, session, window, cx));
        workspace.set_activity_view(activity.into(), cx);
        // The gallery is for working on Studio: a release build offers no such action, and a tab
        // saved by a debug build does not open in one.
        #[cfg(debug_assertions)]
        workspace.register_action(
            |workspace, _: &studio_actions::workspace::OpenDesignSystem, window, cx| {
                open_design_system(workspace, window, cx);
            },
        );
        workspace.register_action(|workspace, _: &OpenSettings, window, cx| {
            open_settings(workspace, window, cx);
        });
        if let Some(updater) = Updater::global(cx) {
            let status = cx.new(|cx| update_status::UpdateStatus::new(updater.clone(), cx));
            workspace.set_status_item(
                status.into(),
                move |cx| update_status::says_something(updater.read(cx)),
                cx,
            );
        }
        // GPUI's frame-time overlay exists only with its `profiler` feature, which the `perf`
        // feature turns on: frame times, then their distribution, then hidden again. Another
        // build offers no such action, in the palette or on its key.
        #[cfg(feature = "perf")]
        workspace.register_action(
            |_, _: &studio_actions::workspace::ToggleFrameOverlay, window, _| {
                window.cycle_debug_frame_overlay_mode();
            },
        );
        workspace.register_action(|workspace, _: &OpenGraph, window, cx| {
            let handle = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, |window, cx| {
                OpenGraphModal::new(handle, window, cx)
            });
        });
    })
    .detach();
}

/// Whether the updater's state is one `test` accepts.
fn update_is(cx: &App, test: impl Fn(&UpdateState) -> bool) -> bool {
    Updater::global(cx).is_some_and(|updater| test(updater.read(cx).state()))
}

/// Opens the design system's gallery tab, or shows the one open.
#[cfg(debug_assertions)]
fn open_design_system(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    workspace.open_item(
        "design-system",
        window,
        cx,
        design_gallery::DesignGallery::new,
    );
}

/// Opens the settings tab, offering the workspace's graph as a scope.
fn open_settings(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let session = workspace.session().cloned();
    let graph = session
        .as_ref()
        .map(|session| settings_view::graph_scope(session, cx));
    let made = graph.clone();
    let view = workspace.open_item("settings", window, cx, move |window, cx| {
        settings_view::SettingsView::new(made, window, cx)
    });
    // A tab open from before offers the graph shown now, and its types for "Node names"; and
    // it follows the window's graph from now on.
    if let Some(view) = view {
        let workspace = cx.entity();
        view.update(cx, |view, cx| {
            view.set_graph(graph, window, cx);
            view.set_session(session.map(|session| session.downgrade()), cx);
            view.follow(&workspace, window, cx);
        });
    }
}

/// A window's options, at `bounds` or else where the last moved window was.
pub fn window_options(bounds: Option<gpui::Bounds<gpui::Pixels>>, cx: &App) -> WindowOptions {
    let bounds = bounds
        .or_else(|| studio_workspace::saved_window_bounds(cx))
        .unwrap_or_else(|| {
            gpui::Bounds::centered(
                None,
                size(
                    px(studio_ui::ds::size::WINDOW_WIDTH),
                    px(studio_ui::ds::size::WINDOW_HEIGHT),
                ),
                cx,
            )
        });
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Omnigraph Studio".into()),
            appears_transparent: true,
            traffic_light_position: Some(studio_workspace::traffic_light_position()),
        }),
        app_id: Some("com.modernrelay.omnigraph-studio".into()),
        // As Zed's windows: the header strips sit where a title bar would be and move the
        // window themselves (`Window::start_window_move`), so AppKit must not drag it from
        // that band, which would move the window from a tab being dragged.
        app_owns_titlebar_drag: true,
        ..Default::default()
    }
}

/// Opens Studio's windows: one for `OMNIGRAPH_STUDIO_GRAPH` when it is set, else every window
/// the last run had open with a graph on a target the CLI still lists at its address, each where
/// it was, else one window asking for a graph. The CLI's targets are listed off the main thread
/// ([`ConnectionRegistry::when_listed`]), so the saved windows open at once, where they were,
/// and take their graphs once the targets are listed.
pub fn open_windows(cx: &mut App) {
    if environment_graph(cx).is_some() {
        // A run for the environment's graph opens that alone, and keeps the windows the last
        // ordinary run saved for the next one.
        studio_workspace::keep_saved_windows(cx);
        cx.open_window(window_options(None, cx), build_workspace)
            .ok();
        return;
    }
    let saved = studio_workspace::load_windows();
    if saved.is_empty() {
        cx.open_window(window_options(None, cx), build_workspace)
            .ok();
        return;
    }
    let opened: Vec<_> = saved
        .into_iter()
        .filter_map(|saved| {
            let bounds = saved.bounds(cx);
            let handle = cx
                .open_window(window_options(bounds, cx), new_workspace)
                .ok()?;
            Some((handle, saved))
        })
        .collect();
    let registry = ConnectionRegistry::global(cx);
    if registry.read(cx).listed() {
        fill_reopened(opened, cx);
        return;
    }
    let listed = registry.update(cx, |registry, _| registry.when_listed());
    cx.spawn(async move |cx| {
        listed.await;
        cx.update(|cx| fill_reopened(opened, cx));
    })
    .detach();
}

/// Gives each window a start reopened the graphs it had on targets the CLI lists, each at the
/// address it had, a graph listed by two windows in the first alone. A window left with none
/// closes, unless no window has a graph: the first then asks for one ([`ask_for_graph`]), once
/// the links that came meanwhile are opened and only if they leave it without a graph.
fn fill_reopened(opened: Vec<(WindowHandle<Workspace>, SavedWindow)>, cx: &mut App) {
    let registry = ConnectionRegistry::global(cx);
    let known: HashSet<(String, String)> = registry
        .read(cx)
        .connections
        .iter()
        .map(|connection| {
            let server = &connection.read(cx).server;
            (server.name.clone(), server.url.clone())
        })
        .collect();
    let mut taken = HashSet::new();
    let mut empty = Vec::new();
    let mut filled = false;
    for (handle, saved) in opened {
        let listed = |target: &str, url: &str| known.contains(&(target.to_owned(), url.to_owned()));
        let Some(saved) = reopenable(saved, listed, &mut taken) else {
            empty.push(handle);
            continue;
        };
        filled = true;
        cx.update_window(handle.into(), |root, window, cx| {
            if let Ok(workspace) = root.downcast::<Workspace>() {
                reopen_window(&workspace, &saved, window, cx);
            }
        })
        .ok();
    }
    let mut empty = empty.into_iter();
    if !filled && let Some(first) = empty.next() {
        cx.update_window(first.into(), |root, window, cx| {
            if let Ok(workspace) = root.downcast::<Workspace>() {
                ask_for_graph(&workspace, window, cx);
            }
        })
        .ok();
    }
    for handle in empty {
        cx.update_window(handle.into(), |_, window, _| window.remove_window())
            .ok();
    }
}

/// Builds the root view of a window and opens its first graph: the environment's, else none,
/// asking for one. The windows the last run left open are reopened by [`open_windows`].
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

/// `window` with only its graphs on CLI targets `known` lists at their addresses and not
/// `taken` by a window before it, or `None` when it is left with none. A graph is open in one
/// window at a time: its tabs and drafts are saved by graph, so two windows holding it would
/// each write over the other's. One listed by two windows from an earlier session reopens in the
/// first.
fn reopenable(
    mut window: SavedWindow,
    known: impl Fn(&str, &str) -> bool,
    taken: &mut HashSet<(String, String)>,
) -> Option<SavedWindow> {
    let shown = window.session.graphs.get(window.session.shown).cloned();
    window.session.graphs.retain(|graph| {
        known(&graph.target, &graph.url) && taken.insert((graph.url.clone(), graph.graph.clone()))
    });
    window.session.shown = shown
        .and_then(|shown| window.session.graphs.iter().position(|g| *g == shown))
        .unwrap_or(0);
    (!window.session.graphs.is_empty()).then_some(window)
}

/// Reopens a saved window's graphs, showing the one it showed. The shown graph's server is
/// connected first, and its token is the one the CLI gives first (the graph waits for it);
/// the parked graphs' servers after, each asked nothing by its graph until it is shown.
fn reopen_window(
    workspace: &Entity<Workspace>,
    saved: &SavedWindow,
    window: &mut Window,
    cx: &mut App,
) {
    let registry = ConnectionRegistry::global(cx);
    let connections = registry.read(cx).connections.clone();
    let connection_of = |graph: &SavedGraph, cx: &App| {
        connections
            .iter()
            .find(|c| {
                let server = &c.read(cx).server;
                server.name == graph.target && server.url == graph.url
            })
            .cloned()
    };
    if let Some(shown) = saved.session.graphs.get(saved.session.shown)
        && let Some(connection) = connection_of(shown, cx)
    {
        connection.update(cx, |connection, cx| connection.connect_if_idle(cx));
    }
    // Every graph left is on a listed target ([`reopenable`]), the one shown among them.
    for (index, graph) in saved.session.graphs.iter().enumerate() {
        let Some(connection) = connection_of(graph, cx) else {
            continue;
        };
        connection.update(cx, |connection, cx| connection.connect_if_idle(cx));
        let (graph_id, branch) = (graph.graph.clone(), graph.branch.clone());
        if index == saved.session.shown {
            let session = open_session(connection, graph_id, branch, cx);
            workspace.update(cx, |workspace, cx| {
                workspace.set_session(session, window, cx)
            });
        } else {
            // Parked: it sends nothing, its first reads included, until it is first shown.
            let session = open_session_behind(connection, graph_id, branch, cx);
            workspace.update(cx, |workspace, cx| {
                workspace.add_session_behind(session, cx)
            });
        }
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

/// Links that came while the CLI's targets were not yet listed, still to open, and the windows
/// that would have asked for a graph meanwhile: a link opens one, so they wait for the links,
/// and ask once the last is opened, if they still have none ([`link_opened`]).
#[derive(Default)]
struct LinksWaiting {
    count: usize,
    asking: Vec<gpui::AnyWindowHandle>,
}

impl Global for LinksWaiting {}

/// Shows "Open a graph" in `workspace`'s window, once the CLI's targets are listed: at a start
/// they are listed off the main thread. It is not shown over a graph or a dialog the window has
/// by then. While links that came meanwhile wait to open, it waits for them: one opens a graph,
/// and the dialog would connect every server only for the link to hide it. One that does not
/// open (a link from `omnigraph studio`, a server Studio does not know) leaves the window
/// without a graph, which then asks.
fn ask_for_graph(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let registry = ConnectionRegistry::global(cx);
    if registry.read(cx).listed() {
        ask_unless_busy(workspace, window, cx);
        return;
    }
    let listed = registry.update(cx, |registry, _| registry.when_listed());
    let workspace = workspace.downgrade();
    window
        .spawn(cx, async move |cx| {
            listed.await;
            cx.update(|window, cx| {
                if let Some(workspace) = workspace.upgrade() {
                    ask_unless_busy(&workspace, window, cx);
                }
            })
            .ok();
        })
        .detach();
}

/// [`ask_for_graph`] once the CLI's targets are listed: the dialog, unless the window has a graph
/// or a dialog, or links still wait to open.
fn ask_unless_busy(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let busy = {
        let workspace = workspace.read(cx);
        workspace.session().is_some() || workspace.has_modal()
    };
    if busy {
        return;
    }
    let waiting = cx.default_global::<LinksWaiting>();
    if waiting.count > 0 {
        waiting.asking.push(window.window_handle());
        return;
    }
    show_open_dialog(workspace, window, cx);
}

/// A link that waited for the CLI's targets has been opened, or refused: once it is the last, each
/// window that waited to ask for a graph asks now if it still has none, so a refused link leaves
/// the dialog rather than an empty window.
fn link_opened(cx: &mut App) {
    let waiting = cx.default_global::<LinksWaiting>();
    waiting.count -= 1;
    if waiting.count > 0 {
        return;
    }
    for handle in std::mem::take(&mut waiting.asking) {
        cx.update_window(handle, |root, window, cx| {
            if let Ok(workspace) = root.downcast::<Workspace>() {
                ask_unless_busy(&workspace, window, cx);
            }
        })
        .ok();
    }
}

/// Connects every server the CLI lists and shows "Open a graph" over them: only when the dialog
/// lists them, so a start that a link or a reopened window opens a graph for connects only the
/// servers it shows.
fn show_open_dialog(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let registry = ConnectionRegistry::global(cx);
    for connection in registry.read(cx).connections.clone() {
        connection.update(cx, |connection, cx| connection.connect_if_idle(cx));
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
    } else {
        ask_for_graph(workspace, window, cx);
    }
}

/// Opens an `omnigraph-studio://` link (`deep_link`): in the window that already has its graph
/// open, else in the active window, else in a new one. What the link names is shown, never run;
/// a server Studio does not know is refused rather than added. A link that comes at a start,
/// before the CLI's targets are listed, opens once they are: they say which servers Studio
/// knows. A window that waited for it to ask for a graph asks then, if the link left it with
/// none ([`link_opened`]).
pub fn open_link(link: &str, cx: &mut App) {
    let registry = ConnectionRegistry::global(cx);
    if !registry.read(cx).listed() {
        cx.default_global::<LinksWaiting>().count += 1;
        let listed = registry.update(cx, |registry, _| registry.when_listed());
        let link = link.to_owned();
        cx.spawn(async move |cx| {
            listed.await;
            cx.update(|cx| {
                open_link(&link, cx);
                link_opened(cx);
            });
        })
        .detach();
        return;
    }
    match deep_link::parse(link) {
        Ok(DeepLink::Open(open)) => open_cli_link(open, cx),
        Ok(link) => open_parsed(link, cx),
        Err(message) => tell(cx, ToastKind::Error, message),
    }
}

/// Opens a link from `omnigraph studio`, which names a CLI target: the target's server, once
/// the digests the link carries match what this Mac's CLI reports for the target (its
/// `binding`, of what the name resolves to, and `home`, of the operator file it was read
/// from), so a link made elsewhere, or before the target changed, opens nothing. The check
/// asks the CLI, off the main thread; a start's window waits for it as for any link.
fn open_cli_link(open: CliLink, cx: &mut App) {
    let registry = ConnectionRegistry::global(cx);
    let known = registry.read(cx).connections.iter().any(|connection| {
        let server = &connection.read(cx).server;
        server.source == ServerSource::Cli && server.name == open.target
    });
    if !known {
        return tell(
            cx,
            ToastKind::Error,
            format!(
                "Studio does not know a CLI target named {}. Add it with `omnigraph target add` \
                 and refresh under “Open a graph”; a link never adds a server.",
                open.target
            ),
        );
    }
    cx.default_global::<LinksWaiting>().count += 1;
    let digests = registry.read(cx).target_digests(&open.target);
    cx.spawn(async move |cx| {
        let digests = digests.await;
        cx.update(|cx| {
            match digests.and_then(|digests| link_target(&open, digests, cx)) {
                Ok(target) => open_parsed(DeepLink::Graph(target), cx),
                Err(message) => tell(cx, ToastKind::Error, message),
            }
            link_opened(cx);
        });
    })
    .detach();
}

/// Where an `open` link points once its digests match `digests`, the CLI's for its target: the
/// target's server, and the graph the link names, else the target's default graph, else the
/// first graph known on the server.
fn link_target(
    open: &CliLink,
    digests: graph_store::cli::TargetDigests,
    cx: &App,
) -> Result<deep_link::Target, String> {
    deep_link::verify(open, &digests.binding, &digests.home)?;
    let connection = ConnectionRegistry::global(cx)
        .read(cx)
        .connections
        .iter()
        .find(|connection| {
            let server = &connection.read(cx).server;
            server.source == ServerSource::Cli && server.name == open.target
        })
        .ok_or_else(|| format!("Studio no longer knows the CLI target {}.", open.target))?;
    let server = &connection.read(cx).server;
    let graph = open
        .graph
        .clone()
        .or(digests.default_graph)
        .or_else(|| server.known_graphs.first().cloned())
        .ok_or_else(|| {
            format!(
                "Target {} names no graph; open one on {} from “Open a graph”.",
                open.target, server.name
            )
        })?;
    Ok(deep_link::Target {
        server: server.url.clone(),
        graph,
        branch: None,
    })
}

/// Opens a parsed link ([`open_link`]).
fn open_parsed(link: DeepLink, cx: &mut App) {
    let Some(target) = link.target().cloned() else {
        return tell(
            cx,
            ToastKind::Info,
            "This link has a shape no `omnigraph studio` sends. Open the graph from “Open a \
             graph” instead."
                .into(),
        );
    };
    let connection = match link_connection(&target, cx) {
        Ok(connection) => connection,
        Err(message) => return tell(cx, ToastKind::Error, message),
    };
    // A link that names no branch opens a graph on main, and leaves one the window has open on
    // its own branch, as "Open a graph" does.
    let named_branch = target.branch.clone();
    // The window that has the graph open, else the active one.
    let window = window_holding(&connection, &target.graph, cx)
        .map(|(handle, _)| handle.into())
        .or_else(|| link_window(cx));
    let open_in = move |workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App| {
        // The link is what the person asked for now: a dialog in the window ("Open a graph",
        // shown at a start with no graph) gives way to it. One that cannot close yet (a write
        // running) keeps the focus, with the link opened under it.
        let dialog_stays = !workspace.update(cx, |workspace, cx| workspace.hide_modal(window, cx));
        // The branch the graph is open on, when the link names another: every tab of it reads,
        // and writes to, the link's from now on, which the window says.
        let moved_from = named_branch.as_ref().and_then(|named| {
            let url = connection.read(cx).server.url.clone();
            let session = workspace
                .read(cx)
                .open_session_for(&url, &target.graph, cx)?;
            let now = session.read(cx).branch.clone();
            (now != *named).then_some(now)
        });
        // The graph, if the window has it open, moved to the branch the link names; else opened
        // here.
        show_graph_in(
            workspace,
            connection.clone(),
            target.graph.clone(),
            named_branch.clone(),
            window,
            cx,
        );
        if !dialog_stays {
            let focus = workspace.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        if let (Some(from), Some(to)) = (moved_from, &named_branch) {
            let message = format!(
                "{} moved from {from} to {to}, the branch the link names",
                target.graph
            );
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(ToastKind::Info, message, cx)
            });
        }
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
            DeepLink::Graph(_) | DeepLink::Open(_) | DeepLink::Connect => {}
        }
        window.activate_window();
    };
    match window {
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

/// The known server a link's `target` opens with, matched by its address
/// (`graph_client::server_root`): the one server there, or, of two CLI targets at one address,
/// each with its own credential, the one a window has the graph open on. The link names only
/// the address, so it picks no credential the person has not: otherwise why not, naming them.
fn link_connection(
    target: &deep_link::Target,
    cx: &App,
) -> Result<Entity<graph_store::Connection>, String> {
    let wanted = graph_client::server_root(&target.server).unwrap_or(target.server.clone());
    let at: Vec<Entity<graph_store::Connection>> = ConnectionRegistry::global(cx)
        .read(cx)
        .connections
        .iter()
        .filter(|connection| {
            let url = &connection.read(cx).server.url;
            graph_client::server_root(url).as_deref().unwrap_or(url) == wanted
        })
        .cloned()
        .collect();
    match at.as_slice() {
        [] => Err(format!(
            "Studio does not know the server {}. Add it under “Open a graph” first; a link never \
             adds a server.",
            target.server
        )),
        [one] => Ok(one.clone()),
        several => several
            .iter()
            .find(|connection| window_holding(connection, &target.graph, cx).is_some())
            .cloned()
            .ok_or_else(|| {
                let names: Vec<String> = several
                    .iter()
                    .map(|connection| connection.read(cx).server.name.clone())
                    .collect();
                format!(
                    "Studio knows {} servers at {}: {}. A link names only the address, so open \
                     {} from “Open a graph”, which says which.",
                    several.len(),
                    target.server,
                    names.join(", "),
                    target.graph
                )
            }),
    }
}

/// The workspace window that has `graph` on `connection`'s server open, with its session there.
/// A graph is open in one window at a time: its tabs and drafts are saved by graph.
pub(crate) fn window_holding(
    connection: &Entity<graph_store::Connection>,
    graph: &str,
    cx: &App,
) -> Option<(
    gpui::WindowHandle<Workspace>,
    Entity<graph_store::GraphSession>,
)> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<Workspace>())
        .find_map(|handle| {
            let url = &connection.read(cx).server.url;
            let session = handle.read(cx).ok()?.open_session_for(url, graph, cx)?;
            Some((handle, session))
        })
}

/// Shows `graph` in `workspace`'s window: the session the window has open, moved to `branch`
/// when one is named, else a new one on `branch` (main when none is). A link and the "Open a
/// graph" dialog show a graph this way.
pub(crate) fn show_graph_in(
    workspace: &Entity<Workspace>,
    connection: Entity<graph_store::Connection>,
    graph: String,
    branch: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let url = connection.read(cx).server.url.clone();
    match workspace.read(cx).open_session_for(&url, &graph, cx) {
        Some(session) => {
            if let Some(branch) = branch {
                session.update(cx, |session, cx| session.set_branch(branch, cx));
            }
            workspace.update(cx, |workspace, cx| {
                workspace.show_graph(session.entity_id(), window, cx)
            });
        }
        None => {
            connection.update(cx, |connection, cx| connection.connect_if_idle(cx));
            let branch = branch.unwrap_or_else(|| "main".into());
            let session = open_session(connection, graph, branch, cx);
            workspace.update(cx, |workspace, cx| {
                workspace.set_session(session, window, cx)
            });
        }
    }
}

/// The workspace window a link opens in: the active one, else any.
fn link_window(cx: &App) -> Option<gpui::AnyWindowHandle> {
    let is_workspace = |window: &gpui::AnyWindowHandle| window.downcast::<Workspace>().is_some();
    cx.active_window()
        .filter(is_workspace)
        .or_else(|| cx.windows().into_iter().find(is_workspace))
}

/// Where Reveal Log File goes.
#[derive(Debug, PartialEq, Eq)]
enum LogPlace {
    File(std::path::PathBuf),
    /// A run from a terminal logs to its output: the folder the file would be in, made.
    Folder(std::path::PathBuf),
}

/// The log `file` when it exists, else its folder, made first; or why the folder could not be
/// made, naming it.
fn log_to_reveal(file: &std::path::Path) -> Result<LogPlace, String> {
    if file.exists() {
        return Ok(LogPlace::File(file.to_owned()));
    }
    let dir = file.parent().unwrap_or(file);
    match std::fs::create_dir_all(dir) {
        Ok(()) => Ok(LogPlace::Folder(dir.to_owned())),
        Err(error) => Err(format!(
            "The log folder could not be made at {}: {error}",
            dir.display()
        )),
    }
}

/// Says what did not happen (a link that did not open, a file not written), in a window if
/// there is one. Deferred by a caller in a window's dispatch, which holds that window.
pub(crate) fn tell(cx: &mut App, kind: ToastKind, message: String) {
    match link_window(cx) {
        Some(handle) => {
            cx.update_window(handle, |root, _, cx| {
                if let Ok(workspace) = root.downcast::<Workspace>() {
                    workspace.update(cx, |workspace, cx| workspace.show_toast(kind, message, cx));
                }
            })
            .ok();
        }
        // A window opened to say it asks for a graph, as an empty one does: the message may
        // point at "Open a graph".
        None => {
            cx.open_window(window_options(None, cx), move |window, cx| {
                let workspace = new_workspace(window, cx);
                workspace.update(cx, |workspace, cx| workspace.show_toast(kind, message, cx));
                ask_for_graph(&workspace, window, cx);
                workspace
            })
            .ok();
        }
    }
}

#[cfg(test)]
mod reopen_tests {
    use super::*;

    /// A graph two saved windows listed reopens in the first alone; the second shows what is
    /// left, and a window left with nothing is not reopened. Graphs are taken by their CLI
    /// target at its address: one on a target the CLI no longer lists is not reopened, nor one
    /// on a target whose name the CLI now gives another server, whose saved state it is not.
    #[test]
    fn a_graph_two_windows_listed_reopens_in_the_first() {
        let saved = |graphs: &[&str], url: &str, shown: usize| -> SavedWindow {
            serde_json::from_value(serde_json::json!({
                "graphs": graphs
                    .iter()
                    .map(|graph| serde_json::json!({
                        "target": "studio",
                        "url": url,
                        "graph": graph,
                        "branch": "main",
                    }))
                    .collect::<Vec<_>>(),
                "shown": shown,
            }))
            .unwrap()
        };
        let window = |graphs: &[&str], shown: usize| saved(graphs, "https://studio.test", shown);
        let elsewhere: SavedWindow = serde_json::from_value(serde_json::json!({
            "graphs": [{"target": "gone", "url": "https://gone.test", "graph": "crm", "branch": "main"}],
            "shown": 0,
        }))
        .unwrap();
        let listed = |target: &str, url: &str| target == "studio" && url == "https://studio.test";
        let mut taken = HashSet::new();
        let windows: Vec<SavedWindow> = [
            window(&["crm", "notes"], 0),
            window(&["notes", "ops"], 1),
            window(&["crm"], 0),
            elsewhere,
            saved(&["people"], "https://studio-before.test", 0),
        ]
        .into_iter()
        .filter_map(|window| reopenable(window, listed, &mut taken))
        .collect();
        let graphs: Vec<Vec<&str>> = windows
            .iter()
            .map(|window| {
                window
                    .session
                    .graphs
                    .iter()
                    .map(|g| g.graph.as_str())
                    .collect()
            })
            .collect();
        assert_eq!(graphs, vec![vec!["crm", "notes"], vec!["ops"]]);
        assert_eq!(windows[1].session.shown, 0, "ops, the graph it showed");
    }
}

#[cfg(test)]
mod start_tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use gpui::TestAppContext;
    use graph_store::{Connection, ConnectionState};

    use super::*;

    /// The host of every request sent, in order.
    type Asked = Arc<Mutex<Vec<String>>>;

    /// Studio with servers `a`, `b` and `c` the CLI lists, none connected yet; and the host of
    /// every request sent.
    fn studio_with_servers(
        cx: &mut TestAppContext,
    ) -> (Entity<ConnectionRegistry>, Vec<Entity<Connection>>, Asked) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
            editor::init(cx);
        });
        let (registry, asked) = cx.update(ConnectionRegistry::init_for_tests);
        let servers = registry.update(cx, |registry, cx| {
            ["a", "b", "c"]
                .map(|name| {
                    registry.add_server_for_tests(name, &format!("http://{name}.test"), None, cx)
                })
                .to_vec()
        });
        (registry, servers, asked)
    }

    /// Whether each server has been connected.
    fn connected(servers: &[Entity<Connection>], cx: &TestAppContext) -> Vec<bool> {
        cx.read(|cx| {
            servers
                .iter()
                .map(|server| server.read(cx).state != ConnectionState::Idle)
                .collect()
        })
    }

    /// A link that opens Studio connects the server it names alone, once the CLI's targets
    /// are listed: the start's window does not ask for a graph, so the other servers are not
    /// connected. Before, the start connected every server the CLI lists, a link or not.
    #[gpui::test]
    fn a_link_at_a_start_connects_its_server_alone(cx: &mut TestAppContext) {
        let (registry, servers, asked) = studio_with_servers(cx);
        registry.update(cx, |registry, cx| registry.set_listed_for_tests(false, cx));
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        cx.update(|window, cx| ask_for_graph(&workspace, window, cx));
        cx.update(|_, cx| open_link("omnigraph-studio://graph?server=http://c.test&graph=g", cx));
        cx.run_until_parked();
        assert_eq!(
            connected(&servers, cx),
            [false, false, false],
            "waiting for the list"
        );
        assert!(asked.lock().unwrap().is_empty());

        registry.update(cx, |registry, cx| registry.set_listed_for_tests(true, cx));
        cx.run_until_parked();
        assert_eq!(connected(&servers, cx), [false, false, true]);
        let hosts = std::mem::take(&mut *asked.lock().unwrap());
        assert!(hosts.iter().all(|host| host == "c.test"), "{hosts:?}");
        workspace.read_with(cx, |workspace, cx| {
            assert!(!workspace.has_modal(), "no dialog over the link's graph");
            let session = workspace.session().unwrap().read(cx);
            assert_eq!(session.graph_id, "g");
            assert_eq!(session.connection, servers[2]);
        });
    }

    /// Studio with CLI target `law` at `http://law.test`, default graph `law`, whose CLI
    /// reports digests of all b and all a for it.
    fn studio_with_a_cli_target(cx: &mut TestAppContext) -> (Entity<Connection>, Asked) {
        let (registry, _servers, asked) = studio_with_servers(cx);
        let law = registry.update(cx, |registry, cx| {
            registry.set_target_digests_for_tests(HashMap::from([(
                "law".to_owned(),
                Ok(graph_store::cli::TargetDigests {
                    binding: format!("sha256:{}", "b".repeat(64)),
                    home: format!("sha256:{}", "a".repeat(64)),
                    default_graph: Some("law".into()),
                }),
            )]));
            registry.add_cli_target_for_tests("law", "http://law.test", "law", cx)
        });
        (law, asked)
    }

    fn cli_link(target: &str, binding: char, home: char, graph: Option<&str>) -> String {
        let graph = graph
            .map(|graph| format!("&graph={graph}"))
            .unwrap_or_default();
        format!(
            "omnigraph-studio://open?target={target}&binding=sha256:{}&home=sha256:{}{graph}",
            binding.to_string().repeat(64),
            home.to_string().repeat(64)
        )
    }

    /// A link from `omnigraph studio` names a CLI target; once the digests it carries are what
    /// this Mac's CLI reports for the target, its server opens, on the graph the link names,
    /// else the target's default graph. The check runs after the link is parsed, so a start's
    /// window waits for it as for any link.
    #[gpui::test]
    fn a_link_from_the_cli_opens_its_target_once_the_digests_match(cx: &mut TestAppContext) {
        let (law, asked) = studio_with_a_cli_target(cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        cx.cx
            .update(|cx| open_link(&cli_link("law", 'b', 'a', None), cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.toast_messages().is_empty(), "nothing refused");
            let session = workspace.session().unwrap().read(cx);
            assert_eq!(session.graph_id, "law", "the target's default graph");
            assert_eq!(session.connection, law);
        });
        let hosts = std::mem::take(&mut *asked.lock().unwrap());
        assert!(hosts.iter().all(|host| host == "law.test"), "{hosts:?}");

        cx.cx
            .update(|cx| open_link(&cli_link("law", 'b', 'a', Some("cases")), cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let session = workspace.session().unwrap().read(cx);
            assert_eq!(session.graph_id, "cases", "the graph the link names");
        });
    }

    /// A link whose digests are not the CLI's here (made on another Mac, or before the target
    /// changed), or whose target the CLI does not list, opens nothing and says which differs.
    #[gpui::test]
    fn a_link_from_the_cli_is_refused_when_its_digests_differ(cx: &mut TestAppContext) {
        let (_law, _asked) = studio_with_a_cli_target(cx);
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        for (link, said) in [
            (cli_link("law", 'c', 'a', None), "binding digest differs"),
            (cli_link("law", 'b', 'c', None), "home digest differs"),
            (
                cli_link("tax", 'b', 'a', None),
                "does not know a CLI target named tax",
            ),
        ] {
            cx.cx.update(|cx| open_link(&link, cx));
            cx.run_until_parked();
            workspace.read_with(cx, |workspace, _| {
                assert!(workspace.session().is_none(), "{link}");
                let toasts = workspace.toast_messages();
                assert!(
                    toasts.last().is_some_and(|toast| toast.contains(said)),
                    "{link}: {toasts:?}"
                );
            });
        }
    }

    /// A reopened window connects its own graphs' servers and no other; the Open dialog,
    /// listing every server, connects the rest.
    #[gpui::test]
    fn a_reopened_window_connects_its_own_servers(cx: &mut TestAppContext) {
        let (_registry, servers, asked) = studio_with_servers(cx);
        let saved: SavedWindow = serde_json::from_value(serde_json::json!({
            "graphs": [
                {"target": "a", "url": "http://a.test", "graph": "notes", "branch": "main"},
                {"target": "b", "url": "http://b.test", "graph": "people", "branch": "main"},
            ],
            "shown": 1,
        }))
        .unwrap();
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        cx.update(|window, cx| reopen_window(&workspace, &saved, window, cx));
        cx.run_until_parked();
        assert_eq!(connected(&servers, cx), [true, true, false]);
        let hosts = std::mem::take(&mut *asked.lock().unwrap());
        assert!(!hosts.iter().any(|host| host == "c.test"), "{hosts:?}");

        cx.update(|window, cx| show_open_dialog(&workspace, window, cx));
        cx.run_until_parked();
        assert_eq!(connected(&servers, cx), [true, true, true]);
        workspace.read_with(cx, |workspace, _| assert!(workspace.has_modal()));
    }

    /// A link that opens Studio and cannot be opened leaves its window asking for a graph: the
    /// window waited for the link rather than ask, and asks once it is refused, whatever was
    /// wrong with it (one from `omnigraph studio`, a server Studio does not know, which is how a
    /// target that needs a token reads on Windows). The refusal is said too. Before, the window
    /// stayed empty with no dialog, and the toast pointed at "Open a graph", which was not there.
    #[gpui::test]
    fn a_link_refused_at_a_start_leaves_the_open_dialog(cx: &mut TestAppContext) {
        refused_at_a_start("omnigraph-studio://connect?code=abc", cx);
    }

    #[gpui::test]
    fn a_link_to_an_unknown_server_at_a_start_leaves_the_open_dialog(cx: &mut TestAppContext) {
        refused_at_a_start(
            "omnigraph-studio://graph?server=http://unknown.test&graph=g",
            cx,
        );
    }

    /// A link refused while no window is open (on macOS Studio runs on with none) opens a window
    /// that says why and asks for a graph. Before, the new window showed the refusal alone, and
    /// it pointed at "Open a graph", which was not there.
    #[gpui::test]
    fn a_link_refused_with_no_window_asks_for_a_graph(cx: &mut TestAppContext) {
        let _studio = studio_with_servers(cx);
        cx.update(|cx| open_link("omnigraph-studio://connect?code=abc", cx));
        cx.run_until_parked();
        let windows = cx.update(|cx| cx.windows());
        assert_eq!(windows.len(), 1);
        let workspace = windows[0].downcast::<Workspace>().unwrap();
        workspace
            .update(cx, |workspace, _, _| {
                assert!(workspace.has_modal(), "the dialog");
                assert_eq!(workspace.toast_messages().len(), 1, "the refusal is said");
            })
            .unwrap();
    }

    /// `link` opens Studio, whose one window would ask for a graph, and is refused.
    fn refused_at_a_start(link: &str, cx: &mut TestAppContext) {
        let (registry, servers, _asked) = studio_with_servers(cx);
        registry.update(cx, |registry, cx| registry.set_listed_for_tests(false, cx));
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        cx.update(|window, cx| ask_for_graph(&workspace, window, cx));
        cx.update(|_, cx| open_link(link, cx));
        registry.update(cx, |registry, cx| registry.set_listed_for_tests(true, cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.session().is_none());
            assert!(workspace.has_modal(), "the dialog");
            assert_eq!(workspace.toast_messages().len(), 1, "the refusal is said");
        });
        assert_eq!(
            connected(&servers, cx),
            [true, true, true],
            "as the dialog does"
        );
    }

    /// A start that reopens no window asks for a graph only after the links that came while
    /// the CLI's targets were listed: one that opens its graph keeps the dialog away, and with
    /// it every server but its own unconnected. Before, the reopened windows' path showed the
    /// dialog at once, connecting every server, and the link then hid it.
    #[gpui::test]
    fn a_link_at_a_reopened_start_keeps_the_dialog_away(cx: &mut TestAppContext) {
        let (registry, servers, asked) = studio_with_servers(cx);
        registry.update(cx, |registry, cx| registry.set_listed_for_tests(false, cx));
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let handle = gpui::VisualContext::window_handle(cx)
            .downcast::<Workspace>()
            .unwrap();
        let gone: SavedWindow = serde_json::from_value(serde_json::json!({
            "graphs": [{"target": "gone", "url": "http://gone.test", "graph": "g", "branch": "main"}],
            "shown": 0,
        }))
        .unwrap();
        cx.update(|_, cx| open_link("omnigraph-studio://graph?server=http://c.test&graph=g", cx));
        // Listed, and the reopened windows filled before the link opens, as at a start: from the
        // app, as a start fills them, since inside the window's own update `fill_reopened`
        // could not update the window, and would ask nothing.
        registry.update(cx, |registry, cx| registry.set_listed_for_tests(true, cx));
        cx.cx.update(|cx| fill_reopened(vec![(handle, gone)], cx));
        assert_eq!(
            connected(&servers, cx),
            [false, false, false],
            "no dialog yet"
        );
        cx.run_until_parked();
        assert_eq!(connected(&servers, cx), [false, false, true]);
        let hosts = std::mem::take(&mut *asked.lock().unwrap());
        assert!(hosts.iter().all(|host| host == "c.test"), "{hosts:?}");
        workspace.read_with(cx, |workspace, cx| {
            assert!(!workspace.has_modal(), "no dialog over the link's graph");
            assert_eq!(workspace.session().unwrap().read(cx).graph_id, "g");
        });
    }

    /// A link names a server by its address alone. Of two CLI targets there, each with its own
    /// credential, it opens with the one a window has the graph open on, and with neither open
    /// it is refused, naming both. Before, it took whichever was listed first.
    #[gpui::test]
    fn a_link_to_two_servers_at_one_address_picks_no_credential(cx: &mut TestAppContext) {
        let (registry, _servers, _asked) = studio_with_servers(cx);
        let (work, admin) = registry.update(cx, |registry, cx| {
            registry.set_listed_for_tests(true, cx);
            (
                registry.add_server_for_tests("work", "http://shared.test", None, cx),
                registry.add_server_for_tests("admin", "http://shared.test", None, cx),
            )
        });
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let link = "omnigraph-studio://graph?server=http://shared.test&graph=g";
        // From the app, as `OpenLink` defers it: a toast cannot be put in a window being updated.
        cx.cx.update(|cx| open_link(link, cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.session().is_none(), "nothing opened");
            let said = workspace.toast_messages();
            assert_eq!(said.len(), 1);
            assert!(said[0].contains("work, admin"), "{said:?}");
        });

        // The graph open on `admin`: the link opens there.
        cx.update(|window, cx| {
            show_graph_in(&workspace, admin.clone(), "g".into(), None, window, cx)
        });
        cx.cx.update(|cx| open_link(link, cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let session = workspace.session().unwrap().read(cx);
            assert_eq!(session.graph_id, "g");
            assert_eq!(session.connection.read(cx).server.name, "admin");
        });
        assert_eq!(
            work.read_with(cx, |work, _| work.state.clone()),
            ConnectionState::Idle,
            "the other credential is not used"
        );
    }

    /// A window opens with every feature's part of the workspace made and drawn. Each feature
    /// makes its part while the workspace is being made (`observe_new`), when reading the
    /// workspace panics: the Conversations panel's read of the shown tab there aborted every
    /// launch before a window showed.
    #[gpui::test]
    fn a_window_opens_with_every_feature(cx: &mut TestAppContext) {
        studio_with_servers(cx);
        cx.update(|cx| {
            command_palette::init(cx);
            type_table::init(cx);
            relationship_tree::init(cx);
            graph_canvas::init(cx);
            query_editor::init(cx);
            review::init(cx);
            history::init(cx);
            agent_panel::init(cx);
        });
        let (_workspace, cx) = cx.add_window_view(Workspace::new);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reveal Log File shows the file, else its folder made first; a folder that cannot be made
    /// is said with its path and why, rather than nothing happening.
    #[test]
    fn a_log_folder_that_cannot_be_made_is_said() {
        let root = std::env::temp_dir().join(format!("studio-reveal-log-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("logs").join("Studio.log");
        assert_eq!(
            log_to_reveal(&file),
            Ok(LogPlace::Folder(root.join("logs"))),
            "the folder, made"
        );
        std::fs::write(&file, "").unwrap();
        assert_eq!(log_to_reveal(&file), Ok(LogPlace::File(file.clone())));
        // A file where the folder would be.
        let blocker = root.join("blocker");
        std::fs::write(&blocker, "").unwrap();
        let blocked = blocker.join("logs").join("Studio.log");
        let said = log_to_reveal(&blocked).unwrap_err();
        let folder = blocker.join("logs");
        assert!(
            said.starts_with(&format!(
                "The log folder could not be made at {}: ",
                folder.display()
            )),
            "{said}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Studio's header strips are where a title bar would be, and the workspace moves the window
    /// from them itself. Were AppKit to own that band, a press on a tab would drag the window.
    #[gpui::test]
    fn windows_leave_the_titlebar_drag_to_the_app(cx: &mut gpui::TestAppContext) {
        let bounds = gpui::Bounds::new(gpui::point(px(0.), px(0.)), size(px(1440.), px(900.)));
        let options = cx.update(|cx| window_options(Some(bounds), cx));
        assert!(options.app_owns_titlebar_drag);
        assert!(options.is_movable, "moved from its header strips");
    }
}
