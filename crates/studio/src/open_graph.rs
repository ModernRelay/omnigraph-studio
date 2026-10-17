//! "Open a graph" (spec 003): servers with their state, a server's graphs, a graph id field,
//! and a form to add a server by URL and token.

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, WeakEntity, Window,
};
use graph_store::{
    Connection, ConnectionEvent, ConnectionRegistry, ConnectionState, GraphListing, ServerSource,
    open_session,
};
use studio_workspace::ModalView;
use studio_workspace::Workspace;
use ui::prelude::*;
use ui::{Indicator, ListItem, ListItemSpacing};

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
        let mut subscriptions = vec![cx.observe(&registry, |_, _, cx| cx.notify())];
        for connection in &connections {
            subscriptions
                .push(cx.subscribe(connection, |_, _, _: &ConnectionEvent, cx| cx.notify()));
        }
        let graph_id = input("Graph id, for example knowledge", window, cx);
        subscriptions.push(cx.subscribe_in(
            &graph_id,
            window,
            |this, _, event: &EditorEvent, _, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.error = None;
                    cx.notify();
                }
            },
        ));
        let new_token = input("Token (stored in the keychain)", window, cx);
        new_token.update(cx, |editor, cx| editor.set_masked(true, cx));
        let replacement_token = input("New token", window, cx);
        replacement_token.update(cx, |editor, cx| editor.set_masked(true, cx));
        let selected = connections
            .iter()
            .find(|c| c.read(cx).state == ConnectionState::Ready)
            .or_else(|| connections.first())
            .cloned();
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            selected,
            graph_id,
            adding: connections.is_empty(),
            new_name: input("Name, for example dev", window, cx),
            new_url: input("http://127.0.0.1:8787", window, cx),
            new_token,
            replacement_token,
            error: None,
            _subscriptions: subscriptions,
        }
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
        let session = open_session(connection, graph_id.trim().to_owned(), "main".into(), cx);
        ConnectionRegistry::global(cx).read(cx).save(cx);
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.set_session(session, window, cx)
            });
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
        let result = registry.update(cx, |registry, cx| {
            registry.add_server(name, url, token, Vec::new(), cx)
        });
        match result {
            Ok(connection) => {
                self._subscriptions
                    .push(cx.subscribe(&connection, |_, _, _: &ConnectionEvent, cx| cx.notify()));
                connection.update(cx, |c, cx| c.connect(cx));
                self.select(connection, cx);
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
        match registry.update(cx, |registry, cx| {
            registry.update_token(&connection, token, cx)
        }) {
            Ok(()) => {
                self.error = None;
                self.replacement_token
                    .update(cx, |editor, cx| editor.set_text("", window, cx));
            }
            Err(error) => self.error = Some(error.to_string().into()),
        }
        cx.notify();
    }

    fn render_servers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let connections = ConnectionRegistry::global(cx).read(cx).connections.clone();
        v_flex()
            .w(px(240.))
            .flex_none()
            .gap_0p5()
            .child(
                Label::new("Servers")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .children(
                connections
                    .into_iter()
                    .enumerate()
                    .map(|(index, connection)| {
                        let c = connection.read(cx);
                        let color = state_color(&c.state);
                        let selected = self.selected.as_ref() == Some(&connection);
                        let source = match c.server.source {
                            ServerSource::Cli => "CLI config",
                            ServerSource::Studio => "added in Studio",
                            ServerSource::Environment => "environment",
                        };
                        ListItem::new(("server", index))
                            .spacing(ListItemSpacing::Sparse)
                            .toggle_state(selected)
                            .start_slot(Indicator::dot().color(color))
                            .child(
                                v_flex()
                                    .child(Label::new(c.server.name.clone()).size(LabelSize::Small))
                                    .child(
                                        Label::new(format!("{} · {source}", c.server.url))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
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
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.adding = true;
                        window.focus(&this.new_url.focus_handle(cx), cx);
                        cx.notify();
                    })),
            )
    }

    fn render_graphs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(connection) = &self.selected else {
            return v_flex().child(Label::new("Add a server to start.").color(Color::Muted));
        };
        let c = connection.read(cx);
        let status = match &c.state {
            ConnectionState::Unreachable(error) => format!("Could not reach the server: {error}"),
            ConnectionState::Unauthorized => "The server refused the token.".into(),
            state => state.label(),
        };
        let state = c.state.clone();
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
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(Indicator::dot().color(state_color(&state)))
                    .child(
                        Label::new(status)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .map(|row| match &state {
                        ConnectionState::Connecting => row.child(
                            studio_ui::ds::button("cancel-connect", "Cancel")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    selected.update(cx, |c, cx| c.cancel(cx));
                                })),
                        ),
                        ConnectionState::Unreachable(_) | ConnectionState::Idle => row.child(
                            studio_ui::ds::button("retry-connect", "Retry")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::RotateCw).size(IconSize::XSmall))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    selected.update(cx, |c, cx| c.connect(cx));
                                })),
                        ),
                        _ => row,
                    }),
            )
            .when(state == ConnectionState::Unauthorized, |pane| {
                pane.child(
                    h_flex()
                        .gap_2()
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
            .child(
                Label::new("Graphs")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .children(
                listing_note.map(|n| Label::new(n).size(LabelSize::Small).color(Color::Muted)),
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
                        ListItem::new(("graph", index))
                            .spacing(ListItemSpacing::Sparse)
                            .start_slot(
                                Icon::new(IconName::Folder)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(Label::new(graph).size(LabelSize::Small))
                                    .child(
                                        Label::new(detail)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open(id.clone(), window, cx)
                            }))
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
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
                .gap_1()
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    studio_ui::ds::field_frame(false, cx)
                        .child(studio_ui::ds::field_input(editor.clone())),
                )
        };
        v_flex()
            .flex_1()
            .gap_3()
            .child(Label::new("Add a server").size(LabelSize::Small))
            .child(field("Address", &self.new_url))
            .child(field("Name", &self.new_name))
            .child(field("Bearer token", &self.new_token))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        studio_ui::ds::button("add", "Add and connect")
                            .style(ButtonStyle::Filled)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_server(window, cx)),
                            ),
                    )
                    .child(
                        studio_ui::ds::button("cancel-add", "Cancel").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.adding = false;
                                cx.notify();
                            },
                        )),
                    ),
            )
    }
}

fn state_color(state: &ConnectionState) -> Color {
    match state {
        ConnectionState::Ready => Color::Success,
        ConnectionState::Connecting | ConnectionState::Idle => Color::Muted,
        ConnectionState::ContractMismatch { .. } => Color::Warning,
        ConnectionState::Unauthorized | ConnectionState::Unreachable(_) => Color::Error,
    }
}

impl Render for OpenGraphModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let surface = cx.theme().colors().elevated_surface_background;
        let border = cx.theme().colors().border;
        let border_variant = cx.theme().colors().border_variant;
        let servers = self.render_servers(cx).into_any_element();
        let body = if self.adding {
            self.render_add(cx).into_any_element()
        } else {
            self.render_graphs(cx).into_any_element()
        };
        v_flex()
            .key_context("OpenGraphModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(px(760.))
            .max_h(px(560.))
            .p_4()
            .gap_3()
            .rounded_lg()
            .bg(surface)
            .border_1()
            .border_color(border)
            .shadow_lg()
            .child(Headline::new("Open a graph").size(HeadlineSize::Small))
            .child(
                h_flex()
                    .items_start()
                    .gap_4()
                    .child(servers)
                    .child(div().w_px().h(px(320.)).bg(border_variant))
                    .child(body),
            )
            .children(
                self.error
                    .clone()
                    .map(|e| Label::new(e).size(LabelSize::Small).color(Color::Error)),
            )
    }
}
