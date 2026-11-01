//! The schema's source, read-only (spec 007): the `.pg` text the server holds for the graph,
//! coloured and folded by the schema grammar. Text can be selected and copied; a schema changes
//! by deployment (`omnigraph cluster apply`), never from Studio. It follows the session: a branch
//! switch or a refresh that brings another schema shows the new text. A schema another writer
//! changed, seen at a head move, waits for the person: the tab offers the newer schema.

use editor::scroll::Autoscroll;
use editor::{Editor, MultiBufferOffset, SelectionEffects};
use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString};
use graph_store::{GraphSession, Loadable, SessionEvent};
use studio_actions::graph::OpenMap;
use studio_actions::{ViewKind, ViewLink};
use studio_ui::ds::Tipped;
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::TintColor;
use ui::prelude::*;

use crate::syntax::{Syntax, language};

pub const SCHEMA_KEY: &str = "schema";

pub struct SchemaView {
    session: Entity<GraphSession>,
    editor: Entity<Editor>,
    parsed: bool,
    /// The source the editor shows, so a reload with the same text keeps the scroll position.
    shown: Option<std::sync::Arc<str>>,
    /// A type to show once the schema has been read.
    reveal: Option<String>,
    /// The session reads under a newer schema than the one shown.
    newer: bool,
    _subscription: gpui::Subscription,
}

impl EventEmitter<ItemEvent> for SchemaView {}

impl Focusable for SchemaView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for SchemaView {
    fn tab_title(&self, _cx: &App) -> SharedString {
        "Source".into()
    }

    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        Some(studio_actions::Thing::Schema)
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::Code)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(SCHEMA_KEY.into())
    }

    fn views(&self, _cx: &App) -> Vec<ViewLink> {
        vec![
            ViewLink::to(ViewKind::Map),
            ViewLink::current(ViewKind::Source),
        ]
    }

    fn view_action(&self, kind: ViewKind, _cx: &App) -> Option<Box<dyn gpui::Action>> {
        (kind == ViewKind::Map)
            .then(|| Box::new(OpenMap { preview: true }) as Box<dyn gpui::Action>)
    }

    fn is_stale(&self, _cx: &App) -> bool {
        self.newer
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: SCHEMA_KEY.into(),
            data: serde_json::Value::Null,
        })
    }
}

impl SchemaView {
    pub fn new(session: Entity<GraphSession>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor
        });
        let subscription = cx.subscribe_in(
            &session,
            window,
            |this, _, event: &SessionEvent, window, cx| match event {
                SessionEvent::SchemaChanged => this.show_source(window, cx),
                SessionEvent::SchemaMoved if this.shown.is_some() => {
                    this.newer = true;
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }
                _ => {}
            },
        );
        let mut this = Self {
            session,
            editor,
            parsed: false,
            shown: None,
            reveal: None,
            newer: false,
            _subscription: subscription,
        };
        if let Some(language) = language(Syntax::Pg, cx)
            && let Some(buffer) = this.editor.read(cx).buffer().read(cx).as_singleton()
        {
            buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
            this.parsed = true;
        }
        this.show_source(window, cx);
        this
    }

    /// Shows a type's declaration, now or once the schema has been read.
    pub fn reveal(&mut self, type_name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.reveal = Some(type_name);
        self.show_source(window, cx);
    }

    fn show_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(schema) = self.session.read(cx).schema_model() else {
            cx.notify();
            return;
        };
        self.newer = false;
        if self.shown.as_ref() != Some(&schema.source) {
            let source = schema.source.clone();
            self.editor
                .update(cx, |editor, cx| editor.set_text(source, window, cx));
            self.shown = Some(schema.source.clone());
        }
        if let Some(type_name) = self.reveal.take()
            && let Some(name) = declaration(&schema.source, &type_name)
        {
            self.editor.update(cx, |editor, cx| {
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::top_relative(2.)),
                    window,
                    cx,
                    |s| {
                        s.select_ranges(
                            [MultiBufferOffset(name.start)..MultiBufferOffset(name.end)],
                        )
                    },
                );
            });
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }
}

/// Where `type_name` is named in its `node`, `edge` or `interface` declaration.
fn declaration(source: &str, type_name: &str) -> Option<std::ops::Range<usize>> {
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let code = line.trim_start();
        for keyword in ["node", "edge", "interface"] {
            let Some(rest) = code.strip_prefix(keyword) else {
                continue;
            };
            let name = rest.trim_start();
            if name.len() == rest.len() {
                continue;
            }
            let ends = name
                .strip_prefix(type_name)
                .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'));
            if ends {
                let start = offset + (line.len() - name.len());
                return Some(start..start + type_name.len());
            }
        }
        offset += line.len();
    }
    None
}

impl Render for SchemaView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.parsed {
            // Recolours the language when the theme has changed since it last coloured it.
            language(Syntax::Pg, cx);
        }
        if self.shown.is_none() {
            return match &self.session.read(cx).schema {
                Loadable::Failed(error) => {
                    studio_ui::ds::error_state("The schema could not be read", error.clone())
                }
                _ => studio_ui::ds::loading_state("Reading the schema…"),
            }
            .into_any_element();
        }
        v_flex()
            .size_full()
            .child(
                studio_ui::ds::toolbar(cx)
                    .children(studio_workspace::view_strip(Item::views(self, cx), cx)),
            )
            .when(self.newer, |d| {
                d.child(
                    h_flex()
                        .px(px(studio_ui::ds::space::MD))
                        .pb(px(studio_ui::ds::space::XS))
                        .child(
                            studio_ui::ds::button("newer-schema", "Newer schema · Refresh")
                                .style(ButtonStyle::Tinted(TintColor::Accent))
                                .tip(studio_ui::ds::tooltip(
                                    "The schema changed elsewhere; show the one the graph has now",
                                ))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.show_source(window, cx)),
                                ),
                        ),
                )
            })
            .child(self.editor.clone())
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::declaration;

    #[test]
    fn a_type_is_found_at_its_declaration_and_not_in_a_comment_or_a_longer_name() {
        let source = "// node Person is below\nnode PersonNote {}\n  node Person @description(\"x\") {\n}\nedge Knows: Person -> Person\n";
        let person = declaration(source, "Person").unwrap();
        assert_eq!(&source[person.clone()], "Person");
        assert!(source[..person.start].ends_with("  node "));
        let knows = declaration(source, "Knows").unwrap();
        assert!(source[..knows.start].ends_with("edge "));
        assert_eq!(declaration(source, "Company"), None);
        assert_eq!(declaration("nodePerson {}", "Person"), None);
    }
}
