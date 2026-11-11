//! "Open a graph" (spec 003): servers with their state, a server's graphs, a graph id field,
//! and a form to add a server by URL and token (by URL alone on Windows, whose CLI gives Studio
//! no token: the token field is disabled there and says why). ↑, ↓ and Tab (Zed's select
//! actions) move through the servers, the chosen one's graphs and the id field; ↵ runs the row.

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Render,
    ScrollHandle, SharedString, Subscription, Task, WeakEntity, Window,
};
use graph_client::Unauthorized;
use graph_store::cli_config::{self, ManagedTarget, NoCli, Platform};
use graph_store::sign_in::{Kept, SignIn, left_in_keychain};
use graph_store::{
    Connection, ConnectionEvent, ConnectionRegistry, ConnectionState, ConnectionWatch,
    ControlPlane, GraphListing, HeldRefusal, ServerSource,
};
use studio_ui::ds::Tipped;
use studio_ui::ds::{self, MenuRow, size, space};
use studio_workspace::ModalView;
use studio_workspace::{Workspace, connection_color};

use crate::first_run::{self, Choice};
use crate::organizations;
use ui::prelude::*;

/// Where the keyboard is in the dialog: a server, one of the chosen server's graphs, or a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    Server(EntityId),
    /// A control plane's one row in the sidebar while it lists no organizations: ↵ signs in,
    /// cancels a sign-in under way, or reads its organizations again (spec 021).
    Plane(EntityId),
    /// An organization in the sidebar, under its control plane: moving to it shows its
    /// clusters, and ↵ goes to the first.
    Organization(EntityId, String),
    /// A cluster of an organization's pane that opens from its control plane, or that chooses
    /// the CLI target naming it: its organization and `cluster_id`.
    Cluster(EntityId, String, String),
    Graph(String),
    /// The field for a new token, shown when the server refused the token.
    Token,
    /// The graph id field.
    GraphId,
    /// A row of the start screen (spec 029).
    Choice(Choice),
}

/// What the start screen shows: its choices, or the recipe for a graph on this computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Start {
    Choices,
    Local,
}

/// The recipe's server, added and waited for: checked every [`first_run::WAIT_EVERY`] while the
/// pane waits, for at most [`first_run::WAIT_AT_MOST`].
struct Waiting {
    connection: Entity<Connection>,
    /// The wait ran out without an answer.
    gave_up: bool,
    _task: Option<Task<()>>,
}

/// Where a line the dialog says is shown: beside what it is about (constitution VII).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Said {
    /// Under the pane: about the server or cluster chosen.
    Pane,
    /// Under the graph id field and Open.
    GraphId,
    /// Under the field for a new token and Update token.
    Token,
    /// Under the add form's buttons: the add's refusal.
    Add,
    /// Under the add form's buttons, as progress rather than an error.
    Adding,
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
    /// The control plane, and its organization, the pane shows in place of the selected
    /// server's graphs.
    plane_shown: Option<organizations::Shown>,
    /// The row or field ↑, ↓ and Tab moved to, which ↵ runs.
    cursor: Option<Place>,
    graph_id: Entity<Editor>,
    adding: bool,
    /// The start screen, shown in place of the add form while no server is listed (spec 029).
    start: Option<Start>,
    /// The recipe's server, while the dialog waits for it to answer.
    waiting: Option<Waiting>,
    new_name: Entity<Editor>,
    new_url: Entity<Editor>,
    new_token: Entity<Editor>,
    /// The graph to open on the server added, for one that does not list its graphs (spec 029).
    new_graph: Entity<Editor>,
    /// Whether the CLI gives Studio a token here, as the registry holds it: where it gives none
    /// (Windows), the new server's token field is disabled and says why.
    platform: Platform,
    /// The server being added, while the CLI adds its target.
    _adding: Option<Task<()>>,
    /// A replacement for a token the selected server refused.
    replacement_token: Entity<Editor>,
    error: Option<SharedString>,
    /// Where [`Self::error`] shows: beside the field or button it is about.
    error_at: Said,
    /// The server and graph lists, which scroll within the dialog: the keyboard's row is
    /// scrolled into view ([`Self::move_to`]).
    servers_scroll: ScrollHandle,
    graphs_scroll: ScrollHandle,
    /// The control planes the CLI's profiles name, and the managed targets, as the registry
    /// last held them (spec 021).
    planes: Vec<Entity<ControlPlane>>,
    managed: Vec<ManagedTarget>,
    /// A subscription to each control plane shown, by its entity.
    followed_planes: Vec<(EntityId, Subscription)>,
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
        let platform = registry.read(cx).platform();
        // A reload of the operator's files can add, drop or replace servers: the dialog lists
        // and follows what the registry holds now.
        let observe = cx.observe_in(&registry, window, |this, registry, window, cx| {
            this.follow(registry.read(cx).connections.clone(), window, cx);
            let (planes, managed) = {
                let registry = registry.read(cx);
                (
                    registry.control_planes.clone(),
                    registry.managed_targets.clone(),
                )
            };
            this.follow_planes(planes, managed, cx);
        });
        let (planes, managed) = {
            let registry = registry.read(cx);
            (
                registry.control_planes.clone(),
                registry.managed_targets.clone(),
            )
        };
        // The window's own server is the one chosen: the page for a graph it would not open sends
        // the person here for a new token or another graph (spec 029).
        let shown = workspace.upgrade().and_then(|workspace| {
            let session = workspace.read(cx).session()?;
            Some(session.read(cx).connection.clone())
        });
        let mut this = Self::build(workspace, connections, vec![observe], platform, window, cx);
        this.follow_planes(planes, managed, cx);
        if let Some(connection) = shown.filter(|c| this.connections.contains(c)) {
            this.select(connection, window, cx);
        }
        this
    }

    /// The dialog over `connections`, kept in line with the registry by `subscriptions`, on
    /// `platform`.
    fn build(
        workspace: WeakEntity<Workspace>,
        connections: Vec<Entity<Connection>>,
        mut subscriptions: Vec<Subscription>,
        platform: Platform,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let graph_id = input("Graph id, for example knowledge", window, cx);
        let new_name = input("Name, for example dev", window, cx);
        let new_url = input(
            "https://omnigraph.example.com or http://127.0.0.1:8787",
            window,
            cx,
        );
        let new_token = input("Token (kept by the Omnigraph CLI)", window, cx);
        new_token.update(cx, |editor, cx| editor.set_masked(true, cx));
        let new_graph = input(
            "Graph id, if the server does not list its graphs",
            window,
            cx,
        );
        let replacement_token = input("New token", window, cx);
        replacement_token.update(cx, |editor, cx| editor.set_masked(true, cx));
        // An error is about what was typed: typing again clears it. Typing in a field puts the
        // keyboard there, so ↵ uses what was typed. A field the dialog clears while the
        // keyboard is elsewhere was not typed in.
        for (editor, place) in [
            (&graph_id, Some(Place::GraphId)),
            (&new_url, None),
            (&new_token, None),
            (&new_graph, None),
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
        // With no server listed the dialog starts on its choices, the first under the keyboard.
        let start = connections.is_empty().then_some(Start::Choices);
        let mut this = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            connections: Vec::new(),
            followed: Vec::new(),
            cursor: match &selected {
                Some(connection) => Some(Place::Server(connection.entity_id())),
                None => Some(Place::Choice(Choice::Cloud)),
            },
            selected,
            plane_shown: None,
            graph_id,
            adding: false,
            start,
            waiting: None,
            new_name,
            new_url,
            new_token,
            new_graph,
            platform,
            replacement_token,
            _adding: None,
            error: None,
            error_at: Said::Pane,
            servers_scroll: ScrollHandle::new(),
            graphs_scroll: ScrollHandle::new(),
            planes: Vec::new(),
            managed: Vec::new(),
            followed_planes: Vec::new(),
            _subscriptions: subscriptions,
        };
        // The clusters are held while the dialog is open, and let go when it closes (spec 021).
        this._subscriptions.push(cx.on_release(|this, cx| {
            for plane in &this.planes {
                plane.update(cx, |plane, cx| plane.forget_clusters(cx));
            }
        }));
        this.follow(connections, window, cx);
        this
    }

    /// Shows `planes`, the control planes the registry holds now, with `managed`, the CLI's
    /// managed targets. Each signed-in plane's clusters are read once as the dialog shows it, and
    /// once more when a sign-in lands while it is open; never on a timer (spec 021).
    fn follow_planes(
        &mut self,
        planes: Vec<Entity<ControlPlane>>,
        managed: Vec<ManagedTarget>,
        cx: &mut Context<Self>,
    ) {
        let listed = |id: EntityId| planes.iter().any(|p| p.entity_id() == id);
        self.followed_planes.retain(|(id, _)| listed(*id));
        for plane in &planes {
            let id = plane.entity_id();
            if self
                .followed_planes
                .iter()
                .any(|(followed, _)| *followed == id)
            {
                continue;
            }
            let subscription = cx.observe(plane, |_, plane, cx| {
                // A sign-in that landed while the dialog shows the plane lists its clusters.
                if plane.read(cx).holds_sign_in() && plane.read(cx).organizations().is_none() {
                    plane.update(cx, |plane, cx| plane.read_clusters(cx));
                }
                cx.notify();
            });
            self.followed_planes.push((id, subscription));
            plane.update(cx, |plane, cx| {
                if plane.holds_sign_in() && plane.organizations().is_none() {
                    plane.read_clusters(cx);
                }
            });
        }
        if let Some((shown, _)) = &self.plane_shown
            && !planes.iter().any(|plane| plane.entity_id() == *shown)
        {
            self.plane_shown = None;
        }
        self.planes = planes;
        self.managed = managed;
        cx.notify();
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
        // Opened before the CLI's targets were listed, it showed its start screen for want of
        // one: the first listed is chosen once they land, as it would have been, unless a server
        // is being added, the form was typed in, or the recipe is shown.
        let first_listed = self.selected.is_none()
            && self.connections.is_empty()
            && !connections.is_empty()
            && self._adding.is_none()
            && self.start != Some(Start::Local)
            && self.waiting.is_none()
            && [&self.new_name, &self.new_url, &self.new_token]
                .iter()
                .all(|editor| editor.read(cx).text(cx).is_empty());
        if first_listed {
            self.adding = false;
            self.start = None;
            if matches!(self.cursor, Some(Place::Choice(_))) {
                self.cursor = Some(Place::Server(connections[0].entity_id()));
            }
        }
        if first_listed
            || self
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

    /// Where the keyboard can go, in order: the sidebar's servers, then its organizations (or a
    /// control plane's sign-in), then what the pane shows: an organization's clusters, or the
    /// selected server's field for a new token when it asks for one (not one it signs in to),
    /// its graphs and the graph id field. None while a server is added.
    fn places(&self, cx: &App) -> Vec<Place> {
        if self.adding {
            return Vec::new();
        }
        let mut places: Vec<Place> = self
            .connections
            .iter()
            .map(|connection| Place::Server(connection.entity_id()))
            .collect();
        places.extend(organizations::nav_places(&self.planes, cx));
        match self.start {
            Some(Start::Choices) => {
                let no_cli = self.no_cli(cx);
                places.extend(
                    Choice::ALL
                        .into_iter()
                        .filter(|choice| !(no_cli && choice.needs_cli()))
                        .map(Place::Choice),
                );
                return places;
            }
            Some(Start::Local) => return places,
            None => {}
        }
        match self.shown_plane() {
            Some((plane, Some(organization))) => places.extend(organizations::pane_places(
                &plane,
                &organization,
                &self.managed,
                cx,
            )),
            Some((_, None)) => {}
            None => {
                if let Some(selected) = &self.selected {
                    let connection = selected.read(cx);
                    if connection.asks_token() {
                        places.push(Place::Token);
                    }
                    places.extend(connection.graph_ids().into_iter().map(Place::Graph));
                    places.push(Place::GraphId);
                }
            }
        }
        places
    }

    /// The control plane the pane shows, and its organization, when it shows one.
    fn shown_plane(&self) -> Option<(Entity<ControlPlane>, Option<String>)> {
        let (id, organization) = self.plane_shown.as_ref()?;
        let plane = self.planes.iter().find(|p| p.entity_id() == *id)?;
        Some((plane.clone(), organization.clone()))
    }

    /// Why the new server's token field is disabled: where the CLI gives Studio no token
    /// (Windows), Studio could not use a server that needs one. `None` where the field takes a
    /// token. The registry refuses a token there too ([`ConnectionRegistry::add_target`]).
    fn token_refusal(&self) -> Option<SharedString> {
        (!self.platform.exports_tokens()).then(|| {
            format!(
                "Studio on Windows adds only servers that need no token: {}.",
                cli_config::PIPE_ONLY
            )
            .into()
        })
    }

    /// ↓ or Tab (`by` 1), ↑ or ⇧Tab (-1), wrapping around. While a server is added, through
    /// that form's fields instead, past a token field that is disabled.
    fn step(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.adding {
            let mut fields = vec![&self.new_url, &self.new_name];
            if self.token_refusal().is_none() {
                fields.push(&self.new_token);
            }
            fields.push(&self.new_graph);
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
    pub(crate) fn move_to(&mut self, place: Place, window: &mut Window, cx: &mut Context<Self>) {
        match &place {
            Place::Server(id) => {
                self.plane_shown = None;
                self.start = None;
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
            Place::Plane(id) => {
                self.show_plane(*id, None);
                window.focus(&self.focus_handle, cx)
            }
            Place::Organization(id, organization) => {
                self.show_plane(*id, Some(organization.clone()));
                window.focus(&self.focus_handle, cx)
            }
            Place::Graph(_) | Place::Cluster(..) => window.focus(&self.focus_handle, cx),
            Place::Token => window.focus(&self.replacement_token.focus_handle(cx), cx),
            Place::GraphId => window.focus(&self.graph_id.focus_handle(cx), cx),
            Place::Choice(_) => window.focus(&self.focus_handle, cx),
        }
        // However long its list, the row the keyboard is on shows.
        match &place {
            Place::Server(id) => {
                if let Some(at) = self
                    .connections
                    .iter()
                    .position(|connection| connection.entity_id() == *id)
                {
                    // After the sidebar's "Servers" label.
                    self.servers_scroll.scroll_to_item(at + 1);
                }
            }
            Place::Graph(graph) => {
                let at = self.selected.as_ref().and_then(|connection| {
                    connection
                        .read(cx)
                        .graph_ids()
                        .iter()
                        .position(|id| id == graph)
                });
                if let Some(at) = at {
                    self.graphs_scroll.scroll_to_item(at);
                }
            }
            Place::Token
            | Place::GraphId
            | Place::Choice(_)
            | Place::Plane(_)
            | Place::Organization(..)
            | Place::Cluster(..) => {}
        }
        self.cursor = Some(place);
        cx.notify();
    }

    /// Shows the control plane `id` in the pane, with `organization`'s clusters when it names
    /// one, in place of a server's graphs or the form to add one.
    fn show_plane(&mut self, id: EntityId, organization: Option<String>) {
        self.plane_shown = Some((id, organization));
        self.adding = false;
        self.start = None;
        self.error = None;
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
        if self.start == Some(Start::Local) {
            self.connect_local(window, cx);
            return;
        }
        if let (Some(Start::Choices), Some(Place::Choice(choice))) = (self.start, &self.cursor) {
            self.take_choice(*choice, window, cx);
            return;
        }
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
                Some(Place::Plane(id)) if !self.graph_id.focus_handle(cx).is_focused(window) => {
                    if let Some(plane) = self.planes.iter().find(|p| p.entity_id() == id) {
                        plane.update(cx, organizations::enter);
                    }
                }
                Some(Place::Organization(id, organization))
                    if !self.graph_id.focus_handle(cx).is_focused(window) =>
                {
                    let first =
                        self.planes
                            .iter()
                            .find(|p| p.entity_id() == id)
                            .and_then(|plane| {
                                organizations::pane_places(plane, &organization, &self.managed, cx)
                                    .into_iter()
                                    .next()
                            });
                    if let Some(first) = first {
                        self.move_to(first, window, cx);
                    }
                }
                Some(Place::Cluster(id, organization, cluster_id))
                    if !self.graph_id.focus_handle(cx).is_focused(window) =>
                {
                    let found =
                        self.planes
                            .iter()
                            .find(|p| p.entity_id() == id)
                            .and_then(|plane| {
                                organizations::cluster(plane, &organization, &cluster_id, cx)
                                    .map(|cluster| (plane.clone(), cluster))
                            });
                    if let Some((plane, cluster)) = found {
                        match organizations::cluster_row(&cluster, &self.managed).target {
                            Some(target) => self.select_target(&target, window, cx),
                            None => self.open_cluster(&plane, &organization, &cluster, window, cx),
                        }
                    }
                }
                // A control plane's pane has no graph id field.
                _ if self.plane_shown.is_some() => {}
                _ => {
                    let graph_id = self.graph_id.read(cx).text(cx);
                    self.open(graph_id, window, cx);
                }
            }
        }
    }

    /// Shows `text` at `at`, in place of anything said before.
    pub(crate) fn say(&mut self, at: Said, text: impl Into<SharedString>) {
        self.error = Some(text.into());
        self.error_at = at;
    }

    /// What is said at `at`, if anything, drawn there.
    fn said_at(&self, at: Said) -> Option<AnyElement> {
        let text = self.error.clone().filter(|_| self.error_at == at)?;
        Some(match at {
            Said::Adding => ds::text::muted(text).into_any_element(),
            _ => ds::text::error(text).into_any_element(),
        })
    }

    fn show_add(&mut self, adding: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.adding = adding;
        if adding {
            self.plane_shown = None;
            self.start = None;
        }
        self.error = None;
        if adding {
            window.focus(&self.new_url.focus_handle(cx), cx);
        } else if self.connections.is_empty() {
            // Nothing else to show: back to the start screen.
            self.show_start(Start::Choices, window, cx);
        }
        cx.notify();
    }

    /// Whether Studio found no Omnigraph CLI it can drive, which the start screen's choices that
    /// add a CLI target need.
    fn no_cli(&self, cx: &App) -> bool {
        ConnectionRegistry::try_global(cx)
            .is_some_and(|registry| registry.read(cx).no_cli().is_some())
    }

    /// Shows the start screen at `start`, its first choice under the keyboard.
    pub(crate) fn show_start(&mut self, start: Start, window: &mut Window, cx: &mut Context<Self>) {
        self.start = Some(start);
        self.adding = false;
        self.plane_shown = None;
        self.error = None;
        if start == Start::Choices {
            self.cursor = Some(Place::Choice(Choice::Cloud));
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Takes a choice of the start screen: Omnigraph Cloud's sign-in, the add form, or the
    /// recipe. One that needs the CLI does nothing while there is none (its row says so).
    pub(crate) fn take_choice(
        &mut self,
        choice: Choice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if choice.needs_cli() && self.no_cli(cx) {
            return;
        }
        match choice {
            Choice::Cloud => {
                let plane = self
                    .planes
                    .iter()
                    .find(|plane| plane.read(cx).is_default())
                    .or_else(|| self.planes.first())
                    .map(|plane| plane.entity_id());
                match plane {
                    Some(id) => self.move_to(Place::Plane(id), window, cx),
                    None => {
                        self.say(
                            Said::Pane,
                            "Omnigraph Cloud's address is not set. Set it in Settings → Omnigraph \
                             Cloud.",
                        );
                        cx.notify();
                    }
                }
            }
            Choice::Server => self.show_add(true, window, cx),
            Choice::Local => self.show_start(Start::Local, window, cx),
        }
    }

    /// The recipe's Connect: adds its server as the anonymous CLI target `local` (one listed at
    /// its address already is used as it is), then waits for it to answer.
    fn connect_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._adding.is_some() || self.waiting.as_ref().is_some_and(|w| !w.gave_up) {
            return;
        }
        let listed = self
            .connections
            .iter()
            .find(|connection| connection.read(cx).server.url == first_run::LOCAL_ADDRESS)
            .cloned();
        if let Some(connection) = listed {
            self.wait_for(connection, window, cx);
            return;
        }
        let adding = ConnectionRegistry::global(cx).update(cx, |registry, cx| {
            registry.add_target(
                first_run::LOCAL_TARGET.into(),
                first_run::LOCAL_ADDRESS.into(),
                None,
                cx,
            )
        });
        self.say(Said::Adding, "Adding the server through the Omnigraph CLI…");
        cx.notify();
        self._adding = Some(cx.spawn_in(window, async move |this, cx| {
            let added = adding.await;
            this.update_in(cx, |this, window, cx| {
                this._adding = None;
                this.error = None;
                match added {
                    Ok(connection) => {
                        let registry = ConnectionRegistry::global(cx);
                        this.follow(registry.read(cx).connections.clone(), window, cx);
                        this.wait_for(connection, window, cx);
                    }
                    Err(error) => this.say(Said::Add, format!("The server was not added: {error}")),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Waits for `connection`, the recipe's server, to answer: it is connected again every
    /// [`first_run::WAIT_EVERY`] while the pane shows the wait, for at most
    /// [`first_run::WAIT_AT_MOST`]. Once it answers, its graph opens; once it refuses (a token,
    /// another contract), the dialog shows it, as a server chosen would be.
    fn wait_for(
        &mut self,
        connection: Entity<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        connection.update(cx, |c, cx| c.connect(cx));
        let task = cx.spawn_in(window, async move |this, cx| {
            let mut waited = std::time::Duration::ZERO;
            loop {
                cx.background_executor().timer(first_run::WAIT_EVERY).await;
                waited += first_run::WAIT_EVERY;
                let done = this
                    .update_in(cx, |this, window, cx| this.check_wait(waited, window, cx))
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
        });
        self.waiting = Some(Waiting {
            connection,
            gave_up: false,
            _task: Some(task),
        });
        cx.notify();
    }

    /// One check of the wait, `waited` into it: whether it is over.
    fn check_wait(
        &mut self,
        waited: std::time::Duration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(waiting) = &mut self.waiting else {
            return true;
        };
        let connection = waiting.connection.clone();
        let state = connection.read(cx).state.clone();
        match state {
            ConnectionState::Ready => {
                self.waiting = None;
                self.select(connection, window, cx);
                self.open(first_run::LOCAL_GRAPH.to_owned(), window, cx);
                true
            }
            ConnectionState::Unauthorized | ConnectionState::ContractMismatch { .. } => {
                self.waiting = None;
                self.select(connection, window, cx);
                true
            }
            ConnectionState::Connecting => false,
            ConnectionState::Idle | ConnectionState::Unreachable(_)
                if waited >= first_run::WAIT_AT_MOST =>
            {
                waiting.gave_up = true;
                cx.notify();
                true
            }
            ConnectionState::Idle | ConnectionState::Unreachable(_) => {
                connection.update(cx, |c, cx| c.connect(cx));
                false
            }
        }
    }

    /// Stops waiting for the recipe's server. The target stays, for the next try.
    fn cancel_wait(&mut self, cx: &mut Context<Self>) {
        self.waiting = None;
        cx.notify();
    }

    /// Opens `cluster`, which no CLI target names, from its control plane, and chooses it
    /// (spec 021). A window that opens one of its graphs reopens it at the next start.
    pub(crate) fn open_cluster(
        &mut self,
        plane: &Entity<ControlPlane>,
        organization: &str,
        cluster: &control_client::ClusterView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let opened = ConnectionRegistry::global(cx).update(cx, |registry, cx| {
            registry.open_cluster(plane, organization, cluster, cx)
        });
        match opened {
            Some(connection) => {
                let connections = ConnectionRegistry::global(cx).read(cx).connections.clone();
                self.follow(connections, window, cx);
                self.select(connection, window, cx);
            }
            None => {
                self.say(
                    Said::Pane,
                    format!(
                        "Studio could not open {}: sign in to its organization's control plane \
                         first.",
                        cluster.name
                    ),
                );
                cx.notify();
            }
        }
    }

    /// Chooses the server of the CLI target `name`, as a cluster that target names was clicked.
    pub(crate) fn select_target(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let found = self
            .connections
            .iter()
            .find(|connection| connection.read(cx).server.name == name)
            .cloned();
        match found {
            Some(connection) => {
                connection.update(cx, |c, cx| c.connect_if_idle(cx));
                self.select(connection, window, cx);
            }
            None => {
                self.say(
                    Said::Pane,
                    format!("Studio lists no CLI target named {name}."),
                );
                cx.notify();
            }
        }
    }

    pub(crate) fn select(
        &mut self,
        connection: Entity<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.plane_shown = None;
        if retries_on_select(connection.read(cx)) {
            connection.update(cx, |c, cx| c.connect(cx));
        }
        self.cursor = Some(Place::Server(connection.entity_id()));
        self.choose(Some(connection), window, cx);
        self.adding = false;
        self.start = None;
        self.error = None;
        cx.notify();
    }

    /// Makes `connection` the server chosen, by a click, the keyboard or the registry dropping
    /// the one chosen. A new token typed for another server is that server's alone: the field
    /// and its error are cleared, so Update token never sends it to this server's address.
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
            self.say(Said::GraphId, "Enter a graph id.");
            cx.notify();
            return;
        }
        let graph_id = graph_id.trim().to_owned();
        // A server that says which graphs the token reaches does not have one it leaves out: the
        // dialog says so beside the field, rather than closing on a window that cannot open it.
        if let Some(missing) = missing_graph(connection.read(cx), &graph_id) {
            self.say(Said::GraphId, missing);
            cx.notify();
            return;
        }
        // A graph id typed in is kept with its server, for a token that cannot list graphs. Where
        // the server says which graphs the token reaches, none is kept: one it lists needs no
        // keeping, and kept would be offered after its deletion; one it does not is a mistyped
        // id, offered for good.
        let says_which = matches!(
            connection.read(cx).graphs,
            GraphListing::Listed(_) | GraphListing::Discovered(_)
        );
        if !says_which {
            connection.update(cx, |connection, _| connection.remember_graph(&graph_id));
            if let Some(registry) = ConnectionRegistry::try_global(cx) {
                registry.update(cx, |registry, cx| registry.save(cx));
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

    /// Adds the server typed as a CLI target, through the CLI (`target add`, and `credentials
    /// set` for a token, which the CLI keeps): Studio keeps no server of its own. The CLI's
    /// refusal is shown as it said it.
    fn add_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._adding.is_some() {
            return;
        }
        let name = self.new_name.read(cx).text(cx);
        let url = self.new_url.read(cx).text(cx);
        let token = self.new_token.read(cx).text(cx);
        let graph = self.new_graph.read(cx).text(cx).trim().to_owned();
        // What the fields held as the add began, to clear them once it lands.
        let fields = [name.clone(), url.clone(), token.clone()];
        let token = (!token.trim().is_empty()).then(|| token.trim().to_owned());
        let registry = ConnectionRegistry::global(cx);
        let listed: Vec<_> = registry
            .read(cx)
            .connections
            .iter()
            .map(|connection| connection.entity_id())
            .collect();
        let adding = registry.update(cx, |registry, cx| registry.add_target(name, url, token, cx));
        self.say(Said::Adding, "Adding the target through the Omnigraph CLI…");
        cx.notify();
        self._adding = Some(cx.spawn_in(window, async move |this, cx| {
            let added = adding.await;
            this.update_in(cx, |this, window, cx| {
                this._adding = None;
                this.error = None;
                match added {
                    // An address listed already is that server: it is selected, followed and
                    // connected once, as it was.
                    Ok(connection) if listed.contains(&connection.entity_id()) => {
                        let name = connection.read(cx).server.name.clone();
                        this.select(connection, window, cx);
                        this.say(Said::Pane, format!("{name} has that address already."));
                    }
                    Ok(connection) => {
                        // Followed now, before the registry's notice reaches the dialog.
                        let registry = ConnectionRegistry::global(cx);
                        this.follow(registry.read(cx).connections.clone(), window, cx);
                        connection.update(cx, |c, cx| c.connect(cx));
                        this.select(connection, window, cx);
                        this.clear_added(&fields, window, cx);
                        // A graph named with it is opened now: a server that lists none (one run
                        // without a cluster policy, spec 012 E47) shows its graph this way.
                        if !graph.is_empty() {
                            this.new_graph
                                .update(cx, |editor, cx| editor.set_text("", window, cx));
                            this.open(graph.clone(), window, cx);
                            return;
                        }
                        window.focus(&this.graph_id.focus_handle(cx), cx);
                    }
                    Err(error) => {
                        this.say(Said::Add, format!("The server was not added: {error}"));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Clears the fields a server was added from, so a second server is typed fresh, not
    /// added again from them: those that still hold what was added (`added`). One typed in
    /// while the add ran keeps what was typed.
    fn clear_added(&mut self, added: &[String; 3], window: &mut Window, cx: &mut Context<Self>) {
        for (editor, added) in [&self.new_name, &self.new_url, &self.new_token]
            .into_iter()
            .zip(added)
        {
            if editor.read(cx).text(cx) == *added {
                editor.update(cx, |editor, cx| editor.set_text("", window, cx));
            }
        }
    }

    fn update_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.selected.clone() else {
            return;
        };
        let token = self.replacement_token.read(cx).text(cx);
        if token.trim().is_empty() {
            self.say(Said::Token, "Enter the new token.");
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

    /// The sidebar: the CLI's servers, Add server…, then each control plane's organizations.
    /// It scrolls within the dialog.
    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let server_shown = self.plane_shown.is_none() && !self.adding && self.start.is_none();
        let servers = self
            .connections
            .clone()
            .into_iter()
            .enumerate()
            .map(|(index, connection)| {
                let c = connection.read(cx);
                let source = source_label(&c.server.source, c.login_profile().as_deref());
                let tip = format!("{} · {source}", c.server.url);
                ds::SidebarRow::new(("server", index), c.server.name.clone())
                    .start(ds::dot(connection_color(&c.state)))
                    .selected(server_shown && self.selected.as_ref() == Some(&connection))
                    .tip(ds::tooltip(tip))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select(connection.clone(), window, cx)
                    }))
            })
            .collect::<Vec<_>>();
        v_flex()
            .id("server-list")
            .w(px(size::NAV))
            .flex_none()
            .h_full()
            .pb(px(space::LG))
            .gap(px(space::WITHIN))
            .overflow_y_scroll()
            .track_scroll(&self.servers_scroll)
            .border_r_1()
            .border_color(ds::surface::floating_border(cx))
            .child(ds::section_label("Servers").pt(px(space::LG)))
            .children(servers)
            .child(
                ds::SidebarRow::new("add-server", "Add server…")
                    .icon(IconName::Plus)
                    .muted(!self.adding)
                    .selected(self.adding)
                    .tip(ds::tooltip(
                        "Adds a server as a target of the Omnigraph CLI",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.show_add(true, window, cx))),
            )
            .child(
                ds::SidebarRow::new("get-started", "Get started")
                    .icon(IconName::Sparkle)
                    .muted(self.start.is_none())
                    .selected(self.start.is_some())
                    .tip(ds::tooltip(
                        "Omnigraph Cloud, a server by its address, or a graph on this computer",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_start(Start::Choices, window, cx)
                    })),
            )
            .children(organizations::nav(
                &self.planes,
                self.plane_shown.as_ref(),
                cx,
            ))
    }

    /// While Studio finds no Omnigraph CLI it can drive, a band over the pane says so, with the
    /// CLI's reason, how to install one, and Check again; it stays until one is found (spec 029),
    /// where a toast would have gone by behind the dialog.
    fn render_cli_band(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let registry = ConnectionRegistry::try_global(cx)?;
        let (no_cli, checking) = {
            let registry = registry.read(cx);
            (registry.no_cli()?.clone(), registry.checking_cli())
        };
        Some(cli_band(&no_cli, checking, registry, cx))
    }

    /// The start screen's choices: a row each, ↑/↓ and ↵, one that needs the CLI disabled while
    /// there is none, saying so (spec 029).
    fn render_start(&self, cx: &mut Context<Self>) -> AnyElement {
        let no_cli = self.no_cli(cx);
        let rows = Choice::ALL.into_iter().enumerate().map(|(index, choice)| {
            let disabled = no_cli && choice.needs_cli();
            let icon = match choice {
                Choice::Cloud => IconName::UserCheck,
                Choice::Server => IconName::Server,
                Choice::Local => IconName::Terminal,
            };
            MenuRow::new(("start", index))
                .height(size::ROW)
                .disabled(disabled)
                .toggle_state(self.cursor == Some(Place::Choice(choice)))
                .start_slot(studio_ui::ds::icon::lead(icon))
                .child(
                    h_flex()
                        .gap(px(space::MD))
                        .child(studio_ui::ds::text::body(choice.title()))
                        .child(studio_ui::ds::text::meta(if disabled {
                            "Needs the Omnigraph CLI"
                        } else {
                            choice.line()
                        })),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.cursor = Some(Place::Choice(choice));
                    this.take_choice(choice, window, cx)
                }))
        });
        v_flex()
            .size_full()
            .gap(px(space::LG))
            .child(pane_head(
                "Get started",
                Some(ds::text::meta("Open a graph you have, or make one.").into_any_element()),
                Vec::new(),
            ))
            .child(v_flex().gap(px(space::XXS)).children(rows))
            .into_any_element()
    }

    /// The recipe for a graph on this computer: its steps, each a command to copy, then
    /// Connect, which adds the server they start and waits for it (spec 029).
    fn render_local(&self, cx: &mut Context<Self>) -> AnyElement {
        let steps = first_run::steps(cfg!(windows));
        let footer = match &self.waiting {
            Some(waiting) if waiting.gave_up => {
                h_flex()
                    .gap(px(space::MD))
                    .child(div().flex_1().min_w_0().child(ds::text::error(
                        "The server did not start. Check the terminal where you ran step 4.",
                    )))
                    .child(ds::action_button("retry-local", "Retry").on_click(
                        cx.listener(|this, _, window, cx| this.connect_local(window, cx)),
                    ))
            }
            Some(_) => h_flex()
                .gap(px(space::MD))
                .child(div().flex_1().min_w_0().child(ds::text::muted(format!(
                    "Waiting for the server at {}…",
                    first_run::LOCAL_ADDRESS
                ))))
                .child(
                    ds::cancel_button("cancel-local")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_wait(cx))),
                ),
            None => h_flex()
                .gap(px(space::MD))
                .justify_end()
                .child(ds::cancel_button("back-local").on_click(
                    cx.listener(|this, _, window, cx| this.show_start(Start::Choices, window, cx)),
                ))
                .child(
                    ds::action_button("connect-local", "Connect")
                        .tip(studio_ui::ds::tooltip(format!(
                            "Adds {} as the CLI target {}, and opens the graph {} once it answers",
                            first_run::LOCAL_ADDRESS,
                            first_run::LOCAL_TARGET,
                            first_run::LOCAL_GRAPH
                        )))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.connect_local(window, cx)),
                        ),
                ),
        };
        v_flex()
            .size_full()
            .min_h_0()
            .gap(px(space::LG))
            .child(pane_head(
                "Make a local graph",
                Some(
                    ds::text::meta(
                        "Run these in a terminal, in order. Studio connects once the server \
                         answers.",
                    )
                    .into_any_element(),
                ),
                Vec::new(),
            ))
            .child(
                v_flex()
                    .id("local-recipe")
                    .flex_1()
                    .min_h_0()
                    .px(px(space::MD))
                    .gap(px(space::LG))
                    .overflow_y_scroll()
                    .children(steps.into_iter().enumerate().map(|(index, step)| {
                        v_flex()
                            .gap(px(space::XS))
                            .child(ds::text::meta(format!("{}. {}", index + 1, step.says)))
                            .child(studio_ui::ds::command_block(
                                ("local-step", index),
                                step.command,
                                cx,
                            ))
                    })),
            )
            .child(footer)
            .children(self.said_at(Said::Add))
            .children(self.said_at(Said::Adding))
            .into_any_element()
    }

    fn render_graphs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(connection) = &self.selected else {
            return v_flex().child(studio_ui::ds::text::state("Add a server to start."));
        };
        let c = connection.read(cx);
        let status = server_status(c);
        let state = c.state.clone();
        let actions = line_actions(c);
        // A token typed in replaces one the server refused, or one the CLI would not give to a
        // server that asks for one. A server that offers a sign-in is signed in to instead.
        let asks_token = c.asks_token();
        let token_tooltip = token_tooltip(c, self.platform);
        let selected = connection.clone();
        let listing_note = match &c.graphs {
            GraphListing::NotAllowed => {
                Some("This server does not list its graphs to this token. Enter a graph id.")
            }
            GraphListing::Failed(_) => Some("The graph list could not be read. Enter a graph id."),
            _ => None,
        };
        let graphs = c.graph_ids();
        // An empty list says why, rather than leaving a blank pane.
        let listing_note =
            listing_note.or_else(|| graphs.is_empty().then_some(empty_list_note(&c.state)));
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
        let (name, address) = {
            let c = connection.read(cx);
            let source = source_label(&c.server.source, c.login_profile().as_deref());
            (
                c.server.name.clone(),
                format!("{} · {source}", c.server.url),
            )
        };
        let head = pane_head(
            name,
            Some(ds::text::meta(address).truncate().into_any_element()),
            actions
                .into_iter()
                .map(|action| line_button(action, &selected, cx).into_any_element())
                .collect(),
        );
        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .gap(px(space::LG))
            .child(head)
            .child(
                // Wraps, so the CLI's whole reason shows within the pane.
                h_flex()
                    .px(px(space::MD))
                    .gap(px(space::MD))
                    .items_start()
                    .child(
                        div()
                            .pt(px(space::XS))
                            .child(ds::dot(connection_color(&state))),
                    )
                    .child(div().flex_1().min_w_0().child(ds::text::muted(status))),
            )
            .when(asks_token, |pane| {
                pane.child(
                    h_flex()
                        .gap(px(space::MD))
                        .child(
                            studio_ui::ds::field_frame(false, cx)
                                .flex_1()
                                .child(studio_ui::ds::field_input(self.replacement_token.clone())),
                        )
                        .child(
                            studio_ui::ds::button("update-token", "Update token")
                                .style(ButtonStyle::Filled)
                                .tip(studio_ui::ds::tooltip(token_tooltip))
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.update_token(window, cx)
                                    }),
                                ),
                        ),
                )
                .children(self.said_at(Said::Token))
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(space::XXS))
                    .child(pane_label("Graphs"))
                    .children(
                        listing_note
                            .map(|n| div().px(px(space::MD)).child(studio_ui::ds::text::muted(n))),
                    )
                    .child(
                        // Scrolls within the dialog: the id field and Open stay under it.
                        v_flex()
                            .id("graph-list")
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.graphs_scroll)
                            .children(graphs.into_iter().zip(infos).enumerate().map(
                                |(index, (graph, info))| {
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
                                    let at_cursor =
                                        self.cursor.as_ref() == Some(&Place::Graph(graph.clone()));
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
                                },
                            )),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .debug_selector(|| "open-graph".into())
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
            .children(self.said_at(Said::GraphId))
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
        // Disabled where Studio could not use a token (Windows): it takes no text, Tab passes it
        // by, and it says why.
        let token_note = new_token_note(&self.new_url.read(cx).text(cx));
        let token = match self.token_refusal() {
            None => field("Bearer token", &self.new_token)
                .children(token_note.map(studio_ui::ds::text::meta))
                .into_any_element(),
            Some(reason) => v_flex()
                .gap(px(space::XS))
                .child(studio_ui::ds::text::meta("Bearer token"))
                .child(
                    studio_ui::ds::field_frame(false, cx)
                        .id("new-token-disabled")
                        .debug_selector(|| "new-token-disabled".into())
                        .opacity(studio_ui::ds::opacity::DIMMED)
                        .cursor_not_allowed()
                        .child(studio_ui::ds::text::muted("Not available on Windows"))
                        .tip(studio_ui::ds::tooltip(reason)),
                )
                .into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(space::LG))
            .child(studio_ui::ds::text::body("Add a server"))
            .child(field("Address", &self.new_url))
            .child(field("Name", &self.new_name))
            .child(token)
            .child(field("Graph (optional)", &self.new_graph))
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
            .children(self.said_at(Said::Add))
            .children(self.said_at(Said::Adding))
    }
}

/// The band [`OpenGraphModal::render_cli_band`] draws for `no_cli`, its Check again acting on
/// `registry`; "Looking for the CLI…" while a look it asked is under way.
fn cli_band(
    no_cli: &NoCli,
    checking: bool,
    registry: Entity<ConnectionRegistry>,
    cx: &mut Context<OpenGraphModal>,
) -> AnyElement {
    let title = if no_cli.unfit {
        "Studio needs a newer Omnigraph CLI"
    } else {
        "Studio needs the Omnigraph CLI"
    };
    let check = studio_ui::ds::button("check-cli", "Check again")
        .style(ButtonStyle::Subtle)
        .disabled(checking)
        .tip(studio_ui::ds::tooltip(
            "Looks for the Omnigraph CLI again and lists its targets",
        ))
        .on_click(cx.listener(move |_, _, _, cx| {
            registry.update(cx, |registry, cx| registry.check_cli_again(cx))
        }));
    studio_ui::ds::callout(ui::Severity::Warning, title)
        .description_slot(
            v_flex()
                .gap(px(space::SM))
                .child(ds::text::muted(if checking {
                    "Looking for the CLI…".to_owned()
                } else {
                    format!("{} Install it, then Check again:", sentence(&no_cli.why))
                }))
                .child(studio_ui::ds::command_block(
                    "copy-install",
                    graph_store::cli::INSTALL,
                    cx,
                )),
        )
        .actions_slot(check)
        .into_any_element()
}

/// `text` ending in a full stop.
fn sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.ends_with('.') {
        text.to_owned()
    } else {
        format!("{text}.")
    }
}

/// What the dialog says of `graph_id` on `c`'s server when the server says which graphs the
/// token reaches and it is not one: the graphs it has instead. `None` for a server that lists
/// none, whose 404 the window's page explains.
fn missing_graph(c: &Connection, graph_id: &str) -> Option<String> {
    let listed = match &c.graphs {
        GraphListing::Listed(infos) => infos.iter().map(|info| info.graph_id.clone()).collect(),
        GraphListing::Discovered(ids) => ids.clone(),
        _ => return None,
    };
    if listed.iter().any(|id: &String| id == graph_id) {
        return None;
    }
    let name = &c.server.name;
    Some(match listed.as_slice() {
        [] => format!("{name} has no graph “{graph_id}”, and lists none."),
        listed => format!(
            "{name} has no graph “{graph_id}”. It has: {}.",
            listed.join(", ")
        ),
    })
}

/// Why a server's graph list is empty, by its state: what would show them, never a connect
/// that will not.
fn empty_list_note(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::Ready => "This server lists no graphs. Enter a graph id.",
        ConnectionState::Unauthorized => "Its graphs show once the server accepts a token.",
        ConnectionState::Unreachable(_) => "Its graphs show once Studio reaches it.",
        ConnectionState::ContractMismatch { .. } => {
            "Studio cannot read this server's graphs: it speaks another contract."
        }
        ConnectionState::Idle | ConnectionState::Connecting => {
            "Its graphs show once Studio connects."
        }
    }
}

/// What the chosen server's line offers, beside what it says of its state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LineAction {
    /// Stops waiting for the browser.
    CancelSignIn,
    /// Opens the sign-in service, named, in the browser.
    SignIn {
        service: String,
    },
    /// A sign-in held that the server does not admit, replaced by another.
    SignInAsSomeoneElse,
    SignOut,
    /// Stops the connect under way.
    Cancel,
    /// Connects again ([`retries_on_select`]).
    Retry,
}

/// The actions the chosen server's line offers, in order (spec 016). A sign-in being kept offers
/// none, as it is not to be cancelled; a sign-in held offers Sign out wherever it stands, and
/// Retry beside it where connecting again is what helps (the server unreachable, the sign-in's
/// service not answering, the token sent never arrived).
fn line_actions(c: &Connection) -> Vec<LineAction> {
    actions_for(
        &c.state,
        &c.sign_in,
        c.holds_sign_in(),
        retries_on_select(c),
    )
}

/// [`line_actions`] for a server in `state` whose sign-in reads `sign_in`, one held (`holds`)
/// or not, and connecting again helping (`retries`) or not.
fn actions_for(
    state: &ConnectionState,
    sign_in: &SignIn,
    holds: bool,
    retries: bool,
) -> Vec<LineAction> {
    let retry = retries.then_some(LineAction::Retry);
    match (state, sign_in) {
        (_, SignIn::Waiting { .. }) => vec![LineAction::CancelSignIn],
        // Details read again for a sign-in as someone else, the one held standing.
        (_, SignIn::Reading) if holds => vec![LineAction::SignOut],
        (_, SignIn::Finishing { .. }) => Vec::new(),
        (ConnectionState::Connecting, _) if holds => vec![LineAction::Cancel, LineAction::SignOut],
        // A sign-in as someone else that failed leaves the one held, and Retry where its token
        // never arrived.
        (ConnectionState::Unauthorized, SignIn::Offered { service, .. }) => {
            let mut actions = vec![LineAction::SignIn {
                service: service.clone(),
            }];
            actions.extend(holds.then_some(LineAction::SignOut));
            actions.extend(retry);
            actions
        }
        // The one held admitted meanwhile, or the server unreachable.
        (_, SignIn::Offered { .. }) if holds => {
            std::iter::once(LineAction::SignOut).chain(retry).collect()
        }
        // Not admitted: another account may be. A token that never arrived offers Retry.
        (ConnectionState::Unauthorized, SignIn::SignedIn { .. }) if retry.is_none() => {
            vec![LineAction::SignInAsSomeoneElse, LineAction::SignOut]
        }
        (_, SignIn::SignedIn { .. }) => std::iter::once(LineAction::SignOut).chain(retry).collect(),
        (ConnectionState::Connecting, _) => vec![LineAction::Cancel],
        _ => retry.into_iter().collect(),
    }
}

/// The button for `action` on the chosen server's line.
fn line_button(
    action: LineAction,
    selected: &Entity<Connection>,
    cx: &mut Context<OpenGraphModal>,
) -> ds::Button {
    let selected = selected.clone();
    match action {
        LineAction::CancelSignIn => studio_ui::ds::button("cancel-sign-in", "Cancel")
            .style(ButtonStyle::Subtle)
            .on_click(cx.listener(move |_, _, _, cx| {
                selected.update(cx, |c, cx| c.cancel_sign_in(cx));
            })),
        LineAction::SignIn { service } => studio_ui::ds::button("sign-in", "Sign in")
            .style(ButtonStyle::Filled)
            .tip(studio_ui::ds::tooltip(format!(
                "Opens {service} in your browser to sign in to this server."
            )))
            .on_click(cx.listener(move |_, _, _, cx| selected.update(cx, |c, cx| c.sign_in(cx)))),
        LineAction::SignInAsSomeoneElse => {
            studio_ui::ds::button("sign-in-again", "Sign in as someone else")
                .style(ButtonStyle::Subtle)
                .on_click(
                    cx.listener(move |_, _, _, cx| selected.update(cx, |c, cx| c.sign_in(cx))),
                )
        }
        LineAction::SignOut => sign_out_button(&selected, cx),
        LineAction::Cancel => cancel_button(&selected, cx),
        LineAction::Retry => retry_button(&selected, cx),
    }
}

/// Signs out of the chosen server (spec 016).
fn sign_out_button(selected: &Entity<Connection>, cx: &mut Context<OpenGraphModal>) -> ds::Button {
    let selected = selected.clone();
    studio_ui::ds::button("sign-out", "Sign out")
        .style(ButtonStyle::Subtle)
        .tip(studio_ui::ds::tooltip(
            "Forgets this sign-in, and asks the sign-in service to end it where it can.",
        ))
        .on_click(cx.listener(move |_, _, _, cx| selected.update(cx, |c, cx| c.sign_out(cx))))
}

fn cancel_button(selected: &Entity<Connection>, cx: &mut Context<OpenGraphModal>) -> ds::Button {
    let selected = selected.clone();
    studio_ui::ds::button("cancel-connect", "Cancel")
        .style(ButtonStyle::Subtle)
        .on_click(cx.listener(move |_, _, _, cx| selected.update(cx, |c, cx| c.cancel(cx))))
}

fn retry_button(selected: &Entity<Connection>, cx: &mut Context<OpenGraphModal>) -> ds::Button {
    let selected = selected.clone();
    studio_ui::ds::button("retry-connect", "Retry")
        .style(ButtonStyle::Subtle)
        .start_icon(Icon::new(IconName::RotateCw))
        .on_click(cx.listener(move |_, _, _, cx| selected.update(cx, |c, cx| c.connect(cx))))
}

/// What the chosen server's line says of its state. A 401 says what it was an answer to: no
/// token at all (a server Studio has none configured for asks for one), a token sent that never
/// arrived, or the token sent. A
/// CLI server the CLI gave no token is asked nothing, and says why. A server that offers a
/// sign-in says where, and who is signed in (spec 016).
fn server_status(c: &Connection) -> String {
    let mut status = state_status(c);
    // A token typed for a target the CLI keeps none for is Studio's alone, for this run.
    if c.token_given() && c.server.source == ServerSource::Cli && !c.has_token_source() {
        status.push_str(" Token kept until Studio quits.");
    }
    // An item the keychain would not delete, which Studio may read again later, is said
    // whatever the state.
    match c.keychain_left() {
        Some(refused) => format!("{status} {}", left_in_keychain(&refused)),
        None => status,
    }
}

/// The line of a server signed in as `account` at `service`, by what the keychain holds for it.
fn signed_in_status(account: &str, service: &str, kept: &Kept) -> String {
    let lasts = match kept {
        Kept::This => "",
        Kept::Nothing => " The keychain did not keep the sign-in, so it lasts until Studio quits.",
        Kept::Left(_) => " The keychain did not keep the sign-in.",
    };
    format!("Signed in as {account} at {service}.{lasts}")
}

/// [`server_status`] but for what the keychain kept.
fn state_status(c: &Connection) -> String {
    match (&c.sign_in, &c.state) {
        (SignIn::Waiting { service }, _) => {
            return format!("Finish signing in at {service} in your browser.");
        }
        (SignIn::Finishing { service }, _) => {
            return format!("Finishing the sign-in at {service}.");
        }
        // One finishing that a drop or a rename ended is kept before a sign-in is offered.
        (SignIn::NotOffered, ConnectionState::Unauthorized) if c.sign_in_under_way() => {
            return "Studio is finishing a sign-in.".into();
        }
        (SignIn::Reading, ConnectionState::Unauthorized) => {
            return "This server asks you to sign in. Studio is reading how.".into();
        }
        // A sign-in as someone else that failed, the one held standing: the server does not admit
        // it, its service did not answer, or the token sent never arrived.
        (
            SignIn::Offered {
                service,
                why: Some(why),
            },
            ConnectionState::Unauthorized,
        ) if c.holds_sign_in() => {
            return match c.held_refusal() {
                HeldRefusal::NotAnswering => {
                    format!("{why} You stay signed in at {service}; Studio asks again.")
                }
                HeldRefusal::NotReceived => format!(
                    "{why} You stay signed in at {service}; the server received no token: \
                     something between them may drop it."
                ),
                HeldRefusal::NotAdmitted => format!(
                    "{why} You stay signed in at {service}, as an account this server may not \
                     admit."
                ),
            };
        }
        (SignIn::Offered { service, why }, ConnectionState::Unauthorized) => {
            return match why {
                Some(why) => format!("{why} The sign-in is at {service}."),
                None => format!("This server asks you to sign in at {service}."),
            };
        }
        // A sign-in as someone else that failed, the one held admitted meanwhile.
        (
            SignIn::Offered {
                service,
                why: Some(why),
            },
            ConnectionState::Ready,
        ) if c.holds_sign_in() => {
            return format!("{why} You stay signed in at {service}.");
        }
        (SignIn::Unavailable(why), ConnectionState::Unauthorized) => {
            return format!("This server asks for a sign-in Studio cannot make. {why}");
        }
        (
            SignIn::SignedIn {
                service, account, ..
            },
            ConnectionState::Unauthorized,
        ) => {
            return match c.held_refusal() {
                // The sign-in stands, and is asked again.
                HeldRefusal::NotAnswering => {
                    let why = c.token_unread.as_deref().unwrap_or_default();
                    format!("{why} You stay signed in at {service}; Studio asks again.")
                }
                HeldRefusal::NotReceived => format!(
                    "The server received no token, though Studio sent the one signed in at \
                     {service}: something between them may drop it."
                ),
                HeldRefusal::NotAdmitted => format!(
                    "The server refused {account}, signed in at {service}: it may not admit \
                     this account."
                ),
            };
        }
        (SignIn::SignedIn { service, account }, ConnectionState::Ready) => {
            return signed_in_status(account, service, &c.keychain());
        }
        _ => {}
    }
    match (&c.state, &c.token_unread) {
        (ConnectionState::Unreachable(error), _) => {
            format!("Could not reach the server: {error}")
        }
        (ConnectionState::Unauthorized, Some(reason)) => {
            format!("The Omnigraph CLI gave no token for this server: {reason}")
        }
        // As a read or a write says it.
        (ConnectionState::Unauthorized, None) => c
            .refusal
            .unwrap_or(Unauthorized::TokenNeeded)
            .reason()
            .into(),
        (state, _) => state.label(),
    }
}

/// Whether the CLI gave this server no token: asked again, it may give one, once the person has
/// signed in (`omnigraph login`) or put the target's credential right.
fn gave_no_token(c: &Connection) -> bool {
    c.state == ConnectionState::Unauthorized && c.token_unread.is_some()
}

/// Whether choosing this server, or its Retry, connects it again: one never connected or
/// unreachable, one the CLI gave no token, one whose token never arrived (a proxy that drops it,
/// which may be put right), and one whose sign-in Studio could not use, whose details the
/// connect reads again.
fn retries_on_select(c: &Connection) -> bool {
    matches!(
        c.state,
        ConnectionState::Idle | ConnectionState::Unreachable(_)
    ) || gave_no_token(c)
        || c.token_not_received()
        || matches!(c.sign_in, SignIn::Unavailable(_))
}

/// What Update token does with the token typed: it lasts until Studio quits or asks the CLI for
/// the target's token again ([`ConnectionRegistry::update_token`]), and for a CLI server on Unix the CLI's own command
/// keeps one for the CLI and Studio. Where the CLI exports no token (Windows), that command
/// would make the target one that takes a token, which Studio there refuses
/// ([`Platform::exports_tokens`]): a CLI server the dialog lists there is an anonymous target,
/// and the token typed is the only one Studio can use for it. For a managed cluster reached on
/// the CLI's login, which offers no sign-in of its own, it first says where that cluster's token
/// comes from.
fn token_tooltip(c: &Connection, platform: Platform) -> String {
    // A CLI target added anonymous has no credential the CLI could keep a token in.
    if c.server.source == ServerSource::Cli && !c.has_token_source() {
        return ANONYMOUS_TOKEN.to_owned();
    }
    tooltip_for(
        &c.server.source,
        &c.server.name,
        c.login_profile().as_deref(),
        platform,
    )
}

/// What a token typed for a CLI target that takes none does: the CLI keeps none for it.
const ANONYMOUS_TOKEN: &str = "Used until Studio quits. The Omnigraph CLI keeps no token for a \
                               server it reaches without one.";

/// What the new server's token field says of a token for `url`: one at `http://` on this
/// machine lasts until Studio quits, since the CLI keeps tokens only for `https://` servers.
fn new_token_note(url: &str) -> Option<&'static str> {
    let url = graph_client::server_root(url).ok()?;
    (url.starts_with("http://") && graph_client::is_loopback(&url)).then_some(
        "The Omnigraph CLI keeps tokens only for https:// servers. Studio keeps this one until \
         it quits.",
    )
}

/// [`token_tooltip`] for a server of `source` named `name` on `platform`, reached on the CLI's
/// login of `profile` when there is one.
fn tooltip_for(
    source: &ServerSource,
    name: &str,
    profile: Option<&str>,
    platform: Platform,
) -> String {
    if *source == ServerSource::ControlPlane {
        let profile = profile
            .map(|profile| format!(" --profile {profile}"))
            .unwrap_or_default();
        return format!(
            "Studio asks the organization's control plane for this cluster's token, keeps it in \
             memory and asks again before it ends. To use this cluster from the CLI too, add it \
             as a target with `omnigraph target add {name} --cluster <id>{profile}`."
        );
    }
    if let Some(profile) = profile {
        return format!(
            "Studio uses the token the Omnigraph CLI gives for this cluster from its profile \
             {profile} (for a profile that signs in, your `omnigraph login --profile {profile}`). \
             Studio keeps it in memory and asks the CLI again when the server refuses it; the CLI \
             gives the same token while it holds and a new one once it lapses. A token typed \
             here is used until Studio quits or asks the CLI for this target's token again. \
             This cluster offers no sign-in of its own; one that does is signed in to instead."
        );
    }
    match source {
        ServerSource::Cli if platform.exports_tokens() => format!(
            "Used until Studio quits or asks the CLI for this target's token again. To keep one \
             for the CLI and Studio, pipe it to `omnigraph credentials set --target {name} \
             --stdin`."
        ),
        ServerSource::Cli => format!(
            "Used until Studio quits. Studio on Windows cannot use a target that needs a token \
             yet ({}), so the CLI cannot keep this one for it.",
            cli_config::PIPE_ONLY
        ),
        ServerSource::Environment | ServerSource::ControlPlane => "Used until Studio quits.".into(),
    }
}

/// Where a server in the list comes from, after its address: a CLI target, naming the CLI
/// profile whose managed token Studio uses when it does, or the environment.
fn source_label(source: &ServerSource, profile: Option<&str>) -> String {
    match (source, profile) {
        (ServerSource::Cli, Some(profile)) => format!("CLI target, CLI profile {profile}"),
        (ServerSource::Cli, None) => "CLI target".to_owned(),
        (ServerSource::Environment, _) => "environment".to_owned(),
        (ServerSource::ControlPlane, _) => "from the control plane".to_owned(),
    }
}

/// A pane's head: what it shows, named, a line under the name, and its actions on the right;
/// on its rows' text's edge.
pub(crate) fn pane_head(
    title: impl Into<SharedString>,
    detail: Option<AnyElement>,
    actions: Vec<AnyElement>,
) -> Div {
    h_flex()
        .px(px(space::MD))
        .gap(px(space::MD))
        .items_center()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(space::XXS))
                .child(ds::text::title(title).single_line().truncate())
                .children(detail),
        )
        .children(actions)
}

/// A pane's name, over its rows and on their text's edge.
pub(crate) fn pane_label(title: &'static str) -> Div {
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
        let nav = self.render_nav(cx).into_any_element();
        let cli_band = self.render_cli_band(cx);
        let body = match (self.adding, self.shown_plane()) {
            (true, _) => self.render_add(cx).into_any_element(),
            (false, _) if self.start == Some(Start::Choices) => self.render_start(cx),
            (false, _) if self.start == Some(Start::Local) => self.render_local(cx),
            (false, Some((plane, organization))) => organizations::pane(
                &plane,
                organization.as_deref(),
                &self.managed,
                self.cursor.as_ref(),
                cx,
            ),
            (false, None) => self.render_graphs(cx).into_any_element(),
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
            // One height whatever the pane shows, so choosing in the sidebar moves nothing.
            .h(px(size::MODAL_MAX_HEIGHT))
            .debug_selector(|| "open-graph-dialog".into())
            .child(
                ds::ModalHeader::new("Open a graph")
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                // The sidebar and the pane stretch to the height the dialog leaves them, which
                // their lists scroll within (a row centers its children, at their own height,
                // unless told otherwise).
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .border_t_1()
                    .border_color(ds::surface::floating_border(cx))
                    .child(nav)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .px(px(space::XL))
                            .pt(px(space::XL))
                            .pb(px(space::LG))
                            .gap(px(space::MD))
                            .children(cli_band)
                            .child(div().flex_1().min_h_0().child(body))
                            .children(self.said_at(Said::Pane)),
                    ),
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
            connection.wait_for_a_token();
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
        dialog_on(connections, Platform::Unix, cx)
    }

    /// [`dialog`], on `platform`.
    fn dialog_on(
        connections: Vec<Entity<Connection>>,
        platform: Platform,
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
                platform,
                window,
                cx,
            )
        });
        cx.update(|window, cx| window.focus(&modal.focus_handle(cx), cx));
        (modal, cx)
    }

    /// ↓ and Tab move through the servers, choosing each, then the chosen one's graphs and the
    /// id field, wrapping around; ↵ on a server goes to its graphs, and on a graph opens it.
    /// An empty graph list says what would show its graphs: a connect while connecting, a token
    /// while refused, reaching it while unreachable; never a connect that will not.
    #[test]
    fn an_empty_list_says_what_would_show_it() {
        assert_eq!(
            empty_list_note(&ConnectionState::Unauthorized),
            "Its graphs show once the server accepts a token."
        );
        assert_eq!(
            empty_list_note(&ConnectionState::Unreachable("refused".into())),
            "Its graphs show once Studio reaches it."
        );
        assert_eq!(
            empty_list_note(&ConnectionState::Connecting),
            "Its graphs show once Studio connects."
        );
    }

    /// An id the server does not list is said beside the field, naming the graphs it has, and
    /// the dialog stays open; one it lists opens (spec 029).
    #[gpui::test]
    fn an_id_the_server_does_not_list_is_said_before_the_dialog_closes(cx: &mut TestAppContext) {
        let connection = server(&[], cx);
        connection.update(cx, |connection, _| {
            connection.graphs = GraphListing::Discovered(vec!["knowledge".into(), "notes".into()])
        });
        let name = connection.read_with(cx, |c, _| c.server.name.clone());
        let (modal, cx) = dialog(vec![connection], cx);
        let dismissed = Rc::new(Cell::new(false));
        let _dismissed = cx.update(|_, cx| {
            let dismissed = dismissed.clone();
            cx.subscribe(&modal, move |_, _: &DismissEvent, _| dismissed.set(true))
        });
        modal.update_in(cx, |modal, window, cx| {
            modal.open("knowlege".into(), window, cx)
        });
        assert_eq!(
            modal.read_with(cx, |modal, _| (modal.error_at, modal.error.clone())),
            (
                Said::GraphId,
                Some(format!("{name} has no graph “knowlege”. It has: knowledge, notes.").into())
            )
        );
        cx.run_until_parked();
        assert!(!dismissed.get());
        modal.update_in(cx, |modal, window, cx| {
            modal.open("notes".into(), window, cx)
        });
        cx.run_until_parked();
        assert!(dismissed.get());
    }

    /// An empty dialog starts on its choices: ↓ moves through them, ↵ on Connect to a server
    /// shows the form, whose Cancel comes back; ↵ on Make a local graph shows the recipe
    /// (spec 029).
    #[gpui::test]
    fn an_empty_dialog_starts_on_its_choices(cx: &mut TestAppContext) {
        let (modal, cx) = dialog(Vec::new(), cx);
        let at = |cx: &mut VisualTestContext| {
            modal.read_with(cx, |modal, _| {
                (modal.start, modal.adding, modal.cursor.clone())
            })
        };
        assert_eq!(
            at(cx),
            (
                Some(Start::Choices),
                false,
                Some(Place::Choice(Choice::Cloud))
            )
        );
        cx.dispatch_action(menu::SelectNext);
        assert_eq!(at(cx).2, Some(Place::Choice(Choice::Server)));
        cx.dispatch_action(menu::Confirm);
        assert_eq!(at(cx).0, None);
        assert!(at(cx).1, "the form to add a server");
        modal.update_in(cx, |modal, window, cx| modal.show_add(false, window, cx));
        assert_eq!(at(cx).0, Some(Start::Choices), "back to the choices");
        cx.dispatch_action(menu::SelectLast);
        assert_eq!(at(cx).2, Some(Place::Choice(Choice::Local)));
        cx.dispatch_action(menu::Confirm);
        assert_eq!(at(cx).0, Some(Start::Local));
    }

    /// The recipe's wait opens its graph once the server answers, shows the server once it
    /// refuses, and gives up after its time, keeping nothing running.
    #[gpui::test]
    fn the_recipes_wait_opens_its_graph_once_the_server_answers(cx: &mut TestAppContext) {
        let local = server(&[], cx);
        let (modal, cx) = dialog(Vec::new(), cx);
        let dismissed = Rc::new(Cell::new(false));
        let _dismissed = cx.update(|_, cx| {
            let dismissed = dismissed.clone();
            cx.subscribe(&modal, move |_, _: &DismissEvent, _| dismissed.set(true))
        });
        let wait = |modal: &Entity<OpenGraphModal>, cx: &mut VisualTestContext| {
            modal.update_in(cx, |modal, _, _| {
                modal.waiting = Some(Waiting {
                    connection: local.clone(),
                    gave_up: false,
                    _task: None,
                });
            });
        };
        let check = |waited: u64, cx: &mut VisualTestContext| {
            modal.update_in(cx, |modal, window, cx| {
                modal.check_wait(std::time::Duration::from_secs(waited), window, cx)
            })
        };

        wait(&modal, cx);
        local.update(cx, |c, _| {
            c.state = ConnectionState::Unreachable("refused".into())
        });
        assert!(!check(2, cx), "still waiting");
        local.update(cx, |c, _| {
            c.state = ConnectionState::Unreachable("refused".into())
        });
        assert!(check(first_run::WAIT_AT_MOST.as_secs(), cx), "gave up");
        assert!(modal.read_with(cx, |modal, _| modal.waiting.as_ref().unwrap().gave_up));

        wait(&modal, cx);
        local.update(cx, |c, _| c.state = ConnectionState::Unauthorized);
        assert!(check(2, cx));
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.selected.clone()),
            Some(local.clone()),
            "a refusal shows the server"
        );
        assert!(!dismissed.get());

        wait(&modal, cx);
        local.update(cx, |c, _| c.state = ConnectionState::Ready);
        assert!(check(4, cx));
        cx.run_until_parked();
        assert!(dismissed.get(), "its graph opens");
    }

    /// A token for a server on this machine at `http://` is said to last until Studio quits,
    /// before it is added; one for an `https://` server is the CLI's to keep.
    #[test]
    fn a_loopback_token_is_said_to_last_until_studio_quits() {
        assert!(new_token_note("127.0.0.1:8787").is_some());
        assert!(new_token_note("http://localhost:8080").is_some());
        assert!(new_token_note("https://graph.example.com").is_none());
        assert!(new_token_note("").is_none());
    }

    /// What the dialog says shows beside what it is about: an id not entered under the id field,
    /// a token not entered under Update token.
    #[gpui::test]
    fn an_error_shows_beside_its_field(cx: &mut TestAppContext) {
        let alpha = server(&["one"], cx);
        alpha.update(cx, |connection, _| {
            connection.state = ConnectionState::Unauthorized;
            connection.refusal = Some(Unauthorized::TokenRefused);
        });
        let (modal, cx) = dialog(vec![alpha], cx);
        let said = |cx: &mut VisualTestContext| {
            modal.read_with(cx, |modal, _| (modal.error_at, modal.error.clone()))
        };
        modal.update_in(cx, |modal, window, cx| {
            modal.open(String::new(), window, cx)
        });
        assert_eq!(said(cx), (Said::GraphId, Some("Enter a graph id.".into())));
        modal.update_in(cx, |modal, window, cx| modal.update_token(window, cx));
        assert_eq!(said(cx), (Said::Token, Some("Enter the new token.".into())));
    }

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
        // Each refused the token sent.
        for connection in [&alpha, &beta] {
            connection.update(cx, |connection, _| {
                connection.state = ConnectionState::Unauthorized;
                connection.refusal = Some(Unauthorized::TokenRefused);
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

    /// The keyboard reaches the token field only where it is drawn: for a server that refused its
    /// token and offers no sign-in, with none under way. Before, a sign-in finishing after a drop
    /// hid the field while the keyboard could still move to it, and what was typed went to a
    /// field nobody saw.
    #[gpui::test]
    fn the_keyboard_reaches_the_token_field_only_where_it_is_drawn(cx: &mut TestAppContext) {
        let refused = server(&["one"], cx);
        refused.update(cx, |connection, _| {
            connection.state = ConnectionState::Unauthorized;
            connection.refusal = Some(Unauthorized::TokenRefused);
        });
        let (modal, cx) = dialog(vec![refused.clone()], cx);
        let reaches_token = |cx: &mut VisualTestContext| {
            modal.read_with(cx, |modal, cx| modal.places(cx).contains(&Place::Token))
        };
        assert!(reaches_token(cx));
        // A token that never arrived: one typed would be dropped the same way.
        refused.update(cx, |connection, _| {
            connection.refusal = Some(Unauthorized::TokenNotReceived)
        });
        assert!(
            !reaches_token(cx),
            "a token that never arrived hides the field"
        );
        refused.update(cx, |connection, _| {
            connection.refusal = Some(Unauthorized::TokenRefused)
        });
        refused.update(cx, |connection, _| connection.hold_sign_in_for_tests());
        assert!(!reaches_token(cx), "a sign-in under way hides the field");
    }

    /// The chosen server's line offers Retry beside a sign-in held wherever connecting again is
    /// what helps: the server unreachable, the sign-in's service not answering, or sign-in
    /// details that could not be read, or a token that never arrived. One the server does not
    /// admit offers another account instead. Before, a sign-in held hid Retry, so an unreachable server waited for its next
    /// check, up to two minutes.
    #[gpui::test]
    fn retry_stands_beside_a_sign_in_where_connecting_again_helps(cx: &mut TestAppContext) {
        let connection = server(&[], cx);
        let offers = |state: ConnectionState,
                      sign_in: SignIn,
                      unread: Option<&str>,
                      cx: &mut TestAppContext| {
            connection.update(cx, |connection, _| {
                connection.state = state;
                connection.sign_in = sign_in;
                connection.token_unread = unread.map(str::to_owned);
                connection.refusal = Some(Unauthorized::TokenRefused);
                line_actions(connection)
            })
        };
        let signed_in = || SignIn::SignedIn {
            service: "id.example".into(),
            account: "user_1".into(),
        };
        assert_eq!(
            offers(
                ConnectionState::Unreachable("down".into()),
                signed_in(),
                None,
                cx
            ),
            [LineAction::SignOut, LineAction::Retry]
        );
        assert_eq!(
            offers(
                ConnectionState::Unauthorized,
                signed_in(),
                Some("the sign-in service did not answer"),
                cx
            ),
            [LineAction::SignOut, LineAction::Retry]
        );
        assert_eq!(
            offers(ConnectionState::Unauthorized, signed_in(), None, cx),
            [LineAction::SignInAsSomeoneElse, LineAction::SignOut]
        );
        // A token that never arrived: Retry, once whatever drops it is put right.
        let not_received = connection.update(cx, |connection, _| {
            connection.state = ConnectionState::Unauthorized;
            connection.sign_in = signed_in();
            connection.token_unread = None;
            connection.refusal = Some(Unauthorized::TokenNotReceived);
            line_actions(connection)
        });
        assert_eq!(not_received, [LineAction::SignOut, LineAction::Retry]);
        // So does a sign-in as someone else that failed, the one held standing.
        let offered = SignIn::Offered {
            service: "id.example".into(),
            why: Some("refused".into()),
        };
        assert_eq!(
            actions_for(&ConnectionState::Unauthorized, &offered, true, true),
            [
                LineAction::SignIn {
                    service: "id.example".into()
                },
                LineAction::SignOut,
                LineAction::Retry
            ]
        );
        assert_eq!(
            offers(
                ConnectionState::Unauthorized,
                SignIn::Unavailable(graph_store::sign_in::Refusal::passing(
                    "the details did not answer"
                )),
                None,
                cx
            ),
            [LineAction::Retry]
        );
        assert_eq!(
            offers(ConnectionState::Ready, signed_in(), None, cx),
            [LineAction::SignOut]
        );
    }

    /// A sign-in held offers Sign out wherever it stands: while the details are read again for
    /// a sign-in as someone else, and while connecting. Before, reading them again offered
    /// nothing, the one held standing.
    #[gpui::test]
    fn a_sign_in_held_offers_sign_out_while_another_is_read(_: &mut TestAppContext) {
        assert_eq!(
            actions_for(
                &ConnectionState::Unauthorized,
                &SignIn::Reading,
                true,
                false
            ),
            [LineAction::SignOut]
        );
        assert_eq!(
            actions_for(
                &ConnectionState::Unauthorized,
                &SignIn::Reading,
                false,
                false
            ),
            []
        );
        assert_eq!(
            actions_for(
                &ConnectionState::Connecting,
                &SignIn::NotOffered,
                true,
                false
            ),
            [LineAction::Cancel, LineAction::SignOut]
        );
    }

    /// A server that offers a sign-in says where, while waiting says to finish in the browser,
    /// and signed in says as whom; one that refuses the account signed in says so (spec 016).
    #[gpui::test]
    fn a_server_that_offers_a_sign_in_says_where_and_as_whom(cx: &mut TestAppContext) {
        let connection = server(&[], cx);
        // A 401 here refused the token sent.
        let said = |state: ConnectionState, sign_in: SignIn, cx: &mut TestAppContext| {
            connection.update(cx, |connection, _| {
                connection.state = state;
                connection.refusal = Some(Unauthorized::TokenRefused);
                connection.sign_in = sign_in;
                (server_status(connection), connection.label())
            })
        };
        let offered = SignIn::Offered {
            service: "id.example".into(),
            why: None,
        };
        assert_eq!(
            said(ConnectionState::Unauthorized, offered, cx),
            (
                "This server asks you to sign in at id.example.".to_owned(),
                "Sign in needed".to_owned()
            )
        );
        let ended = SignIn::Offered {
            service: "id.example".into(),
            why: Some("Signed out: The sign-in is no longer valid. Sign in again.".into()),
        };
        assert_eq!(
            said(ConnectionState::Unauthorized, ended, cx).0,
            "Signed out: The sign-in is no longer valid. Sign in again. The sign-in is at \
             id.example."
        );
        let waiting = SignIn::Waiting {
            service: "id.example".into(),
        };
        assert_eq!(
            said(ConnectionState::Unauthorized, waiting, cx),
            (
                "Finish signing in at id.example in your browser.".to_owned(),
                "Signing in…".to_owned()
            )
        );
        let finishing = SignIn::Finishing {
            service: "id.example".into(),
        };
        assert_eq!(
            said(ConnectionState::Unauthorized, finishing, cx),
            (
                "Finishing the sign-in at id.example.".to_owned(),
                "Signing in…".to_owned()
            )
        );
        let signed_in = || SignIn::SignedIn {
            service: "id.example".into(),
            account: "user_1".into(),
        };
        // The signed-in line, by what the keychain holds for the target.
        let line = |kept: Kept| signed_in_status("user_1", "id.example", &kept);
        assert_eq!(line(Kept::This), "Signed in as user_1 at id.example.");
        assert!(line(Kept::Nothing).ends_with("lasts until Studio quits."));
        assert_eq!(
            line(Kept::Left("locked".into())),
            "Signed in as user_1 at id.example. The keychain did not keep the sign-in."
        );
        // The line reads the connection's custody: this one holds no sign-in, so nothing kept.
        assert!(
            said(ConnectionState::Ready, signed_in(), cx)
                .0
                .ends_with("lasts until Studio quits.")
        );
        // An item the keychain kept that nobody signs in with is said whatever the state (this
        // connection holds no sign-in, so it reads what was left).
        connection.update(cx, |connection, _| {
            connection.set_left_for_tests(Kept::Left("locked".into()))
        });
        let left = "The keychain kept a sign-in it would not delete (locked), which Studio may \
                    read again later.";
        assert_eq!(
            said(ConnectionState::Ready, signed_in(), cx).0,
            format!(
                "Signed in as user_1 at id.example. The keychain did not keep the sign-in. {left}"
            )
        );
        assert!(
            said(ConnectionState::Unauthorized, signed_in(), cx)
                .0
                .ends_with(&format!("may not admit this account. {left}"))
        );
        // A token that never arrived reads so, not as an account the server does not admit.
        let not_received = connection.update(cx, |connection, _| {
            connection.state = ConnectionState::Unauthorized;
            connection.refusal = Some(Unauthorized::TokenNotReceived);
            connection.sign_in = signed_in();
            (server_status(connection), connection.label())
        });
        assert!(
            not_received.0.starts_with(
                "The server received no token, though Studio sent the one signed in at id.example"
            ),
            "{not_received:?}"
        );
        assert_eq!(not_received.1, "Token not received");
        let offered = || SignIn::Offered {
            service: "id.example".into(),
            why: None,
        };
        assert_eq!(
            said(ConnectionState::Unauthorized, offered(), cx).0,
            format!("This server asks you to sign in at id.example. {left}")
        );
        let unreachable = said(
            ConnectionState::Unreachable("offline".into()),
            offered(),
            cx,
        )
        .0;
        assert!(unreachable.ends_with(left), "{unreachable}");
        connection.update(cx, |connection, _| {
            connection.set_left_for_tests(Kept::Nothing)
        });
        assert_eq!(
            said(ConnectionState::Unauthorized, signed_in(), cx),
            (
                "The server refused user_1, signed in at id.example: it may not admit this \
                 account."
                    .to_owned(),
                "Not admitted".to_owned()
            )
        );
        // A sign-in whose service did not answer stands; it was not refused.
        connection.update(cx, |connection, _| {
            connection.token_unanswered = true;
            connection.token_unread =
                Some("The sign-in service could not be reached: offline.".into());
        });
        assert_eq!(
            said(ConnectionState::Unauthorized, signed_in(), cx),
            (
                "The sign-in service could not be reached: offline. You stay signed in at \
                 id.example; Studio asks again."
                    .to_owned(),
                "Sign-in not answering".to_owned()
            )
        );
    }

    /// A 401 to a request that carried no token says the server asks for one, not that it
    /// refused one; a CLI server the CLI gave no token says why; Update token names the CLI's
    /// own command, and no command the CLI lacks.
    #[gpui::test]
    fn a_server_sent_no_token_is_said_to_ask_for_one(cx: &mut TestAppContext) {
        let connection = server(&[], cx);
        connection.update(cx, |connection, cx| connection.set_token(None, cx));
        let said = |refusal: Unauthorized, cx: &mut TestAppContext| {
            connection.update(cx, |connection, _| {
                connection.state = ConnectionState::Unauthorized;
                connection.refusal = Some(refusal);
                (server_status(connection), connection.label())
            })
        };
        assert_eq!(
            said(Unauthorized::TokenNeeded, cx),
            (
                "This server asks for a token; none is configured for it.".to_owned(),
                "Asks for a token".to_owned()
            )
        );
        // One sent that never arrived, as a proxy drops it.
        assert_eq!(
            said(Unauthorized::TokenNotReceived, cx),
            (
                "The server received no token, though Studio sent one: something between them \
                 may drop it."
                    .to_owned(),
                "Token not received".to_owned()
            )
        );
        assert_eq!(
            said(Unauthorized::TokenRefused, cx),
            (
                "The server refused the token.".to_owned(),
                "Token refused".to_owned()
            )
        );
        connection.update(cx, |connection, _| {
            connection.token_unread = Some("No credential is configured for 'cloud'".into());
        });
        assert_eq!(
            said(Unauthorized::TokenNeeded, cx),
            (
                "The Omnigraph CLI gave no token for this server: No credential is configured \
                 for 'cloud'"
                    .to_owned(),
                "No token from the CLI".to_owned()
            )
        );
        let tooltip = |source: ServerSource, platform: Platform, cx: &mut TestAppContext| {
            connection.update(cx, |connection, _| {
                connection.server.source = source;
                connection.server.name = "cloud".into();
                token_tooltip(connection, platform)
            })
        };
        let cli = tooltip(ServerSource::Cli, Platform::Unix, cx);
        assert!(
            cli.contains("`omnigraph credentials set --target cloud --stdin`"),
            "{cli}"
        );
        assert!(!cli.contains("login"), "{cli}");
        // A target the CLI gives tokens for asks it again over a typed token (Refresh, a change
        // to the target or to the CLI), so the token is not said to last the run.
        assert!(
            cli.starts_with("Used until Studio quits or asks the CLI for this target's token"),
            "{cli}"
        );
        // Where the CLI exports no token, its command would make a target Studio refuses: the
        // tooltip says the token lasts this run, and names no command.
        assert_eq!(
            tooltip(ServerSource::Cli, Platform::Other, cx),
            "Used until Studio quits. Studio on Windows cannot use a target that needs a token \
             yet (the Omnigraph CLI gives a token only through a Unix pipe), so the CLI cannot \
             keep this one for it."
        );
        assert_eq!(
            tooltip(ServerSource::Environment, Platform::Other, cx),
            "Used until Studio quits."
        );
        assert_eq!(source_label(&ServerSource::Cli, None), "CLI target");

        // A managed cluster reached with the CLI's token names its profile and how it signs in.
        let login = tooltip_for(&ServerSource::Cli, "cloud", Some("work"), Platform::Unix);
        assert!(
            login.contains("from its profile work")
                && login.contains("`omnigraph login --profile work`")
                && login.contains("used until Studio quits or asks the CLI"),
            "{login}"
        );
        assert_eq!(
            source_label(&ServerSource::Cli, Some("work")),
            "CLI target, CLI profile work"
        );

        // A server the CLI gave no token is asked again when chosen, as its Retry does: the
        // person may have signed in since. A token the server refused is not asked again by a
        // click.
        let gave_none = |reason: Option<&str>, cx: &mut TestAppContext| {
            connection.update(cx, |connection, _| {
                connection.state = ConnectionState::Unauthorized;
                connection.token_unread = reason.map(str::to_owned);
                retries_on_select(connection)
            })
        };
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("typed".into()), cx);
            connection.refusal = Some(Unauthorized::TokenRefused);
        });
        assert!(!gave_none(None, cx), "a refused token");
        connection.update(cx, |connection, cx| connection.set_token(None, cx));
        assert!(gave_none(Some("login required"), cx));
        let (modal, cx) = dialog(vec![connection.clone()], cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.select(connection.clone(), window, cx)
        });
        assert_eq!(
            connection.read_with(cx, |connection, _| connection.state.clone()),
            ConnectionState::Connecting
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

    /// Opened before the CLI's targets were listed, the dialog shows its start screen; when they
    /// land, the first is chosen, as it would have been, unless the form was typed in or the
    /// recipe is shown.
    #[gpui::test]
    fn the_first_server_listed_after_the_dialog_opened_is_chosen(cx: &mut TestAppContext) {
        let alpha = server(&["one"], cx);
        let (modal, cx) = dialog(Vec::new(), cx);
        modal.read_with(cx, |modal, _| assert_eq!(modal.start, Some(Start::Choices)));
        modal.update_in(cx, |modal, window, cx| {
            modal.follow(vec![alpha.clone()], window, cx)
        });
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.selected, Some(alpha.clone()));
            assert!(!modal.adding);
            assert_eq!(modal.start, None);
            assert_eq!(modal.cursor, Some(Place::Server(alpha.entity_id())));
        });

        let gamma = server(&["three"], cx);
        let (modal, cx) = dialog(Vec::new(), cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.show_start(Start::Local, window, cx);
            modal.follow(vec![gamma.clone()], window, cx)
        });
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.start, Some(Start::Local), "the recipe stays");
        });

        let beta = server(&["two"], cx);
        let (modal, cx) = dialog(Vec::new(), cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.show_add(true, window, cx);
            modal.new_url.update(cx, |editor, cx| {
                editor.set_text("https://typed.test", window, cx)
            });
            modal.follow(vec![beta.clone()], window, cx)
        });
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.selected, None, "what was typed stays");
            assert!(modal.adding);
        });
    }

    /// A graph id typed in is remembered only where the server does not say which graphs the
    /// token reaches: one it does not list is a mistyped id, which would be offered for good.
    #[gpui::test]
    fn a_graph_id_is_remembered_only_where_the_server_lists_none(cx: &mut TestAppContext) {
        for (graphs, remembered) in [
            (GraphListing::Discovered(vec!["knowledge".into()]), false),
            (GraphListing::NotAllowed, true),
        ] {
            let connection = server(&[], cx);
            connection.update(cx, |connection, _| connection.graphs = graphs.clone());
            let (modal, cx) = dialog(vec![connection.clone()], cx);
            modal.update_in(cx, |modal, window, cx| {
                modal.open("knowlege".into(), window, cx)
            });
            connection.read_with(cx, |connection, _| {
                assert_eq!(
                    connection
                        .server
                        .known_graphs
                        .contains(&"knowlege".to_owned()),
                    remembered,
                    "{graphs:?}"
                );
            });
        }
    }

    /// A server added clears the fields it was added from, so a second is typed fresh; a field
    /// typed in while the add ran keeps what was typed. Before, every field was cleared.
    #[gpui::test]
    fn a_field_typed_in_while_a_server_is_added_keeps_its_text(cx: &mut TestAppContext) {
        let (registry, _) = cx.update(ConnectionRegistry::init_for_tests);
        registry.update(cx, |registry, _| {
            // Where the CLI gives a token, so the token typed is taken (not on Windows).
            registry.set_platform_for_tests(Platform::Unix);
            registry.add_targets_in_memory_for_tests();
        });
        let (modal, cx) = dialog(Vec::new(), cx);
        modal.update_in(cx, |modal, window, cx| {
            let typed = |editor: &Entity<Editor>, text: &str, window: &mut Window, cx: &mut App| {
                editor.update(cx, |editor, cx| editor.set_text(text, window, cx))
            };
            typed(&modal.new_name, "work", window, cx);
            typed(&modal.new_url, "https://graph.test", window, cx);
            typed(&modal.new_token, "secret", window, cx);
            modal.add_server(window, cx);
            // The next server's address, typed while the first is added.
            typed(&modal.new_url, "https://next.test", window, cx);
        });
        cx.run_until_parked();
        modal.read_with(cx, |modal, cx| {
            assert_eq!(modal.error, None, "added");
            let text = |editor: &Entity<Editor>| editor.read(cx).text(cx);
            assert_eq!(text(&modal.new_name), "");
            assert_eq!(text(&modal.new_url), "https://next.test");
            assert_eq!(text(&modal.new_token), "");
        });
    }

    /// Which of Add server's fields has the keyboard.
    fn add_field_focused(
        modal: &Entity<OpenGraphModal>,
        cx: &mut VisualTestContext,
    ) -> &'static str {
        cx.update(|window, cx| {
            let modal = modal.read(cx);
            let fields = [
                ("address", &modal.new_url),
                ("name", &modal.new_name),
                ("token", &modal.new_token),
                ("graph", &modal.new_graph),
            ];
            fields
                .into_iter()
                .find(|(_, editor)| editor.focus_handle(cx).is_focused(window))
                .map_or("none", |(name, _)| name)
        })
    }

    /// The fields Tab visits in Add server, from the address, three steps on.
    fn add_tab_order(
        modal: &Entity<OpenGraphModal>,
        cx: &mut VisualTestContext,
    ) -> Vec<&'static str> {
        modal.update_in(cx, |modal, window, cx| modal.show_add(true, window, cx));
        let mut visited = vec![add_field_focused(modal, cx)];
        for _ in 0..4 {
            cx.dispatch_action(menu::SelectNext);
            visited.push(add_field_focused(modal, cx));
        }
        visited
    }

    /// Where the CLI gives Studio no token (Windows), Add server's token field is disabled and
    /// says why: it is drawn without an editor, Tab passes it by, and its reason names the Unix
    /// pipe. A token that reaches Add server anyway is refused by the registry, and said, with
    /// nothing added and nothing sent.
    #[gpui::test]
    fn without_token_export_the_new_servers_token_field_is_disabled(cx: &mut TestAppContext) {
        let (registry, asked) = cx.update(ConnectionRegistry::init_for_tests);
        registry.update(cx, |registry, _| {
            registry.set_platform_for_tests(Platform::Other)
        });
        let (modal, cx) = dialog_on(Vec::new(), Platform::Other, cx);
        assert_eq!(
            modal
                .read_with(cx, |modal, _| modal.token_refusal())
                .as_deref(),
            Some(
                "Studio on Windows adds only servers that need no token: the Omnigraph CLI \
                 gives a token only through a Unix pipe."
            )
        );
        assert_eq!(
            add_tab_order(&modal, cx),
            ["address", "name", "graph", "address", "name"]
        );
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("new-token-disabled").is_some(),
            "drawn disabled"
        );

        modal.update_in(cx, |modal, window, cx| {
            for (editor, text) in [
                (&modal.new_url, "https://new.test"),
                (&modal.new_token, "secret-typed"),
            ] {
                editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
            }
            modal.add_server(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            modal
                .read_with(cx, |modal, _| modal.error.clone())
                .as_deref(),
            Some(
                "The server was not added: the Omnigraph CLI gives a token only through a Unix \
                 pipe, so Studio on Windows cannot use a server that needs a token yet; leave \
                 the token empty to add one that needs none"
            )
        );
        assert!(registry.read_with(cx, |registry, _| registry.connections.is_empty()));
        assert!(asked.lock().unwrap().is_empty(), "nothing sent");
    }

    /// A profile's control plane is shown under the servers with its sign-in, and lists no
    /// clusters, nor sends anything, before a sign-in is held (spec 021).
    #[gpui::test]
    fn a_control_plane_without_a_sign_in_offers_one(cx: &mut TestAppContext) {
        let (modal, cx) = dialog(vec![server(&["knowledge"], cx)], cx);
        let plane = cx.new(|cx| {
            let mut plane = ControlPlane::new("https://control.test".into(), None, cx);
            plane.profiles = vec![("work".into(), Some("org_1".into()))];
            plane
        });
        modal.update(cx, |modal, cx| {
            modal.follow_planes(vec![plane.clone()], Vec::new(), cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("control-plane-sign-in").is_some());
        assert!(cx.debug_bounds("control-plane-clusters").is_none());
        assert!(plane.read_with(cx, |plane, _| plane.organizations().is_none()));
    }

    /// ↓ from the last server reaches a control plane's line, before the chosen server's
    /// graphs; ↵ there asks the plane to sign in (constitution VIII).
    #[gpui::test]
    fn the_keyboard_reaches_a_control_plane(cx: &mut TestAppContext) {
        let alpha = server(&["one"], cx);
        let (modal, cx) = dialog(vec![alpha.clone()], cx);
        let plane = cx.new(|cx| {
            let mut plane = ControlPlane::new("https://control.test".into(), None, cx);
            plane.profiles = vec![("work".into(), Some("org_1".into()))];
            plane
        });
        modal.update(cx, |modal, cx| {
            modal.follow_planes(vec![plane.clone()], Vec::new(), cx)
        });
        let places = modal.read_with(cx, |modal, cx| modal.places(cx));
        assert_eq!(
            places[..2],
            [
                Place::Server(alpha.entity_id()),
                Place::Plane(plane.entity_id())
            ]
        );
        cx.dispatch_action(menu::SelectNext);
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.cursor.clone()),
            Some(Place::Plane(plane.entity_id()))
        );
        // Without a sign-in context there is nothing to sign in with: ↵ leaves it offered.
        cx.dispatch_action(menu::Confirm);
        assert!(plane.read_with(cx, |plane, _| matches!(
            plane.sign_in_state(),
            SignIn::Offered { .. }
        )));
    }

    /// A control plane signed in to, listing `org_1` (Acme) with two clusters: `acme` (`01C`),
    /// which [`managed`]'s target names, and `beta`, which opens from the control plane.
    fn signed_in_plane(cx: &mut VisualTestContext) -> Entity<ControlPlane> {
        let cluster = |id: &str, name: &str| {
            serde_json::json!({
                "cluster_id": id, "name": name, "incarnation": "01I",
                "canonical_root": "s3://b/c", "binding": {"mode": "github", "repository": "a/g"},
                "state": "ready", "cell": {"namespace": "ns", "cluster": "cell"},
                "created_by": "u", "created_at": "2026-11-01T00:00:00Z",
                "data_endpoint": format!("https://{name}.test"),
            })
        };
        let listed: control_client::Envelope<control_client::ClusterList> =
            serde_json::from_value(serde_json::json!({
                "data": {"clusters": [cluster("01C", "acme"), cluster("02C", "beta")]},
                "meta": {"provenance": "service_db", "assurance": "observed", "stale": false,
                         "observed_at": "2026-11-05T00:00:00Z"},
            }))
            .unwrap();
        cx.new(|cx| {
            let mut plane = ControlPlane::new("https://control.test".into(), None, cx);
            plane.signed_in_for_test(
                vec![control_client::OrganizationSummary {
                    organization_id: "org_1".into(),
                    name: Some("Acme".into()),
                }],
                vec![("org_1".into(), graph_store::Listing::Listed(listed))],
                cx,
            );
            plane
        })
    }

    fn managed(target: &str) -> Vec<ManagedTarget> {
        vec![ManagedTarget {
            name: target.into(),
            profile: "work".into(),
            cluster_id: "01C".into(),
            cluster_incarnation: "01I".into(),
            endpoint: None,
            default_graph: None,
        }]
    }

    /// Signed in, a control plane's organizations are in the sidebar by name; choosing one shows
    /// its clusters in the pane, in place of the server's graphs, and ↵ goes to its first
    /// cluster (spec 021).
    #[gpui::test]
    fn an_organization_chosen_in_the_sidebar_shows_its_clusters(cx: &mut TestAppContext) {
        let alpha = server(&["one"], cx);
        let (modal, cx) = dialog(vec![alpha.clone()], cx);
        let target = alpha.read_with(cx, |c, _| c.server.name.clone());
        let plane = signed_in_plane(cx);
        modal.update(cx, |modal, cx| {
            modal.follow_planes(vec![plane.clone()], managed(&target), cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("control-plane-clusters").is_none());
        let organization = Place::Organization(plane.entity_id(), "org_1".into());
        let places = modal.read_with(cx, |modal, cx| modal.places(cx));
        assert_eq!(places[1], organization);
        cx.dispatch_action(menu::SelectNext);
        cx.run_until_parked();
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.cursor.clone()),
            Some(organization)
        );
        assert!(cx.debug_bounds("control-plane-clusters").is_some());
        assert!(cx.debug_bounds("open-graph").is_none());
        // Both clusters are reached: one a target names, one that opens from the control plane.
        let places = modal.read_with(cx, |modal, cx| modal.places(cx));
        let acme = Place::Cluster(plane.entity_id(), "org_1".into(), "01C".into());
        let beta = Place::Cluster(plane.entity_id(), "org_1".into(), "02C".into());
        assert_eq!(places[2..], [acme.clone(), beta]);
        cx.dispatch_action(menu::Confirm);
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.cursor.clone()),
            Some(acme)
        );
    }

    /// ↵ on a cluster a CLI target names chooses that target's server, and the pane shows its
    /// graphs again.
    #[gpui::test]
    fn a_cluster_a_target_names_chooses_its_server(cx: &mut TestAppContext) {
        let alpha = server(&["one"], cx);
        let beta = server(&["two"], cx);
        // The test servers share a name: the second is the target's.
        beta.update(cx, |c, _| c.server.name = "prod".into());
        let (modal, cx) = dialog(vec![alpha.clone(), beta.clone()], cx);
        let target = "prod".to_owned();
        let plane = signed_in_plane(cx);
        modal.update(cx, |modal, cx| {
            modal.follow_planes(vec![plane.clone()], managed(&target), cx)
        });
        let acme = Place::Cluster(plane.entity_id(), "org_1".into(), "01C".into());
        modal.update_in(cx, |modal, window, cx| {
            modal.move_to(
                Place::Organization(plane.entity_id(), "org_1".into()),
                window,
                cx,
            );
            modal.move_to(acme, window, cx);
        });
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        let (selected, shown) = modal.read_with(cx, |modal, _| {
            (modal.selected.clone(), modal.plane_shown.clone())
        });
        assert_eq!(selected.map(|c| c.entity_id()), Some(beta.entity_id()));
        assert_eq!(shown, None);
        assert!(cx.debug_bounds("open-graph").is_some());
    }

    /// A server with more graphs than the dialog holds scrolls them within it: the id field and
    /// Open stay in the dialog, and the row the keyboard moves to is scrolled into view.
    #[gpui::test]
    fn a_long_graph_list_scrolls_within_the_dialog(cx: &mut TestAppContext) {
        let names: Vec<String> = (0..40).map(|i| format!("graph-{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let many = server(&names, cx);
        let (modal, cx) = dialog(vec![many], cx);
        cx.run_until_parked();
        let dialog = cx.debug_bounds("open-graph-dialog").unwrap();
        let open = cx.debug_bounds("open-graph").unwrap();
        assert!(open.bottom() <= dialog.bottom(), "{open:?} in {dialog:?}");
        modal.update_in(cx, |modal, window, cx| {
            modal.move_to(Place::Graph("graph-39".into()), window, cx)
        });
        cx.run_until_parked();
        let scrolled = modal.read_with(cx, |modal, _| modal.graphs_scroll.offset().y);
        assert!(scrolled < px(0.), "{scrolled:?}");
    }

    /// Where the CLI gives Studio a token (Unix), Add server's token field is a field like the
    /// others, in Tab's way.
    #[gpui::test]
    fn the_new_servers_token_field_takes_a_token_where_the_cli_exports_one(
        cx: &mut TestAppContext,
    ) {
        let (modal, cx) = dialog_on(Vec::new(), Platform::Unix, cx);
        assert_eq!(modal.read_with(cx, |modal, _| modal.token_refusal()), None);
        assert_eq!(
            add_tab_order(&modal, cx),
            ["address", "name", "token", "graph", "address"]
        );
        cx.run_until_parked();
        assert!(cx.debug_bounds("new-token-disabled").is_none());
    }
}
