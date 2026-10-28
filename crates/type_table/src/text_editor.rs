//! A text property in an editor of its own (spec 014): Zed's editor beside the markdown it
//! renders, as Zed's markdown preview to the side. ⌘S saves it bound to the value it was read
//! at, or a draft opened here to the value it was typed over, as every node property saves, so
//! a value changed elsewhere meanwhile is not overwritten.

use std::sync::Arc;
use std::time::Duration;

use editor::{CurrentLineHighlight, Editor, EditorEvent, MultiBufferOffset};
use gpui::{
    AnyView, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    StyleRefinement, Subscription, Task, Window,
};
use gq_language::input::edit_text;
use gq_language::{SchemaModel, builder};
use graph_store::{DraftEntry, DraftKey, GraphSession, Loadable, SessionEvent};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde_json::{Value, json};
use studio_actions::node::SaveText;
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;
use ui::{Indicator, Tooltip};

use crate::model::{Notice, SaveResult, Tone, edited, node_label, node_rows};

/// How long typing must pause before the preview shows the text: each update copies all of
/// it, and Zed's own markdown preview waits longer.
const PREVIEW_AFTER: Duration = Duration::from_millis(100);

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
    /// What saves are bound to: the value as it was read (for a draft, the value it was typed
    /// over), or as it was last saved; set through [`Self::set_old`] only.
    old: Loadable<Value>,
    /// `old` as the editor shows it, made once per value rather than at each keystroke.
    old_text: Option<String>,
    /// The text differs from `old`: found once per edit, not on every draw of the tab.
    unsaved: bool,
    /// The commit the value is read at: the one a field was being edited at, else the head.
    read_at: Option<String>,
    /// Text typed in a field before it was opened here, kept over the value read.
    typed: Option<String>,
    /// The value `typed` was typed over, when it is the property's draft: what the first save is
    /// bound to, whatever the read finds at `read_at`, which a Refresh since may have moved past
    /// a change made elsewhere.
    typed_over: Option<Value>,
    label: Option<String>,
    editor: Entity<Editor>,
    preview: Entity<Markdown>,
    preview_view: Entity<Preview>,
    /// The preview shows older text: typed while it was hidden, it catches up when shown.
    preview_behind: bool,
    _preview: Option<Task<()>>,
    mode: Mode,
    state: State,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    _read: Option<Task<()>>,
    /// The node is read when the tab is next drawn ([`Self::owe_read`]): a tab restored with the
    /// workspace, or one behind another when the branch moves, reads nothing until it is shown.
    read_owed: bool,
}

impl TextEditor {
    /// The editor for `property` of a node, read at `read_at` (the commit a field was being
    /// edited at, so saves stay bound to what was read) or the branch head, showing `typed`
    /// when text was typed before it was opened here. Typed text that is the property's draft
    /// on the session's branch is bound, as the draft is, to the value it was typed over.
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
        let preview_view = cx.new(|cx| Preview {
            markdown: preview.clone(),
            _changed: cx.observe(&preview, |_, _, cx| cx.notify()),
        });
        let mut subscriptions = vec![
            // The preview follows the text once typing pauses; the tab's dot follows whether
            // it differs from the saved value.
            cx.subscribe_in(&editor, window, |this, _, event: &EditorEvent, _, cx| {
                if let EditorEvent::BufferEdited = event {
                    let was = this.unsaved;
                    this.unsaved = this.differs(cx);
                    this.preview_later(cx);
                    let saved = matches!(this.state, State::Saved);
                    if saved {
                        this.state = State::Idle;
                    }
                    // The tab and the window around it draw again when the dot or the state
                    // changes, not at every key: the editor draws its own text.
                    if saved || this.unsaved != was {
                        cx.emit(ItemEvent::UpdateTab);
                        cx.notify();
                    }
                }
            }),
        ];
        let schema = session.read(cx).schema_model();
        let branch = session.read(cx).branch.clone();
        // A draft opened here keeps the value it was typed over, which the session holds with
        // it: the value read now may be one changed elsewhere since, which a save bound to it
        // would overwrite.
        let key = DraftKey {
            type_name: type_name.clone(),
            id: id.clone(),
            property: property.clone(),
        };
        let typed_over = typed.as_ref().and_then(|text| {
            let draft = session.read(cx).draft_on(&branch, &key)?;
            (draft.text == *text).then(|| draft.old.clone())
        });
        // On another branch, an editor with nothing unsaved reads the property there; one
        // with unsaved text keeps writing to the branch it was read on, and says so.
        let follow = cx.subscribe_in(&session, window, |this, _, event, _, cx| {
            // The schema in at last (a tab restored as Studio starts): the node is read when the
            // tab is shown.
            if let SessionEvent::SchemaChanged = event {
                let schema = this.session.read(cx).schema_model();
                let waited = this.schema.is_none() && schema.is_some();
                this.schema = schema;
                if waited {
                    this.owe_read(cx);
                }
                cx.notify();
            } else if let SessionEvent::BranchChanged = event
                && !this.unsaved
                && this.typed.is_none()
                && this.read_at.is_none()
                && !matches!(this.state, State::Saving)
            {
                this.state = State::Idle;
                this.branch = this.session.read(cx).branch.clone();
                this.owe_read(cx);
            } else if let SessionEvent::BranchChanged | SessionEvent::AccessChanged = event {
                cx.notify();
            }
        });
        subscriptions.push(follow);
        Self {
            session,
            branch,
            schema,
            type_name,
            id,
            property,
            old: Loadable::Loading,
            old_text: None,
            unsaved: false,
            read_at,
            typed,
            typed_over,
            label: None,
            editor,
            preview,
            preview_view,
            preview_behind: false,
            _preview: None,
            mode: Mode::Split,
            state: State::Idle,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
            _read: None,
            read_owed: true,
        }
    }

    /// Reads the node when the tab is next drawn ([`Self::read_owed`]). Until then nothing is
    /// saved: the value saves were bound to was the branch left's, or none was read yet.
    fn owe_read(&mut self, cx: &mut Context<Self>) {
        self._read = None;
        self.set_old(Loadable::Loading);
        self.read_owed = true;
        cx.notify();
    }

    /// One editor per node property.
    pub fn key(type_name: &str, id: &str, property: &str) -> String {
        format!("text:{type_name}:{id}:{property}")
    }

    fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.set_text(text.clone(), window, cx));
        self.preview_behind = false;
        self.preview
            .update(cx, |preview, cx| preview.replace(text, cx));
    }

    /// Binds saves to `old`, and keeps the text it shows for comparing with what is typed.
    fn set_old(&mut self, old: Loadable<Value>) {
        self.old_text = match &old {
            Loadable::Loaded(value) => Some(edit_text(value)),
            _ => None,
        };
        self.old = old;
    }

    /// Whether the text differs from the value saves are bound to, compared where it lies:
    /// a keystroke copies none of it.
    fn differs(&self, cx: &App) -> bool {
        let Some(old) = &self.old_text else {
            return false;
        };
        let buffer = self.editor.read(cx).buffer().read(cx).read(cx);
        let end = buffer.len();
        !studio_ui::text::pieces_are(end.0, buffer.text_for_range(MultiBufferOffset(0)..end), old)
    }

    /// Brings the preview up to the text once typing pauses, or when it is next shown.
    fn preview_later(&mut self, cx: &mut Context<Self>) {
        if self.mode == Mode::Edit {
            self.preview_behind = true;
            self._preview = None;
            return;
        }
        self._preview = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_AFTER).await;
            this.update(cx, |this, cx| this.preview_now(cx)).ok();
        }));
    }

    fn preview_now(&mut self, cx: &mut Context<Self>) {
        self.preview_behind = false;
        let text = self.text(cx);
        self.preview
            .update(cx, |preview, cx| preview.replace(text, cx));
    }

    fn set_mode(&mut self, to: Mode, cx: &mut Context<Self>) {
        self.mode = to;
        if to != Mode::Edit && self.preview_behind {
            self.preview_now(cx);
        }
        cx.notify();
    }

    /// Reads the node: the value saves are bound to, and the text, unless some was typed.
    fn read(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(schema) = self.schema.clone() else {
            self.set_old(Loadable::Failed("The schema is still loading.".into()));
            return;
        };
        let query = match builder::node_by_id(&schema, &self.type_name, &self.id) {
            Ok(query) => query,
            Err(error) => {
                self.set_old(Loadable::Failed(error.to_string()));
                return;
            }
        };
        self.set_old(Loadable::Loading);
        // The branch the editor saves to, which it keeps while the window moves elsewhere.
        let read = match self.read_at.clone() {
            Some(at) => self
                .session
                .update(cx, |session, cx| session.read(&query, Some(at), cx)),
            None => {
                let branch = self.branch.clone();
                self.session
                    .update(cx, |session, cx| session.read_on(&branch, &query, cx))
            }
        };
        let property = self.property.clone();
        self._read = Some(cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(output) => match node_rows(&output.rows).into_iter().next() {
                        Some(row) => {
                            let read = row.values.get(&property).cloned().unwrap_or(Value::Null);
                            this.label = this.label_from(&row.values);
                            // A draft's text stays bound to the value it was typed over: a
                            // change made since shows as a conflict when it is saved.
                            let value = this.typed_over.take().unwrap_or(read);
                            this.set_old(Loadable::Loaded(value));
                            let old = this.old_text.clone().unwrap_or_default();
                            let text = this.typed.take().unwrap_or(old);
                            this.unsaved = this.old_text.as_ref() != Some(&text);
                            this.set_text(text, window, cx);
                        }
                        None => this.set_old(Loadable::Failed(
                            "This node does not exist on this branch.".into(),
                        )),
                    },
                    Err(error) => this.set_old(Loadable::Failed(error.to_string())),
                }
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .ok();
        }));
    }

    fn label_from(&self, values: &serde_json::Map<String, Value>) -> Option<String> {
        node_label(self.schema.as_ref()?.node(&self.type_name)?, values)
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
        if matches!(self.state, State::Saving) || self.change_refusal(cx).is_some() {
            return;
        }
        let old = old.clone();
        let text = self.text(cx);
        // Text asking for the value read sends nothing: the text the editor was filled with, or
        // that value spelled otherwise.
        let new = match edited(&property, &old, &text) {
            Some(Ok(value)) => value,
            Some(Err(message)) => {
                self.state = State::Failed(Notice::new(Tone::Invalid, message));
                cx.notify();
                return;
            }
            None => return,
        };
        let write = match crate::write::write_property(
            &self.session,
            &self.branch,
            &schema,
            &self.type_name,
            &self.id,
            &self.property,
            &old,
            &new,
            None,
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
        let session = self.session.clone();
        let key = DraftKey {
            type_name: self.type_name.clone(),
            id: self.id.clone(),
            property: self.property.clone(),
        };
        // The property's draft as the save goes out: one retyped or let go elsewhere while it
        // is out stands (`GraphSession::settle_draft_on`).
        let sent = session.read(cx).draft_on(&branch, &key).cloned();
        // Detached: closing the tab never cancels a write.
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            let shown = this.update(cx, |this, cx| {
                this.state = match &result {
                    SaveResult::Saved { .. } => {
                        // Typing while it was written stays unsaved.
                        this.set_old(Loadable::Loaded(new));
                        this.unsaved = this.differs(cx);
                        // A draft kept for the property elsewhere on the branch written is now
                        // saved too, though the window moved to another branch meanwhile.
                        let key = key.clone();
                        this.session.update(cx, |session, cx| {
                            session.settle_draft_on(&branch, key, sent.as_ref(), None, cx)
                        });
                        State::Saved
                    }
                    SaveResult::NoMatch => State::Conflict,
                    SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                        State::Failed(notice.clone())
                    }
                };
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            });
            // The tab closed while the text was written: a draft of the property on the branch
            // written is let go when it landed, and holds the text when it did not.
            if shown.is_err() {
                let keep = (!matches!(result, SaveResult::Saved { .. }))
                    .then_some(DraftEntry { old, text });
                session.update(cx, |session, cx| {
                    session.settle_draft_on(&branch, key, sent.as_ref(), keep, cx)
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// After a conflict: the value as it is now replaces the person's text.
    fn take_theirs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state = State::Idle;
        self.read_at = None;
        self.typed_over = None;
        self.read(window, cx);
    }

    /// After a conflict: the person's text stays, and the next save is bound to the value as
    /// it is now, so it replaces what the other writer saved.
    fn keep_mine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state = State::Idle;
        self.read_at = None;
        self.typed = Some(self.text(cx));
        self.typed_over = None;
        self.read(window, cx);
    }

    /// Why the credential may not change the branch saves go to, once the access route said so
    /// (spec 002): Save is disabled with it and not registered, and the text stays in the tab.
    fn change_refusal(&self, cx: &App) -> Option<String> {
        crate::write::change_refusal(&self.session, &self.branch, cx)
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = self.branch.clone();
        let refused = self.change_refusal(cx);
        // Unsaved text read on a branch the window has left still saves there.
        let elsewhere = (self.session.read(cx).branch != branch)
            .then(|| studio_ui::ds::text::body(format!("Saves to {branch}")).color(Color::Warning));
        let dirty = self.is_dirty(cx);
        let mode = self.mode;
        let segment = |id: &'static str, label: &'static str, to: Mode, cx: &mut Context<Self>| {
            studio_ui::ds::segment(id, label, mode == to, cx)
                .on_click(cx.listener(move |this, _, _, cx| this.set_mode(to, cx)))
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
                        studio_ui::ds::text::body("Changed elsewhere while you edited")
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
                    .child(studio_ui::ds::text::muted(notice.message.clone()))
                    .into_any_element(),
            ),
        };
        studio_ui::ds::toolbar(cx)
            .child(studio_ui::type_swatch(&self.type_name, cx))
            .child(
                studio_ui::ds::text::muted(self.label.clone().unwrap_or_else(|| self.id.clone()))
                    .single_line(),
            )
            .child(studio_ui::ds::text::body(self.property.clone()).single_line())
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
                    .disabled(!dirty || matches!(self.state, State::Saving) || refused.is_some())
                    // A key that is not registered is not shown.
                    .when(refused.is_none(), |button| {
                        button.key_binding(ui::KeyBinding::for_action_in(
                            &SaveText,
                            &self.focus_handle,
                            cx,
                        ))
                    })
                    .tooltip(Tooltip::text(refused.unwrap_or_else(|| {
                        format!("Write to {branch}, bound to the value it was read at")
                    })))
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

/// The rendered markdown as a view of its own, drawn again only when its text changes: drawn
/// with the editor, the whole document would be laid out again at every key and cursor blink.
struct Preview {
    markdown: Entity<Markdown>,
    _changed: Subscription,
}

impl Render for Preview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("text-preview")
            .size_full()
            .overflow_y_scroll()
            .px(px(studio_ui::ds::space::PAGE))
            .py(px(studio_ui::ds::space::XL))
            .child(MarkdownElement::new(
                self.markdown.clone(),
                preview_style(window, cx),
            ))
    }
}

impl Render for TextEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.read_owed) {
            cx.defer_in(window, |this, window, cx| this.read(window, cx));
        }
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
                let preview = div().flex_1().min_w_0().h_full().child(
                    AnyView::from(self.preview_view.clone())
                        .cached(StyleRefinement::default().size_full()),
                );
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
            .when(self.change_refusal(cx).is_none(), |editor| {
                editor.on_action(cx.listener(Self::save))
            })
            .size_full()
            .bg(studio_ui::ds::surface::document(cx))
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    use crate::test_support::{PEOPLE, committed, session_answering, session_routing};

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

    /// A tab restored as Studio starts opens before the schema is in; it reads its node once
    /// the schema arrives, rather than saying the schema is loading for good.
    #[gpui::test]
    async fn a_tab_opened_before_the_schema_reads_its_node_once_it_is_in(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session_answering(PEOPLE, 200, committed("c2", "c1"), cx);
        let reloaded = session.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            TextEditor::new(
                session,
                "Person".into(),
                "p1".into(),
                "name".into(),
                Some("c1".into()),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        // As the tab was when it opened with no schema in.
        view.update_in(cx, |view, window, cx| {
            view.schema = None;
            view.read(window, cx);
            assert!(matches!(view.old, Loadable::Failed(_)), "no schema yet");
        });
        reloaded.update(cx, |session, cx| session.reload_schema(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(&view.old, Loadable::Loaded(old) if old == "Ada"));
        });
    }

    /// A tab not drawn (restored with the workspace behind another) reads nothing: not when it
    /// is made, nor when the branch moves; it saves nothing until it has read. Before, every such
    /// tab read its node at start and at each branch move.
    #[gpui::test]
    async fn a_tab_not_drawn_reads_nothing(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session_answering(PEOPLE, 200, committed("c2", "c1"), cx);
        cx.run_until_parked();
        let (_blank, cx) = cx.add_window_view(|_, _| gpui::EmptyView);
        let hidden = cx.update(|window, cx| {
            let session = session.clone();
            cx.new(|cx| {
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
            })
        });
        cx.run_until_parked();
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        hidden.read_with(cx, |hidden, _| {
            assert!(hidden._read.is_none(), "no read sent");
            assert!(hidden.read_owed, "it reads when drawn");
            assert!(
                matches!(hidden.old, Loadable::Loading),
                "nothing to save against"
            );
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

    /// After a conflict on a branch the window has left, the person's text stays bound to that
    /// branch: Keep mine reads the value there again, not on the window's branch.
    #[gpui::test]
    async fn keep_mine_reads_again_on_the_branch_the_text_saves_to(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let branches = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = branches.clone();
        let (session, _) = session_routing(
            PEOPLE,
            move |request| {
                if let Some(branch) = request["branch"].as_str() {
                    seen.lock().unwrap().push(branch.to_owned());
                }
                json!([{"n": {"@id": "p1", "slug": "p1", "name": "Ada"}}])
            },
            cx,
        );
        cx.run_until_parked();
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
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
        view.update_in(cx, |view, window, cx| {
            view.set_text("Ada Lovelace".into(), window, cx);
        });
        cx.run_until_parked();
        session.update(cx, |session, cx| session.set_branch("main".into(), cx));
        cx.run_until_parked();
        let before = branches.lock().unwrap().len();
        view.update_in(cx, |view, window, cx| view.keep_mine(window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.branch, "fix", "still saves to fix");
            assert_eq!(view.text(cx), "Ada Lovelace", "the text stays");
        });
        assert_eq!(
            branches.lock().unwrap()[before..].to_vec(),
            ["fix"],
            "read again on fix"
        );
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

    /// A draft opened here (⌘⇧↵ on a peek's field) after its node was read again, another
    /// writer's change in between, stays bound to the value it was typed over, not to the one
    /// read now: its save finds the change rather than overwriting it. Without a draft, typed
    /// text is bound to the value read, as before.
    #[gpui::test]
    async fn a_draft_opened_here_is_bound_to_the_value_it_was_typed_over(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, sent) = crate::test_support::session_recording_writes(PEOPLE, cx);
        cx.run_until_parked();
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        // Typed over "Ada Byron", which another writer has made "Ada" since: the read finds
        // "Ada".
        session.update(cx, |session, cx| {
            let draft = DraftEntry {
                old: json!("Ada Byron"),
                text: "Ada Lovelace".into(),
            };
            session.set_draft(key, draft, cx);
        });
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            move |window, cx| {
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
            }
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(
                matches!(&view.old, Loadable::Loaded(old) if old == "Ada Byron"),
                "the draft's value, not the one read: {:?}",
                view.old
            );
            assert_eq!(view.text(cx), "Ada Lovelace");
            assert!(view.is_dirty(cx));
        });
        save(&view, cx);
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "{sent:?}");
        let params: Vec<&Value> = sent[0]["params"].as_object().unwrap().values().collect();
        assert!(params.contains(&&json!("Ada Byron")), "{params:?}");
        assert!(!params.contains(&&json!("Ada")), "{params:?}");
    }

    /// Typing finds the tab dirty or clean against the value read, a text typed back to it
    /// included; the preview catches up once typing pauses, and a hidden one when shown.
    #[gpui::test]
    async fn typing_is_compared_with_the_value_and_previewed_after_a_pause(
        cx: &mut TestAppContext,
    ) {
        let (view, cx) = editor_answering(200, committed("c2", "c1"), cx);
        let type_text = |text: &'static str, cx: &mut VisualTestContext| {
            view.update_in(cx, |view, window, cx| {
                view.editor
                    .update(cx, |editor, cx| editor.set_text(text, window, cx))
            });
            cx.run_until_parked();
        };
        let preview = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, cx| view.preview.read(cx).source().to_string())
        };
        type_text("Ada", cx);
        view.read_with(cx, |view, cx| assert!(!view.is_dirty(cx), "typed back"));
        type_text("Adb", cx);
        view.read_with(cx, |view, cx| assert!(view.is_dirty(cx), "same length"));
        assert_eq!(preview(cx), "Ada Lovelace", "not while typing");
        cx.executor().advance_clock(PREVIEW_AFTER);
        cx.run_until_parked();
        assert_eq!(preview(cx), "Adb", "once typing pauses");
        view.update(cx, |view, cx| view.set_mode(Mode::Edit, cx));
        type_text("Ada Byron", cx);
        cx.executor().advance_clock(PREVIEW_AFTER);
        cx.run_until_parked();
        assert_eq!(preview(cx), "Adb", "hidden");
        view.update(cx, |view, cx| view.set_mode(Mode::Split, cx));
        assert_eq!(preview(cx), "Ada Byron", "shown");
    }

    /// A refused save keeps the text and says why (constitution III).
    #[gpui::test]
    async fn a_refused_text_is_kept_with_the_reason(cx: &mut TestAppContext) {
        let refused = json!({ "error": "policy denied", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } });
        let (view, cx) = editor_answering(403, refused, cx);
        save(&view, cx);
        view.read_with(cx, |view, cx| {
            assert!(matches!(&view.state, State::Failed(notice) if notice.tone == Tone::Refused));
            assert_eq!(view.text(cx), "Ada Lovelace");
            assert!(view.is_dirty(cx));
        });
    }

    /// On a branch the access route says the credential may not change, Save is neither
    /// registered nor sent; the text stays in the tab.
    #[gpui::test]
    async fn a_text_on_a_refused_branch_is_not_saved(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, sent, _) = crate::test_support::session_with_access(
            PEOPLE,
            crate::test_support::read_only_access(),
            cx,
        );
        cx.run_until_parked();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            TextEditor::new(
                session,
                "Person".into(),
                "p1".into(),
                "name".into(),
                None,
                Some("Ada Lovelace".into()),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let focus = view.read(cx).focus_handle.clone();
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        assert!(!cx.update(|window, cx| window.is_action_available(&SaveText, cx)));
        save(&view, cx);
        assert!(sent.lock().unwrap().is_empty(), "nothing was sent");
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.change_refusal(cx).as_deref(),
                Some("Your credential may not change main")
            );
            assert_eq!(view.text(cx), "Ada Lovelace");
            assert!(view.is_dirty(cx));
        });
    }

    /// The tab closed while its text was written: refused, the text is kept as the property's
    /// draft, bound to the value it was read at, which a table of the type shows.
    #[gpui::test]
    async fn a_refused_text_whose_tab_closed_is_kept_as_a_draft(cx: &mut TestAppContext) {
        let refused = json!({ "error": "policy denied", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } });
        let (view, cx) = editor_answering(403, refused, cx);
        let session = view.read_with(cx, |view, _| view.session.clone());
        view.update_in(cx, |view, window, cx| view.save(&SaveText, window, cx));
        drop(view);
        cx.update(|window, _| window.remove_window());
        cx.run_until_parked();
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        let draft = session.read_with(cx, |session, _| session.draft(&key).cloned());
        assert_eq!(
            draft,
            Some(DraftEntry {
                old: json!("Ada"),
                text: "Ada Lovelace".into(),
            })
        );
    }
}
