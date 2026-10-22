//! "Open a graph" (spec 003): servers with their state, a server's graphs, a graph id field,
//! and a form to add a server by URL and token. ↑, ↓ and Tab (Zed's select actions) move
//! through the servers, the chosen one's graphs and the id field; ↵ runs the row.

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, WeakEntity, Window,
};
use graph_store::{
    Connection, ConnectionEvent, ConnectionRegistry, ConnectionState, ConnectionWatch,
    GraphListing, ServerSource,
};
use studio_ui::ds::{self, MenuRow, size, space};
use studio_workspace::ModalView;
use studio_workspace::{Workspace, connection_color};
use ui::Indicator;
use ui::prelude::*;

/// Where the keyboard is in the dialog: a server, one of the chosen server's graphs, or a field.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Place {
    Server(EntityId),
    Graph(String),
    /// The field for a new token, shown when the server refused the token.
    Token,
    /// The graph id field.
    GraphId,
}

pub struct OpenGraphModal {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    /// The servers listed, as the registry last held them.
    connections: Vec<Entity<Connection>>,
    /// A subscription to each listed server's state, by the server's entity, and a watch that
    /// keeps its health checked while the dialog shows it ([`Connection::watch`]).
    followed: Vec<(EntityId, Subscription, ConnectionWatch)>,
    selected: Option<Entity<Connection>>,
    /// The row or field ↑, ↓ and Tab moved to, which ↵ runs.
    cursor: Option<Place>,
    graph_id: Entity<Editor>,
    adding: bool,
    new_name: Entity<Editor>,
    new_url: Entity<Editor>,
    new_token: Entity<Editor>,
    /// A replacement for a token the selected server refused.
    replacement_token: Entity<Editor>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for OpenGraphModal {}

impl ModalView for OpenGraphModal {}

impl Focusable for OpenGraphModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn input(
    placeholder: &str,
    window: &mut Window,
    cx: &mut Context<OpenGraphModal>,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor
    })
}

impl OpenGraphModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let registry = ConnectionRegistry::global(cx);
        let connections = registry.read(cx).connections.clone();
        // A reload of the operator's files can add, drop or replace servers: the dialog lists
        // and follows what the registry holds now.
        let observe = cx.observe_in(&registry, window, |this, registry, window, cx| {
            this.follow(registry.read(cx).connections.clone(), window, cx)
        });
        Self::build(workspace, connections, vec![observe], window, cx)
    }

    /// The dialog over `connections`, kept in line with the registry by `subscriptions`.
    fn build(
        workspace: WeakEntity<Workspace>,
        connections: Vec<Entity<Connection>>,
        mut subscriptions: Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let graph_id = input("Graph id, for example knowledge", window, cx);
        let new_name = input("Name, for example dev", window, cx);
        let new_url = input("http://127.0.0.1:8787", window, cx);
        let new_token = input("Token (stored in the keychain)", window, cx);
        new_token.update(cx, |editor, cx| editor.set_masked(true, cx));
        let replacement_token = input("New token", window, cx);
        replacement_token.update(cx, |editor, cx| editor.set_masked(true, cx));
        // An error is about what was typed: typing again clears it. Typing in a field puts the
        // keyboard there, so ↵ uses what was typed. A field the dialog clears while the
        // keyboard is elsewhere was not typed in.
        for (editor, place) in [
            (&graph_id, Some(Place::GraphId)),
            (&new_url, None),
            (&new_token, None),
            (&replacement_token, Some(Place::Token)),
        ] {
            subscriptions.push(cx.subscribe_in(
                editor,
                window,
                move |this, editor, event: &EditorEvent, window, cx| {
                    if let EditorEvent::BufferEdited = event
                        && editor.focus_handle(cx).is_focused(window)
                    {
                        this.error = None;
                        if place.is_some() {
                            this.cursor = place.clone();
                        }
                        cx.notify();
                    }
                },
            ));
        }
        let selected = first_choice(&connections, cx);
        // A server the dialog shows first is connected, as one chosen in it is.
        if let Some(connection) = &selected {
            connection.update(cx, |c, cx| c.connect_if_idle(cx));
        }
        let mut this = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            connections: Vec::new(),
            followed: Vec::new(),
            cursor: selected
                .as_ref()
                .map(|connection| Place::Server(connection.entity_id())),
            selected,
            graph_id,
            adding: connections.is_empty(),
            new_name,
            new_url,
            new_token,
            replacement_token,
            error: None,
            _subscriptions: subscriptions,
        };
        this.follow(connections, window, cx);
        this
    }

    /// Lists `connections`, the registry's servers now. Each one's state is followed, its health
    /// checked while the dialog shows it; one the registry dropped or replaced (a reload of the
    /// operator's files makes a new connection for a changed address) is not any more. A
    /// selection the registry no longer holds moves to one it does.
    fn follow(
        &mut self,
        connections: Vec<Entity<Connection>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let listed = |id: EntityId| connections.iter().any(|c| c.entity_id() == id);
        self.followed.retain(|(id, ..)| listed(*id));
        for connection in &connections {
            let id = connection.entity_id();
            if !self.followed.iter().any(|(followed, ..)| *followed == id) {
                let subscription =
                    cx.subscribe(connection, |_, _, _: &ConnectionEvent, cx| cx.notify());
                let watch = connection.update(cx, |connection, cx| connection.watch(cx));
                self.followed.push((id, subscription, watch));
            }
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| !connections.contains(selected))
        {
            self.choose(first_choice(&connections, cx), window, cx);
            if let Some(connection) = &self.selected {
                connection.update(cx, |c, cx| c.connect_if_idle(cx));
            }
            if matches!(self.cursor, Some(Place::Server(_))) {
                self.cursor = self.selected.as_ref().map(|c| Place::Server(c.entity_id()));
            }
        }
        self.connections = connections;
        cx.notify();
    }

    /// Where the keyboard can go, in order: the servers, the field for a new token when the
    /// server asks for one, the chosen server's graphs, then the graph id field. None while a
    /// server is added.
    fn places(&self, cx: &App) -> Vec<Place> {
        if self.adding {
            return Vec::new();
        }
        let mut places: Vec<Place> = self
            .connections
            .iter()
            .map(|connection| Place::Server(connection.entity_id()))
            .collect();
        if let Some(selected) = &self.selected {
            let connection = selected.read(cx);
            if connection.state == ConnectionState::Unauthorized {
                places.push(Place::Token);
            }
            places.extend(connection.graph_ids().into_iter().map(Place::Graph));
            places.push(Place::GraphId);
        }
        places
    }

    /// ↓ or Tab (`by` 1), ↑ or ⇧Tab (-1), wrapping around. While a server is added, through
    /// that form's fields instead.
    fn step(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.adding {
            let fields = [&self.new_url, &self.new_name, &self.new_token];
            let at = fields
                .iter()
                .position(|field| field.focus_handle(cx).is_focused(window));
            let next = match at {
                Some(at) => (at as isize + by).rem_euclid(fields.len() as isize) as usize,
                None => 0,
            };
            window.focus(&fields[next].focus_handle(cx), cx);
            return;
        }
        let places = self.places(cx);
        if places.is_empty() {
            return;
        }
        let at = self
            .cursor
            .as_ref()
            .and_then(|cursor| places.iter().position(|place| place == cursor));
        let next = match at {
            Some(at) => (at as isize + by).rem_euclid(places.len() as isize) as usize,
            None if by > 0 => 0,
            None => places.len() - 1,
        };
        self.move_to(places[next].clone(), window, cx);
    }

    /// Puts the keyboard at `place`. Moving to a server chooses it, which shows its graphs and
    /// connects it if it never was; one that could not be reached is tried again with ↵, not
    /// at every key that passes it.
    fn move_to(&mut self, place: Place, window: &mut Window, cx: &mut Context<Self>) {
        match &place {
            Place::Server(id) => {
                if let Some(connection) = self
                    .connections
                    .iter()
                    .find(|connection| connection.entity_id() == *id)
                    .cloned()
                {
                    connection.update(cx, |c, cx| c.connect_if_idle(cx));
                    self.choose(Some(connection), window, cx);
                    self.error = None;
                }
                window.focus(&self.focus_handle, cx);
            }
            Place::Graph(_) => window.focus(&self.focus_handle, cx),
            Place::Token => window.focus(&self.replacement_token.focus_handle(cx), cx),
            Place::GraphId => window.focus(&self.graph_id.focus_handle(cx), cx),
        }
        self.cursor = Some(place);
        cx.notify();
    }

    /// ↵ on a server: connects it unless it is connected, then goes to its first graph, or to
    /// the id field when it lists none.
    fn enter_server(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self
            .connections
            .iter()
            .find(|connection| connection.entity_id() == id)
            .cloned()
        else {
            return;
        };
        self.select(connection.clone(), window, cx);
        let c = connection.read(cx);
        let next = match c.graph_ids().into_iter().next() {
            Some(graph) => Some(Place::Graph(graph)),
            None if c.state == ConnectionState::Ready => Some(Place::GraphId),
            // Its graphs are not known yet: the keyboard stays on it.
            None => None,
        };
        match next {
            Some(place) => self.move_to(place, window, cx),
            None => self.cursor = Some(Place::Server(id)),
        }
    }

    /// Enter: adds the server being added, else saves a new token being typed, else opens the
    /// graph whose id is typed; with the keyboard on a row, chooses that server or opens that
    /// graph.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.adding {
            self.add_server(window, cx);
        } else if self.replacement_token.focus_handle(cx).is_focused(window) {
            self.update_token(window, cx);
        } else {
            match self.cursor.clone() {
                Some(Place::Server(id)) if !self.graph_id.focus_handle(cx).is_focused(window) => {
                    self.enter_server(id, window, cx)
                }
                Some(Place::Graph(id)) if !self.graph_id.focus_handle(cx).is_focused(window) => {
                    self.open(id, window, cx)
                }
                _ => {
                    let graph_id = self.graph_id.read(cx).text(cx);
                    self.open(graph_id, window, cx);
                }
            }
        }
    }

    fn show_add(&mut self, adding: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.adding = adding;
        self.error = None;
        if adding {
            window.focus(&self.new_url.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn select(
        &mut self,
        connection: Entity<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            connection.read(cx).state,
            ConnectionState::Idle | ConnectionState::Unreachable(_)
        ) {
            connection.update(cx, |c, cx| c.connect(cx));
        }
        self.cursor = Some(Place::Server(connection.entity_id()));
        self.choose(Some(connection), window, cx);
        self.adding = false;
        self.error = None;
        cx.notify();
    }

    /// Makes `connection` the server chosen, by a click, the keyboard or the registry dropping
    /// the one chosen. A new token typed for another server is that server's alone: the field
    /// and its error are cleared, so Update token never sends it to this server's address nor
    /// saves it under this server's keychain entry.
    fn choose(
        &mut self,
        connection: Option<Entity<Connection>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected == connection {
            return;
        }
        self.selected = connection;
        self.error = None;
        let field = self.replacement_token.clone();
        if field.read(cx).text(cx).is_empty() {
            return;
        }
        // The keyboard leaves the field it was in, which shows another server's token now.
        if field.focus_handle(cx).is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        field.update(cx, |editor, cx| editor.set_text("", window, cx));
    }

    fn open(&mut self, graph_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.selected.clone() else {
            return;
        };
        if graph_id.trim().is_empty() {
            self.error = Some("Enter a graph id.".into());
            cx.notify();
            return;
        }
        let graph_id = graph_id.trim().to_owned();
        // A graph id typed in is kept with its server, for a token that cannot list graphs. One
        // the server lists needs no keeping, and kept would be offered after its deletion.
        let listed = match &connection.read(cx).graphs {
            GraphListing::Listed(graphs) => graphs.iter().any(|g| g.graph_id == graph_id),
            GraphListing::Discovered(ids) => ids.contains(&graph_id),
            _ => false,
        };
        if !listed {
            connection.update(cx, |connection, _| connection.remember_graph(&graph_id));
            if let Some(registry) = ConnectionRegistry::try_global(cx) {
                registry.read(cx).save(cx);
            }
        }
        if let Some(workspace) = self.workspace.upgrade() {
            // A graph the window has open is shown as it is, on its own branch.
            let url = &connection.read(cx).server.url;
            let open = workspace
                .read(cx)
                .open_session_for(url, &graph_id, cx)
                .map(|session| session.entity_id());
            // One another window has open is shown there: a graph is open in one window at a
            // time, since its tabs and drafts are saved by graph.
            let elsewhere = open
                .is_none()
                .then(|| crate::window_holding(&connection, &graph_id, cx))
                .flatten();
            match (open, elsewhere) {
                (None, Some((handle, session))) => cx.defer(move |cx| {
                    handle
                        .update(cx, |workspace, window, cx| {
                            workspace.show_graph(session.entity_id(), window, cx);
                            window.activate_window();
                        })
                        .ok();
                }),
                // Open here already, shown on its own branch; else opened here on main.
                _ => crate::show_graph_in(&workspace, connection, graph_id, None, window, cx),
            }
        }
        cx.emit(DismissEvent);
    }

    fn add_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.new_name.read(cx).text(cx);
        let url = self.new_url.read(cx).text(cx);
        let token = self.new_token.read(cx).text(cx);
        let name = if name.trim().is_empty() {
            url.clone()
        } else {
            name.trim().to_owned()
        };
        let token = (!token.trim().is_empty()).then(|| token.trim().to_owned());
        let registry = ConnectionRegistry::global(cx);
        let listed: Vec<_> = registry
            .read(cx)
            .connections
            .iter()
            .map(|connection| connection.entity_id())
            .collect();
        let result = registry.update(cx, |registry, cx| {
            registry.add_server(name, url, token, Vec::new(), cx)
        });
        match result {
            // An address listed already is that server, which a new token reconnects itself:
            // it is selected, followed and connected once, as it was.
            Ok(connection) if listed.contains(&connection.entity_id()) => {
                let name = connection.read(cx).server.name.clone();
                self.select(connection, window, cx);
                self.error = Some(format!("{name} has that address already.").into());
                cx.notify();
            }
            Ok(connection) => {
                // Followed now, before the registry's notice reaches the dialog.
                self.follow(registry.read(cx).connections.clone(), window, cx);
                connection.update(cx, |c, cx| c.connect(cx));
                self.select(connection, window, cx);
                // A second server is typed fresh, not added again from the first one's fields.
                for editor in [&self.new_name, &self.new_url, &self.new_token] {
                    editor.update(cx, |editor, cx| editor.set_text("", window, cx));
                }
                window.focus(&self.graph_id.focus_handle(cx), cx);
            }
            Err(error) => {
                self.error = Some(format!("That address is not valid: {error}").into());
                cx.notify();
            }
        }
    }

    fn update_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.selected.clone() else {
            return;
        };
        let token = self.replacement_token.read(cx).text(cx);
        if token.trim().is_empty() {
            self.error = Some("Enter the new token.".into());
            cx.notify();
            return;
        }
        let registry = ConnectionRegistry::global(cx);
        registry.update(cx, |registry, cx| {
            registry.update_token(&connection, token, cx)
        });
        self.error = None;
        self.replacement_token
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        cx.notify();
    }

    fn render_servers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let connections = self.connections.clone();
        // A third of the wide modal; the graphs take the rest.
        v_flex()
            .w(px(size::MODAL_WIDE / 3.))
            .flex_none()
            .gap(px(space::XXS))
            .child(pane_label("Servers"))
            .children(
                connections
                    .into_iter()
                    .enumerate()
                    .map(|(index, connection)| {
                        let c = connection.read(cx);
                        let color = connection_color(&c.state);
                        let selected = self.selected.as_ref() == Some(&connection);
                        let source = match c.server.source {
                            ServerSource::Cli => "CLI config",
                            ServerSource::Studio => "added in Studio",
                            ServerSource::Environment => "environment",
                        };
                        MenuRow::new(("server", index))
                            .height(size::FEED_ROW)
                            .toggle_state(selected)
                            .start_slot(Indicator::dot().color(color))
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(studio_ui::ds::text::body(c.server.name.clone()))
                                    .child(
                                        studio_ui::ds::text::meta(format!(
                                            "{} · {source}",
                                            c.server.url
                                        ))
                                        .truncate(),
                                    ),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select(connection.clone(), window, cx)
                            }))
                    }),
            )
            .child(
                studio_ui::ds::button("add-server", "Add server…")
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(IconName::Plus))
                    .on_click(cx.listener(|this, _, window, cx| this.show_add(true, window, cx))),
            )
    }

    fn render_graphs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(connection) = &self.selected else {
            return v_flex().child(studio_ui::ds::text::state("Add a server to start."));
        };
        let c = connection.read(cx);
        let status = server_status(c);
        let state = c.state.clone();
        // A token typed in replaces one the server refused, or one the keychain would not give
        // to a server that asks for one.
        let asks_token = state == ConnectionState::Unauthorized;
        // Why the keychain gave no token, under a status that does not say it already.
        let unread = c
            .token_unread
            .clone()
            .filter(|_| state != ConnectionState::Unauthorized);
        let unsaved = c.token_unsaved.clone();
        let token_tooltip = token_tooltip(c);
        let selected = connection.clone();
        let listing_note = match &c.graphs {
            GraphListing::NotAllowed => Some("This token may not list graphs. Enter a graph id."),
            GraphListing::Failed(_) => Some("The graph list could not be read. Enter a graph id."),
            _ => None,
        };
        let graphs = c.graph_ids();
        // What each id's row says: a listed graph's state, nothing more for one the server
        // named by discovery, and "remembered" for one only typed in before.
        let discovered =
            |id: &String| matches!(&c.graphs, GraphListing::Discovered(ids) if ids.contains(id));
        let infos: Vec<_> = graphs
            .iter()
            .map(|id| match c.graph_info(id) {
                Some(info) => Some(Some((info.state, info.write_available))),
                None if discovered(id) => Some(None),
                None => None,
            })
            .collect();
        v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(space::XXS))
            .child(
                h_flex()
                    .min_h(px(size::CONTROL))
                    .px(px(space::MD))
                    .gap(px(space::MD))
                    .child(Indicator::dot().color(connection_color(&state)))
                    .child(studio_ui::ds::text::muted(status))
                    .child(div().flex_1())
                    .map(|row| match &state {
                        ConnectionState::Connecting => row.child(
                            studio_ui::ds::button("cancel-connect", "Cancel")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    selected.update(cx, |c, cx| c.cancel(cx));
                                })),
                        ),
                        ConnectionState::Unreachable(_) | ConnectionState::Idle => row.child(
                            studio_ui::ds::button("retry-connect", "Retry")
                                .style(ButtonStyle::Subtle)
                                .start_icon(Icon::new(IconName::RotateCw))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    selected.update(cx, |c, cx| c.connect(cx));
                                })),
                        ),
                        _ => row,
                    }),
            )
            .children(unread.map(|reason| {
                div()
                    .px(px(space::MD))
                    .child(studio_ui::ds::text::muted(format!(
                        "Studio could not read the token from the keychain: {reason}. It \
                         connects without one."
                    )))
            }))
            .children(unsaved.map(|reason| {
                div()
                    .px(px(space::MD))
                    .child(studio_ui::ds::text::muted(format!(
                        "The token could not be saved to the keychain: {reason}. It lasts until \
                     Studio quits."
                    )))
            }))
            .when(asks_token, |pane| {
                pane.child(
                    h_flex()
                        .py(px(space::XS))
                        .gap(px(space::MD))
                        .child(
                            studio_ui::ds::field_frame(false, cx)
                                .flex_1()
                                .child(studio_ui::ds::field_input(self.replacement_token.clone())),
                        )
                        .child(
                            studio_ui::ds::button("update-token", "Update token")
                                .style(ButtonStyle::Filled)
                                .tooltip(ui::Tooltip::text(token_tooltip))
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.update_token(window, cx)
                                    }),
                                ),
                        ),
                )
            })
            .child(pane_label("Graphs"))
            .children(
                listing_note.map(|n| div().px(px(space::MD)).child(studio_ui::ds::text::muted(n))),
            )
            .children(
                graphs
                    .into_iter()
                    .zip(infos)
                    .enumerate()
                    .map(|(index, (graph, info))| {
                        let detail = match info {
                            Some(Some((state, writable))) => format!(
                                "{}{}",
                                format!("{state:?}").to_lowercase(),
                                if writable { "" } else { " · read only" }
                            ),
                            Some(None) => String::new(),
                            None => "remembered".into(),
                        };
                        let id = graph.clone();
                        let at_cursor = self.cursor.as_ref() == Some(&Place::Graph(graph.clone()));
                        MenuRow::new(("graph", index))
                            .height(size::ROW)
                            .toggle_state(at_cursor)
                            .start_slot(studio_ui::ds::icon::lead(IconName::Folder))
                            .child(
                                h_flex()
                                    .gap(px(space::MD))
                                    .child(studio_ui::ds::text::body(graph))
                                    .child(studio_ui::ds::text::meta(detail)),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open(id.clone(), window, cx)
                            }))
                    }),
            )
            .child(
                h_flex()
                    .pt(px(space::SM))
                    .gap(px(space::MD))
                    .child(
                        studio_ui::ds::field_frame(false, cx)
                            .flex_1()
                            .child(studio_ui::ds::field_input(self.graph_id.clone())),
                    )
                    .child(
                        studio_ui::ds::button("open", "Open")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let id = this.graph_id.read(cx).text(cx);
                                this.open(id, window, cx);
                            })),
                    ),
            )
    }

    fn render_add(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let field = |label: &'static str, editor: &Entity<Editor>| {
            v_flex()
                .gap(px(space::XS))
                .child(studio_ui::ds::text::meta(label))
                .child(
                    studio_ui::ds::field_frame(false, cx)
                        .child(studio_ui::ds::field_input(editor.clone())),
                )
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(space::LG))
            .child(studio_ui::ds::text::body("Add a server"))
            .child(field("Address", &self.new_url))
            .child(field("Name", &self.new_name))
            .child(field("Bearer token", &self.new_token))
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .justify_end()
                    .child(ds::cancel_button("cancel-add").on_click(
                        cx.listener(|this, _, window, cx| this.show_add(false, window, cx)),
                    ))
                    .child(
                        ds::action_button("add", "Add and connect").on_click(
                            cx.listener(|this, _, window, cx| this.add_server(window, cx)),
                        ),
                    ),
            )
    }
}

/// What the chosen server's line says of its state. A 401 says what it was an answer to: a
/// token the keychain would not give, no token at all (a server Studio has none configured
/// for, such as a managed CLI target, asks for one), or the token sent.
fn server_status(c: &Connection) -> String {
    match (&c.state, &c.token_unread) {
        (ConnectionState::Unreachable(error), _) => {
            format!("Could not reach the server: {error}")
        }
        (ConnectionState::Unauthorized, Some(reason)) => format!(
            "The server asks for a token, and Studio could not read it from the keychain: \
             {reason}"
        ),
        (ConnectionState::Unauthorized, None) if !c.refused_with_token => {
            "This server asks for a token; none is configured for it.".into()
        }
        (ConnectionState::Unauthorized, None) => "The server refused the token.".into(),
        (state, _) => state.label(),
    }
}

/// What Update token does with the token typed: a server added in Studio keeps it in the
/// keychain; a CLI server's lasts this run, and the CLI's own command keeps one for both, as
/// one from the environment lasts this run.
fn token_tooltip(c: &Connection) -> String {
    match c.server.source {
        ServerSource::Studio => "Stored in the keychain and used from now on.".into(),
        ServerSource::Cli => format!(
            "Used until Studio quits. To keep one for the CLI and Studio, pipe it to `omnigraph \
             credentials set --target {} --stdin`.",
            c.server.name
        ),
        ServerSource::Environment => "Used until Studio quits.".into(),
    }
}

/// A pane's name, over its rows and on their text's edge.
fn pane_label(title: &'static str) -> Div {
    div()
        .px(px(space::MD))
        .pb(px(space::XS))
        .child(ds::text::meta(title))
}

/// The server a dialog starts on: the first ready one, else the first.
fn first_choice(connections: &[Entity<Connection>], cx: &App) -> Option<Entity<Connection>> {
    connections
        .iter()
        .find(|c| c.read(cx).state == ConnectionState::Ready)
        .or_else(|| connections.first())
        .cloned()
}

impl Render for OpenGraphModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let servers = self.render_servers(cx).into_any_element();
        let body = if self.adding {
            self.render_add(cx).into_any_element()
        } else {
            self.render_graphs(cx).into_any_element()
        };
        ds::modal(cx)
            .key_context("OpenGraphModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(window, cx)))
            // Zed's keymap sends these from ↑, ↓, Tab and ⇧Tab, and from a single-line field,
            // which passes them on.
            .on_action(
                cx.listener(|this, _: &menu::SelectNext, window, cx| this.step(1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &menu::SelectPrevious, window, cx| this.step(-1, window, cx)),
            )
            .on_action(cx.listener(|this, _: &menu::SelectFirst, window, cx| {
                if let Some(first) = this.places(cx).first().cloned() {
                    this.move_to(first, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &menu::SelectLast, window, cx| {
                if let Some(last) = this.places(cx).last().cloned() {
                    this.move_to(last, window, cx);
                }
            }))
            .w(px(size::MODAL_WIDE))
            .max_h(px(size::MODAL_MAX_HEIGHT))
            .child(
                ds::ModalHeader::new("Open a graph")
                    .description(SharedString::from(
                        "Pick a server, then one of its graphs, or enter a graph's id.",
                    ))
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                ds::modal_body()
                    .child(
                        h_flex()
                            .items_start()
                            .gap(px(space::XL))
                            .child(servers)
                            .child(
                                div()
                                    .w(px(size::HAIRLINE))
                                    .self_stretch()
                                    .bg(ds::surface::floating_border(cx)),
                            )
                            .child(body),
                    )
                    .children(self.error.clone().map(studio_ui::ds::text::error)),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{TestAppContext, VisualTestContext};

    use super::*;

    /// A server with `graphs` remembered. It waits for a token, so connecting it sends nothing.
    fn server(graphs: &[&str], cx: &mut TestAppContext) -> Entity<Connection> {
        let (session, _) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n}\n",
            |_| serde_json::json!([]),
            cx,
        );
        let connection = session.read_with(cx, |session, _| session.connection.clone());
        connection.update(cx, |connection, _| {
            connection.await_token();
            for graph in graphs {
                connection.remember_graph(graph);
            }
        });
        connection
    }

    fn dialog(
        connections: Vec<Entity<Connection>>,
        cx: &mut TestAppContext,
    ) -> (Entity<OpenGraphModal>, &mut VisualTestContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        // The registry is Studio's, which a test has none of: the dialog is told its servers.
        let (modal, cx) = cx.add_window_view(|window, cx| {
            OpenGraphModal::build(
                WeakEntity::new_invalid(),
                connections,
                Vec::new(),
                window,
                cx,
            )
        });
        cx.update(|window, cx| window.focus(&modal.focus_handle(cx), cx));
        (modal, cx)
    }

    /// ↓ and Tab move through the servers, choosing each, then the chosen one's graphs and the
    /// id field, wrapping around; ↵ on a server goes to its graphs, and on a graph opens it.
    #[gpui::test]
    fn the_keyboard_chooses_a_server_and_opens_its_graph(cx: &mut TestAppContext) {
        let alpha = server(&["one"], cx);
        let beta = server(&["two", "three"], cx);
        let (modal, cx) = dialog(vec![alpha.clone(), beta.clone()], cx);
        let at = |cx: &mut VisualTestContext| {
            modal.read_with(cx, |modal, _| {
                (modal.cursor.clone(), modal.selected.clone())
            })
        };
        let on = |connection: &Entity<Connection>| Some(Place::Server(connection.entity_id()));
        assert_eq!(at(cx), (on(&alpha), Some(alpha.clone())));

        cx.dispatch_action(menu::SelectNext);
        assert_eq!(
            at(cx),
            (on(&beta), Some(beta.clone())),
            "↓ chooses the next server"
        );
        cx.dispatch_action(menu::SelectNext);
        assert_eq!(at(cx).0, Some(Place::Graph("two".into())));
        cx.dispatch_action(menu::SelectPrevious);
        cx.dispatch_action(menu::SelectPrevious);
        assert_eq!(at(cx), (on(&alpha), Some(alpha.clone())));
        cx.dispatch_action(menu::Confirm);
        assert_eq!(
            at(cx).0,
            Some(Place::Graph("one".into())),
            "↵ goes to its graphs"
        );
        cx.dispatch_action(menu::SelectNext);
        assert_eq!(at(cx).0, Some(Place::GraphId));
        let field = modal.read_with(cx, |modal, _| modal.graph_id.clone());
        assert!(cx.update(|window, cx| field.focus_handle(cx).is_focused(window)));
        cx.dispatch_action(menu::SelectNext);
        assert_eq!(at(cx).0, on(&alpha), "the last place wraps to the first");

        let dismissed = Rc::new(Cell::new(false));
        let _dismissed = cx.update(|_, cx| {
            let dismissed = dismissed.clone();
            cx.subscribe(&modal, move |_, _: &DismissEvent, _| dismissed.set(true))
        });
        cx.dispatch_action(menu::SelectLast);
        cx.dispatch_action(menu::SelectPrevious);
        assert_eq!(at(cx).0, Some(Place::Graph("one".into())));
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert!(dismissed.get(), "↵ on a graph opens it");
    }

    /// A new token typed for one refused server is gone once another is chosen, by a click, by
    /// the keyboard or by the registry dropping the first: Update token then asks for one
    /// rather than sending the first server's to the second's address.
    #[gpui::test]
    fn a_token_typed_for_one_server_is_cleared_when_another_is_chosen(cx: &mut TestAppContext) {
        let alpha = server(&["one"], cx);
        let beta = server(&["two"], cx);
        for connection in [&alpha, &beta] {
            connection.update(cx, |connection, _| {
                connection.state = ConnectionState::Unauthorized
            });
        }
        let (modal, cx) = dialog(vec![alpha.clone(), beta.clone()], cx);
        let field = modal.read_with(cx, |modal, _| modal.replacement_token.clone());
        let typed = |cx: &mut VisualTestContext| field.read_with(cx, |field, cx| field.text(cx));
        let type_token = |token: &str, cx: &mut VisualTestContext| {
            modal.update_in(cx, |modal, window, cx| {
                modal.move_to(Place::Token, window, cx)
            });
            cx.simulate_input(token);
            assert_eq!(typed(cx), token);
        };
        let selected =
            |cx: &mut VisualTestContext| modal.read_with(cx, |modal, _| modal.selected.clone());

        // Typed for alpha, then beta clicked.
        type_token("alpha-secret", cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.select(beta.clone(), window, cx)
        });
        cx.run_until_parked();
        assert_eq!(selected(cx), Some(beta.clone()));
        assert_eq!(typed(cx), "", "the click clears it");
        modal.update_in(cx, |modal, window, cx| modal.update_token(window, cx));
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.error.clone()),
            Some("Enter the new token.".into()),
            "nothing is sent"
        );
        assert!(!cx.update(|window, cx| field.focus_handle(cx).is_focused(window)));

        // Typed for beta, then ↑ to alpha.
        type_token("beta-secret", cx);
        cx.dispatch_action(menu::SelectPrevious);
        cx.dispatch_action(menu::SelectPrevious);
        cx.run_until_parked();
        assert_eq!(selected(cx), Some(alpha.clone()));
        assert_eq!(typed(cx), "", "the keyboard clears it");
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.cursor.clone()),
            Some(Place::Server(alpha.entity_id())),
            "and stays on the server"
        );

        // Typed for alpha, which the registry then drops.
        type_token("alpha-again", cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.follow(vec![beta.clone()], window, cx)
        });
        cx.run_until_parked();
        assert_eq!(selected(cx), Some(beta.clone()));
        assert_eq!(typed(cx), "", "a server dropped takes its token along");

        // The same server chosen again keeps what was typed for it.
        type_token("beta-kept", cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.select(beta.clone(), window, cx)
        });
        assert_eq!(typed(cx), "beta-kept");
    }

    /// A 401 to a request that carried no token says the server asks for one, not that it
    /// refused one; Update token names no command the CLI lacks, and keeps a Studio server's
    /// token in the keychain.
    #[gpui::test]
    fn a_server_sent_no_token_is_said_to_ask_for_one(cx: &mut TestAppContext) {
        let connection = server(&[], cx);
        let said = |refused_with_token: bool, cx: &mut TestAppContext| {
            connection.update(cx, |connection, _| {
                connection.state = ConnectionState::Unauthorized;
                connection.refused_with_token = refused_with_token;
                (server_status(connection), connection.label())
            })
        };
        assert_eq!(
            said(false, cx),
            (
                "This server asks for a token; none is configured for it.".to_owned(),
                "Asks for a token".to_owned()
            )
        );
        assert_eq!(
            said(true, cx),
            (
                "The server refused the token.".to_owned(),
                "Token refused".to_owned()
            )
        );
        let tooltip = |source: ServerSource, cx: &mut TestAppContext| {
            connection.update(cx, |connection, _| {
                connection.server.source = source;
                connection.server.name = "cloud".into();
                token_tooltip(connection)
            })
        };
        let cli = tooltip(ServerSource::Cli, cx);
        assert!(
            cli.contains("`omnigraph credentials set --target cloud --stdin`"),
            "{cli}"
        );
        assert!(!cli.contains("login"), "{cli}");
        assert_eq!(
            tooltip(ServerSource::Studio, cx),
            "Stored in the keychain and used from now on."
        );
    }

    /// A reload that replaces a server (a new address in the CLI config) is followed: the new
    /// connection's state changes reach the dialog, the old one's no longer do, and the choice
    /// moves off the server that left.
    #[gpui::test]
    fn a_server_the_registry_replaces_is_followed(cx: &mut TestAppContext) {
        let alpha = server(&["one"], cx);
        let beta = server(&["two"], cx);
        let replacement = server(&["two"], cx);
        let (modal, cx) = dialog(vec![alpha.clone(), beta.clone()], cx);
        cx.dispatch_action(menu::SelectNext);
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.selected.clone()),
            Some(beta.clone())
        );
        let drawn = Rc::new(Cell::new(0));
        let _drawn = cx.update(|_, cx| {
            let drawn = drawn.clone();
            cx.observe(&modal, move |_, _| drawn.set(drawn.get() + 1))
        });
        modal.update_in(cx, |modal, window, cx| {
            modal.follow(vec![alpha.clone(), replacement.clone()], window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.selected.clone()),
            Some(alpha.clone())
        );
        let before = drawn.get();
        beta.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        assert_eq!(drawn.get(), before, "the server replaced is not followed");
        replacement.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        assert_eq!(drawn.get(), before + 1, "its replacement is");
    }
}
