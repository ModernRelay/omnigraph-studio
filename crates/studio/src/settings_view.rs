//! The settings tab (spec 002): a left nav of sections and, for the chosen one, a row per
//! setting with a title, a one-line description, a control and Reset. Rows and controls are
//! generated from the settings schema (`studio_settings::fields`); nothing here names a
//! setting. A search across every section replaces the section while it has text. Changes
//! write `settings.json` and apply at once.

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Subscription,
    Window,
};
use serde_json::Value;
use studio_settings::{FieldKind, SettingField, StudioSettings};
use studio_ui::ds::{self, radius, size, space};
use studio_workspace::{Item, ItemEvent, SavedItem};
use theme::ThemeRegistry;
use ui::prelude::*;
use ui::{Callout, ContextMenu, PopoverMenu, Severity, Switch, Tooltip};

pub struct SettingsView {
    search: Entity<Editor>,
    section: usize,
    _subscription: Subscription,
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
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search settings", window, cx);
            editor
        });
        let subscription = cx.subscribe(&search, |_, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                cx.notify();
            }
        });
        Self {
            search,
            section: 0,
            _subscription: subscription,
        }
    }
}

/// Writes `value` at the field's path.
fn set(path: &[String], value: Value, cx: &mut App) {
    let path = path.to_vec();
    studio_settings::update(cx, move |settings| {
        if let Ok(next) = settings.with_value(&path, value) {
            *settings = next;
        }
    });
}

fn id(field: &SettingField, part: &str) -> SharedString {
    format!("{}-{part}", field.path.join(".")).into()
}

/// A stepper within the field's range.
fn stepper(
    field: &SettingField,
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
    let (down, up) = (field.path.clone(), field.path.clone());
    h_flex()
        .gap(px(space::XS))
        .child(
            IconButton::new(id(field, "down"), IconName::Dash)
                .icon_size(IconSize::Small)
                .disabled(value <= min)
                .on_click(move |_, _, cx| {
                    set(&down, to_value((value - step).max(min)), cx);
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
            IconButton::new(id(field, "up"), IconName::Plus)
                .icon_size(IconSize::Small)
                .disabled(value >= max)
                .on_click(move |_, _, cx| {
                    set(&up, to_value((value + step).min(max)), cx);
                }),
        )
        .into_any_element()
}

/// A menu of named values; the current one is checked.
fn dropdown(
    field: &SettingField,
    current: Value,
    options: Vec<(SharedString, Value)>,
) -> AnyElement {
    let shown = options
        .iter()
        .find(|(_, value)| *value == current)
        .map(|(label, _)| label.clone())
        .unwrap_or_else(|| current.as_str().unwrap_or_default().to_owned().into());
    let path = field.path.clone();
    PopoverMenu::new(id(field, "menu"))
        .trigger(
            Button::new(id(field, "trigger"), shown)
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Outlined)
                .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
        )
        .menu(move |window, cx| {
            let options = options.clone();
            let current = current.clone();
            let path = path.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                for (label, value) in options {
                    let path = path.clone();
                    let checked = value == current;
                    menu = menu.toggleable_entry(label, checked, IconPosition::Start, None, {
                        move |_, cx| set(&path, value.clone(), cx)
                    });
                }
                menu
            }))
        })
        .anchor(gpui::Anchor::TopRight)
        .into_any_element()
}

/// The agent's presets as a segmented choice, with the command it runs underneath.
fn agent_control(field: &SettingField, current: Value, cx: &App) -> AnyElement {
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
        let path = field.path.clone();
        let selected = preset == command;
        segments = segments.child(
            ds::segment(id(field, &index.to_string()), name, selected, cx)
                .on_click(move |_, _, cx| set(&path, Value::from(preset.clone()), cx)),
        );
    }
    if !known {
        segments = segments.child(ds::segment(id(field, "custom"), "Custom", true, cx));
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

fn control(field: &SettingField, settings: &StudioSettings, cx: &App) -> AnyElement {
    let current = settings.value_at(&field.path);
    match &field.kind {
        FieldKind::Choice(choices) if choices.len() <= 4 => {
            let mut segments = ds::segmented(cx);
            for choice in choices {
                let path = field.path.clone();
                let value = Value::from(choice.value.clone());
                let selected = current == value;
                segments = segments.child(
                    ds::segment(id(field, &choice.value), choice.label.clone(), selected, cx)
                        .when_some(choice.description.clone(), |segment, description| {
                            segment.tooltip(Tooltip::text(description))
                        })
                        .on_click(move |_, _, cx| set(&path, value.clone(), cx)),
                );
            }
            segments.into_any_element()
        }
        FieldKind::Choice(choices) => dropdown(
            field,
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
            field,
            current.as_f64().unwrap_or_default(),
            *min,
            *max,
            *step,
            *integer,
        ),
        FieldKind::Toggle => {
            let on = current.as_bool().unwrap_or_default();
            let path = field.path.clone();
            Switch::new(id(field, "switch"), on.into())
                .on_click(move |state, _, cx| {
                    set(&path, Value::Bool(*state == ToggleState::Selected), cx)
                })
                .into_any_element()
        }
        FieldKind::Custom(kind) if kind == "theme" => {
            let names = ThemeRegistry::global(cx).list_names();
            dropdown(
                field,
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
            dropdown(field, current, options)
        }
        FieldKind::Custom(kind) if kind == "agent" => agent_control(field, current, cx),
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

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).text(cx).to_lowercase();
        let searching = !query.trim().is_empty();
        let sections = studio_settings::sections();
        self.section = self.section.min(sections.len().saturating_sub(1));
        let chosen = sections.get(self.section).copied().unwrap_or_default();
        let settings = StudioSettings::get(cx).clone();
        let matches = |field: &SettingField| {
            if searching {
                field.title.to_lowercase().contains(query.trim())
                    || field.description.to_lowercase().contains(query.trim())
                    || field.path.join(".").contains(query.trim())
            } else {
                field.section == chosen
            }
        };
        let shown: Vec<&SettingField> = studio_settings::fields()
            .iter()
            .filter(|f| matches(f))
            .collect();
        let path = studio_settings::settings_path();
        let error = studio_settings::file_error(cx);
        let rule = ds::surface::rule(cx);

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
                    .filter(|f| f.section == *section && !settings.is_default_at(&f.path))
                    .count();
                ds::SidebarRow::new(("settings-section", index), *section)
                    .selected(!searching && index == self.section)
                    .when(count > 0, |row| {
                        row.end(ds::sidebar_count(format!("{count} changed")))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.section = index;
                        this.search
                            .update(cx, |search, cx| search.clear(window, cx));
                        cx.notify();
                    }))
            }));

        // While searching, the matches are grouped by section under a label each.
        let mut groups: Vec<(&str, Vec<&SettingField>)> = Vec::new();
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
                            let reset = (!settings.is_default_at(&field.path)).then(|| {
                                let path = field.path.clone();
                                Box::new(
                                    move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                                        let path = path.clone();
                                        studio_settings::update(cx, move |s| {
                                            *s = s.reset_at(&path)
                                        });
                                    },
                                ) as _
                            });
                            let row = ds::setting_row(
                                id(field, "row"),
                                field.title.clone(),
                                field.description.clone(),
                                control(field, &settings, cx),
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
                body.child(div().pt(px(space::XXXL)).child(ds::text::muted(format!(
                    "No setting matches “{}”",
                    query.trim()
                ))))
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
                            .child(
                                h_flex()
                                    .w(px(240.))
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
                                IconButton::new("reveal-settings", IconName::Folder)
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
