//! The settings tab (spec 002): every setting as a row with a title, a one-line description, a
//! control and Reset, filtered by a search box. Changes write `settings.json` and apply at once.

use editor::{Editor, EditorEvent};
use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Subscription, Window};
use studio_settings::{StudioSettings, ThemeMode};
use studio_workspace::{Item, ItemEvent};
use theme::ThemeRegistry;
use ui::prelude::*;
use ui::{ContextMenu, PopoverMenu};

pub struct SettingsView {
    focus_handle: FocusHandle,
    search: Entity<Editor>,
    _subscription: Subscription,
}

impl EventEmitter<ItemEvent> for SettingsView {}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
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
            focus_handle: cx.focus_handle(),
            search,
            _subscription: subscription,
        }
    }
}

struct Row {
    title: &'static str,
    description: &'static str,
    is_default: bool,
    control: AnyElement,
    reset: fn(&mut StudioSettings),
}

fn stepper(
    id: &'static str,
    value: String,
    step: impl Fn(&mut StudioSettings, i64) + Clone + 'static,
) -> AnyElement {
    let down = step.clone();
    h_flex()
        .gap_1()
        .child(
            IconButton::new((id, 0usize), IconName::Dash)
                .icon_size(IconSize::Small)
                .on_click(move |_, _, cx| {
                    let down = down.clone();
                    studio_settings::update(cx, move |s| down(s, -1));
                }),
        )
        .child(div().w(px(56.)).flex().justify_center().child(Label::new(value).size(LabelSize::Small)))
        .child(
            IconButton::new((id, 1usize), IconName::Plus)
                .icon_size(IconSize::Small)
                .on_click(move |_, _, cx| {
                    let up = step.clone();
                    studio_settings::update(cx, move |s| up(s, 1));
                }),
        )
        .into_any_element()
}

fn theme_picker(id: &'static str, current: String, dark: bool, cx: &App) -> AnyElement {
    let names: Vec<SharedString> = ThemeRegistry::global(cx).list_names();
    PopoverMenu::new(id)
        .trigger(Button::new((id, 0usize), format!("{current} ▾")).label_size(LabelSize::Small))
        .menu(move |window, cx| {
            let names = names.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                for name in names {
                    let chosen = name.to_string();
                    menu = menu.entry(name, None, move |_, cx| {
                        let chosen = chosen.clone();
                        studio_settings::update(cx, move |s| {
                            if dark {
                                s.theme.dark = chosen;
                            } else {
                                s.theme.light = chosen;
                            }
                        });
                    });
                }
                menu
            }))
        })
        .into_any_element()
}

impl SettingsView {
    fn rows(&self, cx: &App) -> Vec<Row> {
        let settings = StudioSettings::get(cx).clone();
        let defaults = StudioSettings::default();
        let mode_button = |label: &'static str, mode: ThemeMode| {
            Button::new(label, label)
                .label_size(LabelSize::Small)
                .toggle_state(settings.theme.mode == mode)
                .style(ButtonStyle::Subtle)
                .on_click(move |_, _, cx| studio_settings::update(cx, move |s| s.theme.mode = mode))
        };
        vec![
            Row {
                title: "Appearance",
                description: "Follow the system, or always use the light or dark theme.",
                is_default: settings.theme.mode == defaults.theme.mode,
                control: h_flex()
                    .gap_1()
                    .child(mode_button("System", ThemeMode::System))
                    .child(mode_button("Light", ThemeMode::Light))
                    .child(mode_button("Dark", ThemeMode::Dark))
                    .into_any_element(),
                reset: |s| s.theme.mode = StudioSettings::default().theme.mode,
            },
            Row {
                title: "Light theme",
                description: "The theme used in light appearance.",
                is_default: settings.theme.light == defaults.theme.light,
                control: theme_picker("light-theme", settings.theme.light.clone(), false, cx),
                reset: |s| s.theme.light = StudioSettings::default().theme.light,
            },
            Row {
                title: "Dark theme",
                description: "The theme used in dark appearance.",
                is_default: settings.theme.dark == defaults.theme.dark,
                control: theme_picker("dark-theme", settings.theme.dark.clone(), true, cx),
                reset: |s| s.theme.dark = StudioSettings::default().theme.dark,
            },
            Row {
                title: "Interface font size",
                description: "Text size for menus, tables and panels.",
                is_default: settings.ui_font_size == defaults.ui_font_size,
                control: stepper("ui-font", format!("{}", settings.ui_font_size), |s, d| {
                    s.ui_font_size = (s.ui_font_size + d as f32).clamp(10., 24.)
                }),
                reset: |s| s.ui_font_size = StudioSettings::default().ui_font_size,
            },
            Row {
                title: "Editor font size",
                description: "Text size in the query editor.",
                is_default: settings.buffer_font_size == defaults.buffer_font_size,
                control: stepper("buffer-font", format!("{}", settings.buffer_font_size), |s, d| {
                    s.buffer_font_size = (s.buffer_font_size + d as f32).clamp(10., 24.)
                }),
                reset: |s| s.buffer_font_size = StudioSettings::default().buffer_font_size,
            },
            Row {
                title: "Table page size",
                description: "Rows read per page in type tables.",
                is_default: settings.studio.type_table_page_size == defaults.studio.type_table_page_size,
                control: stepper("page-size", settings.studio.type_table_page_size.to_string(), |s, d| {
                    let next = s.studio.type_table_page_size as i64 + d * 25;
                    s.studio.type_table_page_size = next.clamp(25, 1000) as u32
                }),
                reset: |s| s.studio.type_table_page_size = StudioSettings::default().studio.type_table_page_size,
            },
            Row {
                title: "Relationship page size",
                description: "Neighbours shown per page in the relationship tree.",
                is_default: settings.studio.tree_page_size == defaults.studio.tree_page_size,
                control: stepper("tree-size", settings.studio.tree_page_size.to_string(), |s, d| {
                    let next = s.studio.tree_page_size as i64 + d * 10;
                    s.studio.tree_page_size = next.clamp(10, 500) as u32
                }),
                reset: |s| s.studio.tree_page_size = StudioSettings::default().studio.tree_page_size,
            },
            Row {
                title: "Canvas node limit",
                description: "Most nodes the canvas draws before offering a table instead.",
                is_default: settings.studio.canvas_max_nodes == defaults.studio.canvas_max_nodes,
                control: stepper("canvas-max", settings.studio.canvas_max_nodes.to_string(), |s, d| {
                    let next = s.studio.canvas_max_nodes as i64 + d * 500;
                    s.studio.canvas_max_nodes = next.clamp(500, 20_000) as u32
                }),
                reset: |s| s.studio.canvas_max_nodes = StudioSettings::default().studio.canvas_max_nodes,
            },
        ]
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).text(cx).to_lowercase();
        let border = cx.theme().colors().border;
        let border_variant = cx.theme().colors().border_variant;
        let rows: Vec<Row> = self
            .rows(cx)
            .into_iter()
            .filter(|row| {
                query.is_empty()
                    || row.title.to_lowercase().contains(&query)
                    || row.description.to_lowercase().contains(&query)
            })
            .collect();
        let path = studio_settings::settings_path();
        let error = studio_settings::file_error(cx);
        v_flex()
            .id("settings")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .gap_4()
            .child(Headline::new("Settings"))
            .child(
                Label::new(format!("Saved to {}", path.display()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(error.map(|e| Label::new(e).size(LabelSize::Small).color(Color::Error)))
            .child(
                div()
                    .w(px(360.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .child(self.search.clone()),
            )
            .child(
                v_flex()
                    .max_w(px(760.))
                    .children(rows.into_iter().enumerate().map(|(index, row)| {
                        let reset = row.reset;
                        h_flex()
                            .py_3()
                            .gap_4()
                            .border_b_1()
                            .border_color(border_variant)
                            .child(
                                v_flex()
                                    .flex_1()
                                    .child(Label::new(row.title))
                                    .child(Label::new(row.description).size(LabelSize::Small).color(Color::Muted)),
                            )
                            .child(row.control)
                            .child(
                                div().w(px(64.)).children((!row.is_default).then(|| {
                                    Button::new(("reset", index), "Reset")
                                        .label_size(LabelSize::Small)
                                        .style(ButtonStyle::Subtle)
                                        .on_click(move |_, _, cx| studio_settings::update(cx, reset))
                                })),
                            )
                    })),
            )
    }
}
