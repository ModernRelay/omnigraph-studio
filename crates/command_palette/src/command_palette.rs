//! The command palette (spec 002): every action available where the person is, with its
//! binding, fuzzy filtered and ranked by recent use, plus quick-open rows for node and edge
//! types. The same picker serves ⌘P (go to a type or a node), ⌘⇧B (switch branch) and `?`
//! (the keyboard shortcuts available where the person is).
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
// Written after zed-industries/zed crates/command_palette/src/command_palette.rs @ v1.23.2: the
// palette lists `window.available_actions` captured from the previously focused element,
// restores that focus, then dispatches. Studio does not depend on Zed's `picker`, which needs
// `project` and `workspace`; the list, matching and persistence are Studio's own.

mod matching;
mod recent;

use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding,
    Keystroke, Render, ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle,
    Window, uniform_list,
};
use gq_language::builder::node_search;
use graph_store::{GraphSession, Loadable};
use serde_json::Value;
use studio_actions::graph::{OpenNode, OpenType};
use studio_actions::workspace::{GoTo, ShowKeyBindings, SwitchBranch, ToggleCommandPalette};
use studio_workspace::{ToastKind, Workspace};
use ui::prelude::*;
use ui::{HighlightedLabel, ListItem, ListItemSpacing};

use matching::{Candidate, Group, Ranked, rank};
pub use matching::{humanize_action_name, normalize_query};

/// Registers the palette with every new workspace and binds ⌘K and ⌘⇧P (Ctrl elsewhere) unless
/// the keymap already does.
pub fn init(cx: &mut App) {
    recent::load(cx);
    bind_default_keys(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleCommandPalette, window, cx| {
            open(workspace, Mode::Commands, window, cx);
        });
        workspace.register_action(|workspace, _: &GoTo, window, cx| {
            open(workspace, Mode::GoTo, window, cx);
        });
        workspace.register_action(|workspace, _: &SwitchBranch, window, cx| {
            open(workspace, Mode::Branches, window, cx);
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
    if session.is_none() && matches!(mode, Mode::GoTo | Mode::Branches) {
        workspace.show_toast(ToastKind::Info, "Open a graph first.", cx);
        return;
    }
    workspace.toggle_modal(window, cx, move |window, cx| {
        CommandPalette::new(mode, session, window, cx)
    });
}

/// What the picker lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// ⌘K: every action available where the person is, then the graph's types.
    Commands,
    /// ⌘P: the graph's types, then nodes whose id starts with, or label contains, the text.
    GoTo,
    /// ⌘⇧B: the session's branches.
    Branches,
    /// `?`: every action with a key binding where the person is.
    Keys,
}

impl Mode {
    fn placeholder(self) -> &'static str {
        match self {
            Mode::Commands => "Run a command or open a type…",
            Mode::GoTo => "Go to a type, or find a node by id or name…",
            Mode::Branches => "Switch to a branch…",
            Mode::Keys => "Search keyboard shortcuts…",
        }
    }

    fn noun(self, count: usize) -> &'static str {
        match (self, count == 1) {
            (Mode::Commands, true) => "command",
            (Mode::Commands, false) => "commands",
            (Mode::GoTo, true) => "result",
            (Mode::GoTo, false) => "results",
            (Mode::Branches, true) => "branch",
            (Mode::Branches, false) => "branches",
            (Mode::Keys, true) => "shortcut",
            (Mode::Keys, false) => "shortcuts",
        }
    }

    fn empty(self) -> &'static str {
        match self {
            Mode::Commands => "No command matches.",
            Mode::GoTo => "No type or node matches.",
            Mode::Branches => "No branch matches.",
            Mode::Keys => "No shortcut matches.",
        }
    }
}

/// How long typing must pause before ⌘P looks for nodes.
const NODE_SEARCH_DELAY: Duration = Duration::from_millis(150);
/// Nodes found per type, and in all.
const NODES_PER_TYPE: u32 = 5;
const NODES_SHOWN: usize = 30;

fn bind_default_keys(cx: &mut App) {
    let keys: &[&str] = if cfg!(target_os = "macos") {
        &["cmd-k", "cmd-shift-p"]
    } else {
        &["ctrl-k", "ctrl-shift-p"]
    };
    let mut missing = Vec::new();
    for key in keys {
        let Ok(keystroke) = Keystroke::parse(key) else {
            continue;
        };
        let bound = cx
            .all_bindings_for_input(std::slice::from_ref(&keystroke))
            .iter()
            .any(|binding| {
                binding.predicate().is_none() && binding.action().partial_eq(&ToggleCommandPalette)
            });
        if !bound {
            missing.push(KeyBinding::new(key, ToggleCommandPalette, None));
        }
    }
    cx.bind_keys(missing);
}

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
}

impl Row {
    fn action(&self) -> Option<&dyn Action> {
        match self {
            Row::Action { action, .. } | Row::Type { action, .. } | Row::Node { action, .. } => {
                Some(action.as_ref())
            }
            Row::Branch { .. } => None,
        }
    }
}

const ROW_HEIGHT: f32 = 32.;
const VISIBLE_ROWS: usize = 11;

pub struct CommandPalette {
    mode: Mode,
    session: Option<Entity<GraphSession>>,
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
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for CommandPalette {}

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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let previous_focus = window.focused(cx);
        let mut rows = Vec::new();
        let mut candidates = Vec::new();
        match mode {
            Mode::Commands | Mode::Keys => add_action_rows(
                mode,
                previous_focus.as_ref(),
                window,
                cx,
                &mut rows,
                &mut candidates,
            ),
            Mode::GoTo | Mode::Branches => {}
        }
        if let Some(session) = &session {
            match mode {
                Mode::Commands | Mode::GoTo => {
                    add_type_rows(session, window, cx, &mut rows, &mut candidates)
                }
                Mode::Branches => add_branch_rows(session, cx, &mut rows, &mut candidates),
                Mode::Keys => {}
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
                if this.mode == Mode::GoTo {
                    this.schedule_node_search(cx);
                }
            }
        });
        let fixed = rows.len();
        let mut palette = Self {
            mode,
            session,
            previous_focus,
            query,
            rows,
            candidates,
            fixed,
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            node_search: None,
            _subscription: subscription,
        };
        palette.update_matches(cx);
        palette
    }

    /// After a pause in typing, looks up nodes of every node type whose id starts with, or
    /// label contains, the text; replaces the node rows with what comes back.
    fn schedule_node_search(&mut self, cx: &mut Context<Self>) {
        let text = self.query.read(cx).text(cx).trim().to_owned();
        self.rows.truncate(self.fixed);
        self.candidates.truncate(self.fixed);
        let Some(session) = self.session.clone() else {
            return;
        };
        if text.chars().count() < 2 {
            self.node_search = None;
            self.update_matches(cx);
            return;
        }
        self.node_search = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NODE_SEARCH_DELAY).await;
            let reads = session.update(cx, |session, cx| {
                let Some(schema) = session.schema_model() else {
                    return Vec::new();
                };
                schema
                    .nodes
                    .iter()
                    .filter_map(|node| {
                        let query = node_search(&schema, &node.name, &text, NODES_PER_TYPE).ok()?;
                        Some((node.name.clone(), session.read(&query, None, cx)))
                    })
                    .collect::<Vec<_>>()
            });
            let mut found = Vec::new();
            for (type_name, read) in reads {
                // A type that fails to answer simply has no rows; the others still show.
                let Ok(output) = read.await else { continue };
                let Value::Array(rows) = output.rows else {
                    continue;
                };
                for row in rows {
                    let Some(id) = row.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let label = row.get("label").and_then(Value::as_str);
                    found.push((type_name.clone(), id.to_owned(), label.map(str::to_owned)));
                }
            }
            this.update(cx, |this, cx| this.set_nodes(found, cx)).ok();
        }));
    }

    fn set_nodes(&mut self, found: Vec<(String, String, Option<String>)>, cx: &mut Context<Self>) {
        self.rows.truncate(self.fixed);
        self.candidates.truncate(self.fixed);
        for (type_name, id, label) in found.into_iter().take(NODES_SHOWN) {
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

    fn update_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text(cx);
        self.matches = rank(&self.candidates, &query, &recent::keys(cx));
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
        recent::record(&self.candidates[ranked.index].key, cx);
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
                };
                let start: Option<gpui::Div> = match row {
                    Row::Type { .. } => Some(studio_ui::type_swatch(&candidate.label)),
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
        let list = if count == 0 {
            div()
                .h(px(ROW_HEIGHT * 2.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Label::new(self.mode.empty())
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
            .h(px(ROW_HEIGHT * count.min(VISIBLE_ROWS) as f32))
            .py_1()
            .into_any_element()
        };
        let description = self.selected_description();
        v_flex()
            .key_context("CommandPalette Picker")
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
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
                    .child(div().flex_1().child(self.query.clone())),
            )
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
                    .child(
                        Label::new(format!("{count} {}", self.mode.noun(count)))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::hidden;

    #[test]
    fn rows_that_cannot_work_in_studio_are_hidden() {
        assert!(hidden("studio::ToggleCommandPalette"));
        assert!(hidden("menu::SelectNext"));
        assert!(hidden("workspace::NewFile"));
        assert!(!hidden("studio_query::Run"));
        assert!(!hidden("editor::SelectAll"));
    }
}
