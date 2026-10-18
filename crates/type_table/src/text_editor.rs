//! A text property in an editor of its own (spec 014): Zed's editor beside the markdown it
//! renders, as Zed's markdown preview to the side. ⌘S saves it bound to the value it was read
//! at, as every node property saves, so a value changed elsewhere meanwhile is not overwritten.

use std::sync::Arc;

use editor::{CurrentLineHighlight, Editor, EditorEvent};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    Task, Window,
};
use gq_language::input::{edit_text, parse_input};
use gq_language::{SchemaModel, builder};
use graph_store::{DraftKey, GraphSession, Loadable, SessionEvent};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde_json::{Value, json};
use studio_actions::node::SaveText;
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;
use ui::{Indicator, Tooltip};

use crate::model::{Notice, SaveResult, Tone, node_rows, same_value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Edit,
    Split,
    Preview,
}

/// How the last save went, as the toolbar says it.
enum State {
    Idle,
    Saving,
    Saved,
    /// The value changed elsewhere while it was edited: the person's text is kept, and they
    /// choose whose wins.
    Conflict,
    Failed(Notice),
}

pub struct TextEditor {
    session: Entity<GraphSession>,
    /// The branch the value was read on, which saves go to: the window can move to another
    /// branch while text is being written.
    branch: String,
    schema: Option<Arc<SchemaModel>>,
    type_name: String,
    id: String,
    property: String,
    /// What saves are bound to: the value as it was read, or as it was last saved.
    old: Loadable<Value>,
    /// The text differs from `old`: found once per edit, not on every draw of the tab.
    unsaved: bool,
    /// The commit the value is read at: the one a field was being edited at, else the head.
    read_at: Option<String>,
    /// Text typed in a field before it was opened here, kept over the value read.
    typed: Option<String>,
    label: Option<String>,
    editor: Entity<Editor>,
    preview: Entity<Markdown>,
    mode: Mode,
    state: State,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    _read: Option<Task<()>>,
}

impl TextEditor {
    /// The editor for `property` of a node, read at `read_at` (the commit a field was being
    /// edited at, so saves stay bound to what was read) or the branch head, showing `typed`
    /// when text was typed before it was opened here.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: Entity<GraphSession>,
        type_name: String,
        id: String,
        property: String,
        read_at: Option<String>,
        typed: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_soft_wrap();
            editor.set_show_gutter(false, cx);
            editor.set_show_line_numbers(false, cx);
            // Prose: no shaded current line.
            editor.set_current_line_highlight(Some(CurrentLineHighlight::None));
            editor.set_placeholder_text("Write markdown…", window, cx);
            editor
        });
        let preview = cx.new(|cx| Markdown::new(SharedString::default(), None, None, cx));
        let mut subscriptions = vec![
            // The preview follows the text as it is typed; the tab's dot follows whether it
            // differs from the saved value.
            cx.subscribe_in(&editor, window, |this, _, event: &EditorEvent, _, cx| {
                if let EditorEvent::BufferEdited = event {
                    let text = this.editor.read(cx).text(cx);
                    this.unsaved =
                        matches!(&this.old, Loadable::Loaded(old) if text != edit_text(old));
                    this.preview
                        .update(cx, |preview, cx| preview.replace(text, cx));
                    if matches!(this.state, State::Saved) {
                        this.state = State::Idle;
                    }
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }
            }),
        ];
        let schema = session.read(cx).schema_model();
        let branch = session.read(cx).branch.clone();
        // On another branch, an editor with nothing unsaved reads the property there; one
        // with unsaved text keeps writing to the branch it was read on, and says so.
        let follow = cx.subscribe_in(&session, window, |this, _, event, window, cx| {
            if let SessionEvent::BranchChanged = event
                && !this.unsaved
                && this.typed.is_none()
                && this.read_at.is_none()
                && !matches!(this.state, State::Saving)
            {
                this.state = State::Idle;
                this.read(window, cx);
            } else if let SessionEvent::BranchChanged = event {
                cx.notify();
            }
        });
        subscriptions.push(follow);
        let mut this = Self {
            session,
            branch,
            schema,
            type_name,
            id,
            property,
            old: Loadable::Loading,
            unsaved: false,
            read_at,
            typed,
            label: None,
            editor,
            preview,
            mode: Mode::Split,
            state: State::Idle,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
            _read: None,
        };
        this.read(window, cx);
        this
    }

    /// One editor per node property.
    pub fn key(type_name: &str, id: &str, property: &str) -> String {
        format!("text:{type_name}:{id}:{property}")
    }

    fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.set_text(text.clone(), window, cx));
        self.preview
            .update(cx, |preview, cx| preview.replace(text, cx));
    }

    /// Reads the node: the value saves are bound to, and the text, unless some was typed.
    fn read(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(schema) = self.schema.clone() else {
            self.old = Loadable::Failed("The schema is still loading.".into());
            return;
        };
        let query = match builder::node_by_id(&schema, &self.type_name, &self.id) {
            Ok(query) => query,
            Err(error) => {
                self.old = Loadable::Failed(error.to_string());
                return;
            }
        };
        self.old = Loadable::Loading;
        self.branch = self.session.read(cx).branch.clone();
        let at = self.read_at.clone();
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, at, cx));
        let property = self.property.clone();
        self._read = Some(cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(output) => match node_rows(&output.rows).into_iter().next() {
                        Some(row) => {
                            let value = row.values.get(&property).cloned().unwrap_or(Value::Null);
                            this.label = this.label_from(&row.values);
                            let text = this.typed.take().unwrap_or_else(|| edit_text(&value));
                            this.unsaved = text != edit_text(&value);
                            this.set_text(text, window, cx);
                            this.old = Loadable::Loaded(value);
                        }
                        None => {
                            this.old =
                                Loadable::Failed("This node does not exist on this branch.".into())
                        }
                    },
                    Err(error) => this.old = Loadable::Failed(error.to_string()),
                }
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .ok();
        }));
    }

    fn label_from(&self, values: &serde_json::Map<String, Value>) -> Option<String> {
        let node = self.schema.as_ref()?.node(&self.type_name)?;
        let label = node.label_property()?;
        (Some(label) != node.key.as_deref())
            .then(|| values.get(label)?.as_str().map(str::to_owned))
            .flatten()
    }

    fn text(&self, cx: &App) -> String {
        self.editor.read(cx).text(cx)
    }

    fn property(&self) -> Option<gq_language::Property> {
        self.schema
            .as_ref()?
            .node(&self.type_name)?
            .property(&self.property)
            .cloned()
    }

    /// ⌘S: writes the text bound to the value it was read at.
    fn save(&mut self, _: &SaveText, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(property), Some(schema), Loadable::Loaded(old)) =
            (self.property(), self.schema.clone(), &self.old)
        else {
            return;
        };
        if matches!(self.state, State::Saving) {
            return;
        }
        let old = old.clone();
        let new = match parse_input(&property, &self.text(cx)) {
            Ok(value) => value,
            Err(message) => {
                self.state = State::Failed(Notice::new(Tone::Invalid, message));
                cx.notify();
                return;
            }
        };
        if same_value(&new, &old) {
            return;
        }
        let write = match crate::write::write_property(
            &self.session,
            &self.branch,
            &schema,
            &self.type_name,
            &self.id,
            &self.property,
            &old,
            &new,
            cx,
        ) {
            Ok(write) => write,
            Err(error) => {
                self.state = State::Failed(Notice::new(Tone::Invalid, error));
                cx.notify();
                return;
            }
        };
        self.state = State::Saving;
        let branch = self.branch.clone();
        // Detached: closing the tab never cancels a write.
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                this.state = match result {
                    SaveResult::Saved { .. } => {
                        // Typing while it was written stays unsaved.
                        this.unsaved = this.text(cx) != edit_text(&new);
                        this.old = Loadable::Loaded(new);
                        // A draft kept for the property elsewhere on the branch written is now
                        // saved too, though the window moved to another branch meanwhile.
                        let key = DraftKey {
                            type_name: this.type_name.clone(),
                            id: this.id.clone(),
                            property: this.property.clone(),
                        };
                        this.session
                            .update(cx, |session, cx| session.remove_draft_on(&branch, &key, cx));
                        State::Saved
                    }
                    SaveResult::NoMatch => State::Conflict,
                    SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                        State::Failed(notice)
                    }
                };
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// After a conflict: the value as it is now replaces the person's text.
    fn take_theirs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state = State::Idle;
        self.read_at = None;
        self.read(window, cx);
    }

    /// After a conflict: the person's text stays, and the next save is bound to the value as
    /// it is now, so it replaces what the other writer saved.
    fn keep_mine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state = State::Idle;
        self.read_at = None;
        self.typed = Some(self.text(cx));
        self.read(window, cx);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = self.branch.clone();
        // Unsaved text read on a branch the window has left still saves there.
        let elsewhere = (self.session.read(cx).branch != branch).then(|| {
            Label::new(format!("Saves to {branch}"))
                .size(LabelSize::Small)
                .color(Color::Warning)
        });
        let dirty = self.is_dirty(cx);
        let mode = self.mode;
        let segment = |id: &'static str, label: &'static str, to: Mode, cx: &mut Context<Self>| {
            studio_ui::ds::segment(id, label, mode == to, cx).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.mode = to;
                    cx.notify();
                },
            ))
        };
        let status = match &self.state {
            State::Idle => None,
            State::Saving => Some(studio_ui::ds::text::meta("Saving…").into_any_element()),
            State::Saved => Some(studio_ui::ds::text::meta("Saved").into_any_element()),
            State::Conflict => Some(
                h_flex()
                    .gap(px(studio_ui::ds::space::SM))
                    .child(Indicator::dot().color(Color::Warning))
                    .child(
                        Label::new("Changed elsewhere while you edited")
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        studio_ui::ds::button("text-theirs", "Use theirs")
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text(
                                "Replace your text with the value as it is now",
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.take_theirs(window, cx)),
                            ),
                    )
                    .child(
                        studio_ui::ds::button("text-mine", "Keep mine")
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text(
                                "Keep your text; the next save replaces the value as it is now",
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.keep_mine(window, cx)),
                            ),
                    )
                    .into_any_element(),
            ),
            State::Failed(notice) => Some(
                h_flex()
                    .gap(px(studio_ui::ds::space::SM))
                    .child(Indicator::dot().color(notice.tone.color()))
                    .child(
                        Label::new(notice.message.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            ),
        };
        studio_ui::ds::toolbar(cx)
            .child(studio_ui::type_swatch(&self.type_name, cx))
            .child(
                Label::new(self.label.clone().unwrap_or_else(|| self.id.clone()))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .single_line(),
            )
            .child(
                Label::new(self.property.clone())
                    .size(LabelSize::Small)
                    .single_line(),
            )
            .child(div().flex_1())
            .children(elsewhere)
            .children(status)
            .child(
                studio_ui::ds::segmented(cx)
                    .child(segment("text-edit", "Edit", Mode::Edit, cx))
                    .child(segment("text-split", "Split", Mode::Split, cx))
                    .child(segment("text-preview", "Preview", Mode::Preview, cx)),
            )
            .child(
                studio_ui::ds::toolbar_button("text-save", "Save")
                    .style(if dirty {
                        ButtonStyle::Filled
                    } else {
                        ButtonStyle::Subtle
                    })
                    .disabled(!dirty || matches!(self.state, State::Saving))
                    .key_binding(ui::KeyBinding::for_action_in(
                        &SaveText,
                        &self.focus_handle,
                        cx,
                    ))
                    .tooltip(Tooltip::text(format!(
                        "Write to {branch}, bound to the value it was read at"
                    )))
                    .on_click(cx.listener(|this, _, window, cx| this.save(&SaveText, window, cx))),
            )
    }
}

impl EventEmitter<ItemEvent> for TextEditor {}

impl Focusable for TextEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.mode {
            Mode::Preview => self.focus_handle.clone(),
            Mode::Edit | Mode::Split => self.editor.focus_handle(cx),
        }
    }
}

impl Item for TextEditor {
    fn tab_title(&self, _cx: &App) -> SharedString {
        let node = self.label.clone().unwrap_or_else(|| self.id.clone());
        format!("{node} · {}", self.property).into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::Pencil)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(Self::key(&self.type_name, &self.id, &self.property).into())
    }

    /// Unsaved: the text differs from the value saves are bound to.
    fn is_dirty(&self, _cx: &App) -> bool {
        self.unsaved && matches!(self.old, Loadable::Loaded(_))
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "text".into(),
            data: json!({
                "type_name": self.type_name,
                "id": self.id,
                "property": self.property,
            }),
        })
    }
}

/// The preview, as Zed's markdown preview renders a file.
fn preview_style(window: &Window, cx: &App) -> MarkdownStyle {
    MarkdownStyle::themed(MarkdownFont::Preview, window, cx)
}

impl Render for TextEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rule = studio_ui::ds::surface::rule(cx);
        let body = match &self.old {
            Loadable::Failed(error) => {
                studio_ui::ds::empty_state(IconName::Warning, error.clone()).into_any_element()
            }
            Loadable::Loading | Loadable::NotLoaded => {
                studio_ui::ds::empty_state(IconName::Pencil, "Reading…").into_any_element()
            }
            Loadable::Loaded(_) => {
                // The editor runs to its column's edge, so its scrollbar sits there.
                let editor = div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .pl(px(studio_ui::ds::space::PAGE))
                    .pt(px(studio_ui::ds::space::XL))
                    .child(self.editor.clone());
                let preview = div()
                    .id("text-preview")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .px(px(studio_ui::ds::space::PAGE))
                    .py(px(studio_ui::ds::space::XL))
                    .child(MarkdownElement::new(
                        self.preview.clone(),
                        preview_style(window, cx),
                    ));
                match self.mode {
                    Mode::Edit => editor.into_any_element(),
                    Mode::Preview => preview.into_any_element(),
                    Mode::Split => h_flex()
                        .size_full()
                        .child(editor.border_r_1().border_color(rule))
                        .child(preview)
                        .into_any_element(),
                }
            }
        };
        v_flex()
            .key_context("TextEditor")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::save))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    use crate::test_support::{PEOPLE, committed, session_answering};

    fn editor_answering(
        status: u16,
        body: Value,
        cx: &mut TestAppContext,
    ) -> (Entity<TextEditor>, &mut VisualTestContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session_answering(PEOPLE, status, body, cx);
        cx.run_until_parked();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            TextEditor::new(
                session,
                "Person".into(),
                "p1".into(),
                "name".into(),
                Some("c1".into()),
                Some("Ada Lovelace".into()),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        (view, cx)
    }

    fn save(view: &Entity<TextEditor>, cx: &mut VisualTestContext) {
        view.update_in(cx, |view, window, cx| view.save(&SaveText, window, cx));
        cx.run_until_parked();
    }

    /// Opened from a field being edited, the editor shows what was typed over the value read
    /// at that commit; a save binds to that value and leaves the tab clean.
    #[gpui::test]
    async fn a_saved_text_is_bound_to_the_value_read_and_leaves_the_tab_clean(
        cx: &mut TestAppContext,
    ) {
        let (view, cx) = editor_answering(200, committed("c2", "c1"), cx);
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.text(cx),
                "Ada Lovelace",
                "the typed text, not the value read"
            );
            assert!(matches!(&view.old, Loadable::Loaded(old) if old == "Ada"));
            assert!(view.is_dirty(cx));
        });
        save(&view, cx);
        view.read_with(cx, |view, cx| {
            assert!(matches!(view.state, State::Saved));
            assert!(!view.is_dirty(cx), "saved");
        });
    }

    /// With nothing unsaved the editor follows the window to another branch and reads the
    /// value there; with unsaved text it keeps saving to the branch it read it on.
    #[gpui::test]
    async fn unsaved_text_saves_to_the_branch_it_was_read_on(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session_answering(PEOPLE, 200, committed("c2", "c1"), cx);
        cx.run_until_parked();
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            move |window, cx| {
                TextEditor::new(
                    session,
                    "Person".into(),
                    "p1".into(),
                    "name".into(),
                    None,
                    None,
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.branch, "fix", "followed"));
        view.update_in(cx, |view, window, cx| {
            view.set_text("Ada Lovelace".into(), window, cx);
        });
        cx.run_until_parked();
        session.update(cx, |session, cx| session.set_branch("main".into(), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.branch, "fix", "kept with the text")
        });
        save(&view, cx);
        session.read_with(cx, |session, _| {
            assert_eq!(session.outcomes.last().unwrap().branch, "fix");
        });
    }

    /// A value changed elsewhere meanwhile is not overwritten: the save matches nothing, the
    /// text stays, and the person chooses whose wins.
    #[gpui::test]
    async fn a_text_changed_elsewhere_is_kept_and_offered_both_ways(cx: &mut TestAppContext) {
        let mut no_match = committed("c2", "c1");
        no_match["affected_nodes"] = 0.into();
        let (view, cx) = editor_answering(200, no_match, cx);
        save(&view, cx);
        view.read_with(cx, |view, cx| {
            assert!(matches!(view.state, State::Conflict));
            assert_eq!(view.text(cx), "Ada Lovelace");
            assert!(view.is_dirty(cx));
        });
    }

    /// A refused save keeps the text and says why (constitution III).
    #[gpui::test]
    async fn a_refused_text_is_kept_with_the_reason(cx: &mut TestAppContext) {
        let refused = json!({ "error": "policy denied", "code": "forbidden" });
        let (view, cx) = editor_answering(403, refused, cx);
        save(&view, cx);
        view.read_with(cx, |view, cx| {
            assert!(matches!(&view.state, State::Failed(notice) if notice.tone == Tone::Refused));
            assert_eq!(view.text(cx), "Ada Lovelace");
            assert!(view.is_dirty(cx));
        });
    }
}
