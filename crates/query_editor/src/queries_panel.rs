//! The left dock's "Queries" panel (spec 007): this graph's saved `.gq` files, the most
//! recently changed first, then the stored queries the server serves for the graph. A click
//! opens one, as ↵ opens the one ↑ and ↓ select; ⌘S in a new query saves it here.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui::{
    Action, Context, Entity, EntityId, FocusHandle, Focusable, ListAlignment, ListState, Render,
    Subscription, Task, WeakEntity, Window, list,
};
use graph_store::{GraphSession, Loadable, SessionEvent};
use studio_actions::graph::{NewQuery, OpenQueryFile, OpenStoredQuery};
use studio_actions::queries::{OpenSelected, SelectNext, SelectPrevious};
use studio_ui::ds::Tipped;
use studio_workspace::{ToastKind, Workspace, WorkspaceEvent};
use ui::prelude::*;

use crate::SavedQueriesChanged;

pub const QUERIES_PANEL: &str = "queries";

pub struct QueriesPanel {
    session: Option<Entity<GraphSession>>,
    /// The saved files of the graph [`Self::files_of`] names, newest first.
    files: Vec<(PathBuf, SystemTime)>,
    /// The graph the files were listed for: after a switch, the last graph's are not shown,
    /// nor opened, while this one's are listed.
    files_of: Option<String>,
    /// The folder is listed again at the next draw: a hidden panel lists nothing.
    folder_owed: bool,
    /// The list drawn, as descriptors: only the visible rows are built (constitution IX).
    list_state: ListState,
    rows: Vec<PanelRow>,
    /// The panel's keyboard: ↑ and ↓ move among the rows that open a query, ↵ opens one.
    focus_handle: FocusHandle,
    /// The row the keyboard is on, by what it opens, so it stays on its query while rows come
    /// and go. Shown while the panel has the keyboard.
    selected: Option<RowKey>,
    _session_subscription: Option<Subscription>,
    /// The graph whose stored-query catalog the panel has asked for. The catalog is read only
    /// once a view wants it, and the panel wants it when it is drawn, so a hidden panel reads
    /// nothing.
    catalog_asked: Option<EntityId>,
    /// The folder's latest listing: an earlier one, of another graph's folder perhaps, is
    /// dropped rather than landing after it.
    _listing: Option<Task<()>>,
    /// The next draw of the saved files' ages ("2 min ago"), a minute after the last: each
    /// draw schedules it, so a hidden panel stops.
    _tick: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// How often a drawn panel's saved files' ages are drawn again.
const AGE_TICK: Duration = Duration::from_secs(60);

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
}

/// What a row that opens a query is, for the selection: a saved file by its path, a stored
/// query by its name.
#[derive(Clone, Debug, PartialEq)]
enum RowKey {
    Saved(PathBuf),
    Stored(String),
}

impl PanelRow {
    /// The row's key when it opens a query. Headers, notes and placeholders open nothing, so
    /// the keyboard passes over them.
    fn key(&self) -> Option<RowKey> {
        match self {
            PanelRow::Saved { path, .. } => Some(RowKey::Saved(path.clone())),
            PanelRow::Stored { name, .. } => Some(RowKey::Stored(name.clone())),
            _ => None,
        }
    }

    /// Whether the row is the one `key` names.
    fn is(&self, key: &RowKey) -> bool {
        match (self, key) {
            (PanelRow::Saved { path, .. }, RowKey::Saved(at)) => path == at,
            (PanelRow::Stored { name, .. }, RowKey::Stored(named)) => name == named,
            _ => false,
        }
    }

    /// What opening the row dispatches, as a click and ↵ do.
    fn open(&self) -> Option<Box<dyn Action>> {
        match self {
            PanelRow::Saved { path, .. } => Some(Box::new(OpenQueryFile {
                path: Some(path.to_string_lossy().into_owned()),
            })),
            PanelRow::Stored { name, .. } => Some(Box::new(OpenStoredQuery { name: name.clone() })),
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
            vec![cx.observe_global::<SavedQueriesChanged>(|this, cx| this.owe_folder(cx))];
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, _, cx| match event {
                    WorkspaceEvent::SessionChanged => {
                        let session = workspace.read(cx).session().cloned();
                        this.watch_session(session, cx);
                        this.owe_folder(cx);
                    }
                    WorkspaceEvent::ActiveItemChanged => {}
                },
            ));
        }
        // The selected row shows only while the panel has the keyboard.
        let focus_handle = cx.focus_handle();
        subscriptions.push(cx.on_focus_in(&focus_handle, window, |_, _, cx| cx.notify()));
        subscriptions.push(cx.on_focus_out(&focus_handle, window, |_, _, _, cx| cx.notify()));
        let mut this = Self {
            session: None,
            files: Vec::new(),
            files_of: None,
            folder_owed: true,
            list_state: ListState::new(
                0,
                ListAlignment::Top,
                px(studio_ui::ds::size::SIDEBAR_ROW * 8.),
            ),
            rows: Vec::new(),
            focus_handle,
            selected: None,
            _session_subscription: None,
            catalog_asked: None,
            _listing: None,
            _tick: None,
            _subscriptions: subscriptions,
        };
        this.watch_session(session, cx);
        this
    }

    fn watch_session(&mut self, session: Option<Entity<GraphSession>>, cx: &mut Context<Self>) {
        self._session_subscription = session.as_ref().map(|session| {
            cx.subscribe(session, |_, _, event: &SessionEvent, cx| {
                if let SessionEvent::StoredQueriesChanged = event {
                    cx.notify()
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
    /// serves.
    fn panel_rows(&self, cx: &App) -> Vec<PanelRow> {
        let mut rows = Vec::new();
        // Until this graph's folder is listed, none are shown, and nothing says it has none.
        if self.files_of == self.graph_key(cx) {
            if self.files.is_empty() {
                rows.push(PanelRow::SavedEmpty);
            }
            rows.extend(self.files.iter().map(|(path, modified)| PanelRow::Saved {
                path: path.clone(),
                modified: *modified,
            }));
        }
        let Some(session) = &self.session else {
            return rows;
        };
        rows.push(PanelRow::StoredHeader);
        match &session.read(cx).stored_queries {
            Loadable::NotLoaded | Loadable::Loading => rows.push(PanelRow::StoredReading),
            Loadable::Failed(error) => rows.push(PanelRow::StoredFailed(error.clone())),
            Loadable::Loaded(queries) => {
                if queries.is_empty() {
                    rows.push(PanelRow::StoredEmpty);
                }
                rows.extend(queries.iter().map(|query| PanelRow::Stored {
                    name: query.name.clone(),
                    description: query.description.clone(),
                    mutation: query.mutation,
                }));
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
        let opens =
            |row: &PanelRow| matches!(row, PanelRow::Saved { .. } | PanelRow::Stored { .. });
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
                let when = studio_ui::relative_time(
                    studio_ui::now_micros() - age,
                    studio_ui::now_micros(),
                );
                ds::SidebarRow::new(("saved-query", ix), name)
                    .icon(IconName::FileCode)
                    .end(ds::sidebar_count(when))
                    .selected(selected)
                    .tip(ds::tooltip(path.display().to_string()))
                    .on_click(self.click(ix, cx))
                    .into_any_element()
            }
            PanelRow::StoredHeader => {
                let session = self.session.clone();
                let refresh = studio_ui::ds::row_icon_button("stored-refresh", IconName::RotateCw)
                    .icon_color(Color::Muted)
                    .tip(studio_ui::ds::tooltip("Read the stored queries again"))
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
                    .tip(ds::tooltip(tooltip))
                    .on_click(self.click(ix, cx))
                    .into_any_element()
            }
        }
    }

    fn graph_key(&self, cx: &App) -> Option<String> {
        Some(self.session.as_ref()?.read(cx).overrides_key())
    }

    /// The graph's queries folder changed, or the graph did: it is listed again when the
    /// panel is next drawn.
    fn owe_folder(&mut self, cx: &mut Context<Self>) {
        self.folder_owed = true;
        cx.notify();
    }

    /// Lists the graph's queries folder again, off the main thread.
    fn read_folder(&mut self, cx: &mut Context<Self>) {
        let Some(graph) = self.graph_key(cx) else {
            self._listing = None;
            self.files.clear();
            self.files_of = None;
            cx.notify();
            return;
        };
        let of = graph.clone();
        let listing = cx.background_spawn(async move { studio_settings::saved_queries(&of) });
        self._listing = Some(cx.spawn(async move |this, cx| {
            let files = listing.await;
            this.update(cx, |this, cx| {
                this.files = files;
                this.files_of = Some(graph);
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Render for QueriesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(session) = self.session.clone()
            && self.catalog_asked != Some(session.entity_id())
        {
            self.catalog_asked = Some(session.entity_id());
            cx.defer(move |cx| session.update(cx, |session, cx| session.want_stored_queries(cx)));
        }
        if std::mem::take(&mut self.folder_owed) {
            cx.defer_in(window, |this, _, cx| this.read_folder(cx));
        }
        if !self.files.is_empty() && self._tick.is_none() {
            self._tick = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(AGE_TICK).await;
                this.update(cx, |this, cx| {
                    this._tick = None;
                    cx.notify();
                })
                .ok();
            }));
        }
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
                    .tip_fn(|_, cx| {
                        studio_ui::ds::tooltip_for_action_view("New query", &NewQuery, cx)
                    })
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(NewQuery), cx)),
            )
            .child(
                button("queries-open", IconName::FolderOpen)
                    .tip_fn(|_, cx| {
                        studio_ui::ds::tooltip_for_action_view(
                            "Open a .gq file",
                            &OpenQueryFile::default(),
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(OpenQueryFile::default()), cx)
                    }),
            )
            .children(folder.map(|folder| {
                button("queries-reveal", IconName::Folder)
                    .tip(studio_ui::ds::tooltip("Show this graph's queries folder"))
                    .on_click(move |_, window, cx| {
                        let workspace =
                            studio_workspace::workspace_for(window).map(|w| w.downgrade());
                        reveal_folder(folder.clone(), workspace, cx);
                    })
            }))
    }
}

/// Shows the graph's queries folder, made first off the main thread. One that cannot be made
/// is said in a toast in `workspace`: shown, nothing would open.
fn reveal_folder(folder: PathBuf, workspace: Option<WeakEntity<Workspace>>, cx: &mut App) {
    let made = {
        let folder = folder.clone();
        cx.background_spawn(async move { std::fs::create_dir_all(&folder) })
    };
    cx.spawn(async move |cx| {
        let made = made.await;
        cx.update(|cx| match made {
            Ok(()) => cx.reveal_path(&folder),
            Err(error) => {
                if let Some(workspace) = workspace.and_then(|workspace| workspace.upgrade()) {
                    workspace.update(cx, |workspace, cx| {
                        workspace.show_toast(
                            ToastKind::Error,
                            format!(
                                "The queries folder could not be made at {}: {error}",
                                folder.display()
                            ),
                            cx,
                        )
                    });
                }
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::{KeyBinding, TestAppContext};
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};

    use super::*;

    /// A session on a server whose graph `graph` serves two stored queries, `people_titled` and
    /// `set_title`.
    fn session_of(graph: &str, cx: &mut TestAppContext) -> Entity<GraphSession> {
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
        cx.new(|cx| GraphSession::new(connection, graph.into(), "main".into(), cx))
    }

    fn session(cx: &mut TestAppContext) -> Entity<GraphSession> {
        session_of("knowledge", cx)
    }

    /// A queries folder that cannot be made (a file is where it goes) is said in a toast,
    /// where a reveal would open nothing.
    #[gpui::test]
    async fn a_queries_folder_that_cannot_be_made_is_said(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let dir = std::env::temp_dir().join(format!("queries-reveal-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let blocked = dir.join("a-file");
        std::fs::write(&blocked, b"").unwrap();
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let weak = workspace.downgrade();
        cx.update(|_, cx| reveal_folder(blocked.join("queries"), Some(weak), cx));
        cx.run_until_parked();
        let toasts = workspace.read_with(cx, |workspace, _| workspace.toast_messages());
        assert!(
            toasts
                .iter()
                .any(|toast| toast.contains("The queries folder could not be made")),
            "{toasts:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The panel takes the keyboard: ↓ and ↑ move among the rows that open a query, passing
    /// over the Stored header and stopping at either end, and ↵ opens the row selected as a
    /// click does, a saved file and a stored query alike. The selection stays on its query
    /// while the rows above it change. Nothing reads the catalog before the panel is drawn:
    /// drawing it asks for it.
    #[gpui::test]
    async fn the_rows_are_selected_and_opened_by_keys(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let session = session(cx);
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
            panel.files_of = panel.graph_key(cx);
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

        // Another graph shown: the last graph's files are neither shown nor opened while its
        // own folder is listed. Before, they stayed until the listing landed.
        let other = session_of("other", cx);
        panel.update(cx, |panel, cx| {
            panel.watch_session(Some(other), cx);
            panel.owe_folder(cx);
            let rows = panel.panel_rows(cx);
            assert!(
                !rows
                    .iter()
                    .any(|row| matches!(row, PanelRow::Saved { .. } | PanelRow::SavedEmpty)),
                "no saved rows before the folder of `other` is listed"
            );
        });
    }
}
