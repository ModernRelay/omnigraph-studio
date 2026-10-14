//! The settings tab (spec 002): a left nav of sections and, for the chosen one, a row per
//! setting with a title, a one-line description, a control and Reset. Rows and controls are
//! generated from the settings schema (`studio_settings::fields`); nothing here names a
//! setting. A search across every section replaces the section while it has text. With a
//! graph open, a scope switch shows that graph's own values for the settings a graph may
//! override (`graphs` in `settings.json`). Changes write `settings.json` and apply at once.

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Subscription,
    Window,
};
use serde_json::Value;
use studio_settings::{FieldKind, Scope, SettingField, StudioSettings};
use studio_ui::ds::{self, radius, size, space};
use studio_workspace::{Item, ItemEvent, SavedItem};
use theme::ThemeRegistry;
use ui::prelude::*;
use ui::{Callout, ContextMenu, PopoverMenu, Severity, Switch, Tooltip};

pub struct SettingsView {
    search: Entity<Editor>,
    section: usize,
    /// The open graph's settings key and name, when there is one.
    graph: Option<(String, String)>,
    scope: Scope,
    /// A field for each text setting, made with the tab.
    text_inputs: Vec<TextInput>,
    _subscriptions: Vec<Subscription>,
}

/// A text setting's field. It writes when the person presses Enter or leaves it, not on every
/// key, and shows the setting's value again whenever it changes elsewhere.
struct TextInput {
    field: &'static SettingField,
    editor: Entity<Editor>,
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
                    if let EditorEvent::Blurred = event {
                        this.write_text(index, window, cx);
                    }
                },
            ));
            text_inputs.push(TextInput { field, editor });
        }
        let mut this = Self {
            search,
            section: 0,
            graph,
            scope: Scope::All,
            text_inputs,
            _subscriptions: subscriptions,
        };
        this.show_text_values(window, cx);
        this
    }

    /// Each text field shows its setting's value in the current scope, unless it is being
    /// typed in.
    fn show_text_values(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = StudioSettings::get(cx).clone();
        for input in &self.text_inputs {
            let text = text_of(&settings.value_in(&self.scope, input.field));
            input.editor.update(cx, |editor, cx| {
                if !editor.focus_handle(cx).is_focused(window) && editor.text(cx) != text {
                    editor.set_text(text, window, cx);
                }
            });
        }
    }

    /// Writes text field `index` to its setting: empty is unset where the setting may be.
    fn write_text(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.text_inputs.get(index) else {
            return;
        };
        let text = input.editor.read(cx).text(cx).trim().to_owned();
        let value = match input.field.kind {
            FieldKind::Text { nullable: true } if text.is_empty() => Value::Null,
            _ => Value::String(text),
        };
        let target = Target {
            scope: self.scope.clone(),
            field: input.field,
        };
        if StudioSettings::get(cx).value_in(&target.scope, target.field) != value {
            target.set(value, cx);
        }
        // The value as the settings hold it, trimmed or refused.
        self.show_text_values(window, cx);
    }

    /// Text field `index`'s control: the field, which Enter writes.
    fn text_control(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .key_context("SettingsField")
            .w(px(260.))
            .h(px(size::SIDEBAR_ROW))
            .px(px(space::MD))
            .rounded(px(radius::SM))
            .border_1()
            .border_color(cx.theme().colors().border)
            .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                this.write_text(index, window, cx)
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.text_inputs[index].editor.clone()),
            )
            .into_any_element()
    }
}

/// A setting's value as a text field shows it: unset is empty.
fn text_of(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
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
                .icon_size(IconSize::Small)
                .disabled(value <= min)
                .on_click(move |_, _, cx| {
                    down.set(to_value((value - step).max(min)), cx);
                }),
        )
        .child(
            div()
                .w(px(56.))
                .flex()
                .justify_center()
                .child(ds::text::body(shown)),
        )
        .child(
            studio_ui::ds::icon_button(target.id("up"), IconName::Plus)
                .icon_size(IconSize::Small)
                .disabled(value >= max)
                .on_click(move |_, _, cx| {
                    up.set(to_value((value + step).min(max)), cx);
                }),
        )
        .into_any_element()
}

/// A menu of named values; the current one is checked.
fn dropdown(target: &Target, current: Value, options: Vec<(SharedString, Value)>) -> AnyElement {
    let shown = options
        .iter()
        .find(|(_, value)| *value == current)
        .map(|(label, _)| label.clone())
        .unwrap_or_else(|| current.as_str().unwrap_or_default().to_owned().into());
    let target = target.clone();
    PopoverMenu::new(target.id("menu"))
        .trigger(
            studio_ui::ds::button(target.id("trigger"), shown)
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Outlined)
                .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
        )
        .menu(move |window, cx| {
            let options = options.clone();
            let current = current.clone();
            let target = target.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                for (label, value) in options {
                    let target = target.clone();
                    let checked = value == current;
                    menu = menu.toggleable_entry(label, checked, IconPosition::Start, None, {
                        move |_, cx| target.set(value.clone(), cx)
                    });
                }
                menu
            }))
        })
        .anchor(gpui::Anchor::TopRight)
        .into_any_element()
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
                div().max_w(px(320.)).overflow_hidden().child(
                    ds::text::code(command.join(" "), cx)
                        .color(Color::Muted)
                        .single_line()
                        .truncate(),
                ),
            )
        })
        .into_any_element()
}

fn control(target: &Target, settings: &StudioSettings, cx: &App) -> AnyElement {
    let current = settings.value_in(&target.scope, target.field);
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
            Switch::new(target.id("switch"), on.into())
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
            let mut fonts = cx.text_system().all_font_names();
            fonts.sort();
            fonts.dedup();
            let options = std::iter::once(("Default".into(), Value::Null))
                .chain(
                    fonts
                        .into_iter()
                        .filter(|name| !name.starts_with('.'))
                        .map(|name| (name.clone().into(), Value::from(name))),
                )
                .collect();
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
                        .tooltip(Tooltip::text("Settings for every graph"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.scope = Scope::All;
                            this.show_text_values(window, cx);
                            cx.notify();
                        })),
                )
                .child(
                    ds::segment("scope-graph", name.clone(), !all, cx)
                        .tooltip(Tooltip::text(format!(
                            "Values for {name} only, over those for all graphs"
                        )))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.scope = Scope::Graph(key.clone());
                            this.show_text_values(window, cx);
                            cx.notify();
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
        let settings = StudioSettings::get(cx).clone();
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
        let path = studio_settings::settings_path();
        let error = studio_settings::file_error(cx);
        let rule = ds::surface::rule(cx);
        let changed_word = if scope == Scope::All {
            "changed"
        } else {
            "own"
        };

        let nav = v_flex()
            .w(px(200.))
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
                    .filter(|f| f.section == *section && !settings.is_default_in(&scope, f))
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
            .max_w(px(720.))
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
                            let overridden = !settings.is_default_in(&scope, field);
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
                                    studio_ui::format_value(&settings.value_at(&field.path))
                                ),
                                _ => field.description.clone(),
                            };
                            let text_input = self
                                .text_inputs
                                .iter()
                                .position(|input| std::ptr::eq(input.field, field));
                            let control = match text_input {
                                Some(index) => self.text_control(index, cx),
                                None => control(&target, &settings, cx),
                            };
                            let row = ds::setting_row(
                                target.id("row"),
                                field.title.clone(),
                                description,
                                control,
                                reset,
                            );
                            if index + 1 < count {
                                row.border_b_1().border_color(rule)
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
                            .max_w(px(720.))
                            .gap(px(space::CONTROLS))
                            .child(
                                Headline::new(if searching {
                                    SharedString::from("Search")
                                } else {
                                    SharedString::from(chosen)
                                })
                                .size(HeadlineSize::Small),
                            )
                            .child(div().flex_1())
                            .children(self.render_scope(cx))
                            .child(
                                h_flex()
                                    .w(px(220.))
                                    .h(px(size::SIDEBAR_ROW))
                                    .px(px(space::MD))
                                    .gap(px(space::SM))
                                    .rounded(px(radius::SM))
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .child(
                                        Icon::new(IconName::MagnifyingGlass)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(div().flex_1().child(self.search.clone())),
                            )
                            .child(
                                studio_ui::ds::icon_button("reveal-settings", IconName::Folder)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text(format!(
                                        "Show settings.json · {}",
                                        path.display()
                                    )))
                                    .on_click({
                                        let path = path.clone();
                                        move |_, _, cx| cx.reveal_path(&path)
                                    }),
                            ),
                    )
                    .children(error.map(|e| {
                        div().w_full().max_w(px(720.)).child(
                            Callout::new()
                                .severity(Severity::Error)
                                .icon(IconName::Warning)
                                .title("settings.json could not be applied")
                                .description(e),
                        )
                    }))
                    .child(body),
            )
    }
}
