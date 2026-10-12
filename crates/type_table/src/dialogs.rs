//! Insert-row and delete-row dialogs (spec 004). Both name the branch they write to; a delete
//! is confirmed by typing the row's key and is bound to the commit the table was read at
//! (constitution IV). Neither retries; a refused or unknown outcome keeps the form.

use std::sync::Arc;

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Subscription,
    WeakEntity, Window,
};
use gq_language::{Property, ScalarKind, SchemaModel, builder};
use graph_store::GraphSession;
use serde_json::{Map, Value};
use ui::prelude::*;

use crate::model::{Notice, SaveResult, Tone, parse_input, save_result};
use crate::table::TypeTable;

struct Field {
    property: Property,
    editor: Entity<Editor>,
    error: Option<String>,
}

fn notice_color(tone: Tone) -> Color {
    match tone {
        Tone::Refused | Tone::Invalid => Color::Error,
        Tone::Conflict | Tone::Unknown => Color::Warning,
    }
}

fn write_target(branch: &str, cx: &App) -> impl IntoElement {
    let border = cx.theme().colors().border_variant;
    div()
        .px_2()
        .py_0p5()
        .rounded_md()
        .border_1()
        .border_color(border)
        .child(
            Label::new(format!("write · {branch}"))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
}

fn frame(cx: &App) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .w(px(520.))
        .p_4()
        .gap_3()
        .rounded_lg()
        .bg(colors.elevated_surface_background)
        .border_1()
        .border_color(colors.border)
        .shadow_lg()
}

fn field_box(editor: &Entity<Editor>, invalid: bool, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(if invalid {
            cx.theme().status().error_border
        } else {
            colors.border
        })
        .bg(colors.editor_background)
        .child(editor.clone())
}

/// The form for a new node, generated from the node's shape: the key first, then required
/// properties, then optional ones. Blob and Vector values are not entered here.
pub struct InsertRowModal {
    table: WeakEntity<TypeTable>,
    session: Entity<GraphSession>,
    schema: Arc<SchemaModel>,
    type_name: String,
    branch: String,
    fields: Vec<Field>,
    blocked: Option<String>,
    notice: Option<Notice>,
    saving: bool,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for InsertRowModal {}

impl Focusable for InsertRowModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.fields
            .first()
            .map(|field| field.editor.focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }
}

impl InsertRowModal {
    pub fn new(
        table: WeakEntity<TypeTable>,
        session: Entity<GraphSession>,
        schema: Arc<SchemaModel>,
        type_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch = session.read(cx).branch.clone();
        let mut properties: Vec<Property> = schema
            .node(&type_name)
            .map(|node| node.properties.clone())
            .unwrap_or_default();
        let blocked = properties
            .iter()
            .find(|p| !p.nullable && !p.projectable())
            .map(|p| {
                format!(
                    "{type_name} needs a {} value for {}; insert it with a query.",
                    p.base_type, p.name
                )
            });
        properties.retain(Property::projectable);
        properties.sort_by_key(|p| (!p.is_key, p.nullable));
        let mut subscriptions = Vec::new();
        let fields = properties
            .into_iter()
            .map(|property| {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text(&placeholder(&property), window, cx);
                    editor
                });
                subscriptions.push(cx.subscribe(
                    &editor,
                    |this: &mut Self, editor, event: &EditorEvent, cx| {
                        if let EditorEvent::BufferEdited = event {
                            for field in &mut this.fields {
                                if field.editor == editor {
                                    field.error = None;
                                }
                            }
                            cx.notify();
                        }
                    },
                ));
                Field {
                    property,
                    editor,
                    error: None,
                }
            })
            .collect();
        Self {
            table,
            session,
            schema,
            type_name,
            branch,
            fields,
            blocked,
            notice: None,
            saving: false,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || self.blocked.is_some() {
            return;
        }
        let mut values: Vec<(String, Value)> = Vec::new();
        let mut valid = true;
        for field in &mut self.fields {
            let text = field.editor.read(cx).text(cx);
            match parse_input(&field.property, &text) {
                Ok(Value::Null) => {}
                Ok(value) => values.push((field.property.name.clone(), value)),
                Err(error) => {
                    field.error = Some(error);
                    valid = false;
                }
            }
        }
        if !valid {
            cx.notify();
            return;
        }
        let query = match builder::insert_node(&self.schema, &self.type_name, &values) {
            Ok(query) => query,
            Err(error) => {
                self.notice = Some(Notice::new(Tone::Invalid, error.to_string()));
                cx.notify();
                return;
            }
        };
        let key = self
            .schema
            .node(&self.type_name)
            .and_then(|node| node.key.clone())
            .and_then(|key| values.iter().find(|(name, _)| *name == key))
            .and_then(|(_, value)| value.as_str().map(str::to_owned));
        let description = match &key {
            Some(key) => format!("Insert {} {key}", self.type_name),
            None => format!("Insert {}", self.type_name),
        };
        self.saving = true;
        self.notice = None;
        cx.notify();
        let write = self.session.update(cx, |session, cx| {
            session.write(&query, None, description, cx)
        });
        let table = self.table.clone();
        let branch = self.branch.clone();
        let values: Map<String, Value> = values.into_iter().collect();
        // The write is never cancelled with the dialog: the table hears the outcome even when the
        // dialog was closed meanwhile.
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            let result = save_result(&outcome, &branch);
            if let SaveResult::Saved { commit, parent } = &result {
                table
                    .update(cx, |table, cx| {
                        table.row_inserted(key.clone(), values, commit, parent.as_deref(), cx)
                    })
                    .ok();
            }
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    SaveResult::Saved { .. } => cx.emit(DismissEvent),
                    SaveResult::NoMatch => {
                        this.notice = Some(Notice::new(Tone::Refused, "Nothing was inserted."));
                    }
                    SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                        this.notice = Some(notice)
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.saving {
            cx.emit(DismissEvent);
        }
    }

    /// Tab and shift-tab move between fields.
    fn focus_step(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.fields.is_empty() {
            return;
        }
        let current = self
            .fields
            .iter()
            .position(|field| field.editor.focus_handle(cx).is_focused(window))
            .unwrap_or(0) as isize;
        let next = (current + step).rem_euclid(self.fields.len() as isize) as usize;
        window.focus(&self.fields[next].editor.focus_handle(cx), cx);
    }
}

fn placeholder(property: &Property) -> String {
    if let Some(values) = &property.enum_values {
        return values.join(" · ");
    }
    let example = match property.kind {
        ScalarKind::Date => "YYYY-MM-DD",
        ScalarKind::DateTime => "YYYY-MM-DDTHH:MM:SSZ",
        ScalarKind::Integer => "a whole number",
        ScalarKind::Float => "a number",
        ScalarKind::Bool => "true or false",
        _ => "",
    };
    match (property.list, example.is_empty()) {
        (true, _) => "comma-separated".into(),
        (false, true) => String::new(),
        (false, false) => example.into(),
    }
}

impl Render for InsertRowModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fields: Vec<AnyElement> = self
            .fields
            .iter()
            .map(|field| {
                let p = &field.property;
                let required = !p.nullable;
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(p.name.clone()).size(LabelSize::Small))
                            .child(
                                Label::new(p.display_type())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .when(p.is_key, |d| {
                                d.child(
                                    Label::new("key")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent),
                                )
                            })
                            .when(required && !p.is_key, |d| {
                                d.child(
                                    Label::new("required")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(field_box(&field.editor, field.error.is_some(), cx))
                    .children(field.error.clone().map(|error| {
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error)
                    }))
                    .into_any_element()
            })
            .collect();
        let saving = self.saving;
        let blocked = self.blocked.clone();
        frame(cx)
            .key_context("InsertRowModal")
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.submit(window, cx)))
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.cancel(cx)))
            .on_action(
                cx.listener(|this, _: &menu::SelectNext, window, cx| {
                    this.focus_step(1, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, window, cx| {
                this.focus_step(-1, window, cx)
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Headline::new(format!("New {}", self.type_name)).size(HeadlineSize::Small),
                    )
                    .child(div().flex_1())
                    .child(write_target(&self.branch, cx)),
            )
            .children(fields)
            .children(blocked.map(|b| Label::new(b).size(LabelSize::Small).color(Color::Error)))
            .children(self.notice.clone().map(|notice| {
                Label::new(notice.message)
                    .size(LabelSize::Small)
                    .color(notice_color(notice.tone))
            }))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        studio_ui::ds::button("cancel-insert", "Cancel")
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        studio_ui::ds::button(
                            "insert",
                            if saving { "Inserting…" } else { "Insert" },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(saving || self.blocked.is_some())
                        .key_binding(ui::KeyBinding::for_action(&menu::Confirm, cx))
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

/// Deleting one node: type its key to confirm. Bound to the commit the table was read at, so a
/// branch that moved meanwhile refuses the delete.
pub struct DeleteRowModal {
    table: WeakEntity<TypeTable>,
    session: Entity<GraphSession>,
    schema: Arc<SchemaModel>,
    type_name: String,
    branch: String,
    id: String,
    if_commit: Option<String>,
    confirm: Entity<Editor>,
    notice: Option<Notice>,
    saving: bool,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for DeleteRowModal {}

impl Focusable for DeleteRowModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.confirm.focus_handle(cx)
    }
}

impl DeleteRowModal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        table: WeakEntity<TypeTable>,
        session: Entity<GraphSession>,
        schema: Arc<SchemaModel>,
        type_name: String,
        id: String,
        if_commit: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch = session.read(cx).branch.clone();
        let confirm = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&id, window, cx);
            editor
        });
        let subscription = cx.subscribe(&confirm, |this: &mut Self, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                if this
                    .notice
                    .as_ref()
                    .is_some_and(|n| n.tone == Tone::Invalid)
                {
                    this.notice = None;
                }
                cx.notify();
            }
        });
        Self {
            table,
            session,
            schema,
            type_name,
            branch,
            id,
            if_commit,
            confirm,
            notice: None,
            saving: false,
            _subscription: subscription,
        }
    }

    fn confirmed(&self, cx: &App) -> bool {
        self.confirm.read(cx).text(cx).trim() == self.id
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if !self.confirmed(cx) {
            self.notice = Some(Notice::new(
                Tone::Invalid,
                format!("Type {} to confirm.", self.id),
            ));
            cx.notify();
            return;
        }
        let query = match builder::delete_node(&self.schema, &self.type_name, &self.id) {
            Ok(query) => query,
            Err(error) => {
                self.notice = Some(Notice::new(Tone::Invalid, error.to_string()));
                cx.notify();
                return;
            }
        };
        self.saving = true;
        self.notice = None;
        cx.notify();
        let description = format!("Delete {} {}", self.type_name, self.id);
        let if_commit = self.if_commit.clone();
        let write = self.session.update(cx, |session, cx| {
            session.write(&query, if_commit, description, cx)
        });
        let table = self.table.clone();
        let branch = self.branch.clone();
        let id = self.id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            let result = save_result(&outcome, &branch);
            let moved = matches!(&outcome, graph_client::WriteOutcome::Refused(error)
                if error.status == 412 || error.body.precondition_failure.is_some());
            if let SaveResult::Saved { commit, parent } = &result {
                table
                    .update(cx, |table, cx| {
                        table.row_deleted(&id, commit, parent.as_deref(), cx)
                    })
                    .ok();
            } else if moved {
                table.update(cx, |table, cx| table.branch_moved(cx)).ok();
            }
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    SaveResult::Saved { .. } => cx.emit(DismissEvent),
                    SaveResult::NoMatch => {
                        this.notice = Some(Notice::new(
                            Tone::Conflict,
                            "Nothing was deleted: the row no longer exists at the head of the branch.",
                        ));
                    }
                    SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                        this.notice = Some(notice)
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.saving {
            cx.emit(DismissEvent);
        }
    }
}

impl Render for DeleteRowModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let confirmed = self.confirmed(cx);
        let saving = self.saving;
        frame(cx)
            .key_context("DeleteRowModal")
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.submit(window, cx)))
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.cancel(cx)))
            .child(
                h_flex()
                    .gap_2()
                    .child(Headline::new(format!("Delete {}", self.id)).size(HeadlineSize::Small))
                    .child(div().flex_1())
                    .child(write_target(&self.branch, cx)),
            )
            .child(
                Label::new(format!(
                    "This removes the {} and its relationships from {}. Type its key to confirm.",
                    self.type_name, self.branch
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(field_box(&self.confirm, false, cx))
            .children(self.notice.clone().map(|notice| {
                Label::new(notice.message)
                    .size(LabelSize::Small)
                    .color(notice_color(notice.tone))
            }))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        studio_ui::ds::button("cancel-delete", "Cancel")
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        studio_ui::ds::button(
                            "delete",
                            if saving { "Deleting…" } else { "Delete" },
                        )
                        .style(ButtonStyle::Tinted(ui::TintColor::Error))
                        .disabled(saving || !confirmed)
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

/// Names the table's filters and order as a view of its type, saved per graph in the
/// settings (spec 004). A name already used for this type is replaced.
pub struct SaveViewModal {
    graph: String,
    view: studio_settings::SavedView,
    taken: Vec<String>,
    name: Entity<Editor>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for SaveViewModal {}

impl Focusable for SaveViewModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl SaveViewModal {
    /// `view` is the table's state without a name; `taken` the names of this type's views.
    pub fn new(
        graph: String,
        view: studio_settings::SavedView,
        taken: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Name, for example Founders", window, cx);
            editor
        });
        let subscription = cx.subscribe(&name, |_, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                cx.notify();
            }
        });
        Self {
            graph,
            view,
            taken,
            name,
            _subscription: subscription,
        }
    }

    fn entered(&self, cx: &App) -> String {
        self.name.read(cx).text(cx).trim().to_owned()
    }

    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let name = self.entered(cx);
        if name.is_empty() {
            return;
        }
        let graph = self.graph.clone();
        let view = studio_settings::SavedView {
            name,
            ..self.view.clone()
        };
        studio_settings::update(cx, move |settings| settings.save_view(&graph, view));
        cx.emit(DismissEvent);
    }
}

impl Render for SaveViewModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.entered(cx);
        let replaces = self.taken.contains(&name);
        let summary = {
            let filters = self.view.filters.len();
            let filters = match filters {
                0 => "no filters".to_owned(),
                1 => "1 filter".to_owned(),
                n => format!("{n} filters"),
            };
            match &self.view.sort {
                Some(sort) => format!(
                    "{filters}, ordered by {} {}",
                    sort.property,
                    if sort.descending {
                        "descending"
                    } else {
                        "ascending"
                    }
                ),
                None => filters,
            }
        };
        frame(cx)
            .key_context("SaveViewModal")
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.submit(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(
                Headline::new(format!("Save a view of {}", self.view.type_name))
                    .size(HeadlineSize::Small),
            )
            .child(
                Label::new(format!(
                    "The table's {summary}, kept for this graph in your settings."
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(field_box(&self.name, false, cx))
            .when(replaces, |d| {
                d.child(
                    Label::new(format!("Replaces the view “{name}”."))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        studio_ui::ds::button("cancel-view", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        studio_ui::ds::button(
                            "save-view",
                            if replaces { "Replace" } else { "Save" },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(name.is_empty())
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
