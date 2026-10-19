//! The settings tab (spec 002): a left nav of sections and, for the chosen one, a row per
//! setting with a title, a one-line description, a control and Reset. Rows and controls are
//! generated from the settings schema (`studio_settings::fields`); nothing here names a
//! setting. A search across every section replaces the section while it has text. With a
//! graph open, a scope switch shows that graph's own values for the settings a graph may
//! override (`graphs` in `settings.json`). Changes write `settings.json` and apply at once.

use std::rc::Rc;

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle, Render,
    SharedString, StyledText, Subscription, Window,
};
use serde_json::Value;
use studio_settings::{FieldKind, Scope, SettingField, SettingsValues, StudioSettings};
use studio_ui::ds::{self, Dotted as _, space};
use studio_ui::menu::ContextMenu;
use studio_workspace::{Item, ItemEvent, SavedItem};
use theme::ThemeRegistry;
use ui::prelude::*;
use ui::{PopoverMenu, Severity, Tooltip};

pub struct SettingsView {
    search: Entity<Editor>,
    section: usize,
    /// The open graph's settings key and name, when there is one.
    graph: Option<(String, String)>,
    scope: Scope,
    /// A field for each text setting, made with the tab.
    text_inputs: Vec<TextInput>,
    /// The font choices, read from the system when a font setting is first drawn.
    fonts: Option<Options>,
    _subscriptions: Vec<Subscription>,
}

/// A dropdown's choices: each label and the value it sets.
type Options = Rc<[(SharedString, Value)]>;

/// A text setting's field. It writes when the person presses Enter or leaves it, not on every
/// key, and shows the setting's value again whenever it changes elsewhere, unless it holds
/// typing not yet written.
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
                this.show_text_values(window, cx)
            }),
        ];
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
        let mut this = Self {
            search,
            section: 0,
            graph,
            scope: Scope::All,
            text_inputs,
            fonts: None,
            _subscriptions: subscriptions,
        };
        this.show_text_values(window, cx);
        this
    }

    /// Each text field shows its setting's value in the current scope, unless it holds typing
    /// not yet written: a control pressed while a field has focus draws the values before the
    /// field hears it lost focus and writes.
    fn show_text_values(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let values = StudioSettings::get(cx).values();
        for input in &mut self.text_inputs {
            if input.edited(cx) {
                continue;
            }
            let text = studio_ui::value_text(values.value_in(&self.scope, input.field));
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

    /// Shows another scope's values, after writing any typing to the scope it was typed in.
    fn set_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        for index in 0..self.text_inputs.len() {
            if self.text_inputs[index].edited(cx) {
                self.write_text(index, window, cx);
            }
        }
        self.scope = scope;
        self.show_text_values(window, cx);
        cx.notify();
    }

    /// Writes text field `index` to its setting: empty is unset where the setting may be.
    fn write_text(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
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
        // The value as the settings hold it, trimmed or refused.
        self.show_text_values(window, cx);
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

/// "Default", then every font the system has, by name.
fn font_options(cx: &App) -> Options {
    let mut fonts = cx.text_system().all_font_names();
    fonts.sort();
    fonts.dedup();
    std::iter::once(("Default".into(), Value::Null))
        .chain(
            fonts
                .into_iter()
                .filter(|name| !name.starts_with('.'))
                .map(|name| (name.clone().into(), Value::from(name))),
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
                            segment.tooltip(Tooltip::text(description))
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
        FieldKind::Custom(kind) if kind == "font" => dropdown(
            target,
            current,
            fonts.cloned().unwrap_or_else(|| Rc::from([])),
        ),
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
                        .tooltip(Tooltip::text("Settings for every graph"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.set_scope(Scope::All, window, cx)
                        })),
                )
                .child(
                    ds::segment("scope-graph", name.clone(), !all, cx)
                        .tooltip(Tooltip::text(format!(
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
        let sections: Vec<&str> = studio_settings::sections()
            .into_iter()
            .filter(|section| {
                studio_settings::fields()
                    .iter()
                    .filter(in_scope)
                    .any(|f| f.section == *section)
            })
            .collect();
        self.section = self.section.min(sections.len().saturating_sub(1));
        let chosen = sections.get(self.section).copied().unwrap_or_default();
        let values = StudioSettings::get(cx).values();
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
        let wants_fonts = shown
            .iter()
            .any(|field| matches!(&field.kind, FieldKind::Custom(kind) if kind == "font"));
        if wants_fonts && self.fonts.is_none() {
            self.fonts = Some(font_options(cx));
        }
        let fonts = self.fonts.clone();
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
            .children(sections.iter().enumerate().map(|(index, section)| {
                let count = studio_settings::fields()
                    .iter()
                    .filter(in_scope)
                    .filter(|f| f.section == *section && !values.is_default_in(&scope, f))
                    .count();
                ds::SidebarRow::new(("settings-section", index), *section)
                    .selected(!searching && index == self.section)
                    .when(count > 0, |row| {
                        row.end(ds::sidebar_count(format!("{count} {changed_word}")))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.section = index;
                        this.search
                            .update(cx, |search, cx| search.clear(window, cx));
                        cx.notify();
                    }))
            }));

        // While searching, the matches are grouped by section under a label each.
        let mut groups: Vec<(&str, Vec<&'static SettingField>)> = Vec::new();
        for field in shown {
            match groups.last_mut() {
                Some((section, fields)) if *section == field.section => fields.push(field),
                _ => groups.push((&field.section, vec![field])),
            }
        }
        let empty = groups.is_empty();
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
                            let control = match text_input {
                                Some(index) => self.text_control(index, cx),
                                None => control(&target, &values, fonts.as_ref(), cx),
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
                                    .tooltip(Tooltip::text(format!(
                                        "Show settings.json · {}",
                                        path.display()
                                    )))
                                    .on_click(move |_, _, cx| cx.reveal_path(&path)),
                            )
                            .child(
                                studio_ui::ds::icon_button("reveal-keymap", IconName::Keyboard)
                                    .tooltip(Tooltip::text(format!(
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

/// Shows `keymap.json`, written first with a starting point when the person has none.
fn show_keymap_file(cx: &mut App) {
    match crate::keymaps::user_keymap_file() {
        Ok(path) => cx.reveal_path(&path),
        Err(error) => log::error!("keymap.json could not be written: {error}"),
    }
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
            .position(|input| input.field.path.join(".") == "agent.omnigraph_cli")
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
        // The Agent section of an active window, where the field is drawn and hears its focus
        // come and go.
        cx.update(|window, _| window.activate_window());
        view.update(cx, |view, cx| {
            view.section = studio_settings::sections()
                .iter()
                .position(|section| *section == "Agent")
                .unwrap();
            cx.notify();
        });
        cx.run_until_parked();
        let text = |cx: &mut VisualTestContext| editor.read_with(cx, |editor, cx| editor.text(cx));

        // Focused without typing: a change elsewhere shows.
        cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
        let mut settings = StudioSettings::default();
        settings.agent.omnigraph_cli = Some("/usr/local/bin/omnigraph".into());
        cx.update(|_, cx| cx.set_global(settings.clone()));
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
}
