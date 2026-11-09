//! Names a view of a stored query (spec 024): the stored query's name and the parameters' values,
//! kept per graph in the settings. The tab it was saved from becomes the view.

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Subscription,
    WeakEntity, Window,
};
use studio_settings::{StudioSettings, ViewQuery};
use studio_ui::ds::{ModalHeader, action_button, cancel_button, modal, modal_body, modal_footer};
use studio_workspace::ModalView;
use ui::prelude::*;

use crate::view::QueryEditor;

pub struct NameViewModal {
    graph: String,
    query: ViewQuery,
    editor: WeakEntity<QueryEditor>,
    name: Entity<Editor>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for NameViewModal {}

impl ModalView for NameViewModal {}

impl Focusable for NameViewModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl NameViewModal {
    pub fn new(
        graph: String,
        query: ViewQuery,
        editor: WeakEntity<QueryEditor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Name, for example Customers in Berlin", window, cx);
            editor
        });
        let subscription = cx.subscribe(&name, |_, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                cx.notify();
            }
        });
        Self {
            graph,
            query,
            editor,
            name,
            _subscription: subscription,
        }
    }

    fn entered(&self, cx: &App) -> String {
        self.name.read(cx).text(cx).trim().to_owned()
    }

    fn taken(&self, name: &str, cx: &App) -> bool {
        cx.try_global::<StudioSettings>()
            .is_some_and(|settings| settings.view_name_taken(&self.graph, name, None))
    }

    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let name = self.entered(cx);
        if name.is_empty() || self.taken(&name, cx) {
            return;
        }
        let graph = self.graph.clone();
        let query = self.query.clone();
        let mut saved = None;
        studio_settings::update(cx, |settings| {
            saved = settings.save_query_view(&graph, &name, query);
        });
        // Settings that refuse every change (a test's, a file that cannot be written) leave
        // no view to become.
        let saved = saved.filter(|id| StudioSettings::get(cx).view(&self.graph, id).is_some());
        if let Some(id) = saved {
            self.editor
                .update(cx, |editor, cx| editor.become_view(id, cx))
                .ok();
        }
        cx.emit(DismissEvent);
    }
}

impl Render for NameViewModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.entered(cx);
        let taken = self.taken(&name, cx);
        let values = match self.query.params.len() {
            0 => "no parameters".to_owned(),
            1 => "the value of its parameter".to_owned(),
            _ => "the values of its parameters".to_owned(),
        };
        modal(cx)
            .key_context("NameViewModal")
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.submit(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(
                ModalHeader::new(format!("Save a view of {}", self.query.label()))
                    .description(SharedString::from(format!(
                        "The stored query and {values}, kept for this graph in your settings."
                    )))
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                modal_body()
                    .child(
                        studio_ui::ds::field_frame(taken, cx)
                            .child(studio_ui::ds::field_input(self.name.clone())),
                    )
                    .when(taken, |d| {
                        d.child(
                            studio_ui::ds::text::body(format!(
                                "Another view is named “{name}”. Choose another name."
                            ))
                            .color(Color::Error),
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
                        action_button("save-view", "Save")
                            .disabled(name.is_empty() || taken)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
