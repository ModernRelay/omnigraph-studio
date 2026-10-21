//! The left dock's "Queries" panel (spec 007): this graph's saved `.gq` files, the most
//! recently changed first, then the stored queries the server serves for the graph, each with
//! the CLI config's aliases for it. A click opens one; ⌘S in a new query saves it here.

use std::path::PathBuf;
use std::time::SystemTime;

use gpui::{
    Context, Entity, ListAlignment, ListState, Render, Subscription, Task, WeakEntity, Window, list,
};
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
    /// The list drawn, as descriptors: only the visible rows are built (constitution IX).
    list_state: ListState,
    rows: Vec<PanelRow>,
    _session_subscription: Option<Subscription>,
    /// The folder's latest listing: an earlier one, of another graph's folder perhaps, is
    /// dropped rather than landing after it.
    _listing: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// One row of the panel, as what it shows rather than its element, so the list is told only of
/// the rows that changed and keeps its scroll position.
#[derive(Clone, PartialEq)]
enum PanelRow {
    SavedEmpty,
    Saved {
        path: PathBuf,
        modified: SystemTime,
    },
    StoredHeader,
    StoredReading,
    StoredFailed(String),
    StoredEmpty,
    Stored {
        name: String,
        description: Option<String>,
        mutation: bool,
    },
    Alias(CliAlias),
    UnservedHeader {
        count: usize,
        open: bool,
    },
    Unserved(CliAlias),
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
            list_state: ListState::new(
                0,
                ListAlignment::Top,
                px(studio_ui::ds::size::SIDEBAR_ROW * 8.),
            ),
            rows: Vec::new(),
            _session_subscription: None,
            _listing: None,
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

    /// The rows to draw: the saved files, newest first; then the stored queries the graph
    /// serves, each with its aliases under it, then the aliases whose query it does not serve,
    /// folded.
    fn panel_rows(&self, cx: &App) -> Vec<PanelRow> {
        let mut rows = Vec::new();
        if self.files.is_empty() {
            rows.push(PanelRow::SavedEmpty);
        }
        rows.extend(self.files.iter().map(|(path, modified)| PanelRow::Saved {
            path: path.clone(),
            modified: *modified,
        }));
        let Some(session) = &self.session else {
            return rows;
        };
        let session = session.read(cx);
        let (server, graph) = (session.server_name(cx), session.graph_id.clone());
        let aliases: Vec<CliAlias> = ConnectionRegistry::try_global(cx)
            .map(|registry| registry.read(cx).aliases_for(&server, &graph))
            .unwrap_or_default();
        rows.push(PanelRow::StoredHeader);
        match &session.stored_queries {
            Loadable::NotLoaded | Loadable::Loading => rows.push(PanelRow::StoredReading),
            Loadable::Failed(error) => rows.push(PanelRow::StoredFailed(error.clone())),
            Loadable::Loaded(queries) => {
                if queries.is_empty() {
                    rows.push(PanelRow::StoredEmpty);
                }
                for query in queries.iter() {
                    rows.push(PanelRow::Stored {
                        name: query.name.clone(),
                        description: query.description.clone(),
                        mutation: query.mutation,
                    });
                    rows.extend(
                        aliases
                            .iter()
                            .filter(|alias| alias.query == query.name)
                            .cloned()
                            .map(PanelRow::Alias),
                    );
                }
                let unserved: Vec<CliAlias> = aliases
                    .into_iter()
                    .filter(|alias| !queries.iter().any(|query| query.name == alias.query))
                    .collect();
                if !unserved.is_empty() {
                    rows.push(PanelRow::UnservedHeader {
                        count: unserved.len(),
                        open: self.unserved_open,
                    });
                    if self.unserved_open {
                        rows.extend(unserved.into_iter().map(PanelRow::Unserved));
                    }
                }
            }
        }
        rows
    }

    fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        use studio_ui::ds;
        let Some(row) = self.rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        match row {
            PanelRow::SavedEmpty => ds::sidebar_placeholder(
                "queries-empty",
                IconName::FileCode,
                "No saved queries",
                match studio_ui::shortcut_text(&studio_actions::query::Save, cx) {
                    Some(keys) => format!("{keys} in a query saves it here, for this graph"),
                    None => "Saving a query keeps it here, for this graph".to_owned(),
                },
            )
            .into_any_element(),
            PanelRow::Saved { path, modified } => {
                let name = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let age = SystemTime::now()
                    .duration_since(modified)
                    .map(|age| age.as_micros() as i64)
                    .unwrap_or(0);
                let when =
                    studio_ui::relative_time(studio_ui::now_micros() - age, studio_ui::now_micros());
                let open = OpenQueryFile {
                    path: Some(path.to_string_lossy().into_owned()),
                };
                ds::SidebarRow::new(("saved-query", ix), name)
                    .icon(IconName::FileCode)
                    .end(ds::sidebar_count(when))
                    .tooltip(path.display().to_string())
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(Box::new(open.clone()), cx)
                    })
                    .into_any_element()
            }
            PanelRow::StoredHeader => {
                let session = self.session.clone();
                let refresh = studio_ui::ds::row_icon_button("stored-refresh", IconName::RotateCw)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Read the stored queries again"))
                    .on_click(move |_, _, cx| {
                        if let Some(session) = &session {
                            session.update(cx, |session, cx| session.reload_stored_queries(cx));
                        }
                    });
                ds::section_header("Stored", Some(refresh.into_any_element())).into_any_element()
            }
            PanelRow::StoredReading => {
                ds::sidebar_note("Reading stored queries…").into_any_element()
            }
            PanelRow::StoredFailed(error) => ds::sidebar_placeholder(
                "stored-failed",
                IconName::Warning,
                "Could not be read",
                error,
            )
            .into_any_element(),
            PanelRow::StoredEmpty => ds::sidebar_placeholder(
                "stored-empty",
                IconName::Server,
                "No stored queries",
                "A graph's stored queries are declared in its cluster.yaml and applied \
                 with `omnigraph cluster apply`; the server then runs each by name",
            )
            .into_any_element(),
            PanelRow::Stored {
                name,
                description,
                mutation,
            } => {
                let tooltip = description.unwrap_or_else(|| format!("Run {name} on the server"));
                let open = OpenStoredQuery {
                    name: name.clone(),
                    alias: None,
                };
                ds::SidebarRow::new(("stored-query", ix), name)
                    .icon(IconName::Server)
                    .when(mutation, |row| row.end(ds::sidebar_count("writes")))
                    .tooltip(tooltip)
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(Box::new(open.clone()), cx)
                    })
                    .into_any_element()
            }
            PanelRow::Alias(alias) => alias_row(ix, &alias),
            PanelRow::UnservedHeader { count, open } => v_flex()
                .child(
                    ds::section_disclosure("unserved", "Not served", count, open)
                        .tooltip(Tooltip::text(
                            "Aliases in the CLI config whose stored query this graph does not serve",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.unserved_open = !this.unserved_open;
                            cx.notify();
                        })),
                )
                .into_any_element(),
            PanelRow::Unserved(alias) => unserved_row(ix, &alias),
        }
    }

    fn graph_key(&self, cx: &App) -> Option<String> {
        Some(self.session.as_ref()?.read(cx).overrides_key(cx))
    }

    /// Lists the graph's queries folder again, off the main thread.
    fn read_folder(&mut self, cx: &mut Context<Self>) {
        let Some(graph) = self.graph_key(cx) else {
            self._listing = None;
            self.files.clear();
            cx.notify();
            return;
        };
        let listing = cx.background_spawn(async move { studio_settings::saved_queries(&graph) });
        self._listing = Some(cx.spawn(async move |this, cx| {
            let files = listing.await;
            this.update(cx, |this, cx| {
                this.files = files;
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Render for QueriesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.panel_rows(cx);
        studio_ui::rows::sync(&self.list_state, &mut self.rows, rows);
        list(
            self.list_state.clone(),
            // A row's root takes the list's width; rows of one group sit `WITHIN` apart.
            cx.processor(|this, ix, _window, cx| {
                div()
                    .w_full()
                    .pb(px(studio_ui::ds::space::WITHIN))
                    .child(this.render_row(ix, cx))
                    .into_any_element()
            }),
        )
        .size_full()
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
            studio_ui::ds::row_icon_button(id, icon).icon_color(Color::Muted)
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
                .icon(IconName::Link)
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
        .child(studio_ui::ds::icon::meta(IconName::Link).color(Color::Disabled))
        .child(
            studio_ui::ds::text::body(alias.name.clone())
                .color(Color::Disabled)
                .single_line()
                .truncate(),
        )
        .child(div().flex_1())
        .child(
            studio_ui::ds::text::meta(alias.query.clone())
                .color(Color::Disabled)
                .single_line()
                .truncate(),
        )
        .into_any_element()
}
