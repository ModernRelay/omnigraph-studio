//! The settings tab (spec 002): a left nav of sections and, for the chosen one, a row per
//! setting with a title, a one-line description, a control and Reset. Rows and controls are
//! generated from the settings schema (`studio_settings::fields`); nothing here names a
//! setting but one: with a graph open, "Node names" lists that graph's types and the property
//! each one's nodes are named by, which only its schema can offer. A search across every
//! section replaces the section while it has text. With a graph open, a scope switch shows
//! that graph's own values for the settings a graph may override (`graphs` in
//! `settings.json`). Changes write `settings.json` and apply at once.

use std::rc::Rc;
use std::sync::Arc;

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Global, HighlightStyle, Render,
    SharedString, StyledText, Subscription, Task, WeakEntity, Window,
};
use gq_language::SchemaModel;
use graph_store::{GraphSession, SessionEvent};
use serde_json::Value;
use studio_settings::{FieldKind, Scope, SettingField, SettingsValues, StudioSettings, TypeNaming};
use studio_ui::ds::Tipped;
use studio_ui::ds::{self, Dotted as _, space};
use studio_ui::menu::ContextMenu;
use studio_workspace::models::{KeyState, Models};
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace, WorkspaceEvent};
use theme::ThemeRegistry;
use ui::prelude::*;
use ui::{PopoverMenu, Severity};

pub struct SettingsView {
    search: Entity<Editor>,
    /// The section chosen, by name: the sections listed change with the scope and with "Node
    /// names". `None` is the first.
    section: Option<&'static str>,
    /// The open graph's settings key and name, when there is one.
    graph: Option<(String, String)>,
    /// That graph's session, whose schema lists the types "Node names" offers.
    session: Option<WeakEntity<GraphSession>>,
    scope: Scope,
    /// A field for each text setting, made with the tab.
    text_inputs: Vec<TextInput>,
    /// The OpenRouter key being typed (spec 019): stored in the keychain on Store, never shown
    /// again.
    key_editor: Entity<Editor>,
    /// What the last Store, Remove or Test said.
    key_notice: Option<String>,
    /// The connection test under way (spec 019).
    key_test: Option<Task<()>>,
    /// The settings as values, read again when they change rather than at every frame.
    values: SettingsValues,
    /// The window's graph changing, which the tab follows.
    _workspace_subscription: Option<Subscription>,
    /// The session's schema changing, which "Node names" lists the types of.
    _session_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

/// The `graphs` key and name a graph is offered as a scope by.
pub fn graph_scope(session: &Entity<GraphSession>, cx: &App) -> (String, String) {
    let session = session.read(cx);
    (session.overrides_key(), session.graph_id.clone())
}

/// A dropdown's choices: each label and the value it sets.
type Options = Rc<[(SharedString, Value)]>;

/// The section naming each type's nodes (spec 002, "Node names"), offered with a graph open.
const NODE_NAMES: &str = "Node names";

/// A text setting's field. It writes when the person presses Enter or leaves it, not on every
/// key, and when the tab closes or Studio quits with typing in it; it shows the setting's value
/// again whenever it changes elsewhere, unless it holds typing not yet written.
struct TextInput {
    field: &'static SettingField,
    editor: Entity<Editor>,
    /// The text the field last showed or wrote; other text in it is typing not yet written.
    shown: String,
}

impl TextInput {
    fn edited(&self, cx: &App) -> bool {
        self.editor.read(cx).text(cx) != self.shown
    }
}

impl EventEmitter<ItemEvent> for SettingsView {}

/// Opening the tab puts the cursor in the search box, so typing searches at once.
impl Focusable for SettingsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.focus_handle(cx)
    }
}

impl Item for SettingsView {
    /// Settings read nothing from a graph's server.
    fn reads_graph(&self) -> bool {
        false
    }

    fn tab_title(&self, _cx: &App) -> SharedString {
        "Settings".into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::Settings)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some("settings".into())
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "settings".into(),
            data: serde_json::Value::Null,
        })
    }
}

impl SettingsView {
    /// `graph` is the open graph's settings key and name, offered as a scope.
    pub fn new(
        graph: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search settings", window, cx);
            editor
        });
        let mut subscriptions = vec![
            cx.subscribe(&search, |_, _, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    cx.notify();
                }
            }),
            cx.observe_global_in::<StudioSettings>(window, |this, window, cx| {
                this.values = StudioSettings::get(cx).values();
                this.show_text_values(window, cx)
            }),
            // Typing a field holds when the tab closes, or Studio quits, is written: the field
            // hears no loss of focus then. At quit it is written before the last writes are
            // waited for, as the windows close only after.
            cx.on_release(|this, cx| this.commit_typing(cx)),
            cx.on_app_quit(|this, cx| {
                this.commit_typing(cx);
                studio_settings::writes_done();
                async {}
            }),
            // The font fields offer the system's fonts once they are listed.
            cx.observe_global::<FontChoices>(|_, cx| cx.notify()),
        ];
        read_fonts(cx);
        let mut text_inputs = Vec::new();
        for field in studio_settings::fields() {
            if !matches!(field.kind, FieldKind::Text { .. }) {
                continue;
            }
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Not set", window, cx);
                editor.set_text_style_refinement(ds::field_text(cx));
                editor
            });
            let index = text_inputs.len();
            subscriptions.push(cx.subscribe_in(
                &editor,
                window,
                move |this, _, event: &EditorEvent, window, cx| {
                    if let EditorEvent::Blurred = event
                        && this.text_inputs[index].edited(cx)
                    {
                        this.write_text(index, window, cx);
                    }
                },
            ));
            text_inputs.push(TextInput {
                field,
                editor,
                shown: String::new(),
            });
        }
        let key_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("sk-or-…", window, cx);
            editor.set_text_style_refinement(ds::field_text(cx));
            editor.set_masked(true, cx);
            editor
        });
        if let Some(models) = Models::try_global(cx) {
            subscriptions.push(cx.observe(&models, |_, _, cx| cx.notify()));
        }
        let mut this = Self {
            search,
            section: None,
            graph,
            session: None,
            scope: Scope::All,
            text_inputs,
            key_editor,
            key_notice: None,
            key_test: None,
            values: StudioSettings::get(cx).values(),
            _workspace_subscription: None,
            _session_subscription: None,
            _subscriptions: subscriptions,
        };
        this.show_text_values(window, cx);
        this
    }

    /// Each text field shows its setting's value in the current scope, unless it holds typing
    /// not yet written: a control pressed while a field has focus draws the values before the
    /// field hears it lost focus and writes.
    fn show_text_values(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in &mut self.text_inputs {
            if input.edited(cx) {
                continue;
            }
            let text = studio_ui::value_text(self.values.value_in(&self.scope, input.field));
            if input.editor.read(cx).text(cx) != text {
                input
                    .editor
                    .update(cx, |editor, cx| editor.set_text(text.clone(), window, cx));
            }
            input.shown = text;
        }
    }

    /// The graph whose scope the tab offers: the window's, which can change while the tab is
    /// open. A scope of the graph no longer offered goes back to all graphs.
    pub fn set_graph(
        &mut self,
        graph: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.graph == graph {
            return;
        }
        let key = graph.as_ref().map(|(key, _)| key.clone());
        self.graph = graph;
        if matches!(&self.scope, Scope::Graph(shown) if Some(shown) != key.as_ref()) {
            self.set_scope(Scope::All, window, cx);
        }
        cx.notify();
    }

    /// The open graph's session, whose types "Node names" lists: drawn again when its schema
    /// loads or changes, as the tab is drawn cached.
    pub fn set_session(
        &mut self,
        session: Option<WeakEntity<GraphSession>>,
        cx: &mut Context<Self>,
    ) {
        self._session_subscription =
            session
                .as_ref()
                .and_then(WeakEntity::upgrade)
                .map(|session| {
                    cx.subscribe(&session, |_, _, event: &SessionEvent, cx| {
                        if matches!(
                            event,
                            SessionEvent::SchemaChanged | SessionEvent::SchemaMoved
                        ) {
                            cx.notify();
                        }
                    })
                });
        self.session = session;
        cx.notify();
    }

    /// Follows the graph `workspace` shows: a tab opened with no graph, or before another was
    /// shown, offers the one shown now.
    pub fn follow(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._workspace_subscription = Some(cx.subscribe_in(
            workspace,
            window,
            |this, workspace, event: &WorkspaceEvent, window, cx| {
                if !matches!(event, WorkspaceEvent::SessionChanged) {
                    return;
                }
                let session = workspace.read(cx).session().cloned();
                let graph = session.as_ref().map(|session| graph_scope(session, cx));
                this.set_graph(graph, window, cx);
                this.set_session(session.map(|session| session.downgrade()), cx);
            },
        ));
    }

    /// The open graph's session and schema, when its schema is read.
    fn named_graph(&self, cx: &App) -> Option<(Entity<GraphSession>, Arc<SchemaModel>)> {
        self.graph.as_ref()?;
        let session = self.session.as_ref()?.upgrade()?;
        let schema = session.read(cx).schema_model()?;
        Some((session, schema))
    }

    /// The rows of "Node names" for the types `query` finds (all of them without one): each
    /// type, what names its nodes now, and the properties that can.
    fn node_name_rows(
        &self,
        session: &Entity<GraphSession>,
        schema: &SchemaModel,
        query: &str,
        cx: &App,
    ) -> Vec<AnyElement> {
        let section_found = NODE_NAMES.to_lowercase().contains(query);
        let names: Vec<TypeNaming> = schema
            .nodes
            .iter()
            .filter(|node| section_found || node.name.to_lowercase().contains(query))
            .filter_map(|node| session.read(cx).type_naming(&node.name))
            .collect();
        let count = names.len();
        names
            .into_iter()
            .enumerate()
            .map(|(index, names)| {
                let reset = names.chosen.is_some().then(|| {
                    let names = names.clone();
                    Box::new(move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                        names.choose(None, cx)
                    }) as _
                });
                let row = ds::setting_row(
                    SharedString::from(format!("node-name-{}", names.type_name)),
                    names.type_name.clone(),
                    format!("Automatically its {}", names.default),
                    node_name_dropdown(names),
                    reset,
                );
                if index + 1 < count {
                    row.border_b_1().dotted(cx).into_any_element()
                } else {
                    row.into_any_element()
                }
            })
            .collect()
    }

    /// Shows another scope's values, after writing any typing to the scope it was typed in.
    fn set_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_typing(cx);
        self.scope = scope;
        self.show_text_values(window, cx);
        cx.notify();
    }

    /// Writes text field `index` to its setting, then shows the value as the settings hold it,
    /// trimmed or refused.
    fn write_text(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_text(index, cx);
        self.show_text_values(window, cx);
    }

    /// Writes every field's typing not yet written to its setting, in the scope it was typed in.
    fn commit_typing(&mut self, cx: &mut App) {
        for index in 0..self.text_inputs.len() {
            if self.text_inputs[index].edited(cx) {
                self.commit_text(index, cx);
            }
        }
    }

    /// Writes text field `index`'s text to its setting, when it differs: empty is unset where
    /// the setting may be. Every way a field is written comes here.
    fn commit_text(&mut self, index: usize, cx: &mut App) {
        let Some(input) = self.text_inputs.get(index) else {
            return;
        };
        let typed = input.editor.read(cx).text(cx);
        let text = typed.trim().to_owned();
        let value = match input.field.kind {
            FieldKind::Text { nullable: true } if text.is_empty() => Value::Null,
            _ => Value::String(text),
        };
        let target = Target {
            scope: self.scope.clone(),
            field: input.field,
        };
        if *StudioSettings::get(cx)
            .values()
            .value_in(&target.scope, target.field)
            != value
        {
            target.set(value, cx);
        }
        self.text_inputs[index].shown = typed;
    }

    /// The OpenRouter key row (spec 019): whether the keychain holds one, a field to store
    /// one, Remove, and where provider keys are set.
    fn key_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = Models::try_global(cx).map(|models| models.read(cx).key_state());
        let (status, color) = match state {
            Some(KeyState::Stored) => ("Stored", Color::Success),
            Some(KeyState::Reading) => ("Reading the keychain…", Color::Muted),
            Some(KeyState::None) | None => ("None", Color::Muted),
        };
        let stored = state == Some(KeyState::Stored);
        let typing = !self.key_editor.read(cx).text(cx).trim().is_empty();
        v_flex()
            .items_end()
            .gap(px(space::XS))
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .items_center()
                    .child(ds::text::muted(status).color(color))
                    .child(
                        ds::field_frame(false, cx)
                            .key_context("SettingsField")
                            .w(px(ds::size::FIELD))
                            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                this.store_key(window, cx)
                            }))
                            .child(ds::field_input(self.key_editor.clone())),
                    )
                    .child(
                        ds::button("store-key", "Store").disabled(!typing).on_click(
                            cx.listener(|this, _, window, cx| this.store_key(window, cx)),
                        ),
                    )
                    .when(stored, |row| {
                        row.child(
                            ds::button("test-key", "Test")
                                .disabled(self.key_test.is_some())
                                .tip(studio_ui::ds::tooltip(
                                    "Ask the fast model for one word, with this key",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.test_key(cx))),
                        )
                        .child(
                            ds::button("remove-key", "Remove")
                                .on_click(cx.listener(|this, _, _, cx| this.remove_key(cx))),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .items_center()
                    .children(self.key_notice.clone().map(ds::text::muted))
                    .child(
                        ds::button("provider-keys", "Provider keys…")
                            .style(ButtonStyle::Subtle)
                            .tip(studio_ui::ds::tooltip(
                                "Your own provider keys live in your OpenRouter workspace, \
                                 never in Studio",
                            ))
                            .on_click(|_, _, cx| {
                                cx.open_url("https://openrouter.ai/settings/integrations")
                            }),
                    ),
            )
            .into_any_element()
    }

    fn store_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.key_editor.read(cx).text(cx);
        if key.trim().is_empty() {
            return;
        }
        self.key_editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.put_key(key, cx);
    }

    /// The smallest call there is, with the stored key and the fast model: says the model and
    /// the time, or why it failed.
    fn test_key(&mut self, cx: &mut Context<Self>) {
        let Some(models) = Models::try_global(cx) else {
            return;
        };
        let call = models.read(cx).test_connection(cx);
        self.key_notice = Some("Testing…".to_owned());
        self.key_test = Some(cx.spawn(async move |this, cx| {
            let answer = call.await;
            this.update(cx, |this, cx| {
                this.key_notice = Some(match answer {
                    Ok(answer) => format!(
                        "Connected: {} answered in {:.1} s.",
                        answer.model,
                        answer.elapsed.as_secs_f32()
                    ),
                    Err(error) => format!("Not connected: {error}"),
                });
                this.key_test = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn remove_key(&mut self, cx: &mut Context<Self>) {
        self.put_key(String::new(), cx);
    }

    fn put_key(&mut self, key: String, cx: &mut Context<Self>) {
        let Some(models) = Models::try_global(cx) else {
            return;
        };
        let removing = key.trim().is_empty();
        // A test under way was of the key before: its answer would read as this one's.
        self.key_test = None;
        let written = models.update(cx, |models, cx| models.set_key(key, cx));
        cx.spawn(async move |this, cx| {
            let result = written.await;
            this.update(cx, |this, cx| {
                this.key_notice = Some(match (result, removing) {
                    (Ok(()), false) => "Stored in the keychain.".to_owned(),
                    (Ok(()), true) => "Removed.".to_owned(),
                    (Err(error), false) => {
                        format!("Kept for this run; the keychain did not keep it: {error}")
                    }
                    (Err(error), true) => format!("The keychain did not remove it: {error}"),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Text field `index`'s control: the field, which Enter writes.
    fn text_control(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        ds::field_frame(false, cx)
            .key_context("SettingsField")
            .w(px(ds::size::FIELD))
            .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                this.write_text(index, window, cx)
            }))
            .child(ds::field_input(self.text_inputs[index].editor.clone()))
            .into_any_element()
    }
}

/// One setting in one scope: what a control reads and writes.
#[derive(Clone)]
struct Target {
    scope: Scope,
    field: &'static SettingField,
}

impl Target {
    fn set(&self, value: Value, cx: &mut App) {
        let target = self.clone();
        studio_settings::update(cx, move |settings| {
            if let Ok(next) = settings.with_value_in(&target.scope, target.field, value) {
                *settings = next;
            }
        });
    }

    fn id(&self, part: &str) -> SharedString {
        format!("{}-{part}", self.field.path.join(".")).into()
    }
}

/// A stepper within the field's range.
fn stepper(
    target: &Target,
    value: f64,
    min: f64,
    max: f64,
    step: f64,
    integer: bool,
) -> AnyElement {
    let shown = if integer {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    };
    let to_value = move |next: f64| -> Value {
        if integer {
            Value::from(next as i64)
        } else {
            serde_json::Number::from_f64(next).map_or(Value::Null, Value::Number)
        }
    };
    let (down, up) = (target.clone(), target.clone());
    h_flex()
        .gap(px(space::XS))
        .child(
            studio_ui::ds::icon_button(target.id("down"), IconName::Dash)
                .disabled(value <= min)
                .on_click(move |_, _, cx| {
                    down.set(to_value((value - step).max(min)), cx);
                }),
        )
        .child(
            div()
                .w(px(ds::size::STEPPER_VALUE))
                .flex()
                .justify_center()
                .child(ds::text::body(shown)),
        )
        .child(
            studio_ui::ds::icon_button(target.id("up"), IconName::Plus)
                .disabled(value >= max)
                .on_click(move |_, _, cx| {
                    up.set(to_value((value + step).min(max)), cx);
                }),
        )
        .into_any_element()
}

/// A menu of named values; the current one is checked.
fn dropdown(target: &Target, current: Value, options: Options) -> AnyElement {
    let shown = options
        .iter()
        .find(|(_, value)| *value == current)
        .map(|(label, _)| label.clone())
        .unwrap_or_else(|| current.as_str().unwrap_or_default().to_owned().into());
    let target = target.clone();
    PopoverMenu::new(target.id("menu"))
        .trigger(
            studio_ui::ds::button(target.id("trigger"), shown)
                .style(ButtonStyle::Outlined)
                .end_icon(Icon::new(IconName::ChevronDown)),
        )
        .menu(move |window, cx| {
            let options = options.clone();
            let current = current.clone();
            let target = target.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                for (label, value) in options.iter() {
                    let target = target.clone();
                    let checked = *value == current;
                    let value = value.clone();
                    menu =
                        menu.toggleable_entry(label.clone(), checked, IconPosition::Start, None, {
                            move |_, cx| target.set(value.clone(), cx)
                        });
                }
                menu
            }))
        })
        .anchor(gpui::Anchor::TopRight)
        .into_any_element()
}

/// A model picker (spec 019): the models OpenRouter lists that take a schema, with their
/// prices, and the one set when the list does not hold it.
fn model_control(target: &Target, values: &SettingsValues, cx: &App) -> AnyElement {
    let current = values.value_in(&target.scope, target.field).clone();
    let mut options: Vec<(SharedString, Value)> = Models::try_global(cx)
        .map(|models| models.read(cx).choices())
        .unwrap_or_default()
        .into_iter()
        .map(|choice| (SharedString::from(choice.label()), Value::from(choice.id)))
        .collect();
    if let Some(id) = current.as_str()
        && !options.iter().any(|(_, value)| value == &current)
    {
        options.insert(0, (SharedString::from(id.to_owned()), current.clone()));
    }
    dropdown(target, current, Rc::from(options))
}

/// The choice of the property that names a type's nodes, as the canvas and the table offer it.
fn node_name_dropdown(names: TypeNaming) -> AnyElement {
    let id = names.type_name.clone();
    PopoverMenu::new(SharedString::from(format!("node-name-menu-{id}")))
        .trigger(
            studio_ui::ds::button(
                SharedString::from(format!("node-name-trigger-{id}")),
                studio_ui::node_names::shown(&names),
            )
            .style(ButtonStyle::Outlined)
            .end_icon(Icon::new(IconName::ChevronDown)),
        )
        .menu(move |window, cx| {
            let names = names.clone();
            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                studio_ui::node_names::add_to(menu, &names)
            }))
        })
        .anchor(gpui::Anchor::TopRight)
        .into_any_element()
}

/// The font choices, listed from the system once for every Settings tab: "Default", then every
/// font by name. `None` while they are listed.
#[derive(Default)]
struct FontChoices {
    options: Option<Options>,
}

impl Global for FontChoices {}

/// Lists the system's fonts off the main thread, once, when the first Settings tab opens: the
/// platform enumerates every face it has, too slow for a frame (Zed's `FontFamilyCache`
/// prefetches them in the background for the same reason).
fn read_fonts(cx: &mut App) {
    if cx.has_global::<FontChoices>() {
        return;
    }
    cx.set_global(FontChoices::default());
    let text_system = cx.text_system().clone();
    let listing = cx.background_spawn(async move {
        let mut fonts = text_system.all_font_names();
        fonts.sort();
        fonts.dedup();
        fonts.retain(|name| !name.starts_with('.'));
        fonts
    });
    cx.spawn(async move |cx| {
        let fonts = listing.await;
        let options: Options = std::iter::once(("Default".into(), Value::Null))
            .chain(
                fonts
                    .into_iter()
                    .map(|name| (name.clone().into(), Value::from(name))),
            )
            .collect();
        cx.update(|cx| {
            cx.update_global::<FontChoices, _>(|fonts, _| fonts.options = Some(options))
        });
    })
    .detach();
}

/// The font choices once they are listed.
fn font_choices(cx: &App) -> Option<Options> {
    cx.try_global::<FontChoices>()?.options.clone()
}

/// A font field's choices while the system's fonts are listed: "Default" and the font chosen,
/// so the field shows its value.
fn fonts_being_read(current: &Value) -> Options {
    std::iter::once(("Default".into(), Value::Null))
        .chain(
            current
                .as_str()
                .map(|name| (SharedString::from(name.to_owned()), current.clone())),
        )
        .collect()
}

/// The agent's presets as a segmented choice, with the command it runs underneath.
fn agent_control(target: &Target, current: Value, cx: &App) -> AnyElement {
    let command: Vec<String> = serde_json::from_value(current).unwrap_or_default();
    let mut options: Vec<(&str, Vec<String>)> = vec![("None", Vec::new())];
    options.extend(
        agent_panel::PRESETS
            .iter()
            .map(|(name, preset)| (*name, preset.iter().map(|s| (*s).to_owned()).collect())),
    );
    let known = options.iter().any(|(_, preset)| *preset == command);
    let mut segments = ds::segmented(cx);
    for (index, (name, preset)) in options.into_iter().enumerate() {
        let target = target.clone();
        let selected = preset == command;
        segments = segments.child(
            ds::segment(target.id(&index.to_string()), name, selected, cx)
                .on_click(move |_, _, cx| target.set(Value::from(preset.clone()), cx)),
        );
    }
    if !known {
        segments = segments.child(ds::segment(target.id("custom"), "Custom", true, cx));
    }
    v_flex()
        .items_end()
        .gap(px(space::XS))
        .child(segments)
        .when(!command.is_empty(), |control| {
            control.child(
                div()
                    .max_w(px(ds::size::FIELD_WIDE))
                    .overflow_hidden()
                    .child(
                        ds::text::code(command.join(" "), cx)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                    ),
            )
        })
        .into_any_element()
}

fn control(
    target: &Target,
    values: &SettingsValues,
    fonts: Option<&Options>,
    cx: &App,
) -> AnyElement {
    let current = values.value_in(&target.scope, target.field).clone();
    match &target.field.kind {
        FieldKind::Choice(choices) if choices.len() <= 4 => {
            let mut segments = ds::segmented(cx);
            for choice in choices {
                let choice_target = target.clone();
                let value = Value::from(choice.value.clone());
                let selected = current == value;
                segments = segments.child(
                    ds::segment(target.id(&choice.value), choice.label.clone(), selected, cx)
                        .when_some(choice.description.clone(), |segment, description| {
                            segment.tip(studio_ui::ds::tooltip(description))
                        })
                        .on_click(move |_, _, cx| choice_target.set(value.clone(), cx)),
                );
            }
            segments.into_any_element()
        }
        FieldKind::Choice(choices) => dropdown(
            target,
            current,
            choices
                .iter()
                .map(|c| (c.label.clone().into(), Value::from(c.value.clone())))
                .collect(),
        ),
        FieldKind::Number {
            min,
            max,
            step,
            integer,
        } => stepper(
            target,
            current.as_f64().unwrap_or_default(),
            *min,
            *max,
            *step,
            *integer,
        ),
        FieldKind::Toggle => {
            let on = current.as_bool().unwrap_or_default();
            let switch_target = target.clone();
            ds::switch(target.id("switch"), on.into(), cx)
                .on_click(move |state, _, cx| {
                    switch_target.set(Value::Bool(*state == ToggleState::Selected), cx)
                })
                .into_any_element()
        }
        FieldKind::Custom(kind) if kind == "theme" => {
            let names = ThemeRegistry::global(cx).list_names();
            dropdown(
                target,
                current,
                names
                    .into_iter()
                    .map(|name| (name.clone(), Value::from(name.to_string())))
                    .collect(),
            )
        }
        FieldKind::Custom(kind) if kind == "font" => {
            let options = fonts.cloned().unwrap_or_else(|| fonts_being_read(&current));
            dropdown(target, current, options)
        }
        FieldKind::Custom(kind) if kind == "agent" => agent_control(target, current, cx),
        FieldKind::Custom(_) | FieldKind::Text { .. } => {
            let shown = match &current {
                Value::Null => "Not set".to_owned(),
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            ds::text::muted(shown)
                .single_line()
                .truncate()
                .into_any_element()
        }
    }
}

impl SettingsView {
    /// The scope switch, when a graph is open: the shared values, or this graph's own.
    fn render_scope(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (key, name) = self.graph.clone()?;
        let all = self.scope == Scope::All;
        Some(
            ds::segmented(cx)
                .child(
                    ds::segment("scope-all", "All graphs", all, cx)
                        .tip(studio_ui::ds::tooltip("Settings for every graph"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.set_scope(Scope::All, window, cx)
                        })),
                )
                .child(
                    ds::segment("scope-graph", name.clone(), !all, cx)
                        .tip(studio_ui::ds::tooltip(format!(
                            "Values for {name} only, over those for all graphs"
                        )))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_scope(Scope::Graph(key.clone()), window, cx)
                        })),
                ),
        )
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).text(cx).to_lowercase();
        let query = query.trim();
        let searching = !query.is_empty();
        let scope = self.scope.clone();
        let in_scope = |field: &&'static SettingField| scope == Scope::All || field.per_graph;
        let mut sections: Vec<&'static str> = studio_settings::sections()
            .into_iter()
            .filter(|section| {
                studio_settings::fields()
                    .iter()
                    .filter(in_scope)
                    .any(|f| f.section == *section)
            })
            .collect();
        let named = self.named_graph(cx);
        if named.is_some() {
            sections.push(NODE_NAMES);
        }
        let chosen = self
            .section
            .filter(|section| sections.contains(section))
            .or_else(|| sections.first().copied())
            .unwrap_or_default();
        let shown: Vec<&'static SettingField> = studio_settings::fields()
            .iter()
            .filter(in_scope)
            .filter(|field| {
                if searching {
                    field.title.to_lowercase().contains(query)
                        || field.description.to_lowercase().contains(query)
                        || field.path.join(".").contains(query)
                } else {
                    field.section == chosen
                }
            })
            .collect();
        let fonts = font_choices(cx);
        let values = &self.values;
        let path = studio_settings::settings_path();
        let settings_problems = studio_settings::settings_problems(cx);
        let keymap_problems = studio_settings::keymap_problems(cx).to_vec();
        let rule = ds::surface::rule(cx);
        let changed_word = if scope == Scope::All {
            "changed"
        } else {
            "own"
        };

        let nav = v_flex()
            .w(px(ds::size::NAV))
            .flex_none()
            .h_full()
            .pt(px(space::PAGE))
            .gap(px(space::WITHIN))
            .border_r_1()
            .border_color(rule)
            .children(
                sections
                    .iter()
                    .copied()
                    .enumerate()
                    .map(|(index, section)| {
                        let count = match (&self.graph, section == NODE_NAMES) {
                            (Some((key, _)), true) => StudioSettings::get(cx).labels_for(key).len(),
                            _ => studio_settings::fields()
                                .iter()
                                .filter(in_scope)
                                .filter(|f| {
                                    f.section == section && !values.is_default_in(&scope, f)
                                })
                                .count(),
                        };
                        ds::SidebarRow::new(("settings-section", index), section)
                            .selected(!searching && section == chosen)
                            .when(count > 0, |row| {
                                row.end(ds::sidebar_count(format!("{count} {changed_word}")))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.section = Some(section);
                                this.search
                                    .update(cx, |search, cx| search.clear(window, cx));
                                cx.notify();
                            }))
                    }),
            );

        // While searching, the matches are grouped by section under a label each.
        let mut groups: Vec<(&str, Vec<&'static SettingField>)> = Vec::new();
        for field in shown {
            match groups.last_mut() {
                Some((section, fields)) if *section == field.section => fields.push(field),
                _ => groups.push((&field.section, vec![field])),
            }
        }
        // The graph's types, for "Node names" or a search that finds them.
        let node_names = named
            .as_ref()
            .filter(|_| searching || chosen == NODE_NAMES)
            .map(|(session, schema)| {
                let rows =
                    self.node_name_rows(session, schema, if searching { query } else { "" }, cx);
                (
                    self.graph.clone().map(|(_, name)| name).unwrap_or_default(),
                    rows,
                )
            })
            .filter(|(_, rows)| !rows.is_empty());
        let empty = groups.is_empty() && node_names.is_none();
        let body = v_flex()
            .w_full()
            .max_w(px(ds::size::READING_WIDTH))
            .gap(px(space::BETWEEN))
            .children(groups.into_iter().map(|(section, fields)| {
                let count = fields.len();
                v_flex()
                    .gap(px(space::SM))
                    .when(searching, |group| group.child(ds::text::meta(section)))
                    .child(ds::card(cx).px(px(space::XL)).children(
                        fields.into_iter().enumerate().map(|(index, field)| {
                            let target = Target {
                                scope: scope.clone(),
                                field,
                            };
                            let overridden = !values.is_default_in(&scope, field);
                            let reset = overridden.then(|| {
                                let target = target.clone();
                                Box::new(
                                    move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                                        let target = target.clone();
                                        studio_settings::update(cx, move |s| {
                                            *s = s.reset_in(&target.scope, target.field)
                                        });
                                    },
                                ) as _
                            });
                            // In a graph's scope, an own value says what every other graph uses.
                            let description = match (&scope, overridden) {
                                (Scope::Graph(_), true) => format!(
                                    "{} All graphs: {}.",
                                    field.description,
                                    studio_ui::format_value(values.at(&field.path))
                                ),
                                _ => field.description.clone(),
                            };
                            let text_input = self
                                .text_inputs
                                .iter()
                                .position(|input| std::ptr::eq(input.field, field));
                            let control = match (text_input, &field.kind) {
                                (Some(index), _) => self.text_control(index, cx),
                                (None, FieldKind::Custom(kind)) if kind == "openrouter-key" => {
                                    self.key_control(cx)
                                }
                                (None, FieldKind::Custom(kind)) if kind == "model" => {
                                    model_control(&target, values, cx)
                                }
                                _ => control(&target, values, fonts.as_ref(), cx),
                            };
                            let row = ds::setting_row(
                                target.id("row"),
                                field.title.clone(),
                                description,
                                control,
                                reset,
                            );
                            if index + 1 < count {
                                row.border_b_1().dotted(cx)
                            } else {
                                row
                            }
                        }),
                    ))
            }))
            .children(node_names.map(|(graph, rows)| {
                v_flex()
                    .gap(px(space::SM))
                    .when(searching, |group| group.child(ds::text::meta(NODE_NAMES)))
                    .child(ds::text::muted(format!(
                        "The property that names each type's nodes in {graph}: on the canvas, in \
                         the tree, the inspector and tables."
                    )))
                    .child(ds::card(cx).px(px(space::XL)).children(rows))
            }))
            .when(empty, |body| {
                body.child(
                    div()
                        .pt(px(space::XXXL))
                        .child(ds::text::muted(format!("No setting matches “{query}”"))),
                )
            });

        h_flex()
            .id("settings")
            .size_full()
            .items_start()
            .child(nav)
            .child(
                v_flex()
                    .id("settings-page")
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .overflow_y_scroll()
                    .px(px(space::PAGE + space::MD))
                    .py(px(space::PAGE))
                    .gap(px(space::XL))
                    .child(
                        h_flex()
                            .w_full()
                            .max_w(px(ds::size::READING_WIDTH))
                            .gap(px(space::CONTROLS))
                            .child(ds::text::heading(if searching {
                                SharedString::from("Search")
                            } else {
                                SharedString::from(chosen)
                            }))
                            .child(div().flex_1())
                            .children(self.render_scope(cx))
                            .child(ds::search_field(self.search.clone(), cx).w(px(ds::size::FIELD)))
                            .child(
                                studio_ui::ds::icon_button("reveal-settings", IconName::Folder)
                                    .tip(studio_ui::ds::tooltip(format!(
                                        "Show settings.json · {}",
                                        path.display()
                                    )))
                                    .on_click(move |_, _, cx| cx.reveal_path(&path)),
                            )
                            .child(
                                studio_ui::ds::icon_button("reveal-keymap", IconName::Keyboard)
                                    .tip(studio_ui::ds::tooltip(format!(
                                        "Show keymap.json, your shortcuts · {}",
                                        studio_settings::keymap_path().display()
                                    )))
                                    .on_click(|_, _, cx| show_keymap_file(cx)),
                            ),
                    )
                    .children(file_problems(
                        "settings.json",
                        settings_problems,
                        |cx| cx.reveal_path(&studio_settings::settings_path()),
                        cx,
                    ))
                    .children(file_problems(
                        "keymap.json",
                        keymap_problems,
                        show_keymap_file,
                        cx,
                    ))
                    .child(body),
            )
    }
}

/// Shows `keymap.json`, written first with a starting point when the person has none, off the
/// main thread (the write is synced to disk). One that cannot be written is said in a toast
/// with its path and why, as the button would otherwise do nothing.
fn show_keymap_file(cx: &mut App) {
    let written = cx.background_spawn(async { crate::keymaps::user_keymap_file() });
    cx.spawn(async move |cx| {
        let written = written.await;
        cx.update(|cx| match written {
            Ok(path) => cx.reveal_path(&path),
            Err(error) => {
                let message = format!(
                    "keymap.json could not be written at {}: {error}",
                    studio_settings::keymap_path().display()
                );
                log::error!("{message}");
                crate::tell(cx, studio_workspace::ToastKind::Error, message);
            }
        });
    })
    .detach();
}

/// A file's problems, one line each, with a button that shows the file. Nothing when there
/// are none.
fn file_problems(
    file: &'static str,
    problems: Vec<String>,
    show: fn(&mut App),
    cx: &App,
) -> Option<impl IntoElement> {
    let title = match problems.len() {
        0 => return None,
        1 => format!("A problem in {file}"),
        count => format!("{count} problems in {file}"),
    };
    Some(
        div().w_full().max_w(px(ds::size::READING_WIDTH)).child(
            studio_ui::ds::callout(Severity::Warning, title)
                .description_slot(
                    v_flex()
                        .gap(px(space::XS))
                        .children(problems.iter().map(|problem| problem_text(problem, cx))),
                )
                .actions_slot(
                    ds::button(
                        SharedString::from(format!("show-{file}")),
                        format!("Show {file}"),
                    )
                    .on_click(move |_, _, cx| show(cx)),
                ),
        ),
    )
}

/// A problem as the tab shows it: muted text, each `code` span in the text colour on a faint
/// fill, its backticks dropped.
fn problem_text(problem: &str, cx: &App) -> impl IntoElement {
    let mut text = String::with_capacity(problem.len());
    let mut spans = Vec::new();
    let mut open = None;
    for c in problem.chars() {
        if c == '`' {
            match open.take() {
                Some(start) => spans.push(start..text.len()),
                None => open = Some(text.len()),
            }
        } else {
            text.push(c);
        }
    }
    let code = HighlightStyle {
        color: Some(Color::Default.color(cx)),
        background_color: Some(ds::surface::tag(cx)),
        ..HighlightStyle::default()
    };
    div()
        .text_size(ds::text::body_size(cx))
        .text_color(Color::Muted.color(cx))
        .child(StyledText::new(text).with_highlights(spans.into_iter().map(|span| (span, code))))
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};

    use super::*;

    fn cli_field(view: &SettingsView) -> usize {
        view.text_inputs
            .iter()
            .position(|input| input.field.path.join(".") == "omnigraph_cli")
            .expect("the CLI path is a text setting")
    }

    /// A value drawn again while a field holds typing not yet written (a control pressed
    /// before the field hears it lost focus) leaves the typing; a field without any shows the
    /// new value, focused or not. Nothing here writes the settings file.
    #[gpui::test]
    fn typing_not_yet_written_outlives_the_values_drawn_again(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
            studio_settings::init_test(cx);
        });
        let (view, cx): (_, &mut VisualTestContext) = cx.add_window_view(|window, cx| {
            SettingsView::new(Some(("k".into(), "Graph".into())), window, cx)
        });
        let index = view.read_with(cx, |view, _| cli_field(view));
        let editor = view.read_with(cx, |view, _| view.text_inputs[index].editor.clone());
        let section = view.read_with(cx, |view, _| view.text_inputs[index].field.section.clone());
        assert_eq!(section, "Connection", "one CLI, for Studio and the agent");
        // The Connection section of an active window, where the field is drawn and hears its
        // focus come and go.
        cx.update(|window, _| window.activate_window());
        view.update(cx, |view, cx| {
            view.section = Some("Connection");
            cx.notify();
        });
        cx.run_until_parked();
        let text = |cx: &mut VisualTestContext| editor.read_with(cx, |editor, cx| editor.text(cx));

        // Focused without typing: a change elsewhere shows.
        cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
        let settings = StudioSettings {
            omnigraph_cli: Some("/usr/local/bin/omnigraph".into()),
            ..StudioSettings::default()
        };
        cx.update(|_, cx| cx.set_global(settings));
        cx.run_until_parked();
        assert_eq!(text(cx), "/usr/local/bin/omnigraph");

        // Typing; then a control is pressed, which moves the focus and draws the values before
        // the field hears it lost focus.
        editor.update_in(cx, |editor, window, cx| {
            editor.move_to_end(&Default::default(), window, cx)
        });
        cx.simulate_input("2");
        cx.run_until_parked();
        let search = view.read_with(cx, |view, _| view.search.clone());
        let kept = cx.update(|window, cx| {
            window.focus(&search.focus_handle(cx), cx);
            view.update(cx, |view, cx| view.show_text_values(window, cx));
            editor.read(cx).text(cx)
        });
        assert_eq!(kept, "/usr/local/bin/omnigraph2", "the typing stays");
        // The field then writes; this test's settings refuse every write, so it shows the
        // value they hold.
        cx.run_until_parked();
        assert_eq!(text(cx), "/usr/local/bin/omnigraph");
    }

    /// A key stored while a test of the key before is out drops that test: its answer, about
    /// the key before, would read as this one's. Before, a slow "Not connected" landed after
    /// "Stored in the keychain." and read as the new key's.
    #[gpui::test]
    fn storing_a_key_drops_the_test_of_the_one_before(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
            studio_settings::init_test(cx);
        });
        // A model service that never answers: the test stays out.
        let http = http_client::FakeHttpClient::create(|_| std::future::pending());
        cx.update(|cx| Models::init_for_tests(http, Some("old-key"), cx));
        let (view, cx): (_, &mut VisualTestContext) = cx.add_window_view(|window, cx| {
            SettingsView::new(Some(("k".into(), "Graph".into())), window, cx)
        });
        view.update(cx, |view, cx| {
            view.test_key(cx);
            assert!(view.key_test.is_some(), "testing the old key");
            view.put_key("new-key".into(), cx);
            assert!(view.key_test.is_none(), "its test dropped");
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.key_notice.as_deref(), Some("Stored in the keychain."));
        });
    }

    /// The system's fonts are listed off the main thread when a tab opens, once for every tab;
    /// until they are in, a font field offers the font chosen, so it shows its value.
    #[gpui::test]
    fn fonts_are_listed_off_the_main_thread(cx: &mut TestAppContext) {
        cx.update(|cx| {
            read_fonts(cx);
            assert!(
                font_choices(cx).is_none(),
                "listed in the background, not here"
            );
            read_fonts(cx);
        });
        cx.run_until_parked();
        let listed = cx
            .update(|cx| font_choices(cx))
            .expect("the fonts are listed");
        let default = (SharedString::from("Default"), Value::Null);
        assert_eq!(listed[0], default);

        let chosen = Value::from("Iosevka");
        assert_eq!(
            fonts_being_read(&chosen).to_vec(),
            [default.clone(), (SharedString::from("Iosevka"), chosen)]
        );
        assert_eq!(fonts_being_read(&Value::Null).to_vec(), [default]);
    }

    /// Typing a field holds when the tab closes is written then: the field hears no loss of
    /// focus as it goes. Written once, not again by anything after.
    #[gpui::test]
    fn typing_is_written_when_the_tab_closes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
            studio_settings::init_test(cx);
        });
        let (view, cx): (_, &mut VisualTestContext) =
            cx.add_window_view(|window, cx| SettingsView::new(None, window, cx));
        // This test's settings refuse a write by setting their values again, which counts it.
        let writes = Rc::new(std::cell::Cell::new(0));
        let _counted = cx.update(|_, cx| {
            let writes = writes.clone();
            cx.observe_global::<StudioSettings>(move |_| writes.set(writes.get() + 1))
        });
        let index = view.read_with(cx, |view, _| cli_field(view));
        let editor = view.read_with(cx, |view, _| view.text_inputs[index].editor.clone());
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text("/opt/omnigraph", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(writes.get(), 0, "typing alone writes nothing");

        // The tab goes with its window; the update that closes it releases the view.
        drop(view);
        cx.update(|window, _| window.remove_window());
        cx.run_until_parked();
        assert_eq!(writes.get(), 1);
    }
}
