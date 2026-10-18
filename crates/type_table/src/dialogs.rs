//! Insert-row and delete-row dialogs (spec 004), for nodes and edges. Each names the branch it
//! writes to; a delete lists everything it removes, is confirmed with ↵ and offers Undo, and is
//! bound to the commit the table was read at (constitution IV). None retries; a refused or
//! unknown outcome keeps the form.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Subscription,
    Task, WeakEntity, Window,
};
use gq_language::{Property, ScalarKind, SchemaModel, builder};
use graph_store::deletes::{self, DeletePlan};
use graph_store::{DraftKey, FollowUp, GraphSession, Loadable};
use serde_json::{Map, Value};
use ui::prelude::*;

use crate::model::{Notice, SaveResult, Tone, parse_input, save_result};
use crate::offers::Offers;
use crate::suggest::{self, NODE_SUGGESTIONS};
use crate::table::TypeTable;
use studio_actions::table::{SelectNextSuggestion, SelectPreviousSuggestion};
use studio_ui::ds::{
    ModalHeader, Variant, action_button, cancel_button, modal, modal_body, modal_footer,
    write_target,
};
use studio_workspace::ModalView;

struct Field {
    property: Property,
    editor: Entity<Editor>,
    error: Option<String>,
}

fn field_box(editor: &Entity<Editor>, invalid: bool, cx: &App) -> impl IntoElement {
    studio_ui::ds::field_frame(invalid, cx).child(studio_ui::ds::field_input(editor.clone()))
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

impl ModalView for InsertRowModal {
    /// A write still running keeps the dialog open: its outcome shows here.
    fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        !self.saving
    }
}

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
        // On the branch the dialog was opened on, which its checks read.
        let branch = self.branch.clone();
        let write = self.session.update(cx, |session, cx| {
            session.write_on(&branch, &query, None, description, None, cx)
        });
        let table = self.table.clone();
        table.update(cx, |table, _| table.save_started()).ok();
        let values: Map<String, Value> = values.into_iter().collect();
        // The write is never cancelled with the dialog: the table hears the outcome even when the
        // dialog was closed meanwhile.
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            let result = save_result(&outcome, &branch);
            table
                .update(cx, |table, cx| {
                    if let SaveResult::Saved { commit, parent } = &result {
                        table.row_inserted(
                            &branch,
                            key.clone(),
                            values,
                            commit,
                            parent.as_deref(),
                            cx,
                        );
                    }
                    table.save_finished(cx);
                })
                .ok();
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

/// One property's label line, input and error.
fn render_field(field: &Field, cx: &App) -> AnyElement {
    let p = &field.property;
    let required = !p.nullable;
    v_flex()
        .gap(px(studio_ui::ds::space::XS))
        .child(
            h_flex()
                .gap(px(studio_ui::ds::space::MD))
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
}

impl Render for InsertRowModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fields: Vec<AnyElement> = self
            .fields
            .iter()
            .map(|field| render_field(field, cx))
            .collect();
        let saving = self.saving;
        let blocked = self.blocked.clone();
        modal(cx)
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
                ModalHeader::new(format!("New {}", self.type_name))
                    .meta(write_target(self.branch.clone(), cx))
                    .close_disabled(saving)
                    .on_close(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            .child(
                modal_body()
                    .children(fields)
                    .children(
                        blocked.map(|b| Label::new(b).size(LabelSize::Small).color(Color::Error)),
                    )
                    .children(self.notice.clone().map(|notice| {
                        Label::new(notice.message)
                            .size(LabelSize::Small)
                            .color(notice.tone.color())
                    })),
            )
            .child(
                modal_footer(cx)
                    .child(
                        cancel_button("cancel-insert")
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        action_button("insert", if saving { "Inserting…" } else { "Insert" })
                            .disabled(saving || self.blocked.is_some())
                            .key_binding(ui::KeyBinding::for_action(&menu::Confirm, cx))
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

/// One end of a new edge: a node of `type_name`, searched by id or label while it is typed, at
/// the commit the table was read at.
struct Endpoint {
    type_name: String,
    editor: Entity<Editor>,
    /// The nodes the field offers; the first is highlighted, so ↵ picks it.
    offers: Offers,
    /// The id last picked from the list; typing it again does not search.
    picked: Option<String>,
    /// Counts searches, so an answer for older text is dropped.
    generation: u64,
    search: Option<Task<()>>,
    /// The text `offers` answers; while it differs from the field, a search is out.
    answers: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    From,
    To,
}

/// The form for a new edge: its source and target nodes, picked by search, then its
/// properties. An unkeyed edge gets its id from the engine, so the table shows it after a read.
pub struct InsertEdgeModal {
    table: WeakEntity<TypeTable>,
    session: Entity<GraphSession>,
    schema: Arc<SchemaModel>,
    edge_name: String,
    branch: String,
    snapshot: Option<String>,
    from: Endpoint,
    to: Endpoint,
    fields: Vec<Field>,
    blocked: Option<String>,
    notice: Option<Notice>,
    saving: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for InsertEdgeModal {}

impl ModalView for InsertEdgeModal {
    /// A write still running keeps the dialog open: its outcome shows here.
    fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        !self.saving
    }
}

impl Focusable for InsertEdgeModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.from.editor.focus_handle(cx)
    }
}

impl InsertEdgeModal {
    pub fn new(
        table: WeakEntity<TypeTable>,
        session: Entity<GraphSession>,
        schema: Arc<SchemaModel>,
        edge_name: String,
        snapshot: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch = session.read(cx).branch.clone();
        let edge = schema.edge(&edge_name).cloned();
        let (from_type, to_type) = edge
            .as_ref()
            .map(|edge| (edge.from.clone(), edge.to.clone()))
            .unwrap_or_default();
        let mut properties: Vec<Property> = edge.map(|edge| edge.properties).unwrap_or_default();
        let blocked = properties
            .iter()
            .find(|p| !p.nullable && !p.projectable())
            .map(|p| {
                format!(
                    "{edge_name} needs a {} value for {}; insert it with a query.",
                    p.base_type, p.name
                )
            });
        properties.retain(Property::projectable);
        properties.sort_by_key(|p| p.nullable);
        let mut subscriptions = Vec::new();
        let mut endpoint = |end: End, type_name: String, cx: &mut Context<Self>| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(&format!("A {type_name}, by name or id"), window, cx);
                editor
            });
            subscriptions.push(cx.subscribe(
                &editor,
                move |this: &mut Self, _, event: &EditorEvent, cx| {
                    if let EditorEvent::BufferEdited = event {
                        this.endpoint_mut(end).error = None;
                        this.search(end, cx);
                    }
                },
            ));
            Endpoint {
                type_name,
                editor,
                offers: Offers::default(),
                picked: None,
                generation: 0,
                search: None,
                // An empty field answers itself: nothing is being looked up.
                answers: Some(String::new()),
                error: None,
            }
        };
        let from = endpoint(End::From, from_type, cx);
        let to = endpoint(End::To, to_type, cx);
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
            edge_name,
            branch,
            snapshot,
            from,
            to,
            fields,
            blocked,
            notice: None,
            saving: false,
            _subscriptions: subscriptions,
        }
    }

    fn endpoint_mut(&mut self, end: End) -> &mut Endpoint {
        match end {
            End::From => &mut self.from,
            End::To => &mut self.to,
        }
    }

    /// The endpoint whose field has focus.
    fn focused_end(&self, window: &Window, cx: &App) -> Option<End> {
        [End::From, End::To].into_iter().find(|end| {
            let endpoint = match end {
                End::From => &self.from,
                End::To => &self.to,
            };
            endpoint.editor.focus_handle(cx).is_focused(window)
        })
    }

    /// Searches the endpoint's type for its text once typing pauses.
    fn search(&mut self, end: End, cx: &mut Context<Self>) {
        let schema = self.schema.clone();
        let session = self.session.clone();
        let snapshot = self.snapshot.clone();
        let endpoint = self.endpoint_mut(end);
        let text = endpoint.editor.read(cx).text(cx).trim().to_owned();
        endpoint.generation += 1;
        let generation = endpoint.generation;
        if text.is_empty() || endpoint.picked.as_deref() == Some(text.as_str()) {
            endpoint.offers.clear();
            endpoint.search = None;
            endpoint.answers = Some(text);
            cx.notify();
            return;
        }
        endpoint.picked = None;
        let type_name = endpoint.type_name.clone();
        endpoint.search = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let Ok(query) = builder::node_search(&schema, &type_name, &text, NODE_SUGGESTIONS)
            else {
                return;
            };
            let read = cx
                .update(|cx| session.update(cx, |session, cx| session.read(&query, snapshot, cx)));
            let result = read.await;
            this.update(cx, |this, cx| {
                let endpoint = this.endpoint_mut(end);
                if endpoint.generation != generation {
                    return;
                }
                // A failed search offers nothing, not what an earlier one found.
                match result {
                    Ok(output) => endpoint
                        .offers
                        .set(suggest::from_node_rows(&output.rows), true),
                    Err(_) => endpoint.offers.clear(),
                }
                endpoint.answers = Some(text);
                cx.notify();
            })
            .ok();
        }));
    }

    /// ↑/↓ in an endpoint field: highlights another node.
    fn move_suggestion(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(end) = self.focused_end(window, cx) else {
            return;
        };
        if self.endpoint_mut(end).offers.step(step) {
            cx.notify();
        }
    }

    /// Puts a node's id in its endpoint field and moves to the next field.
    fn pick(&mut self, end: End, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let endpoint = self.endpoint_mut(end);
        let Some(suggestion) = endpoint.offers.get(index).cloned() else {
            return;
        };
        endpoint.picked = Some(suggestion.value.clone());
        endpoint.offers.clear();
        let editor = endpoint.editor.clone();
        editor.update(cx, |editor, cx| {
            editor.set_text(suggestion.value, window, cx);
            // Not `move_to_end`: in a single-line editor it only propagates, and the ↵ that
            // picked would then also be typed into the field.
            let end = editor.buffer().read(cx).read(cx).len();
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(vec![end..end])
            });
        });
        let next = match end {
            End::From => Some(self.to.editor.focus_handle(cx)),
            End::To => self.fields.first().map(|f| f.editor.focus_handle(cx)),
        };
        if let Some(next) = next {
            window.focus(&next, cx);
        }
        cx.notify();
    }

    /// ↵ picks the highlighted node in an endpoint field, and inserts otherwise. While the
    /// field's search is out it waits: the nodes offered were found for earlier text.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(end) = self.focused_end(window, cx) {
            let endpoint = self.endpoint_mut(end);
            let typed = endpoint.editor.read(cx).text(cx);
            if endpoint.answers.as_deref() != Some(typed.trim()) {
                return;
            }
            if let Some(index) = endpoint.offers.highlighted() {
                self.pick(end, index, window, cx);
                return;
            }
        }
        self.submit(window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || self.blocked.is_some() {
            return;
        }
        let mut valid = true;
        let mut ends = Vec::new();
        for end in [End::From, End::To] {
            let endpoint = self.endpoint_mut(end);
            let id = endpoint.editor.read(cx).text(cx).trim().to_owned();
            if id.is_empty() {
                endpoint.error = Some(format!("Pick a {}.", endpoint.type_name));
                valid = false;
            }
            ends.push(id);
        }
        let mut values: Vec<(String, Value)> = Vec::new();
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
        let (from, to) = (ends[0].clone(), ends[1].clone());
        let query = match builder::insert_edge(&self.schema, &self.edge_name, &from, &to, &values) {
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
        let description = format!("Insert {} {from} → {to}", self.edge_name);
        // On the branch the dialog was opened on, which its checks read.
        let branch = self.branch.clone();
        let write = self.session.update(cx, |session, cx| {
            session.write_on(&branch, &query, None, description, None, cx)
        });
        let table = self.table.clone();
        table.update(cx, |table, _| table.save_started()).ok();
        // The write is never cancelled with the dialog: the table hears the outcome even when the
        // dialog was closed meanwhile.
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            let result = save_result(&outcome, &branch);
            table
                .update(cx, |table, cx| {
                    if let SaveResult::Saved { commit, parent } = &result {
                        table.row_inserted(
                            &branch,
                            None,
                            Map::new(),
                            commit,
                            parent.as_deref(),
                            cx,
                        );
                    }
                    table.save_finished(cx);
                })
                .ok();
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    SaveResult::Saved { .. } => cx.emit(DismissEvent),
                    SaveResult::NoMatch => {
                        this.notice = Some(Notice::new(
                            Tone::Refused,
                            "Nothing was inserted: check that both nodes exist on this branch.",
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

    /// Tab and shift-tab move through the endpoints, then the properties.
    fn focus_step(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let mut handles = vec![
            self.from.editor.focus_handle(cx),
            self.to.editor.focus_handle(cx),
        ];
        handles.extend(self.fields.iter().map(|f| f.editor.focus_handle(cx)));
        let current = handles
            .iter()
            .position(|handle| handle.is_focused(window))
            .unwrap_or(0) as isize;
        let next = (current + step).rem_euclid(handles.len() as isize) as usize;
        window.focus(&handles[next], cx);
    }

    fn render_endpoint(
        &self,
        end: End,
        label: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let endpoint = match end {
            End::From => &self.from,
            End::To => &self.to,
        };
        let colors = cx.theme().colors();
        let focused = endpoint.editor.focus_handle(cx).is_focused(window);
        let offers = (focused && !endpoint.offers.is_empty()).then(|| {
            let pick = cx.listener(move |this, index: &usize, window, cx| {
                this.pick(end, *index, window, cx)
            });
            studio_ui::ds::row_list(cx)
                .id(("endpoint-suggestions", end as usize))
                .max_h(px(208.))
                .overflow_y_scroll()
                .border_color(colors.border_variant)
                .children(endpoint.offers.rows(
                    "endpoint-suggestion",
                    Rc::new(move |index, window, cx| pick(&index, window, cx)),
                    cx,
                ))
        });
        v_flex()
            .gap(px(studio_ui::ds::space::XS))
            .child(
                h_flex()
                    .gap(px(studio_ui::ds::space::MD))
                    .child(Label::new(label.to_owned()).size(LabelSize::Small))
                    .child(
                        Label::new(endpoint.type_name.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("required")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .key_context("EdgeEndpoint")
                    .on_action(cx.listener(|this, _: &SelectNextSuggestion, window, cx| {
                        this.move_suggestion(1, window, cx)
                    }))
                    .on_action(
                        cx.listener(|this, _: &SelectPreviousSuggestion, window, cx| {
                            this.move_suggestion(-1, window, cx)
                        }),
                    )
                    .child(field_box(&endpoint.editor, endpoint.error.is_some(), cx)),
            )
            .children(offers)
            .children(endpoint.error.clone().map(|error| {
                Label::new(error)
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
            }))
            .into_any_element()
    }
}

impl Render for InsertEdgeModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let from = self.render_endpoint(End::From, "From", window, cx);
        let to = self.render_endpoint(End::To, "To", window, cx);
        let fields: Vec<AnyElement> = self
            .fields
            .iter()
            .map(|field| render_field(field, cx))
            .collect();
        let saving = self.saving;
        let blocked = self.blocked.clone();
        modal(cx)
            .key_context("InsertEdgeModal")
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(window, cx)))
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
                ModalHeader::new(format!("New {}", self.edge_name))
                    .meta(write_target(self.branch.clone(), cx))
                    .close_disabled(saving)
                    .on_close(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            .child(
                modal_body()
                    .child(from)
                    .child(to)
                    .children(fields)
                    .children(
                        blocked.map(|b| Label::new(b).size(LabelSize::Small).color(Color::Error)),
                    )
                    .children(self.notice.clone().map(|notice| {
                        Label::new(notice.message)
                            .size(LabelSize::Small)
                            .color(notice.tone.color())
                    })),
            )
            .child(
                modal_footer(cx)
                    .child(
                        cancel_button("cancel-insert-edge")
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        action_button("insert-edge", if saving { "Inserting…" } else { "Insert" })
                            .disabled(saving || self.blocked.is_some())
                            .key_binding(ui::KeyBinding::for_action(&menu::Confirm, cx))
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

/// A row the delete dialog removes: its id, its label when its type has one, and an edge's
/// source and target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteTarget {
    pub id: String,
    pub label: Option<String>,
    pub endpoints: Option<(String, String)>,
}

impl DeleteTarget {
    fn name(&self) -> String {
        match (&self.endpoints, &self.label) {
            (Some((from, to)), _) => format!("{from} → {to}"),
            (None, Some(label)) if !label.is_empty() => label.clone(),
            _ => self.id.clone(),
        }
    }
}

/// How many dependents the dialog names before "and N more".
const DEPENDENTS_NAMED: usize = 6;

/// Deleting rows (spec 004, constitution IV): the picked rows, or the cursor's. Before it is
/// sent the dialog reads what else the delete removes: a node's relationships go with it, and
/// a node that needs one of them (`@card(n..)`) goes too, listed here, all in one commit bound
/// to the commit the table was read at. ↵ deletes, and the toast offers Undo; typing the key
/// is asked for only when Undo could not bring it all back.
pub struct DeleteRowModal {
    table: WeakEntity<TypeTable>,
    session: Entity<GraphSession>,
    schema: Arc<SchemaModel>,
    type_name: String,
    branch: String,
    targets: Vec<DeleteTarget>,
    /// Relationships rather than nodes: nothing depends on them.
    edges: bool,
    if_commit: Option<String>,
    /// What a node delete removes, read before it is offered.
    plan: Loadable<DeletePlan>,
    /// Why Undo could not restore this delete, once known; typing is then asked for.
    undo_unavailable: Option<String>,
    confirm: Entity<Editor>,
    notice: Option<Notice>,
    saving: bool,
    focus_handle: FocusHandle,
    _plan: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for DeleteRowModal {}

impl ModalView for DeleteRowModal {
    /// A write still running keeps the dialog open: its outcome shows here.
    fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        !self.saving
    }
}

impl Focusable for DeleteRowModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.undo_unavailable.is_some() {
            self.confirm.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl DeleteRowModal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        table: WeakEntity<TypeTable>,
        session: Entity<GraphSession>,
        schema: Arc<SchemaModel>,
        type_name: String,
        targets: Vec<DeleteTarget>,
        if_commit: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch = session.read(cx).branch.clone();
        let edges = schema.edge(&type_name).is_some();
        let phrase = Self::phrase_for(&targets);
        let confirm = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&phrase, window, cx);
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
        let mut this = Self {
            table,
            session,
            schema,
            type_name,
            branch,
            targets,
            edges,
            if_commit,
            plan: Loadable::NotLoaded,
            undo_unavailable: None,
            confirm,
            notice: None,
            saving: false,
            focus_handle: cx.focus_handle(),
            _plan: None,
            _subscription: subscription,
        };
        if edges {
            this.undo_unavailable = this.schema.edge(&this.type_name).and_then(|edge| {
                edge.properties.iter().find(|p| !p.projectable()).map(|p| {
                    format!(
                        "{}.{} holds {} values, which Undo cannot restore.",
                        edge.name, p.name, p.base_type
                    )
                })
            });
        } else {
            this.start_plan(window, cx);
        }
        this
    }

    /// Reads what deleting the nodes takes with them.
    fn start_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.plan = Loadable::Loading;
        let ids = self.targets.iter().map(|t| t.id.clone()).collect();
        let plan = deletes::plan_node_delete(
            self.session.clone(),
            self.schema.clone(),
            self.type_name.clone(),
            ids,
            self.if_commit.clone(),
            cx,
        );
        self._plan = Some(cx.spawn_in(window, async move |this, cx| {
            let plan = plan.await;
            this.update_in(cx, |this, window, cx| {
                match plan {
                    Ok(plan) => {
                        this.undo_unavailable = plan.undo_unavailable.clone();
                        this.plan = Loadable::Loaded(plan);
                        if this.undo_unavailable.is_some() {
                            window.focus(&this.confirm.focus_handle(cx), cx);
                        }
                    }
                    Err(error) => this.plan = Loadable::Failed(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// What is typed to confirm a delete Undo could not restore: the key of one row, or
    /// `delete N` for several.
    fn phrase_for(targets: &[DeleteTarget]) -> String {
        match targets {
            [one] => one.id.clone(),
            many => format!("delete {}", many.len()),
        }
    }

    fn ready(&self) -> bool {
        self.edges || matches!(self.plan, Loadable::Loaded(_))
    }

    fn confirmed(&self, cx: &App) -> bool {
        self.undo_unavailable.is_none()
            || self.confirm.read(cx).text(cx).trim() == Self::phrase_for(&self.targets)
    }

    /// The deleted rows in a few words: "Acme", "3 Company rows".
    fn what(&self) -> String {
        match self.targets.as_slice() {
            [one] => one.name(),
            many if self.edges => format!("{} {} relationships", many.len(), self.type_name),
            many => format!("{} {} rows", many.len(), self.type_name),
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || !self.ready() {
            return;
        }
        if !self.confirmed(cx) {
            self.notice = Some(Notice::new(
                Tone::Invalid,
                format!("Type {} to confirm.", Self::phrase_for(&self.targets)),
            ));
            cx.notify();
            return;
        }
        let ids: Vec<String> = self.targets.iter().map(|t| t.id.clone()).collect();
        let (built, dependents) = match &self.plan {
            Loadable::Loaded(plan) => (plan.query(&self.schema), plan.dependents.len()),
            _ => (
                builder::delete_edges(&self.schema, &self.type_name, &ids),
                0,
            ),
        };
        let query = match built {
            Ok(query) => query,
            Err(error) => {
                self.notice = Some(Notice::new(Tone::Invalid, error.to_string()));
                cx.notify();
                return;
            }
        };
        // Every node the delete removes, for their drafts; the table's own rows leave it.
        let removed: Vec<(String, String)> = match &self.plan {
            Loadable::Loaded(plan) => plan
                .nodes
                .iter()
                .flat_map(|(t, ids)| ids.iter().map(move |id| (t.clone(), id.clone())))
                .collect(),
            _ => Vec::new(),
        };
        let own: Vec<String> = match &self.plan {
            Loadable::Loaded(plan) => plan
                .nodes
                .iter()
                .filter(|(t, _)| *t == self.type_name)
                .flat_map(|(_, ids)| ids.clone())
                .collect(),
            _ => ids,
        };
        let what = match dependents {
            0 => self.what(),
            n => format!("{} and {n} that needed them", self.what()),
        };
        let description = format!("Delete {} {}", self.type_name, what);
        self.saving = true;
        self.notice = None;
        cx.notify();
        // The outcome's toast offers Undo when Undo can put it all back.
        let follow_up = self
            .undo_unavailable
            .is_none()
            .then_some(FollowUp::UndoDelete);
        let (branch, session) = (self.branch.clone(), self.session.clone());
        let write = self.session.update(cx, |session, cx| {
            let if_commit = self.if_commit.clone();
            session.write_on(&branch, &query, if_commit, description, follow_up, cx)
        });
        let table = self.table.clone();
        table.update(cx, |table, _| table.save_started()).ok();
        let several = self.targets.len() > 1 || dependents > 0;
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            let result = save_result(&outcome, &branch);
            let moved = matches!(&outcome, graph_client::WriteOutcome::Refused(error)
                if error.status == 412 || error.body.precondition_failure.is_some());
            if let SaveResult::Saved { .. } = &result {
                session
                    .update(cx, |session, cx| {
                        // The deleted rows' drafts on the branch they were deleted from, though
                        // the window moved to another branch meanwhile.
                        let keys: Vec<DraftKey> = session
                            .drafts_on(&branch)
                            .filter(|(key, _)| {
                                removed.iter().any(|(t, id)| key.type_name == *t && key.id == *id)
                            })
                            .map(|(key, _)| key.clone())
                            .collect();
                        for key in &keys {
                            session.remove_draft_on(&branch, key, cx);
                        }
                    });
            }
            table
                .update(cx, |table, cx| {
                    if let SaveResult::Saved { commit, parent } = &result {
                        table.rows_deleted(&branch, &own, commit, parent.as_deref(), cx);
                    } else if moved {
                        table.branch_moved(cx);
                    }
                    table.save_finished(cx);
                })
                .ok();
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    SaveResult::Saved { .. } => cx.emit(DismissEvent),
                    SaveResult::NoMatch => {
                        this.notice = Some(Notice::new(
                            Tone::Conflict,
                            if several {
                                "Nothing was deleted: they no longer exist at the head of the branch."
                            } else {
                                "Nothing was deleted: it no longer exists at the head of the branch."
                            },
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

    /// What else goes: "Also deletes 2 nodes that need it:", then their names and the
    /// relationship each needs.
    fn render_plan(&self, cx: &App) -> Option<AnyElement> {
        match &self.plan {
            Loadable::NotLoaded => None,
            Loadable::Loading => Some(
                h_flex()
                    .gap(px(studio_ui::ds::space::MD))
                    .child(studio_ui::ds::spinner())
                    .child(
                        Label::new("Finding what depends on it…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            ),
            Loadable::Failed(error) => Some(
                Label::new(format!("Studio could not read what depends on it. {error}"))
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element(),
            ),
            Loadable::Loaded(plan) if plan.dependents.is_empty() => None,
            Loadable::Loaded(plan) => {
                let hidden = plan.dependents.len().saturating_sub(DEPENDENTS_NAMED);
                let list = studio_ui::ds::row_list(cx)
                    .bg(cx.theme().colors().editor_background)
                    .border_color(studio_ui::ds::surface::floating_border(cx))
                    .children(plan.dependents.iter().take(DEPENDENTS_NAMED).map(|d| {
                        h_flex()
                            .h(px(studio_ui::ds::size::SIDEBAR_ROW))
                            .px(px(studio_ui::ds::space::MD))
                            .gap(px(studio_ui::ds::space::MD))
                            .child(studio_ui::type_swatch(&d.type_name, cx))
                            .child(
                                div().min_w_0().flex_1().child(
                                    Label::new(d.label.clone().unwrap_or_else(|| d.id.clone()))
                                        .size(LabelSize::Small)
                                        .single_line()
                                        .truncate(),
                                ),
                            )
                            .child(
                                Label::new(format!("needs {}", d.edge))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .single_line(),
                            )
                    }))
                    .when(hidden > 0, |list| {
                        list.child(
                            div().px(px(studio_ui::ds::space::MD)).child(
                                Label::new(format!("and {hidden} more"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        )
                    });
                Some(list.into_any_element())
            }
        }
    }

    /// The dialog's one paragraph: what goes, what else goes with it, and whether Undo can
    /// bring it back. The body holds only what it names (the dependents, the confirm field).
    fn description(&self) -> String {
        let them = self.targets.len() > 1;
        let mut text = match (self.edges, them) {
            (true, false) => format!("Removes it from {}. Its nodes stay.", self.branch),
            (true, true) => format!("Removes them from {}. Their nodes stay.", self.branch),
            (false, false) => format!("Removes it and its relationships from {}.", self.branch),
            (false, true) => format!("Removes them and their relationships from {}.", self.branch),
        };
        if let Loadable::Loaded(plan) = &self.plan
            && !plan.dependents.is_empty()
        {
            let counts: Vec<String> = plan
                .dependent_counts()
                .into_iter()
                .map(|(t, n)| format!("{n} {t}"))
                .collect();
            let counts = match counts.split_last() {
                Some((last, [])) => last.clone(),
                Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
                None => String::new(),
            };
            let it = if them { "them" } else { "it" };
            text.push_str(&format!(
                " Also deletes {counts} that cannot exist without {it}."
            ));
        }
        match &self.undo_unavailable {
            Some(reason) => text.push_str(&format!(
                " Undo cannot restore this: {reason} Type {} to confirm.",
                Self::phrase_for(&self.targets)
            )),
            None if self.ready() => text.push_str(" You can undo this right after."),
            None => {}
        }
        text
    }
}

impl Render for DeleteRowModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = self.ready();
        let confirmed = self.confirmed(cx);
        let saving = self.saving;
        let headline = format!("Delete {}", self.what());
        let description = self.description();
        let plan = self.render_plan(cx);
        let typing = self.undo_unavailable.is_some();
        let notice = self.notice.clone();
        modal(cx)
            .key_context("DeleteRowModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.submit(window, cx)))
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.cancel(cx)))
            .child(
                ModalHeader::new(headline)
                    .description(SharedString::from(description))
                    .meta(write_target(self.branch.clone(), cx))
                    .close_disabled(saving)
                    .on_close(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            // The body only when it has something: the dependents, the confirm field, or the
            // write's outcome.
            .when(plan.is_some() || typing || notice.is_some(), |dialog| {
                dialog.child(
                    modal_body()
                        .children(plan)
                        .when(typing, |body| {
                            body.child(field_box(&self.confirm, false, cx))
                        })
                        .children(notice.map(|notice| {
                            Label::new(notice.message)
                                .size(LabelSize::Small)
                                .color(notice.tone.color())
                        })),
                )
            })
            .child(
                modal_footer(cx)
                    .child(
                        cancel_button("cancel-delete")
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        action_button("delete", if saving { "Deleting…" } else { "Delete" })
                            .variant(Variant::Destructive)
                            .disabled(saving || !ready || !confirmed)
                            .key_binding(ui::KeyBinding::for_action(&menu::Confirm, cx))
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

impl ModalView for SaveViewModal {}

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
            let mut parts = vec![filters];
            if let Some(sort) = &self.view.sort {
                parts.push(format!(
                    "ordered by {} {}",
                    sort.property,
                    if sort.descending {
                        "descending"
                    } else {
                        "ascending"
                    }
                ));
            }
            if let Some(property) = &self.view.group_by {
                parts.push(format!("grouped by {property}"));
            }
            match self.view.hidden.len() {
                0 => {}
                1 => parts.push("1 hidden column".to_owned()),
                n => parts.push(format!("{n} hidden columns")),
            }
            if !self.view.order.is_empty() {
                parts.push("its column order".to_owned());
            }
            parts.join(", ")
        };
        modal(cx)
            .key_context("SaveViewModal")
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.submit(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(
                ModalHeader::new(format!("Save a view of {}", self.view.type_name))
                    .description(SharedString::from(format!(
                        "The table's {summary}, kept for this graph in your settings."
                    )))
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                modal_body()
                    .child(field_box(&self.name, false, cx))
                    .when(replaces, |d| {
                        d.child(
                            Label::new(format!("Replaces the view “{name}”."))
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    }),
            )
            .child(
                modal_footer(cx)
                    .child(
                        cancel_button("cancel-view")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        action_button("save-view", if replaces { "Replace" } else { "Save" })
                            .disabled(name.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyBinding, TestAppContext};
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};

    /// A server for the dev graph that finds two people for any node search.
    fn session(cx: &mut TestAppContext) -> Entity<GraphSession> {
        let http = FakeHttpClient::create(|request| async move {
            let body = if request.uri().path().ends_with("/query") {
                r#"{"query_name":"studio_find","target":{"branch":"main","snapshot":null},
                    "row_count":2,"columns":["id","label"],
                    "rows":[{"id":"person-000000","label":"Radia Knuth"},
                            {"id":"person-000004","label":"Radia Lamarr"}],
                    "graph_commit_id":"c1"}"#
            } else {
                r#"{"branches":["main"]}"#
            };
            Ok(Response::builder()
                .status(200)
                .header(graph_client::CONTRACT_HEADER, "0.13")
                .body(AsyncBody::from(body))
                .unwrap())
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

    /// ↵ on an offered node puts its id in the field and moves on, and types nothing: the key
    /// that picked must not also reach the field as text.
    #[gpui::test]
    async fn enter_on_an_offered_node_picks_it_and_types_nothing(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
            cx.bind_keys([
                KeyBinding::new("enter", menu::Confirm, None),
                KeyBinding::new("down", SelectNextSuggestion, Some("EdgeEndpoint > Editor")),
            ]);
        });
        let session = session(cx);
        let schema = Arc::new(
            SchemaModel::parse(include_str!("../../../dev/cluster/knowledge.pg")).unwrap(),
        );
        let (modal, cx) = cx.add_window_view(|window, cx| {
            InsertEdgeModal::new(
                WeakEntity::new_invalid(),
                session,
                schema,
                "WorksAt".into(),
                None,
                window,
                cx,
            )
        });
        modal.update_in(cx, |modal, window, cx| {
            window.focus(&modal.focus_handle(cx), cx)
        });
        cx.simulate_input("radia");
        cx.executor().advance_clock(Duration::from_millis(120));
        cx.run_until_parked();
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.from.offers.len(), 2);
            assert_eq!(modal.from.offers.highlighted(), Some(0));
        });

        cx.simulate_keystrokes("down enter");
        modal.read_with(cx, |modal, cx| {
            assert_eq!(modal.from.editor.read(cx).text(cx), "person-000004");
            assert!(modal.from.offers.is_empty());
        });
        modal.update_in(cx, |modal, window, cx| {
            assert!(modal.to.editor.focus_handle(cx).is_focused(window));
        });
    }
}
