//! The command palette (spec 002): every action available where the person is, with its
//! binding, fuzzy filtered and ranked by recent use, plus the graph's types, nodes found by id
//! or name, open query tabs and branches. Scope tabs narrow it (All ⌘1, Types ⌘2, Nodes ⌘3,
//! Queries ⌘4, Branches ⌘5, Commands ⌘6); Tab hands the text to the agent. ⌘K opens All, ⌘P
//! Nodes, ⌘⇧B Branches; `?` opens the keyboard shortcuts available where the person is.
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
use gq_language::builder::node_search;
use graph_store::{GraphSession, Loadable};
use serde_json::Value;
use studio_actions::agent::Ask;
use studio_actions::graph::{OpenNode, OpenQueryFile, OpenType};
use studio_actions::palette::{AskAgent, ShowScope};
use studio_actions::workspace::{GoTo, ShowKeyBindings, SwitchBranch, ToggleCommandPalette};
use studio_workspace::{ToastKind, Workspace};
use ui::prelude::*;
use ui::{HighlightedLabel, ListItem, ListItemSpacing};

use found::{Answers, Found};
use matching::{Candidate, Group, Ranked, rank};
pub use matching::{humanize_action_name, normalize_query};
use studio_workspace::ModalView;

/// Registers the palette with every new workspace and binds ⌘K and ⌘⇧P (Ctrl elsewhere) unless
/// the keymap already does.
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
    let saved: Vec<String> = session
        .as_ref()
        .map(|session| {
            studio_settings::saved_queries(&session.read(cx).settings_key(cx))
                .into_iter()
                .map(|(path, _)| path.to_string_lossy().into_owned())
                .filter(|path| !open_paths.contains(path))
                .collect()
        })
        .unwrap_or_default();
    let handle = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, move |window, cx| {
        CommandPalette::new(mode, session, handle, queries, saved, window, cx)
    });
}

/// A slice of what the picker lists (spec 002, scope tabs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
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
pub enum Mode {
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
}

impl Row {
    fn action(&self) -> Option<&dyn Action> {
        match self {
            Row::Action { action, .. }
            | Row::Type { action, .. }
            | Row::Node { action, .. }
            | Row::Saved { action } => Some(action.as_ref()),
            Row::Branch { .. } | Row::Tab { .. } => None,
        }
    }
}

const ROW_HEIGHT: f32 = 32.;
/// Space above the first row and below the last. The list's height includes it, so the rows
/// fit without scrolling and a selected row is never clipped by the edge.
const LIST_PAD: f32 = 4.;
const VISIBLE_ROWS: usize = 11;

pub struct CommandPalette {
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
    node_search: Option<Task<()>>,
    /// Nodes are being looked up for the current text: from the keystroke, through the pause,
    /// until every type has answered.
    searching: bool,
    /// What each node type answered, shown again or narrowed without a read.
    answers: Answers,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for CommandPalette {}

impl ModalView for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl CommandPalette {
    /// Builds the picker from what the focused element offers. Called before focus moves to
    /// the picker, so `available_actions` and the bindings are those of where the person was.
    pub fn new(
        mode: Mode,
        session: Option<Entity<GraphSession>>,
        workspace: WeakEntity<Workspace>,
        queries: Vec<(EntityId, String, String)>,
        saved: Vec<String>,
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
            for (id, title, first_line) in queries {
                candidates.push(Candidate {
                    label: if first_line.is_empty() {
                        title.clone()
                    } else {
                        format!("{title} · {first_line}")
                    },
                    key: format!("query-tab:{title}"),
                    group: Group::Query,
                });
                rows.push(Row::Tab { id, first_line });
            }
            for path in saved {
                let name = std::path::Path::new(&path)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                candidates.push(Candidate {
                    label: name,
                    key: format!("query-file:{path}"),
                    group: Group::Query,
                });
                rows.push(Row::Saved {
                    action: Box::new(OpenQueryFile { path: Some(path) }),
                });
            }
        }

        let query = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(mode.placeholder(), window, cx);
            editor
        });
        let subscription = cx.subscribe(&query, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
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
            searching: false,
            answers: Answers::default(),
            _subscription: subscription,
        };
        palette.update_matches(cx);
        palette
    }

    /// After a pause in typing, looks up nodes of every node type whose id starts with, or
    /// label contains, the text; replaces the node rows with what comes back. Until then the
    /// earlier node rows stay, ranked against the new text like every row (the matches were
    /// just ranked over them). A type whose earlier answer holds every match is narrowed
    /// without a read; when every type is, the rows change at once.
    fn schedule_node_search(&mut self, cx: &mut Context<Self>) {
        let text = self.query.read(cx).text(cx).trim().to_owned();
        let Some(session) = self.session.clone() else {
            return;
        };
        if text.chars().count() < 2 {
            self.node_search = None;
            self.searching = false;
            self.rows.truncate(self.fixed);
            self.candidates.truncate(self.fixed);
            self.update_matches(cx);
            return;
        }
        let types: Vec<String> = session
            .read(cx)
            .schema_model()
            .map(|schema| schema.nodes.iter().map(|node| node.name.clone()).collect())
            .unwrap_or_default();
        let mut narrowed: Vec<(String, Option<Vec<Found>>)> = types
            .into_iter()
            .map(|type_name| {
                let rows = self.answers.known(&type_name, &text);
                (type_name, rows)
            })
            .collect();
        if !narrowed.is_empty() && narrowed.iter().all(|(_, rows)| rows.is_some()) {
            self.node_search = None;
            self.searching = false;
            let found = narrowed
                .into_iter()
                .map(|(type_name, rows)| (type_name, rows.unwrap_or_default()))
                .collect();
            self.set_nodes(found, cx);
            return;
        }
        self.searching = true;
        cx.notify();
        self.node_search = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NODE_SEARCH_DELAY).await;
            let pending = session.update(cx, |session, cx| {
                let Some(schema) = session.schema_model() else {
                    return Vec::new();
                };
                schema
                    .nodes
                    .iter()
                    .map(|node| {
                        let known = narrowed
                            .iter_mut()
                            .find(|(type_name, _)| *type_name == node.name)
                            .and_then(|(_, rows)| rows.take());
                        let read = match known {
                            Some(_) => None,
                            None => node_search(&schema, &node.name, &text, NODES_PER_TYPE)
                                .ok()
                                .map(|query| session.read(&query, None, cx)),
                        };
                        (node.name.clone(), known, read)
                    })
                    .collect::<Vec<_>>()
            });
            let mut found = Vec::new();
            let mut answered = Vec::new();
            for (type_name, known, read) in pending {
                if let Some(rows) = known {
                    found.push((type_name, rows));
                    continue;
                }
                // A type that fails to answer simply has no rows; the others still show.
                let Some(Ok(output)) = (match read {
                    Some(read) => Some(read.await),
                    None => None,
                }) else {
                    continue;
                };
                let Value::Array(rows) = output.rows else {
                    continue;
                };
                let rows: Vec<Found> = rows
                    .iter()
                    .filter_map(|row| {
                        Some(Found {
                            id: row.get("id")?.as_str()?.to_owned(),
                            label: row.get("label").and_then(Value::as_str).map(str::to_owned),
                        })
                    })
                    .collect();
                answered.push((type_name.clone(), rows.clone()));
                found.push((type_name, rows));
            }
            this.update(cx, |this, cx| {
                for (type_name, rows) in answered {
                    this.answers
                        .keep(type_name, text.clone(), rows, NODES_PER_TYPE as usize);
                }
                this.searching = false;
                this.set_nodes(found, cx);
            })
            .ok();
        }));
    }

    fn set_nodes(&mut self, found: Vec<(String, Vec<Found>)>, cx: &mut Context<Self>) {
        self.rows.truncate(self.fixed);
        self.candidates.truncate(self.fixed);
        let found = found.into_iter().flat_map(|(type_name, rows)| {
            rows.into_iter().map(move |row| (type_name.clone(), row))
        });
        for (type_name, Found { id, label }) in found.take(NODES_SHOWN) {
            let shown = match &label {
                Some(label) if label != &id => format!("{label} · {id}"),
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
                    id,
                }),
                type_name,
            });
        }
        self.update_matches(cx);
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
        if scope.searches_nodes() && self.rows.len() == self.fixed {
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
        self.matches = rank(&self.candidates, &query, &recent::keys(cx))
            .into_iter()
            .filter(|ranked| {
                scope.is_none_or(|scope| scope.includes(self.candidates[ranked.index].group))
            })
            .collect();
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
                let label =
                    HighlightedLabel::new(candidate.label.clone(), ranked.positions.clone())
                        .truncate();
                let muted = |text: String| {
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .into_any_element()
                };
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
                };
                let start: Option<gpui::Div> = match row {
                    Row::Type { kind, .. } => {
                        Some(studio_ui::kind_swatch(&candidate.label, *kind == "edge"))
                    }
                    Row::Node { type_name, .. } => Some(studio_ui::type_swatch(type_name)),
                    Row::Branch { current, .. } => Some(
                        div().child(
                            Icon::new(if *current {
                                IconName::Check
                            } else {
                                IconName::GitBranch
                            })
                            .size(IconSize::Small)
                            .color(Color::Muted),
                        ),
                    ),
                    Row::Tab { .. } | Row::Saved { .. } => Some(
                        div().child(
                            Icon::new(IconName::Code)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        ),
                    ),
                    Row::Action { .. } => None,
                };
                Some(
                    div()
                        .h(px(ROW_HEIGHT))
                        .child(
                            ListItem::new(ix)
                                .inset(true)
                                .spacing(ListItemSpacing::Sparse)
                                .toggle_state(ix == self.selected)
                                .start_slot::<gpui::Div>(start)
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_2()
                                        .justify_between()
                                        .child(label)
                                        .child(div().flex_none().child(end)),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.selected = ix;
                                    this.run_selected(window, cx);
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
    let documentation = cx.action_documentation().clone();
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
        .map(|n| (n.name.clone(), "node"))
        .chain(schema.edges.iter().map(|e| (e.name.clone(), "edge")));
    for (name, kind) in types {
        let action = OpenType {
            type_name: name.clone(),
            at_commit: None,
            filter_id: None,
            via: None,
            preview: false,
        };
        if !window.is_action_available(&action, cx) {
            return;
        }
        candidates.push(Candidate {
            label: name.clone(),
            key: format!("type:{name}"),
            group: Group::Type,
        });
        rows.push(Row::Type {
            count: session.count_of(&name),
            action: Box::new(action),
            kind,
        });
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (surface, border, border_variant) = (
            colors.elevated_surface_background,
            colors.border,
            colors.border_variant,
        );
        let count = self.matches.len();
        // Only a scope that shows nodes waits on them.
        let searching = self.searching && self.mode.scope().is_some_and(Scope::searches_nodes);
        let list = if count == 0 {
            div()
                .h(px(ROW_HEIGHT * 2.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Label::new(
                        self.mode
                            .empty(!self.query.read(cx).is_empty(cx), searching),
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
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
            .py(px(LIST_PAD))
            .into_any_element()
        };
        let description = self.selected_description();
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
            h_flex().px_3().pb_2().child(tabs)
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
            .w(px(580.))
            .rounded_lg()
            .bg(surface)
            .border_1()
            .border_color(border)
            .shadow_lg()
            .overflow_hidden()
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
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
            .child(div().h_px().w_full().bg(border_variant))
            .child(list)
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_3()
                    .border_t_1()
                    .border_color(border_variant)
                    .child(div().flex_1().min_w_0().children(description.map(|d| {
                        Label::new(d)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate()
                    })))
                    // No "0 nodes" while the nodes are still to come.
                    .when(!(searching && count == 0), |footer| {
                        footer.child(
                            div().debug_selector(|| "palette-count".into()).child(
                                Label::new(format!("{count} {}", self.mode.noun(count)))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .when(typed && self.mode != Mode::Keys, |footer| {
                        footer.child(studio_ui::ds::key_hint(&AskAgent, "Ask", &focus, cx))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::{TestAppContext, VisualTestContext};
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::json;

    use super::*;

    struct Host {
        palette: Option<Entity<CommandPalette>>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().children(self.palette.clone())
        }
    }

    /// A server whose graph has people; every read finds Ada.
    fn session(cx: &mut TestAppContext) -> Entity<GraphSession> {
        let schema: Arc<str> = "node Person {\n  slug: String @key\n  name: String\n}\n".into();
        let http = FakeHttpClient::create(move |request| {
            let schema = schema.clone();
            async move {
                let path = request.uri().path().to_owned();
                let body = if path.ends_with("/schema") {
                    json!({ "schema_source": &*schema })
                } else if path.ends_with("/query") {
                    json!({
                        "query_name": "studio_find",
                        "target": {"branch": "main", "snapshot": null},
                        "row_count": 1,
                        "columns": ["id", "label"],
                        "rows": [{"id": "ada", "label": "Ada Lovelace"}],
                        "graph_commit_id": "c1",
                    })
                } else {
                    json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    // `graph_client::CONTRACT_HEADER`.
                    .header("omnigraph-http-api", "0.13")
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
        cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx))
    }

    /// From the keystroke until the server answers, the palette says it is looking for
    /// nodes: a turning circle in the field, and "Searching nodes…" rather than "No node
    /// matches" while nothing has come back.
    #[gpui::test]
    async fn the_palette_shows_it_is_searching_nodes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(cx);
        cx.run_until_parked();
        // The palette is made in a window already drawn, as the workspace makes it: it reads
        // what the focused element offers.
        let (host, cx): (_, &mut VisualTestContext) =
            cx.add_window_view(|_, _| Host { palette: None });
        cx.run_until_parked();
        let palette = host.update_in(cx, |host, window, cx| {
            let palette = cx.new(|cx| {
                CommandPalette::new(
                    Mode::Picker(Scope::Nodes),
                    Some(session),
                    WeakEntity::new_invalid(),
                    Vec::new(),
                    Vec::new(),
                    window,
                    cx,
                )
            });
            host.palette = Some(palette.clone());
            cx.notify();
            palette
        });
        cx.run_until_parked();
        palette.update_in(cx, |palette, window, cx| {
            palette
                .query
                .update(cx, |query, cx| query.set_text("Ad", window, cx))
        });
        cx.run_until_parked();
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
