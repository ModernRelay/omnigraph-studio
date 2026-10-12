//! The command palette (spec 002): every action available where the person is, with its
//! binding, fuzzy filtered and ranked by recent use, plus quick-open rows for node and edge
//! types.
//!
//! Layer 5 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
// Written after zed-industries/zed crates/command_palette/src/command_palette.rs @ v1.23.2: the
// palette lists `window.available_actions` captured from the previously focused element,
// restores that focus, then dispatches. Studio does not depend on Zed's `picker`, which needs
// `project` and `workspace`; the list, matching and persistence are Studio's own.

mod matching;
mod recent;

use editor::{Editor, EditorEvent};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding,
    Keystroke, Render, ScrollStrategy, SharedString, Subscription, UniformListScrollHandle, Window,
    uniform_list,
};
use graph_store::GraphSession;
use studio_actions::graph::OpenType;
use studio_actions::workspace::ToggleCommandPalette;
use studio_workspace::Workspace;
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
            let session = workspace.session().cloned();
            workspace.toggle_modal(window, cx, move |window, cx| {
                CommandPalette::new(session, window, cx)
            });
        });
    })
    .detach();
}

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
}

impl Row {
    fn action(&self) -> &dyn Action {
        match self {
            Row::Action { action, .. } | Row::Type { action, .. } => action.as_ref(),
        }
    }
}

const ROW_HEIGHT: f32 = 32.;
const VISIBLE_ROWS: usize = 11;

pub struct CommandPalette {
    previous_focus: Option<FocusHandle>,
    query: Entity<Editor>,
    rows: Vec<Row>,
    candidates: Vec<Candidate>,
    matches: Vec<Ranked>,
    selected: usize,
    scroll: UniformListScrollHandle,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl CommandPalette {
    /// Builds the palette from what the focused element offers. Called before focus moves to
    /// the palette, so `available_actions` and the bindings are those of where the person was.
    pub fn new(
        session: Option<Entity<GraphSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let previous_focus = window.focused(cx);
        let documentation = cx.action_documentation().clone();
        let mut rows = Vec::new();
        let mut candidates = Vec::new();
        for action in window.available_actions(cx) {
            let name = action.name();
            if hidden(name) {
                continue;
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
        if let Some(session) = session {
            add_type_rows(&session, window, cx, &mut rows, &mut candidates);
        }

        let query = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Run a command or open a type…", window, cx);
            editor
        });
        let subscription = cx.subscribe(&query, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                this.update_matches(cx);
            }
        });
        let mut palette = Self {
            previous_focus,
            query,
            rows,
            candidates,
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _subscription: subscription,
        };
        palette.update_matches(cx);
        palette
    }

    fn update_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text(cx);
        self.matches = rank(&self.candidates, &query, &recent::keys(cx));
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
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
        let action = self.rows[ranked.index].action().boxed_clone();
        recent::record(&self.candidates[ranked.index].key, cx);
        match &self.previous_focus {
            Some(focus) => window.focus(focus, cx),
            None => window.blur(cx),
        }
        cx.emit(DismissEvent);
        window.dispatch_action(action, cx);
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
                let end: AnyElement = match row {
                    Row::Action { action, .. } => match &self.previous_focus {
                        Some(focus) => ui::KeyBinding::for_action_in(action.as_ref(), focus, cx)
                            .into_any_element(),
                        None => ui::KeyBinding::for_action(action.as_ref(), cx).into_any_element(),
                    },
                    Row::Type { kind, count, .. } => Label::new(match count {
                        Some(count) => format!("{kind} · {count} rows"),
                        None => (*kind).to_owned(),
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
                };
                let start: Option<gpui::Div> = match row {
                    Row::Type { .. } => Some(studio_ui::type_swatch(&candidate.label)),
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
                    Label::new("No command matches.")
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
                        Label::new(format!(
                            "{count} command{}",
                            if count == 1 { "" } else { "s" }
                        ))
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
