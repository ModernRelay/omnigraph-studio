//! Edit from the review (spec 008, "Act in place"): a form of a changed node's editable
//! properties, filled with what the branch holds now. Saving sends only the fields that changed,
//! as one mutation on the reviewed branch, bound to the reviewed head like a revert.

use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, WeakEntity,
    Window,
};
use gq_language::Property;
use gq_language::input::{edit_text, parse_input};
use serde_json::Value;
use studio_ui::ds;
use ui::prelude::*;

use crate::view::ReviewView;
use studio_workspace::ModalView;

struct Field {
    property: Property,
    editor: Entity<Editor>,
    /// The value the branch holds, which the save is bound to.
    current: Value,
    error: Option<String>,
}

pub(crate) struct EditModal {
    view: WeakEntity<ReviewView>,
    /// The change the form edits, by its key.
    key: String,
    title: SharedString,
    branch: String,
    fields: Vec<Field>,
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
        branch: String,
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
                    error: None,
                }
            })
            .collect();
        Self {
            view,
            key,
            title: title.into(),
            branch,
            fields,
            focus_handle: cx.focus_handle(),
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let mut changes = Vec::new();
        let mut valid = true;
        for field in &mut self.fields {
            let text = field.editor.read(cx).text(cx);
            match parse_input(&field.property, &text) {
                Ok(value) => {
                    field.error = None;
                    if value != field.current {
                        changes.push((field.property.name.clone(), field.current.clone(), value));
                    }
                }
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
        cx.emit(DismissEvent);
        if changes.is_empty() {
            return;
        }
        let key = self.key.clone();
        self.view
            .update(cx, |view, cx| view.save_edit(&key, changes, cx))
            .ok();
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
                    .meta(ds::write_target(self.branch.clone(), cx))
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(ds::modal_body().children(self.fields.iter().map(|field| {
                ds::field(
                    field.property.name.clone(),
                    v_flex()
                        .gap(px(ds::space::XXS))
                        .child(
                            ds::field_frame(field.error.is_some(), cx)
                                .child(ds::field_input(field.editor.clone())),
                        )
                        .children(field.error.clone().map(studio_ui::ds::text::error_note)),
                )
            })))
            .child(
                ds::modal_footer(cx)
                    .child(
                        ds::cancel_button("cancel-edit")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        ds::action_button("save-edit", "Save")
                            .tooltip(ui::Tooltip::text(format!(
                                "Writes the changed fields to {}, bound to the reviewed head",
                                self.branch
                            )))
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}
