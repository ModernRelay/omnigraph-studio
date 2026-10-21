//! The command palette (spec 002): every action available where the person is, with its
//! binding, fuzzy filtered and ranked by recent use, plus the graph's types, nodes found by id
//! or name, queries (open tabs, saved files, stored queries and their aliases) and branches.
//! Scope tabs narrow it (All ⌘1, Types ⌘2, Nodes ⌘3, Queries ⌘4, Branches ⌘5, Commands ⌘6);
//! Tab hands the text to the agent. ⌘K opens All, ⌘P Nodes, ⌘⇧B Branches; `?` opens the
//! keyboard shortcuts available where the person is.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
// Written after zed-industries/zed crates/command_palette/src/command_palette.rs @ v1.23.2: the
// palette lists `window.available_actions` captured from the previously focused element,
// restores that focus, then dispatches. Studio does not depend on Zed's `picker`, which needs
// `project` and `workspace`; the list, matching and persistence are Studio's own.

mod found;
mod matching;
mod recent;

use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    Render, ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity,
    Window, uniform_list,
};
use gq_language::SchemaModel;
use gq_language::builder::{node_search, search_matches};
use graph_store::{ConnectionRegistry, EntityKind, GraphSession, Loadable};
use serde_json::Value;
use studio_actions::agent::Ask;
use studio_actions::graph::{OpenNode, OpenQueryFile, OpenStoredQuery, OpenType};
use studio_actions::palette::{AskAgent, ShowScope};
use studio_actions::workspace::{GoTo, ShowKeyBindings, SwitchBranch, ToggleCommandPalette};
use studio_ui::ds::{MenuRow, elevation, radii, size, space, surface};
use studio_workspace::{ToastKind, Workspace};
use ui::HighlightedLabel;
use ui::prelude::*;

use found::{Answers, Found};
use matching::humanize_action_name;
use matching::{Candidate, Group, Pattern, Ranked, normalize_query, rank};
use studio_workspace::{ModalPlacement, ModalView};

/// Registers the palette with every new workspace. Its keys (⌘K and ⌘⇧P, Ctrl elsewhere) come
/// from the keymap files (`assets/keymaps/`), as every key does.
pub fn init(cx: &mut App) {
    recent::load(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleCommandPalette, window, cx| {
            open(workspace, Mode::Picker(Scope::All), window, cx);
        });
        workspace.register_action(|workspace, _: &GoTo, window, cx| {
            open(workspace, Mode::Picker(Scope::Nodes), window, cx);
        });
        workspace.register_action(|workspace, _: &SwitchBranch, window, cx| {
            open(workspace, Mode::Picker(Scope::Branches), window, cx);
        });
        workspace.register_action(|workspace, _: &ShowKeyBindings, window, cx| {
            open(workspace, Mode::Keys, window, cx);
        });
    })
    .detach();
}

fn open(
    workspace: &mut Workspace,
    mode: Mode,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let session = workspace.session().cloned();
    if session.is_none() && matches!(mode, Mode::Picker(Scope::Nodes | Scope::Branches)) {
        workspace.show_toast(ToastKind::Info, "Open a graph first.", cx);
        return;
    }
    // Open query tabs, for the Queries scope: the tab, its title and its first line; then the
    // graph's saved query files that no tab has open.
    let mut open_paths = Vec::new();
    let queries: Vec<(EntityId, String, String)> = workspace
        .all_items()
        .filter_map(|item| {
            let saved = item.serialize(cx).filter(|saved| saved.kind == "query")?;
            if let Some(path) = saved.data.get("path").and_then(Value::as_str) {
                open_paths.push(path.to_owned());
            }
            let first = saved
                .data
                .get("text")
                .and_then(Value::as_str)
                .and_then(|text| text.lines().map(str::trim).find(|l| !l.is_empty()))
                .unwrap_or_default()
                .to_owned();
            Some((item.item_id(), item.tab_title(cx).to_string(), first))
        })
        .collect();
    let saved = SavedQueries {
        graph: session
            .as_ref()
            .map(|session| session.read(cx).settings_key(cx)),
        open: open_paths,
    };
    let handle = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, move |window, cx| {
        CommandPalette::new(mode, session, handle, queries, saved, window, cx)
    });
}

/// The graph whose saved query files the picker lists, and the files a tab has open already,
/// which it leaves out.
#[derive(Default)]
pub(crate) struct SavedQueries {
    graph: Option<String>,
    open: Vec<String>,
}

impl SavedQueries {
    /// The saved files no tab has open, newest first: read off the main thread, the folder
    /// listed and each file's time read.
    fn read(self) -> Vec<String> {
        let Some(graph) = self.graph else {
            return Vec::new();
        };
        studio_settings::saved_queries(&graph)
            .into_iter()
            .map(|(path, _)| path.to_string_lossy().into_owned())
            .filter(|path| !self.open.contains(path))
            .collect()
    }
}

/// A slice of what the picker lists (spec 002, scope tabs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    All,
    Types,
    Nodes,
    Queries,
    Branches,
    Commands,
}

impl Scope {
    const TABS: [Scope; 6] = [
        Scope::All,
        Scope::Types,
        Scope::Nodes,
        Scope::Queries,
        Scope::Branches,
        Scope::Commands,
    ];

    fn title(self) -> &'static str {
        match self {
            Scope::All => "All",
            Scope::Types => "Types",
            Scope::Nodes => "Nodes",
            Scope::Queries => "Queries",
            Scope::Branches => "Branches",
            Scope::Commands => "Commands",
        }
    }

    fn includes(self, group: Group) -> bool {
        match self {
            Scope::All => true,
            Scope::Types => group == Group::Type,
            Scope::Nodes => group == Group::Node,
            Scope::Queries => group == Group::Query,
            Scope::Branches => group == Group::Branch,
            Scope::Commands => matches!(group, Group::Studio | Group::Other),
        }
    }

    /// Whether typing looks for nodes on the server.
    fn searches_nodes(self) -> bool {
        matches!(self, Scope::All | Scope::Nodes)
    }
}

/// The picker, in one of its scopes, or the keyboard-shortcut sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Picker(Scope),
    /// `?`: every action with a key binding where the person is.
    Keys,
}

impl Mode {
    fn placeholder(self) -> &'static str {
        match self {
            Mode::Picker(Scope::All) => "Search commands, types, nodes, queries and branches…",
            Mode::Picker(Scope::Types) => "Open a type…",
            Mode::Picker(Scope::Nodes) => "Find a node by id or name…",
            Mode::Picker(Scope::Queries) => "Go to a query…",
            Mode::Picker(Scope::Branches) => "Switch to a branch…",
            Mode::Picker(Scope::Commands) => "Run a command…",
            Mode::Keys => "Search keyboard shortcuts…",
        }
    }

    fn noun(self, count: usize) -> &'static str {
        let one = count == 1;
        match self {
            Mode::Picker(Scope::Types) => ["types", "type"][one as usize],
            Mode::Picker(Scope::Nodes) => ["nodes", "node"][one as usize],
            Mode::Picker(Scope::Queries) => ["queries", "query"][one as usize],
            Mode::Picker(Scope::Branches) => ["branches", "branch"][one as usize],
            Mode::Picker(Scope::Commands) => ["commands", "command"][one as usize],
            Mode::Picker(Scope::All) => ["results", "result"][one as usize],
            Mode::Keys => ["shortcuts", "shortcut"][one as usize],
        }
    }

    fn empty(self, typed: bool, searching: bool) -> &'static str {
        match self {
            Mode::Picker(Scope::Nodes | Scope::All) if searching => "Searching nodes…",
            Mode::Picker(Scope::Nodes) if !typed => "Type an id or a name to find nodes.",
            Mode::Picker(Scope::Nodes) => "No node matches.",
            Mode::Picker(Scope::Types) => "No type matches.",
            Mode::Picker(Scope::Queries) => "No query matches.",
            Mode::Picker(Scope::Branches) => "No branch matches.",
            Mode::Picker(Scope::Commands) => "No command matches.",
            Mode::Picker(Scope::All) => "Nothing matches.",
            Mode::Keys => "No shortcut matches.",
        }
    }

    fn scope(self) -> Option<Scope> {
        match self {
            Mode::Picker(scope) => Some(scope),
            Mode::Keys => None,
        }
    }
}

/// How long typing must pause before ⌘P looks for nodes.
const NODE_SEARCH_DELAY: Duration = Duration::from_millis(150);
/// Nodes found per type, and in all.
const NODES_PER_TYPE: u32 = 5;
const NODES_SHOWN: usize = 30;
/// Node types read for one text before the palette waits to be asked: a graph of 120 types
/// would otherwise send 120 reads at every pause. Eight is as many as a connection reads at
/// once (`graph_client::READ_CONCURRENCY`), so a pause's reads go out together; "Search more
/// types" reads the next eight.
const TYPES_PER_SEARCH: usize = 8;
/// The key of the "Search more types" row.
const MORE_TYPES: &str = "node-search:more";

/// What a row does when run.
enum Row {
    Action {
        action: Box<dyn Action>,
        description: Option<&'static str>,
    },
    Type {
        action: Box<dyn Action>,
        kind: &'static str,
        count: Option<i64>,
    },
    Node {
        action: Box<dyn Action>,
        type_name: String,
    },
    Branch {
        name: String,
        current: bool,
    },
    /// An open query tab: its first line.
    Tab {
        id: EntityId,
        first_line: String,
    },
    /// A saved query file of this graph.
    Saved {
        action: Box<dyn Action>,
    },
    /// A stored query the server serves for this graph.
    Stored {
        action: Box<dyn Action>,
        mutation: bool,
        description: Option<String>,
    },
    /// An alias from the CLI config for a stored query this graph serves.
    Alias {
        action: Box<dyn Action>,
        query: String,
    },
    /// The node types not yet read for the text: running it reads the next ones.
    MoreTypes {
        count: usize,
    },
}

impl Row {
    fn action(&self) -> Option<&dyn Action> {
        match self {
            Row::Action { action, .. }
            | Row::Type { action, .. }
            | Row::Node { action, .. }
            | Row::Saved { action }
            | Row::Stored { action, .. }
            | Row::Alias { action, .. } => Some(action.as_ref()),
            Row::Branch { .. } | Row::Tab { .. } | Row::MoreTypes { .. } => None,
        }
    }
}

const ROW_HEIGHT: f32 = studio_ui::ds::size::PALETTE_ROW;
/// Space above the first row and below the last. The list's height includes it, so the rows
/// fit without scrolling and a selected row is never clipped by the edge.
const LIST_PAD: f32 = studio_ui::ds::space::XS;
const VISIBLE_ROWS: usize = 11;

pub(crate) struct CommandPalette {
    mode: Mode,
    session: Option<Entity<GraphSession>>,
    workspace: WeakEntity<Workspace>,
    previous_focus: Option<FocusHandle>,
    query: Entity<Editor>,
    rows: Vec<Row>,
    candidates: Vec<Candidate>,
    /// Rows before this index are fixed; after it, nodes found for the current text.
    fixed: usize,
    matches: Vec<Ranked>,
    selected: usize,
    scroll: UniformListScrollHandle,
    /// The pause in typing before the node reads go out.
    node_search: Option<Task<()>>,
    /// The text the node rows are, or are being, found for: a scope that shows nodes looks
    /// them up again when the text moved on in one that does not.
    nodes_for: String,
    /// Counts the texts searched: an answer for an earlier one is dropped.
    search: u64,
    /// The nodes found for the text, in the order they came: a type that answers later adds
    /// rows below those shown and moves none of them (constitution V).
    nodes: Vec<(String, Found)>,
    /// The node types not yet read for the text, in the order they would be: what "Search
    /// more types" reads next.
    unread_types: Vec<String>,
    /// One read per node type asked for the text. Dropped, and so stopped, when it changes.
    node_reads: Vec<Task<()>>,
    /// Of those, the reads not yet answered.
    pending_reads: usize,
    /// The graph's saved query files, being read.
    _saved: Option<Task<()>>,
    /// Nodes are being looked up for the current text: from the keystroke, through the pause,
    /// until every type read has answered.
    searching: bool,
    /// What each node type answered, shown again or narrowed without a read.
    answers: Answers,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for CommandPalette {}

impl ModalView for CommandPalette {
    fn placement(&self) -> ModalPlacement {
        ModalPlacement::Palette
    }
}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl CommandPalette {
    /// Builds the picker from what the focused element offers. Called before focus moves to
    /// the picker, so `available_actions` and the bindings are those of where the person was.
    fn new(
        mode: Mode,
        session: Option<Entity<GraphSession>>,
        workspace: WeakEntity<Workspace>,
        queries: Vec<(EntityId, String, String)>,
        saved: SavedQueries,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let previous_focus = window.focused(cx);
        let mut rows = Vec::new();
        let mut candidates = Vec::new();
        add_action_rows(
            mode,
            previous_focus.as_ref(),
            window,
            cx,
            &mut rows,
            &mut candidates,
        );
        if let (Some(session), Mode::Picker(_)) = (&session, mode) {
            add_type_rows(session, window, cx, &mut rows, &mut candidates);
            add_branch_rows(session, cx, &mut rows, &mut candidates);
            add_stored_rows(session, window, cx, &mut rows, &mut candidates);
            for (id, title, first_line) in queries {
                candidates.push(Candidate {
                    label: if first_line.is_empty() {
                        title.clone()
                    } else {
                        format!("{title} · {first_line}")
                    },
                    // By the tab, not its title: two files of one name are two tabs.
                    key: format!("query-tab:{}", id.as_u64()),
                    group: Group::Query,
                });
                rows.push(Row::Tab { id, first_line });
            }
        }
        let read_saved = matches!(mode, Mode::Picker(_)) && saved.graph.is_some();

        let query = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(mode.placeholder(), window, cx);
            editor
        });
        let subscription = cx.subscribe(&query, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                // New text starts on its first match; the node search keeps that row.
                this.update_matches(cx);
                if this.mode.scope().is_some_and(Scope::searches_nodes) {
                    this.schedule_node_search(cx);
                }
            }
        });
        let fixed = rows.len();
        let mut palette = Self {
            mode,
            session,
            workspace,
            previous_focus,
            query,
            rows,
            candidates,
            fixed,
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            node_search: None,
            nodes_for: String::new(),
            search: 0,
            nodes: Vec::new(),
            unread_types: Vec::new(),
            node_reads: Vec::new(),
            pending_reads: 0,
            searching: false,
            answers: Answers::default(),
            _saved: None,
            _subscription: subscription,
        };
        if read_saved {
            let read = cx.background_spawn(async move { saved.read() });
            palette._saved = Some(cx.spawn(async move |this, cx| {
                let saved = read.await;
                this.update(cx, |this, cx| this.add_saved(saved, cx)).ok();
            }));
        }
        palette.update_matches(cx);
        palette
    }

    /// Lists the saved query files with the fixed rows, before any node found.
    fn add_saved(&mut self, saved: Vec<String>, cx: &mut Context<Self>) {
        if saved.is_empty() {
            return;
        }
        let (rows, candidates): (Vec<Row>, Vec<Candidate>) = saved
            .into_iter()
            .map(|path| {
                let name = std::path::Path::new(&path)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                let candidate = Candidate {
                    label: name,
                    key: format!("query-file:{path}"),
                    group: Group::Query,
                };
                let row = Row::Saved {
                    action: Box::new(OpenQueryFile { path: Some(path) }),
                };
                (row, candidate)
            })
            .unzip();
        let selected = self.selected_key();
        let at = self.fixed;
        self.fixed += rows.len();
        self.rows.splice(at..at, rows);
        self.candidates.splice(at..at, candidates);
        self.update_matches(cx);
        self.select_key(selected, cx);
    }

    /// After a pause in typing, looks up nodes whose id starts with, or label contains, the
    /// text: one read per node type, at most [`TYPES_PER_SEARCH`] of them, in
    /// [`search_order`]; the rest wait for "Search more types". Each type's rows show as it
    /// answers. Rows found for the earlier text that the server would not return for this one
    /// go at once; the others stay until their type answers. A type whose earlier answer holds
    /// every match is narrowed without a read; when every type is, the rows change at once.
    fn schedule_node_search(&mut self, cx: &mut Context<Self>) {
        let text = self.query.read(cx).text(cx).trim().to_owned();
        let Some(session) = self.session.clone() else {
            return;
        };
        self.nodes_for = text.clone();
        self.search += 1;
        // The reads for the earlier text stop.
        self.node_search = None;
        self.node_reads.clear();
        self.pending_reads = 0;
        self.unread_types.clear();
        self.searching = false;
        if text.chars().count() < 2 {
            self.nodes.clear();
            self.show_nodes(cx);
            return;
        }
        self.nodes
            .retain(|(_, found)| search_matches(&found.id, found.label.as_deref(), &text));
        let types = {
            let session = session.read(cx);
            session
                .schema_model()
                .map(|schema| {
                    search_order(&schema, |name| session.count_of(EntityKind::Node, name))
                })
                .unwrap_or_default()
        };
        let mut unread = Vec::new();
        for type_name in types {
            match self.answers.known(&type_name, &text) {
                Some(rows) => self.landed(&type_name, rows),
                None => unread.push(type_name),
            }
        }
        self.unread_types = unread.split_off(unread.len().min(TYPES_PER_SEARCH));
        if !unread.is_empty() {
            self.searching = true;
            let search = self.search;
            self.node_search = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(NODE_SEARCH_DELAY).await;
                this.update(cx, |this, cx| this.read_types(search, text, unread, cx))
                    .ok();
            }));
        }
        self.show_nodes(cx);
    }

    /// Sends one read per type of `types` for `text`, each shown as it answers.
    fn read_types(
        &mut self,
        search: u64,
        text: String,
        types: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let schema = session.read(cx).schema_model();
        for type_name in types {
            let Some(query) = schema
                .as_ref()
                .and_then(|schema| node_search(schema, &type_name, &text, NODES_PER_TYPE).ok())
            else {
                continue;
            };
            let read = session.update(cx, |session, cx| session.read(&query, None, cx));
            let text = text.clone();
            self.pending_reads += 1;
            self.node_reads.push(cx.spawn(async move |this, cx| {
                // A type that fails to answer simply has no rows; the others still show.
                let rows = read.await.ok().and_then(|output| found_rows(&output.rows));
                this.update(cx, |this, cx| {
                    this.type_answered(search, type_name, text, rows, cx)
                })
                .ok();
            }));
        }
        self.searching = self.pending_reads > 0;
        cx.notify();
    }

    /// One type's answer for the text of search `search`, `None` when it failed.
    fn type_answered(
        &mut self,
        search: u64,
        type_name: String,
        text: String,
        rows: Option<Vec<Found>>,
        cx: &mut Context<Self>,
    ) {
        if search != self.search {
            return;
        }
        self.pending_reads = self.pending_reads.saturating_sub(1);
        self.searching = self.pending_reads > 0;
        if let Some(rows) = &rows {
            let limit = NODES_PER_TYPE as usize;
            self.answers
                .keep(type_name.clone(), text, rows.clone(), limit);
        }
        self.landed(&type_name, rows.unwrap_or_default());
        self.show_nodes(cx);
    }

    /// "Search more types": reads the next [`TYPES_PER_SEARCH`] node types for the text. The
    /// palette stays open, on this row while types are left.
    fn search_more(&mut self, cx: &mut Context<Self>) {
        let next = self.unread_types.len().min(TYPES_PER_SEARCH);
        let types: Vec<String> = self.unread_types.drain(..next).collect();
        self.read_types(self.search, self.nodes_for.clone(), types, cx);
        self.show_nodes(cx);
    }

    /// Takes `type_name`'s answer for the text: its rows shown that the answer no longer holds
    /// go, a label that changed changes in place, and rows not shown yet are added below every
    /// row shown, up to [`NODES_SHOWN`].
    fn landed(&mut self, type_name: &str, rows: Vec<Found>) {
        self.nodes.retain(|(shown, found)| {
            shown != type_name || rows.iter().any(|row| row.id == found.id)
        });
        for row in rows {
            let at = self
                .nodes
                .iter()
                .position(|(shown, found)| shown == type_name && found.id == row.id);
            match at {
                Some(at) => self.nodes[at].1 = row,
                None if self.nodes.len() < NODES_SHOWN => {
                    self.nodes.push((type_name.to_owned(), row))
                }
                None => {}
            }
        }
    }

    /// Lists the nodes found after the fixed rows, then "Search more types" while types are
    /// left. The row the person moved to stays theirs when nodes come in: ↵ runs it, not the
    /// first row of the new list.
    fn show_nodes(&mut self, cx: &mut Context<Self>) {
        let selected = self.selected_key();
        self.rows.truncate(self.fixed);
        self.candidates.truncate(self.fixed);
        for (type_name, Found { id, label }) in &self.nodes {
            let shown = match label {
                Some(label) if label != id => format!("{label} · {id}"),
                _ => id.clone(),
            };
            self.candidates.push(Candidate {
                label: shown,
                key: format!("node:{type_name}:{id}"),
                group: Group::Node,
            });
            self.rows.push(Row::Node {
                action: Box::new(OpenNode {
                    type_name: type_name.clone(),
                    id: id.clone(),
                }),
                type_name: type_name.clone(),
            });
        }
        let count = self.unread_types.len();
        if count > 0 {
            let noun = ["types", "type"][(count == 1) as usize];
            self.candidates.push(Candidate {
                label: format!("Search {count} more {noun}"),
                key: MORE_TYPES.into(),
                group: Group::Node,
            });
            self.rows.push(Row::MoreTypes { count });
        }
        self.update_matches(cx);
        self.select_key(selected, cx);
    }

    /// Highlights the row with candidate key `key` again, where it is ranked now.
    fn select_key(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        if let Some(index) = key.and_then(|key| {
            self.matches
                .iter()
                .position(|ranked| self.candidates[ranked.index].key == key)
        }) {
            self.select(index, cx);
        }
    }

    /// The key of the highlighted row's candidate.
    fn selected_key(&self) -> Option<String> {
        let ranked = self.matches.get(self.selected)?;
        Some(self.candidates.get(ranked.index)?.key.clone())
    }

    /// Shows another scope, keeping the text.
    fn set_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == Mode::Keys || self.mode == Mode::Picker(scope) {
            return;
        }
        self.mode = Mode::Picker(scope);
        let placeholder = self.mode.placeholder();
        self.query.update(cx, |query, cx| {
            query.set_placeholder_text(placeholder, window, cx)
        });
        if scope.searches_nodes() && self.nodes_for != self.query.read(cx).text(cx).trim() {
            self.schedule_node_search(cx);
        }
        self.update_matches(cx);
    }

    fn show_scope(&mut self, action: &ShowScope, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(scope) = Scope::TABS.get(action.index) {
            self.set_scope(*scope, window, cx);
        }
    }

    /// Tab: the text goes to the agent as a question (spec 011's Ask).
    fn ask_agent(&mut self, _: &AskAgent, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.query.read(cx).text(cx).trim().to_owned();
        if text.is_empty() {
            return;
        }
        match &self.previous_focus {
            Some(focus) => window.focus(focus, cx),
            None => window.blur(cx),
        }
        cx.emit(DismissEvent);
        window.dispatch_action(Box::new(Ask { text }), cx);
    }

    fn update_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text(cx);
        let scope = self.mode.scope();
        let include =
            |candidate: &Candidate| scope.is_none_or(|scope| scope.includes(candidate.group));
        let fixed = &self.candidates[..self.fixed];
        self.matches = rank(fixed, &query, &recent::keys(cx), include);
        // The nodes found follow in the order they came, not by score: a type that answers
        // later adds rows below and moves none shown. The server matched them; the pattern
        // only marks the letters.
        let pattern = Pattern::new(&normalize_query(&query));
        let found: Vec<Ranked> = self
            .candidates
            .iter()
            .enumerate()
            .skip(self.fixed)
            .filter(|(_, candidate)| include(candidate))
            .map(|(index, candidate)| Ranked {
                index,
                positions: pattern
                    .find(&candidate.label)
                    .map(|found| found.positions)
                    .unwrap_or_default(),
            })
            .collect();
        self.matches.extend(found);
        // The branch picker starts on the branch the window is on.
        self.selected = self
            .matches
            .iter()
            .position(|ranked| {
                query.is_empty()
                    && matches!(self.rows[ranked.index], Row::Branch { current: true, .. })
            })
            .unwrap_or(0);
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = index.min(self.matches.len() - 1);
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.matches.is_empty() {
            self.select((self.selected + 1) % self.matches.len(), cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.matches.is_empty() {
            let len = self.matches.len();
            self.select((self.selected + len - 1) % len, cx);
        }
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.matches.len().saturating_sub(1), cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.run_selected(window, cx);
    }

    /// Dismisses the palette, gives focus back to where the person was, then runs the row's
    /// action there.
    fn run_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ranked) = self.matches.get(self.selected) else {
            cx.emit(DismissEvent);
            return;
        };
        let row = &self.rows[ranked.index];
        if let Row::MoreTypes { .. } = row {
            self.search_more(cx);
            return;
        }
        let action = row.action().map(|action| action.boxed_clone());
        let branch = match row {
            Row::Branch { name, .. } => Some(name.clone()),
            _ => None,
        };
        let tab = match row {
            Row::Tab { id, .. } => Some(*id),
            _ => None,
        };
        // A tab is not a destination to remember across launches.
        if tab.is_none() {
            recent::record(&self.candidates[ranked.index].key, cx);
        }
        match &self.previous_focus {
            Some(focus) => window.focus(focus, cx),
            None => window.blur(cx),
        }
        cx.emit(DismissEvent);
        if let Some(action) = action {
            window.dispatch_action(action, cx);
        }
        if let (Some(branch), Some(session)) = (branch, &self.session) {
            session.update(cx, |session, cx| session.set_branch(branch, cx));
        }
        if let Some(id) = tab {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.activate_item_by_id(id, window, cx)
                })
                .ok();
        }
    }

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        range
            .filter_map(|ix| {
                let ranked = self.matches.get(ix)?;
                let candidate = &self.candidates[ranked.index];
                let row = &self.rows[ranked.index];
                // A click runs the row clicked, by its key: rows that came in since this frame
                // was drawn (nodes found, saved queries) can have moved it.
                let key = candidate.key.clone();
                let label =
                    HighlightedLabel::new(candidate.label.clone(), ranked.positions.clone())
                        .truncate();
                let muted = |text: String| studio_ui::ds::text::muted(text).into_any_element();
                let end: AnyElement = match row {
                    Row::Action { action, .. } => match &self.previous_focus {
                        Some(focus) => ui::KeyBinding::for_action_in(action.as_ref(), focus, cx)
                            .into_any_element(),
                        None => ui::KeyBinding::for_action(action.as_ref(), cx).into_any_element(),
                    },
                    Row::Type { kind, count, .. } => muted(match count {
                        Some(count) => format!("{kind} · {count} rows"),
                        None => (*kind).to_owned(),
                    }),
                    Row::Node { type_name, .. } => muted(type_name.clone()),
                    Row::Branch { current, .. } => muted(if *current {
                        "current".into()
                    } else {
                        String::new()
                    }),
                    Row::Tab { .. } => muted("open tab".into()),
                    Row::Saved { .. } => muted("saved".into()),
                    Row::Stored { mutation, .. } => muted(if *mutation {
                        "stored · writes".into()
                    } else {
                        "stored".into()
                    }),
                    Row::Alias { query, .. } => muted(format!("alias · {query}")),
                    Row::MoreTypes { count } => {
                        muted(format!("next {}", (*count).min(TYPES_PER_SEARCH)))
                    }
                };
                let start: Option<gpui::Div> = match row {
                    Row::Type { kind, .. } => Some(studio_ui::kind_swatch(
                        &candidate.label,
                        *kind == "edge",
                        cx,
                    )),
                    Row::Node { type_name, .. } => Some(studio_ui::type_swatch(type_name, cx)),
                    Row::Branch { current, .. } => {
                        Some(div().child(studio_ui::ds::icon::lead(if *current {
                            IconName::Check
                        } else {
                            IconName::GitBranch
                        })))
                    }
                    Row::Tab { .. } | Row::Saved { .. } => {
                        Some(div().child(studio_ui::ds::icon::lead(IconName::Code)))
                    }
                    Row::Stored { .. } => {
                        Some(div().child(studio_ui::ds::icon::lead(IconName::Server)))
                    }
                    Row::Alias { .. } => {
                        Some(div().child(studio_ui::ds::icon::lead(IconName::Link)))
                    }
                    Row::MoreTypes { .. } => {
                        Some(div().child(studio_ui::ds::icon::lead(IconName::MagnifyingGlass)))
                    }
                    Row::Action { .. } => None,
                };
                Some(
                    div()
                        .h(px(ROW_HEIGHT))
                        .child(
                            MenuRow::new(ix)
                                .height(ROW_HEIGHT)
                                .radius(radii(cx).palette_row())
                                .toggle_state(ix == self.selected)
                                .when_some(start, |row, start| row.start_slot(start))
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap(px(space::MD))
                                        .justify_between()
                                        .child(label)
                                        .child(div().flex_none().child(end)),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let at = this.matches.iter().position(|ranked| {
                                        this.candidates[ranked.index].key == key
                                    });
                                    if let Some(at) = at {
                                        this.selected = at;
                                        this.run_selected(window, cx);
                                    }
                                })),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn selected_description(&self) -> Option<SharedString> {
        let ranked = self.matches.get(self.selected)?;
        match &self.rows[ranked.index] {
            Row::Action { description, .. } => description.map(|d| SharedString::new(d.trim())),
            Row::Type { kind, .. } => Some(format!("Open this {kind} type's table.").into()),
            Row::Node { .. } => Some("Show this node in the inspector and the tree.".into()),
            Row::Branch {
                name,
                current: true,
            } => Some(format!("This window is on {name}.").into()),
            Row::Branch { name, .. } => {
                Some(format!("Read and edit on {name} in this window.").into())
            }
            Row::Tab { first_line, .. } if !first_line.is_empty() => {
                Some(format!("Go to this tab: {first_line}").into())
            }
            Row::Tab { .. } => Some("Go to this tab.".into()),
            Row::Saved { .. } => Some("Open this saved query in a tab.".into()),
            Row::Stored {
                description: Some(description),
                ..
            } => Some(description.clone().into()),
            Row::Stored { .. } => {
                Some("Open this stored query in a tab, to run on the server.".into())
            }
            Row::Alias { query, .. } => {
                Some(format!("Open {query} with this alias's values from the CLI config.").into())
            }
            Row::MoreTypes { count } => Some(
                format!(
                    "Look for “{}” in the next {} node types. A search reads {TYPES_PER_SEARCH} \
                     at a time, the types with a name first.",
                    self.nodes_for,
                    (*count).min(TYPES_PER_SEARCH)
                )
                .into(),
            ),
        }
    }
}

/// Actions that make no sense as palette rows: the palette itself, list navigation, and the
/// global handlers Zed's `editor::init` installs for Zed's own workspace (`workspace::NewFile`
/// and `workspace::NewWindow` read Zed's `AppState`, which Studio does not have).
fn hidden(name: &str) -> bool {
    name == "studio::ToggleCommandPalette"
        || name.starts_with("menu::")
        || name.starts_with("workspace::")
        || name.starts_with("zed::")
}

/// Rows for the actions available where the person is. The key sheet keeps only those with a
/// binding there.
fn add_action_rows(
    mode: Mode,
    focus: Option<&FocusHandle>,
    window: &mut Window,
    cx: &mut Context<CommandPalette>,
    rows: &mut Vec<Row>,
    candidates: &mut Vec<Candidate>,
) {
    let documentation = cx.action_documentation();
    for action in window.available_actions(cx) {
        let name = action.name();
        if hidden(name) {
            continue;
        }
        if mode == Mode::Keys {
            let binding = match focus {
                Some(focus) => {
                    window.highest_precedence_binding_for_action_in(action.as_ref(), focus)
                }
                None => window.highest_precedence_binding_for_action(action.as_ref()),
            };
            if binding.is_none() {
                continue;
            }
        }
        candidates.push(Candidate {
            label: humanize_action_name(name),
            key: name.to_owned(),
            group: if name.starts_with("studio") {
                Group::Studio
            } else {
                Group::Other
            },
        });
        rows.push(Row::Action {
            description: documentation.get(name).copied(),
            action,
        });
    }
}

/// Rows for the session's branches, the current one marked.
fn add_branch_rows(
    session: &Entity<GraphSession>,
    cx: &mut Context<CommandPalette>,
    rows: &mut Vec<Row>,
    candidates: &mut Vec<Candidate>,
) {
    let session = session.read(cx);
    let Loadable::Loaded(branches) = &session.branches else {
        return;
    };
    for name in branches {
        candidates.push(Candidate {
            label: name.clone(),
            key: format!("branch:{name}"),
            group: Group::Branch,
        });
        rows.push(Row::Branch {
            current: *name == session.branch,
            name: name.clone(),
        });
    }
}

/// Rows for the graph's stored queries, each followed by the CLI config's aliases for it, when
/// opening one is available where the person is. An alias for a query the graph does not
/// serve cannot open, and is left out.
fn add_stored_rows(
    session: &Entity<GraphSession>,
    window: &mut Window,
    cx: &mut Context<CommandPalette>,
    rows: &mut Vec<Row>,
    candidates: &mut Vec<Candidate>,
) {
    let open = |name: &str, alias: Option<&str>| OpenStoredQuery {
        name: name.to_owned(),
        alias: alias.map(str::to_owned),
    };
    if !window.is_action_available(&open("", None), cx) {
        return;
    }
    let session = session.read(cx);
    let Loadable::Loaded(queries) = &session.stored_queries else {
        return;
    };
    let aliases = ConnectionRegistry::try_global(cx)
        .map(|registry| {
            registry
                .read(cx)
                .aliases_for(&session.server_name(cx), &session.graph_id)
        })
        .unwrap_or_default();
    for query in queries.iter() {
        candidates.push(Candidate {
            label: query.name.clone(),
            key: format!("stored-query:{}", query.name),
            group: Group::Query,
        });
        rows.push(Row::Stored {
            action: Box::new(open(&query.name, None)),
            mutation: query.mutation,
            description: query.description.clone(),
        });
        for alias in aliases.iter().filter(|alias| alias.query == query.name) {
            candidates.push(Candidate {
                label: alias.name.clone(),
                key: format!("query-alias:{}", alias.name),
                group: Group::Query,
            });
            rows.push(Row::Alias {
                action: Box::new(open(&query.name, Some(&alias.name))),
                query: query.name.clone(),
            });
        }
    }
}

/// The node types a search reads, in order: those a property names (a name is what a person
/// types) before those found by id alone, and types known to hold no rows last; each in
/// schema order.
fn search_order(schema: &SchemaModel, count: impl Fn(&str) -> Option<i64>) -> Vec<String> {
    let mut types: Vec<_> = schema
        .nodes
        .iter()
        .enumerate()
        .map(|(at, node)| {
            let named = node
                .label_property()
                .is_some_and(|label| Some(label) != node.key.as_deref());
            let empty = count(&node.name) == Some(0);
            ((empty, !named, at), node.name.clone())
        })
        .collect();
    types.sort();
    types.into_iter().map(|(_, name)| name).collect()
}

/// The nodes a `node_search` read answered: each row's id, and its label when the type has one.
fn found_rows(rows: &Value) -> Option<Vec<Found>> {
    let rows = rows.as_array()?;
    Some(
        rows.iter()
            .filter_map(|row| {
                Some(Found {
                    id: row.get("id")?.as_str()?.to_owned(),
                    label: row.get("label").and_then(Value::as_str).map(str::to_owned),
                })
            })
            .collect(),
    )
}

/// Quick-open rows for the session's node and edge types, when opening a type is available
/// where the person is (constitution VIII).
fn add_type_rows(
    session: &Entity<GraphSession>,
    window: &mut Window,
    cx: &mut Context<CommandPalette>,
    rows: &mut Vec<Row>,
    candidates: &mut Vec<Candidate>,
) {
    let session = session.read(cx);
    let Some(schema) = session.schema_model() else {
        return;
    };
    let types = schema
        .nodes
        .iter()
        .map(|n| (n.name.clone(), "node", graph_store::EntityKind::Node))
        .chain(
            schema
                .edges
                .iter()
                .map(|e| (e.name.clone(), "edge", graph_store::EntityKind::Edge)),
        );
    let open = |type_name: String| OpenType {
        type_name,
        at_commit: None,
        filter_id: None,
        via: None,
        preview: false,
    };
    // Whether a type opens where the person is does not depend on which type it is.
    if !window.is_action_available(&open(String::new()), cx) {
        return;
    }
    for (name, kind, entity) in types {
        let action = open(name.clone());
        candidates.push(Candidate {
            label: name.clone(),
            key: format!("type:{name}"),
            group: Group::Type,
        });
        rows.push(Row::Type {
            count: session.count_of(entity, &name),
            action: Box::new(action),
            kind,
        });
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.matches.len();
        // Only a scope that shows nodes waits on them.
        let searching = self.searching && self.mode.scope().is_some_and(Scope::searches_nodes);
        let list = if count == 0 {
            div()
                .h(px(ROW_HEIGHT * 2.))
                .flex()
                .items_center()
                .justify_center()
                .child(studio_ui::ds::text::muted(
                    self.mode
                        .empty(!self.query.read(cx).is_empty(cx), searching),
                ))
                .into_any_element()
        } else {
            uniform_list(
                "command-palette-rows",
                count,
                cx.processor(|this, range, _window, cx| this.render_rows(range, cx)),
            )
            .track_scroll(&self.scroll)
            .h(px(
                ROW_HEIGHT * count.min(VISIBLE_ROWS) as f32 + 2. * LIST_PAD
            ))
            .p(px(LIST_PAD))
            .into_any_element()
        };
        let description = self.selected_description();
        // "Search more types" is a way on, not a result.
        let results = self
            .matches
            .iter()
            .filter(|ranked| !matches!(self.rows[ranked.index], Row::MoreTypes { .. }))
            .count();
        let typed = !self.query.read(cx).is_empty(cx);
        let focus = self.query.focus_handle(cx);
        // The scope tabs, as a segmented control; each carries its ⌘ number in the tooltip.
        let tabs = self.mode.scope().map(|current| {
            let mut tabs = studio_ui::ds::segmented(cx);
            for (index, scope) in Scope::TABS.into_iter().enumerate() {
                let focus = focus.clone();
                tabs = tabs.child(
                    studio_ui::ds::segment(
                        ("palette-scope", index),
                        scope.title(),
                        scope == current,
                        cx,
                    )
                    .tooltip(move |_, cx| {
                        ui::Tooltip::for_action_in(scope.title(), &ShowScope { index }, &focus, cx)
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.set_scope(scope, window, cx)),
                    ),
                );
            }
            h_flex().px(px(space::SM)).pb(px(space::XS)).child(tabs)
        });
        v_flex()
            .key_context("CommandPalette Picker")
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::show_scope))
            .on_action(cx.listener(Self::ask_agent))
            // Tower's palette: a glass frame holding one solid card, the results, inset by a
            // small gap and rounded concentrically; the search above it on the glass, the
            // description and the keys below.
            .w(px(size::PALETTE_WIDTH))
            .max_w_full()
            .p(px(space::XS))
            .gap(px(space::XS))
            .rounded(px(radii(cx).modal))
            .bg(surface::floating(cx))
            .border_1()
            .border_color(surface::floating_border(cx))
            .shadow(elevation::floating(cx))
            .child(
                h_flex()
                    .px(px(space::CONTROL_INSET))
                    .py(px(space::SM))
                    .gap(px(space::MD))
                    .child(studio_ui::ds::icon::lead(IconName::MagnifyingGlass))
                    .child(div().flex_1().child(self.query.clone()))
                    .when(searching, |row| {
                        row.child(
                            div()
                                .id("palette-searching")
                                .debug_selector(|| "palette-searching".into())
                                .child(studio_ui::ds::spinner())
                                .tooltip(ui::Tooltip::text("Searching nodes on the server")),
                        )
                    }),
            )
            .children(tabs)
            .child(
                div()
                    .rounded(px(radii(cx).palette_list()))
                    .bg(surface::document(cx))
                    .border_1()
                    .border_color(surface::floating_border(cx))
                    .shadow(elevation::raised(cx))
                    .overflow_hidden()
                    .child(list),
            )
            .child(
                h_flex()
                    .px(px(space::CONTROL_INSET))
                    .py(px(space::SM))
                    .gap(px(space::LG))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .children(description.map(|d| studio_ui::ds::text::meta(d).truncate())),
                    )
                    // No "0 nodes" while the nodes are still to come.
                    .when(!(searching && results == 0), |footer| {
                        footer.child(div().debug_selector(|| "palette-count".into()).child(
                            studio_ui::ds::text::meta(format!(
                                "{results} {}",
                                self.mode.noun(results)
                            )),
                        ))
                    })
                    .when(typed && self.mode != Mode::Keys, |footer| {
                        footer.child(studio_ui::ds::key_hint(&AskAgent, "Ask", &focus, cx))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use gpui::{TestAppContext, VisualTestContext};
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::json;

    use super::*;

    struct Host {
        focus: FocusHandle,
        palette: Option<Entity<CommandPalette>>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // Where the person was: opening a stored query is available there, as in the
            // workspace.
            div()
                .size_full()
                .track_focus(&self.focus)
                .on_action(|_: &OpenStoredQuery, _, _| {})
                .children(self.palette.clone())
        }
    }

    const PEOPLE: &str = "node Person {\n  slug: String @key\n  name: String\n}\n";

    /// What a fake server answers the n-th node search with: its rows, and how long it takes.
    type Find = Box<dyn Fn(usize) -> (Value, Duration) + Send + Sync>;

    /// The node searches a fake server was sent. Until `on`, every query answers at once with
    /// no rows: the session's own reads of its head.
    #[derive(Default)]
    struct Searches {
        on: AtomicBool,
        sent: AtomicUsize,
    }

    /// A server serving `schema`, with `catalog` as its stored queries, answering each node
    /// search as `find` says.
    fn served(
        schema: &str,
        catalog: Value,
        find: Find,
        cx: &mut TestAppContext,
    ) -> (Entity<GraphSession>, Arc<Searches>) {
        let schema: Arc<str> = schema.into();
        let searches = Arc::new(Searches::default());
        let (find, executor, counted) = (Arc::new(find), cx.executor(), searches.clone());
        let http = FakeHttpClient::create(move |request| {
            let (schema, catalog, find) = (schema.clone(), catalog.clone(), find.clone());
            let (executor, searches) = (executor.clone(), counted.clone());
            async move {
                let path = request.uri().path().to_owned();
                let body = if path.ends_with("/schema") {
                    json!({ "schema_source": &*schema })
                } else if path.ends_with("/queries") {
                    json!({ "queries": catalog })
                } else if path.ends_with("/query") {
                    let rows = if searches.on.load(Ordering::SeqCst) {
                        let n = searches.sent.fetch_add(1, Ordering::SeqCst);
                        let (rows, takes) = (find.as_ref())(n);
                        executor.timer(takes).await;
                        rows
                    } else {
                        json!([])
                    };
                    json!({
                        "query_name": "studio_find",
                        "target": {"branch": "main", "snapshot": null},
                        "row_count": rows.as_array().map_or(0, Vec::len),
                        "columns": ["id", "label"],
                        "rows": rows,
                        "graph_commit_id": "c1",
                    })
                } else {
                    json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
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
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        (session, searches)
    }

    /// A server whose graph has people; every read finds Ada.
    fn session(cx: &mut TestAppContext) -> Entity<GraphSession> {
        let ada = json!([{"id": "ada", "label": "Ada Lovelace"}]);
        let find: Find = Box::new(move |_| (ada.clone(), Duration::ZERO));
        let (session, searches) = served(PEOPLE, json!([]), find, cx);
        searches.on.store(true, Ordering::SeqCst);
        session
    }

    /// A palette in `scope` over `session`, made in a window already drawn, as the workspace
    /// makes it: it reads what the focused element offers.
    fn palette(
        scope: Scope,
        session: Entity<GraphSession>,
        cx: &mut TestAppContext,
    ) -> (Entity<CommandPalette>, &mut VisualTestContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        cx.run_until_parked();
        let (host, cx): (_, &mut VisualTestContext) = cx.add_window_view(|_, cx| Host {
            focus: cx.focus_handle(),
            palette: None,
        });
        cx.run_until_parked();
        let palette = host.update_in(cx, |host, window, cx| {
            window.focus(&host.focus, cx);
            let palette = cx.new(|cx| {
                CommandPalette::new(
                    Mode::Picker(scope),
                    Some(session),
                    WeakEntity::new_invalid(),
                    Vec::new(),
                    SavedQueries::default(),
                    window,
                    cx,
                )
            });
            host.palette = Some(palette.clone());
            cx.notify();
            palette
        });
        cx.run_until_parked();
        (palette, cx)
    }

    fn type_text(palette: &Entity<CommandPalette>, text: &str, cx: &mut VisualTestContext) {
        palette.update_in(cx, |palette, window, cx| {
            palette
                .query
                .update(cx, |query, cx| query.set_text(text, window, cx))
        });
        cx.run_until_parked();
    }

    /// The keys of the rows listed, in order.
    fn listed(palette: &Entity<CommandPalette>, cx: &mut VisualTestContext) -> Vec<String> {
        palette.read_with(cx, |palette, _| {
            palette
                .matches
                .iter()
                .map(|ranked| palette.candidates[ranked.index].key.clone())
                .collect()
        })
    }

    /// From the keystroke until the server answers, the palette says it is looking for
    /// nodes: a turning circle in the field, and "Searching nodes…" rather than "No node
    /// matches" while nothing has come back.
    #[gpui::test]
    async fn the_palette_shows_it_is_searching_nodes(cx: &mut TestAppContext) {
        let session = session(cx);
        let (palette, cx) = palette(Scope::Nodes, session, cx);
        type_text(&palette, "Ad", cx);
        palette.read_with(cx, |palette, _| {
            assert!(palette.searching);
            assert!(palette.matches.is_empty());
            assert_eq!(palette.mode.empty(true, true), "Searching nodes…");
        });
        assert!(cx.debug_bounds("palette-searching").is_some(), "no spinner");
        assert!(
            cx.debug_bounds("palette-count").is_none(),
            "a count of none"
        );

        cx.executor().advance_clock(NODE_SEARCH_DELAY);
        cx.run_until_parked();
        palette.read_with(cx, |palette, _| {
            assert!(!palette.searching);
            assert_eq!(palette.matches.len(), 1, "Ada was found");
        });
        assert!(cx.debug_bounds("palette-searching").is_none());
        assert!(cx.debug_bounds("palette-count").is_some());

        // Nodes that come in after the person moved keep the row they moved to, and are added
        // below the rows shown.
        palette.update(cx, |palette, cx| {
            let found = |ids: &[&str]| -> Vec<Found> {
                ids.iter()
                    .map(|id| Found {
                        id: (*id).to_owned(),
                        label: Some(format!("Ada {id}")),
                    })
                    .collect()
            };
            palette.landed("Person", found(&["p1", "p2"]));
            palette.show_nodes(cx);
            let p2 = palette
                .matches
                .iter()
                .position(|ranked| palette.candidates[ranked.index].key == "node:Person:p2")
                .unwrap();
            palette.select(p2, cx);
            palette.landed("Person", found(&["p0", "p1", "p2"]));
            palette.show_nodes(cx);
            assert_eq!(palette.selected_key().as_deref(), Some("node:Person:p2"));
            // So do saved queries that are listed later.
            palette.add_saved(vec!["/queries/people.gq".to_owned()], cx);
            assert_eq!(palette.selected_key().as_deref(), Some("node:Person:p2"));
        });
        assert_eq!(
            listed(&palette, cx),
            ["node:Person:p1", "node:Person:p2", "node:Person:p0"]
        );

        // Text typed in a scope without nodes is looked up on going back to one with them.
        palette.update_in(cx, |palette, window, cx| {
            palette.set_scope(Scope::Commands, window, cx);
            palette
                .query
                .update(cx, |query, cx| query.set_text("Bo", window, cx));
        });
        cx.run_until_parked();
        palette.update_in(cx, |palette, window, cx| {
            assert!(!palette.searching);
            palette.set_scope(Scope::Nodes, window, cx);
            assert!(palette.searching, "the node rows were found for other text");
        });
    }

    const THREE_TYPES: &str = "node Person {\n  slug: String @key\n  name: String\n}\n\
        node Company {\n  slug: String @key\n  name: String\n}\n\
        node Topic {\n  slug: String @key\n  title: String\n}\n";

    /// Each type's rows show as it answers, whichever answers first, and rows shown keep their
    /// place as later types come in. The server answers the first search sent last.
    #[gpui::test]
    async fn node_types_show_as_they_answer_and_keep_their_place(cx: &mut TestAppContext) {
        let find: Find = Box::new(|n| {
            let rows = json!([{"id": format!("n{n}"), "label": format!("Ada {n}")}]);
            (rows, Duration::from_millis(100 * (3 - n as u64)))
        });
        let (session, searches) = served(THREE_TYPES, json!([]), find, cx);
        let (palette, cx) = palette(Scope::Nodes, session.clone(), cx);
        searches.on.store(true, Ordering::SeqCst);
        type_text(&palette, "Ad", cx);
        cx.executor().advance_clock(NODE_SEARCH_DELAY);
        cx.run_until_parked();
        assert_eq!(searches.sent.load(Ordering::SeqCst), 3);

        let node = |key: &String| key.rsplit(':').next().unwrap_or_default().to_owned();
        let mut shown = Vec::new();
        for answered in ["n2", "n1", "n0"] {
            assert!(palette.read_with(cx, |palette, _| palette.searching));
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            shown.push(answered.to_owned());
            let listed: Vec<String> = listed(&palette, cx).iter().map(node).collect();
            assert_eq!(listed, shown, "the rows shown stay where they were");
        }
        assert!(!palette.read_with(cx, |palette, _| palette.searching));
    }

    /// New text stops the reads for the old: they leave the session's count of reads under
    /// way, and their answers never show.
    #[gpui::test]
    async fn new_text_stops_the_reads_for_the_old(cx: &mut TestAppContext) {
        let find: Find = Box::new(|n| {
            let rows = json!([{"id": format!("n{n}"), "label": format!("Ada {n}")}]);
            (rows, Duration::from_secs(1))
        });
        let (session, searches) = served(THREE_TYPES, json!([]), find, cx);
        let (palette, cx) = palette(Scope::Nodes, session.clone(), cx);
        searches.on.store(true, Ordering::SeqCst);
        type_text(&palette, "Ad", cx);
        cx.executor().advance_clock(NODE_SEARCH_DELAY);
        cx.run_until_parked();
        let in_flight =
            |cx: &mut VisualTestContext| session.read_with(cx, |s, _| s.reads_in_flight);
        assert_eq!(in_flight(cx), 3);

        type_text(&palette, "Ada", cx);
        assert_eq!(in_flight(cx), 0, "the old reads stopped");
        cx.executor().advance_clock(NODE_SEARCH_DELAY);
        cx.run_until_parked();
        assert_eq!(in_flight(cx), 3);
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        let listed = listed(&palette, cx);
        assert_eq!(listed.len(), 3, "only the new text's answers: {listed:?}");
        let new = |key: &String| ["n3", "n4", "n5"].iter().any(|n| key.ends_with(n));
        assert!(listed.iter().all(new));
    }

    /// A pause reads at most `TYPES_PER_SEARCH` types, those with a name first; "Search more
    /// types" reads the next ones and leaves the palette open.
    #[gpui::test]
    async fn a_search_reads_a_bounded_number_of_types_and_offers_the_rest(cx: &mut TestAppContext) {
        let mut schema = String::new();
        for n in 0..2 {
            schema.push_str(&format!("node Code{n} {{\n  slug: String @key\n}}\n"));
        }
        for n in 0..10 {
            schema.push_str(&format!(
                "node Named{n} {{\n  slug: String @key\n  name: String\n}}\n"
            ));
        }
        let find: Find = Box::new(|_| (json!([]), Duration::ZERO));
        let (session, searches) = served(&schema, json!([]), find, cx);
        let (palette, cx) = palette(Scope::Nodes, session, cx);
        searches.on.store(true, Ordering::SeqCst);
        type_text(&palette, "Ad", cx);
        cx.executor().advance_clock(NODE_SEARCH_DELAY);
        cx.run_until_parked();
        assert_eq!(searches.sent.load(Ordering::SeqCst), TYPES_PER_SEARCH);
        palette.read_with(cx, |palette, _| {
            assert_eq!(palette.unread_types, ["Named8", "Named9", "Code0", "Code1"]);
            assert_eq!(palette.selected_key().as_deref(), Some(MORE_TYPES));
            assert_eq!(
                palette.candidates.last().map(|c| c.label.as_str()),
                Some("Search 4 more types")
            );
        });

        let dismissed = Rc::new(Cell::new(false));
        let _dismissed = cx.update(|_, cx| {
            let dismissed = dismissed.clone();
            cx.subscribe(&palette, move |_, _: &DismissEvent, _| dismissed.set(true))
        });
        palette.update_in(cx, |palette, window, cx| palette.run_selected(window, cx));
        cx.run_until_parked();
        assert_eq!(searches.sent.load(Ordering::SeqCst), TYPES_PER_SEARCH + 4);
        assert!(!dismissed.get(), "the palette stays open");
        assert!(palette.read_with(cx, |palette, _| palette.unread_types.is_empty()));
        assert!(!listed(&palette, cx).iter().any(|key| key == MORE_TYPES));
    }

    #[test]
    fn named_types_are_searched_first_and_empty_ones_last() {
        let schema = SchemaModel::parse(
            "node Code {\n  slug: String @key\n}\n\
             node Empty {\n  slug: String @key\n  name: String\n}\n\
             node Person {\n  slug: String @key\n  name: String\n}\n\
             node Note {\n  slug: String @key\n  title: String\n}\n",
        )
        .unwrap();
        let count = |name: &str| (name == "Empty").then_some(0);
        assert_eq!(
            search_order(&schema, count),
            ["Note", "Person", "Code", "Empty"]
        );
    }

    /// The Queries scope lists the graph's stored queries, each runnable from the keyboard.
    #[gpui::test]
    async fn stored_queries_are_listed_in_the_queries_scope(cx: &mut TestAppContext) {
        let catalog = json!([
            {"name": "people_titled", "description": "People with a title"},
            {"name": "set_title", "mutation": true},
        ]);
        let find: Find = Box::new(|_| (json!([]), Duration::ZERO));
        let (session, _) = served(PEOPLE, catalog, find, cx);
        let (palette, cx) = palette(Scope::Queries, session, cx);
        assert_eq!(
            listed(&palette, cx),
            ["stored-query:people_titled", "stored-query:set_title"]
        );
        palette.read_with(cx, |palette, _| {
            let Row::Stored { action, .. } = &palette.rows[palette.matches[0].index] else {
                panic!("a stored query's row");
            };
            assert_eq!(
                action.as_any().downcast_ref::<OpenStoredQuery>(),
                Some(&OpenStoredQuery {
                    name: "people_titled".into(),
                    alias: None,
                })
            );
            assert_eq!(
                palette.selected_description().as_deref(),
                Some("People with a title")
            );
        });
    }

    #[test]
    fn rows_that_cannot_work_in_studio_are_hidden() {
        assert!(hidden("studio::ToggleCommandPalette"));
        assert!(hidden("menu::SelectNext"));
        assert!(hidden("workspace::NewFile"));
        assert!(!hidden("studio_query::Run"));
        assert!(!hidden("editor::SelectAll"));
    }
}
