//! The left dock's "Queries" panel (spec 007): this graph's saved `.gq` files, the most
//! recently changed first, then the stored queries the server serves for the graph, each with
//! the CLI config's aliases for it. A click opens one; ⌘S in a new query saves it here.

use std::path::PathBuf;
use std::time::SystemTime;

use gpui::{Context, Entity, Render, Subscription, WeakEntity, Window};
use graph_store::cli_config::CliAlias;
use graph_store::{ConnectionRegistry, GraphSession, Loadable, SessionEvent};
use studio_actions::graph::{NewQuery, OpenQueryFile, OpenStoredQuery};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::Tooltip;
use ui::prelude::*;

use crate::SavedQueriesChanged;

pub const QUERIES_PANEL: &str = "queries";

pub struct QueriesPanel {
    session: Option<Entity<GraphSession>>,
    files: Vec<(PathBuf, SystemTime)>,
    /// The aliases whose stored query the graph does not serve are shown.
    unserved_open: bool,
    _session_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl QueriesPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        session: Option<Entity<GraphSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions =
            vec![cx.observe_global::<SavedQueriesChanged>(|this, cx| this.read_folder(cx))];
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, _, cx| match event {
                    WorkspaceEvent::SessionChanged => {
                        let session = workspace.read(cx).session().cloned();
                        this.watch_session(session, cx);
                        this.read_folder(cx);
                    }
                },
            ));
        }
        // Aliases come from the CLI config, which is read again when it changes.
        if let Some(registry) = ConnectionRegistry::try_global(cx) {
            subscriptions.push(cx.observe(&registry, |_, _, cx| cx.notify()));
        }
        let mut this = Self {
            session: None,
            files: Vec::new(),
            unserved_open: false,
            _session_subscription: None,
            _subscriptions: subscriptions,
        };
        this.watch_session(session, cx);
        this.read_folder(cx);
        this
    }

    fn watch_session(&mut self, session: Option<Entity<GraphSession>>, cx: &mut Context<Self>) {
        self._session_subscription = session.as_ref().map(|session| {
            cx.subscribe(session, |_, _, event: &SessionEvent, cx| {
                if *event == SessionEvent::StoredQueriesChanged {
                    cx.notify();
                }
            })
        });
        self.session = session;
        cx.notify();
    }

    /// The stored queries the graph serves, each with its aliases under it, then the aliases
    /// whose query it does not serve, folded.
    fn render_stored(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use studio_ui::ds;
        let session = self.session.clone()?;
        let (catalog, server, graph) = {
            let session = session.read(cx);
            (
                session.stored_queries.clone(),
                session.server_name(cx),
                session.graph_id.clone(),
            )
        };
        let aliases: Vec<CliAlias> = ConnectionRegistry::try_global(cx)
            .map(|registry| registry.read(cx).aliases_for(&server, &graph))
            .unwrap_or_default();
        let refresh = IconButton::new("stored-refresh", IconName::RotateCw)
            .icon_size(IconSize::XSmall)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text("Read the stored queries again"))
            .on_click(move |_, _, cx| {
                session.update(cx, |session, cx| session.reload_stored_queries(cx))
            });
        let header = ds::section_header("Stored", Some(refresh.into_any_element()));
        let mut body = v_flex().gap(px(ds::space::WITHIN));
        let mut unserved = Vec::new();
        match catalog {
            Loadable::NotLoaded | Loadable::Loading => {
                body = body.child(ds::sidebar_note("Reading stored queries…"));
            }
            Loadable::Failed(error) => {
                body = body.child(ds::sidebar_placeholder(
                    "stored-failed",
                    IconName::Warning,
                    "Could not be read",
                    error,
                ));
            }
            Loadable::Loaded(queries) => {
                if queries.is_empty() {
                    body = body.child(ds::sidebar_placeholder(
                        "stored-empty",
                        IconName::Server,
                        "No stored queries",
                        "A graph's stored queries are declared in its cluster.yaml and applied \
                         with `omnigraph cluster apply`; the server then runs each by name",
                    ));
                }
                let mut index = 0;
                for query in queries.iter() {
                    let open = OpenStoredQuery {
                        name: query.name.clone(),
                        alias: None,
                    };
                    let tooltip = query
                        .description
                        .clone()
                        .unwrap_or_else(|| format!("Run {} on the server", query.name));
                    body = body.child(
                        ds::SidebarRow::new(("stored-query", index), query.name.clone())
                            .start(
                                Icon::new(IconName::Server)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .when(query.mutation, |row| row.end(ds::sidebar_count("writes")))
                            .tooltip(tooltip)
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(Box::new(open.clone()), cx)
                            }),
                    );
                    index += 1;
                    for alias in aliases.iter().filter(|alias| alias.query == query.name) {
                        body = body.child(alias_row(index, alias));
                        index += 1;
                    }
                }
                unserved = aliases
                    .into_iter()
                    .filter(|alias| !queries.iter().any(|query| query.name == alias.query))
                    .collect();
            }
        }
        let open = self.unserved_open;
        let unserved_section = (!unserved.is_empty()).then(|| {
            v_flex()
                .child(
                    ds::section_disclosure("unserved", "Not served", unserved.len(), open)
                        .tooltip(Tooltip::text(
                            "Aliases in the CLI config whose stored query this graph does not serve",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.unserved_open = !this.unserved_open;
                            cx.notify();
                        })),
                )
                .when(open, |section| {
                    section.children(
                        unserved
                            .iter()
                            .enumerate()
                            .map(|(index, alias)| unserved_row(index, alias)),
                    )
                })
        });
        Some(
            v_flex()
                .child(header)
                .child(body)
                .children(unserved_section)
                .into_any_element(),
        )
    }

    fn graph_key(&self, cx: &App) -> Option<String> {
        Some(self.session.as_ref()?.read(cx).settings_key(cx))
    }

    /// Lists the graph's queries folder again, off the main thread.
    fn read_folder(&mut self, cx: &mut Context<Self>) {
        let Some(graph) = self.graph_key(cx) else {
            self.files.clear();
            cx.notify();
            return;
        };
        let listing = cx.background_spawn(async move { studio_settings::saved_queries(&graph) });
        cx.spawn(async move |this, cx| {
            let files = listing.await;
            this.update(cx, |this, cx| {
                this.files = files;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for QueriesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = SystemTime::now();
        let saved = if self.files.is_empty() {
            studio_ui::ds::sidebar_placeholder(
                "queries-empty",
                IconName::FileCode,
                "No saved queries",
                "⌘S in a query saves it here, for this graph",
            )
            .into_any_element()
        } else {
            v_flex()
                .gap(px(studio_ui::ds::space::WITHIN))
                .children(
                    self.files
                        .iter()
                        .enumerate()
                        .map(|(index, (path, modified))| {
                            let name = path
                                .file_stem()
                                .map(|stem| stem.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let age = now
                                .duration_since(*modified)
                                .map(|age| age.as_micros() as i64)
                                .unwrap_or(0);
                            let when = studio_ui::relative_time(
                                studio_ui::now_micros() - age,
                                studio_ui::now_micros(),
                            );
                            let open = OpenQueryFile {
                                path: Some(path.to_string_lossy().into_owned()),
                            };
                            studio_ui::ds::SidebarRow::new(("saved-query", index), name)
                                .start(
                                    Icon::new(IconName::FileCode)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .end(studio_ui::ds::sidebar_count(when))
                                .tooltip(path.display().to_string())
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(Box::new(open.clone()), cx)
                                })
                        }),
                )
                .into_any_element()
        };
        let stored = self.render_stored(cx);
        v_flex()
            .id("queries-panel")
            .size_full()
            .overflow_y_scroll()
            .child(saved)
            .children(stored)
    }
}

/// The Queries panel's actions, in its title line: a new query, a file, the folder.
pub struct QueriesActions {
    panel: WeakEntity<QueriesPanel>,
    _observe: Subscription,
}

impl QueriesActions {
    pub fn new(panel: &Entity<QueriesPanel>, cx: &mut Context<Self>) -> Self {
        Self {
            panel: panel.downgrade(),
            _observe: cx.observe(panel, |_, _, cx| cx.notify()),
        }
    }
}

impl Render for QueriesActions {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let folder = self
            .panel
            .upgrade()
            .and_then(|panel| panel.read(cx).graph_key(cx))
            .map(|graph| studio_settings::queries_dir(&graph));
        let button = |id: &'static str, icon: IconName| {
            IconButton::new(id, icon)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
        };
        h_flex()
            .gap(px(studio_ui::ds::space::XXS))
            .child(
                button("queries-new", IconName::Plus)
                    .tooltip(|_, cx| Tooltip::for_action("New query", &NewQuery, cx))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(NewQuery), cx)),
            )
            .child(
                button("queries-open", IconName::FolderOpen)
                    .tooltip(|_, cx| {
                        Tooltip::for_action("Open a .gq file", &OpenQueryFile::default(), cx)
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(OpenQueryFile::default()), cx)
                    }),
            )
            .children(folder.map(|folder| {
                button("queries-reveal", IconName::Folder)
                    .tooltip(Tooltip::text("Show this graph's queries folder"))
                    .on_click(move |_, _, cx| {
                        std::fs::create_dir_all(&folder).ok();
                        cx.reveal_path(&folder)
                    })
            }))
    }
}

/// An alias from the CLI config, under the stored query it runs with its values.
fn alias_row(index: usize, alias: &CliAlias) -> AnyElement {
    let open = OpenStoredQuery {
        name: alias.query.clone(),
        alias: Some(alias.name.clone()),
    };
    // Indented by an icon and its gap, so the alias's icon sits under its query's label.
    div()
        .pl(px(studio_ui::ds::space::XL))
        .child(
            studio_ui::ds::SidebarRow::new(("stored-alias", index), alias.name.clone())
                .start(
                    Icon::new(IconName::Link)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .end(studio_ui::ds::sidebar_count("alias"))
                .tooltip(format!(
                    "Alias from the CLI config: {} with its values",
                    alias.query
                ))
                .on_click(move |_, window, cx| window.dispatch_action(Box::new(open.clone()), cx)),
        )
        .into_any_element()
}

/// An alias whose stored query this graph does not serve: listed under "Not served", dim.
fn unserved_row(index: usize, alias: &CliAlias) -> AnyElement {
    h_flex()
        .id(("unserved-alias", index))
        .h(px(studio_ui::ds::size::SIDEBAR_ROW))
        .mx(px(studio_ui::ds::space::SM))
        .px(px(studio_ui::ds::space::SM))
        .gap(px(studio_ui::ds::space::MD))
        .items_center()
        .tooltip(Tooltip::text(format!(
            "This graph serves no stored query named {}",
            alias.query
        )))
        .child(
            Icon::new(IconName::Link)
                .size(IconSize::XSmall)
                .color(Color::Disabled),
        )
        .child(
            Label::new(alias.name.clone())
                .size(LabelSize::Small)
                .color(Color::Disabled)
                .single_line()
                .truncate(),
        )
        .child(div().flex_1())
        .child(
            Label::new(alias.query.clone())
                .size(LabelSize::XSmall)
                .color(Color::Disabled)
                .single_line()
                .truncate(),
        )
        .into_any_element()
}
