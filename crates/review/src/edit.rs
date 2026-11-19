//! Edit from the review (spec 008, "Act in place"): a form of a changed node's editable
//! properties, filled with what the review shows it holding. Saving sends only the fields typed
//! in, as one statement on the reviewed branch bound to the values shown, sent bound to the
//! reviewed head like a revert. Opened from a merge conflict (spec 039), each field shows the
//! target's value beside it, with Use to take it.

use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, WeakEntity,
    Window,
};
use gq_language::Property;
use gq_language::input::{edit_text, same_value, typed};
use serde_json::Value;
use studio_ui::ds;
use studio_ui::ds::Tipped;
use ui::prelude::*;

use crate::view::ReviewView;
use studio_workspace::ModalView;

struct Field {
    property: Property,
    editor: Entity<Editor>,
    /// The value the branch holds, which the save is bound to.
    current: Value,
    /// The target's value, when the form was opened from a conflict and it holds the node.
    other: Option<Value>,
    error: Option<String>,
}

pub(crate) struct EditModal {
    view: WeakEntity<ReviewView>,
    /// The change the form edits, by its key.
    key: String,
    title: SharedString,
    /// Where the edit writes, named whole: the server, the graph and the branch, as every write
    /// confirmation names it, since two windows can both be on `main`.
    scope: String,
    fields: Vec<Field>,
    /// The branch whose values show beside the fields, and whether it holds the node.
    beside: Option<(String, bool)>,
    /// Why the edit was not sent, shown while the form stays open.
    refused: Option<String>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for EditModal {}

impl ModalView for EditModal {}

impl Focusable for EditModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.fields
            .first()
            .map(|field| field.editor.focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }
}

impl EditModal {
    pub(crate) fn new(
        view: WeakEntity<ReviewView>,
        key: String,
        title: String,
        scope: String,
        values: Vec<(Property, Value)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fields = values
            .into_iter()
            .map(|(property, current)| {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_text(edit_text(&current), window, cx);
                    editor.set_placeholder_text(&property.display_type(), window, cx);
                    editor
                });
                Field {
                    property,
                    editor,
                    current,
                    other: None,
                    error: None,
                }
            })
            .collect();
        Self {
            view,
            key,
            title: title.into(),
            scope,
            fields,
            beside: None,
            refused: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Shows `branch`'s values beside the fields, from its image of the node, `None` where it
    /// does not hold it (spec 039).
    pub(crate) fn beside(
        mut self,
        beside: Option<(String, Option<serde_json::Map<String, Value>>)>,
    ) -> Self {
        let Some((branch, image)) = beside else {
            return self;
        };
        for field in &mut self.fields {
            field.other = image
                .as_ref()
                .and_then(|image| image.get(&field.property.name).cloned());
        }
        self.beside = Some((branch, image.is_some()));
        self
    }

    /// Puts the other branch's value of field `at` in its input.
    fn use_other(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(field) = self.fields.get(at) else {
            return;
        };
        let text = edit_text(field.other.as_ref().unwrap_or(&Value::Null));
        field
            .editor
            .update(cx, |editor, cx| editor.set_text(text, window, cx));
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let mut changes = Vec::new();
        let mut valid = true;
        for field in &mut self.fields {
            let text = field.editor.read(cx).text(cx);
            match typed(&field.property, &field.current, &text) {
                None => field.error = None,
                Some(Ok(value)) => {
                    field.error = None;
                    if !same_value(&value, &field.current) {
                        changes.push((field.property.name.clone(), field.current.clone(), value));
                    }
                }
                Some(Err(error)) => {
                    field.error = Some(error);
                    valid = false;
                }
            }
        }
        if !valid {
            cx.notify();
            return;
        }
        if changes.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        let key = self.key.clone();
        match self
            .view
            .update(cx, |view, cx| view.save_edit(&key, changes, cx))
        {
            Ok(Err(reason)) => {
                self.refused = Some(reason);
                cx.notify();
            }
            _ => cx.emit(DismissEvent),
        }
    }
}

impl Render for EditModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        ds::modal(cx)
            .key_context("ReviewEdit")
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.save(cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(
                ds::ModalHeader::new(format!("Edit {}", self.title))
                    .meta(ds::write_target(self.scope.clone(), cx))
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                ds::modal_body()
                    .children(self.beside.clone().and_then(|(branch, holds)| {
                        (!holds).then(|| {
                            studio_ui::ds::text::meta(format!(
                                "{branch} no longer holds this node."
                            ))
                        })
                    }))
                    .children(self.fields.iter().enumerate().map(|(at, field)| {
                        let beside =
                            self.beside
                                .as_ref()
                                .filter(|(_, holds)| *holds)
                                .map(|(branch, _)| {
                                    let value = field.other.clone().unwrap_or(Value::Null);
                                    let same = same_value(&value, &field.current);
                                    h_flex()
                                        .gap(px(ds::space::INLINE))
                                        .child(
                                            div().min_w_0().child(
                                                studio_ui::ds::text::meta(format!(
                                                    "{branch}: {}",
                                                    match &value {
                                                        Value::Null => "—".to_owned(),
                                                        other => studio_ui::value_text(other),
                                                    }
                                                ))
                                                .truncate(),
                                            ),
                                        )
                                        .when(!same, |row| {
                                            row.child(
                                                ds::row_button(("use-other", at), "Use")
                                                    .tip(ds::tooltip(format!(
                                                        "Put {branch}'s value in the field"
                                                    )))
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.use_other(at, window, cx)
                                                        },
                                                    )),
                                            )
                                        })
                                });
                        ds::field(
                            field.property.name.clone(),
                            v_flex()
                                .gap(px(ds::space::XXS))
                                .child(
                                    ds::field_frame(field.error.is_some(), cx)
                                        .child(ds::field_input(field.editor.clone())),
                                )
                                .children(beside)
                                .children(field.error.clone().map(studio_ui::ds::text::error_note)),
                        )
                    }))
                    .children(self.refused.clone().map(studio_ui::ds::text::error_note)),
            )
            .child(
                ds::modal_footer(cx)
                    .child(
                        ds::cancel_button("cancel-edit")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        ds::action_button("save-edit", "Save")
                            .tip(studio_ui::ds::tooltip(format!(
                                "Writes the changed fields to {}, bound to the reviewed head",
                                self.scope
                            )))
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use gq_language::SchemaModel;
    use serde_json::json;

    use super::*;

    /// A field left as the form filled it asks for nothing, even where its text cannot say the
    /// value again; a field typed in is read as typed.
    #[test]
    fn a_field_left_alone_asks_for_nothing() {
        let schema = SchemaModel::parse(
            "node Person {\n  slug: String @key\n  name: String\n  title: String?\n  tags: [String]?\n  visits: U64?\n}\n",
        )
        .unwrap();
        let person = schema.node("Person").unwrap();
        let property = |name: &str| person.property(name).unwrap();
        // Their texts read back as null, as "required", as a list with an empty item, and as
        // past what an input parses.
        for (name, value) in [
            ("title", json!("")),
            ("name", json!("")),
            ("tags", json!(["a", null])),
            ("visits", json!(u64::MAX)),
        ] {
            assert!(
                typed(property(name), &value, &edit_text(&value)).is_none(),
                "{name}"
            );
        }
        assert_eq!(
            typed(property("title"), &json!(""), "Engineer"),
            Some(Ok(json!("Engineer")))
        );
        assert_eq!(
            typed(property("title"), &json!("Engineer"), ""),
            Some(Ok(Value::Null))
        );
        assert!(matches!(
            typed(property("name"), &json!("Ada"), " "),
            Some(Err(_))
        ));
    }
}
