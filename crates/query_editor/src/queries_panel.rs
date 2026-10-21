//! The left dock's "Queries" panel (spec 007): this graph's saved `.gq` files, the most
//! recently changed first, then the stored queries the server serves for the graph, each with
//! the CLI config's aliases for it. A click opens one, as ↵ opens the one ↑ and ↓ select;
//! ⌘S in a new query saves it here.

use std::path::PathBuf;
use std::time::SystemTime;

use gpui::{
    Action, Context, Entity, FocusHandle, Focusable, ListAlignment, ListState, Render,
    Subscription, Task, WeakEntity, Window, list,
};
use graph_store::cli_config::CliAlias;
use graph_store::{ConnectionRegistry, GraphSession, Loadable, SessionEvent};
use studio_actions::graph::{NewQuery, OpenQueryFile, OpenStoredQuery};
use studio_actions::queries::{OpenSelected, SelectNext, SelectPrevious};
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
    /// The panel's keyboard: ↑ and ↓ move among the rows that open a query, ↵ opens one.
    focus_handle: FocusHandle,
    /// The row the keyboard is on, by what it opens, so it stays on its query while rows come
    /// and go. Shown while the panel has the keyboard.
    selected: Option<RowKey>,
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

/// What a row that opens a query is, for the selection: a saved file by its path, a stored
/// query by its name, an alias by its name and its query's.
#[derive(Clone, Debug, PartialEq)]
enum RowKey {
    Saved(PathBuf),
    Stored(String),
    Alias { name: String, query: String },
}

impl PanelRow {
    /// The row's key when it opens a query. Headers, notes, placeholders and the aliases the
    /// graph does not serve open nothing, so the keyboard passes over them.
    fn key(&self) -> Option<RowKey> {
        match self {
            PanelRow::Saved { path, .. } => Some(RowKey::Saved(path.clone())),
            PanelRow::Stored { name, .. } => Some(RowKey::Stored(name.clone())),
            PanelRow::Alias(alias) => Some(RowKey::Alias {
                name: alias.name.clone(),
                query: alias.query.clone(),
            }),
            _ => None,
        }
    }

    /// Whether the row is the one `key` names.
    fn is(&self, key: &RowKey) -> bool {
        match (self, key) {
            (PanelRow::Saved { path, .. }, RowKey::Saved(at)) => path == at,
            (PanelRow::Stored { name, .. }, RowKey::Stored(named)) => name == named,
            (PanelRow::Alias(alias), RowKey::Alias { name, query }) => {
                alias.name == *name && alias.query == *query
            }
            _ => false,
        }
    }

    /// What opening the row dispatches, as a click and ↵ do.
    fn open(&self) -> Option<Box<dyn Action>> {
        match self {
            PanelRow::Saved { path, .. } => Some(Box::new(OpenQueryFile {
                path: Some(path.to_string_lossy().into_owned()),
            })),
            PanelRow::Stored { name, .. } => Some(Box::new(OpenStoredQuery {
                name: name.clone(),
                alias: None,
            })),
            PanelRow::Alias(alias) => Some(Box::new(OpenStoredQuery {
                name: alias.query.clone(),
                alias: Some(alias.name.clone()),
            })),
            _ => None,
        }
    }
}

impl Focusable for QueriesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
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
        // The selected row shows only while the panel has the keyboard.
        let focus_handle = cx.focus_handle();
        subscriptions.push(cx.on_focus_in(&focus_handle, window, |_, _, cx| cx.notify()));
        subscriptions.push(cx.on_focus_out(&focus_handle, window, |_, _, _, cx| cx.notify()));
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
            focus_handle,
            selected: None,
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
        // Another graph's queries: the keyboard starts again from the first.
        if self.session.as_ref().map(Entity::entity_id) != session.as_ref().map(Entity::entity_id) {
            self.selected = None;
        }
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

    /// Where the selected row is among the rows drawn.
    fn selected_row(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows.iter().position(|row| row.is(selected))
    }

    /// Selects the row at `ix` and scrolls it into view.
    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(key) = self.rows.get(ix).and_then(PanelRow::key) else {
            return;
        };
        self.selected = Some(key);
        self.list_state.scroll_to_reveal_item(ix);
        cx.notify();
    }

    /// Moves the selection to the next row that opens a query, or the previous; at either end
    /// it stays. With none selected, the first is.
    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let opens = |row: &PanelRow| {
            matches!(
                row,
                PanelRow::Saved { .. } | PanelRow::Stored { .. } | PanelRow::Alias(_)
            )
        };
        let to = match self.selected_row() {
            None => self.rows.iter().position(opens),
            Some(at) if forward => self.rows[at + 1..]
                .iter()
                .position(opens)
                .map(|next| at + 1 + next),
            Some(at) => self.rows[..at].iter().rposition(opens),
        };
        if let Some(ix) = to {
            self.select(ix, cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(true, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    /// Opens the selected row's query, as clicking it does.
    fn open_selected(&mut self, _: &OpenSelected, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self
            .selected_row()
            .and_then(|ix| self.rows.get(ix))
            .and_then(PanelRow::open)
        {
            window.dispatch_action(open, cx);
        }
    }

    /// What clicking row `ix` does: selects it, so the keyboard goes on from it, and opens it.
    fn click(
        &self,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let panel = cx.entity().downgrade();
        let row = self.rows.get(ix);
        let (key, open) = (row.and_then(PanelRow::key), row.and_then(PanelRow::open));
        move |_, window, cx| {
            panel
                .update(cx, |panel, cx| {
                    panel.selected = key.clone();
                    cx.notify();
                })
                .ok();
            if let Some(open) = &open {
                window.dispatch_action(open.boxed_clone(), cx);
            }
        }
    }

    fn render_row(&mut self, ix: usize, selected: bool, cx: &mut Context<Self>) -> AnyElement {
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
                ds::SidebarRow::new(("saved-query", ix), name)
                    .icon(IconName::FileCode)
                    .end(ds::sidebar_count(when))
                    .selected(selected)
                    .tooltip(path.display().to_string())
                    .on_click(self.click(ix, cx))
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
                ds::SidebarRow::new(("stored-query", ix), name)
                    .icon(IconName::Server)
                    .when(mutation, |row| row.end(ds::sidebar_count("writes")))
                    .selected(selected)
                    .tooltip(tooltip)
                    .on_click(self.click(ix, cx))
                    .into_any_element()
            }
            PanelRow::Alias(alias) => alias_row(ix, &alias, selected, self.click(ix, cx)),
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
        let selected = self.selected_row();
        v_flex()
            .key_context("QueriesPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::open_selected))
            .child(
                list(
                    self.list_state.clone(),
                    // A row's root takes the list's width; rows of one group sit `WITHIN` apart.
                    cx.processor(move |this, ix, window, cx| {
                        let selected =
                            selected == Some(ix) && this.focus_handle.contains_focused(window, cx);
                        div()
                            .w_full()
                            .pb(px(studio_ui::ds::space::WITHIN))
                            .child(this.render_row(ix, selected, cx))
                            .into_any_element()
                    }),
                )
                .size_full(),
            )
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
fn alias_row(
    index: usize,
    alias: &CliAlias,
    selected: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    // Indented by an icon and its gap, so the alias's icon sits under its query's label.
    div()
        .pl(px(studio_ui::ds::space::XL))
        .child(
            studio_ui::ds::SidebarRow::new(("stored-alias", index), alias.name.clone())
                .icon(IconName::Link)
                .end(studio_ui::ds::sidebar_count("alias"))
                .selected(selected)
                .tooltip(format!(
                    "Alias from the CLI config: {} with its values",
                    alias.query
                ))
                .on_click(on_click),
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::{KeyBinding, TestAppContext};
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};

    use super::*;

    /// A session on a server whose graph serves two stored queries, `people_titled` and
    /// `set_title`.
    fn session(cx: &mut TestAppContext) -> Entity<GraphSession> {
        let http = FakeHttpClient::create(|request| {
            let body = if request.uri().path().ends_with("/queries") {
                r#"{"queries":[{"name":"people_titled"},{"name":"set_title","mutation":true}]}"#
            } else {
                r#"{"branches":["main"]}"#
            };
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx))
    }

    /// The panel takes the keyboard: ↓ and ↑ move among the rows that open a query, passing
    /// over the Stored header and stopping at either end, and ↵ opens the row selected as a
    /// click does, a saved file and a stored query alike. The selection stays on its query
    /// while the rows above it change.
    #[gpui::test]
    async fn the_rows_are_selected_and_opened_by_keys(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let session = session(cx);
        session.update(cx, |session, cx| session.reload_stored_queries(cx));
        cx.run_until_parked();
        let opened: Arc<Mutex<Vec<String>>> = Arc::default();
        cx.update(|cx| {
            cx.bind_keys([
                KeyBinding::new("down", SelectNext, Some("QueriesPanel")),
                KeyBinding::new("up", SelectPrevious, Some("QueriesPanel")),
                KeyBinding::new("enter", OpenSelected, Some("QueriesPanel")),
            ]);
            let files = opened.clone();
            cx.on_action(move |open: &OpenQueryFile, _| {
                files.lock().unwrap().push(format!("file {:?}", open.path));
            });
            let stored = opened.clone();
            cx.on_action(move |open: &OpenStoredQuery, _| {
                stored.lock().unwrap().push(format!("stored {}", open.name));
            });
        });
        let (panel, cx) = cx.add_window_view(|window, cx| {
            QueriesPanel::new(WeakEntity::new_invalid(), Some(session), window, cx)
        });
        cx.run_until_parked();
        let people = PathBuf::from("/queries/people.gq");
        panel.update(cx, |panel, cx| {
            panel.files = vec![(people.clone(), SystemTime::UNIX_EPOCH)];
            cx.notify();
        });
        cx.update(|window, cx| window.focus(&panel.focus_handle(cx), cx));
        cx.run_until_parked();
        let selected = |cx: &mut gpui::VisualTestContext| {
            panel.read_with(cx, |panel, _| panel.selected.clone())
        };

        cx.simulate_keystrokes("down");
        assert_eq!(
            selected(cx),
            Some(RowKey::Saved(people.clone())),
            "the first"
        );
        cx.simulate_keystrokes("enter");
        cx.simulate_keystrokes("down");
        assert_eq!(
            selected(cx),
            Some(RowKey::Stored("people_titled".into())),
            "past the header"
        );
        cx.simulate_keystrokes("down down");
        assert_eq!(
            selected(cx),
            Some(RowKey::Stored("set_title".into())),
            "the last stays"
        );
        cx.simulate_keystrokes("up enter");
        assert_eq!(
            *opened.lock().unwrap(),
            [
                format!("file {:?}", Some(people.to_string_lossy().into_owned())),
                "stored people_titled".to_owned(),
            ]
        );

        // A file saved since goes above it: the selection stays on its query.
        panel.update(cx, |panel, cx| {
            panel.files.insert(
                0,
                (PathBuf::from("/queries/new.gq"), SystemTime::UNIX_EPOCH),
            );
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        assert_eq!(selected(cx), Some(RowKey::Stored("set_title".into())));
    }
}
