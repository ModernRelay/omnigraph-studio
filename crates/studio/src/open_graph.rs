//! "Open a graph" (spec 003): servers with their state, a server's graphs, a graph id field,
//! and a form to add a server by URL and token.

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, WeakEntity, Window,
};
use graph_store::{
    Connection, ConnectionEvent, ConnectionRegistry, ConnectionState, GraphListing, ServerSource,
};
use studio_ui::ds::{self, MenuRow, size, space};
use studio_workspace::ModalView;
use studio_workspace::{Workspace, connection_color};
use ui::Indicator;
use ui::prelude::*;

pub struct OpenGraphModal {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    selected: Option<Entity<Connection>>,
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
        // A reload of the operator's files can replace the selected server: the selection moves
        // to one the registry still holds.
        let mut subscriptions = vec![cx.observe(&registry, |this, registry, cx| {
            let connections = &registry.read(cx).connections;
            if this
                .selected
                .as_ref()
                .is_some_and(|selected| !connections.contains(selected))
            {
                this.selected = first_choice(connections, cx);
            }
            cx.notify();
        })];
        for connection in &connections {
            subscriptions
                .push(cx.subscribe(connection, |_, _, _: &ConnectionEvent, cx| cx.notify()));
        }
        let graph_id = input("Graph id, for example knowledge", window, cx);
        let new_name = input("Name, for example dev", window, cx);
        let new_url = input("http://127.0.0.1:8787", window, cx);
        let new_token = input("Token (stored in the keychain)", window, cx);
        new_token.update(cx, |editor, cx| editor.set_masked(true, cx));
        let replacement_token = input("New token", window, cx);
        replacement_token.update(cx, |editor, cx| editor.set_masked(true, cx));
        // An error is about what was typed: typing again clears it.
        for editor in [&graph_id, &new_url, &new_token, &replacement_token] {
            subscriptions.push(cx.subscribe_in(
                editor,
                window,
                |this, _, event: &EditorEvent, _, cx| {
                    if let EditorEvent::BufferEdited = event {
                        this.error = None;
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
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            selected,
            graph_id,
            adding: connections.is_empty(),
            new_name,
            new_url,
            new_token,
            replacement_token,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    /// Enter: adds the server being added, else saves a new token being typed, else opens the
    /// graph named.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.adding {
            self.add_server(window, cx);
        } else if self.replacement_token.focus_handle(cx).is_focused(window) {
            self.update_token(window, cx);
        } else {
            let graph_id = self.graph_id.read(cx).text(cx);
            self.open(graph_id, window, cx);
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

    fn select(&mut self, connection: Entity<Connection>, cx: &mut Context<Self>) {
        if matches!(
            connection.read(cx).state,
            ConnectionState::Idle | ConnectionState::Unreachable(_)
        ) {
            connection.update(cx, |c, cx| c.connect(cx));
        }
        self.selected = Some(connection);
        self.adding = false;
        self.error = None;
        cx.notify();
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
        let listed = matches!(
            &connection.read(cx).graphs,
            GraphListing::Listed(graphs) if graphs.iter().any(|g| g.graph_id == graph_id)
        );
        if !listed {
            connection.update(cx, |connection, _| connection.remember_graph(&graph_id));
            ConnectionRegistry::global(cx).read(cx).save(cx);
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
                self.select(connection, cx);
                self.error = Some(format!("{name} has that address already.").into());
                cx.notify();
            }
            Ok(connection) => {
                self._subscriptions
                    .push(cx.subscribe(&connection, |_, _, _: &ConnectionEvent, cx| cx.notify()));
                connection.update(cx, |c, cx| c.connect(cx));
                self.select(connection, cx);
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
        let connections = ConnectionRegistry::global(cx).read(cx).connections.clone();
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
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select(connection.clone(), cx)
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
        let status = match &c.state {
            ConnectionState::Unreachable(error) => format!("Could not reach the server: {error}"),
            ConnectionState::Unauthorized => "The server refused the token.".into(),
            ConnectionState::TokenUnreadable(reason) => {
                format!("Studio could not read the token from the keychain: {reason}")
            }
            state => state.label(),
        };
        let state = c.state.clone();
        // A token typed in replaces one the server refused or the keychain would not give.
        let asks_token = matches!(
            state,
            ConnectionState::Unauthorized | ConnectionState::TokenUnreadable(_)
        );
        let unsaved = c.token_unsaved.clone();
        let cli = c.server.source == ServerSource::Cli;
        let selected = connection.clone();
        let listing_note = match &c.graphs {
            GraphListing::NotAllowed => Some("This token may not list graphs. Enter a graph id."),
            GraphListing::Failed(_) => Some("The graph list could not be read. Enter a graph id."),
            _ => None,
        };
        let graphs = c.graph_ids();
        let infos: Vec<_> = graphs
            .iter()
            .map(|id| {
                c.graph_info(id)
                    .map(|info| (info.state, info.write_available))
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
                                .tooltip(ui::Tooltip::text(if cli {
                                    "Used until Studio quits. Run `omnigraph login` to keep it \
                                     for the CLI and Studio."
                                } else {
                                    "Stored in the keychain and used from now on."
                                }))
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
                            Some((state, writable)) => format!(
                                "{}{}",
                                format!("{state:?}").to_lowercase(),
                                if writable { "" } else { " · read only" }
                            ),
                            None => "remembered".into(),
                        };
                        let id = graph.clone();
                        MenuRow::new(("graph", index))
                            .height(size::ROW)
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
