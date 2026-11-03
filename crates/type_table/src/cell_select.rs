//! The select a cell of a few values edits with (spec 004, editing): an enum's values, or a
//! boolean's, listed under the cell as a select-only combobox lists them. ↑/↓ move the
//! highlight around the ends and a letter moves it to the next value beginning with what was
//! typed; ↵, ⌥↵ and Tab take the value highlighted, as the table's keys take a typed edit, and
//! a click takes the one clicked. Esc, or leaving the cell, takes none.
//!
//! The select is a view of its own, drawn over the cell from outside the grid as the cell's
//! editor is, so moving its highlight draws neither the grid nor the table.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    Anchor, App, FocusHandle, Focusable, KeyDownEvent, MouseButton, Role, ScrollStrategy,
    UniformListScrollHandle, Window, anchored, deferred, point, uniform_list,
};
use gq_language::input::{parse_input, same_value};
use gq_language::{Property, ScalarKind};
use studio_actions::table::{SelectNextSuggestion, SelectPreviousSuggestion};
use studio_ui::ROW_HEIGHT;
use studio_ui::ds::{size, space};
use studio_ui::menu::{MenuRow, menu_separator, menu_surface};
use ui::prelude::*;

use crate::model::{Notice, Tone};

/// How long after a letter the next one still extends what was typed, as a menu's type-ahead
/// reads a word typed quickly.
const TYPE_AHEAD: Duration = Duration::from_millis(1000);

/// Rows shown before the list scrolls.
const ROWS_SHOWN: usize = 8;

/// One value a select offers: as its row reads it, and as the edit's text writes it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Choice {
    pub label: SharedString,
    pub text: String,
    /// The None of a nullable property, after its values.
    pub none: bool,
}

/// The values a cell of `property` is chosen from: an enum's in the schema's order, or a
/// boolean's, and None after them for a nullable property. `None` for a property written as
/// text: a list, a number, a date, free text.
pub(crate) fn choices(property: &Property) -> Option<Vec<Choice>> {
    if property.list {
        return None;
    }
    let values = match (&property.enum_values, property.kind) {
        (Some(values), _) => values.clone(),
        (None, ScalarKind::Bool) => vec!["true".to_owned(), "false".to_owned()],
        _ => return None,
    };
    let mut choices: Vec<Choice> = values
        .into_iter()
        .map(|value| Choice {
            label: value.clone().into(),
            text: value,
            none: false,
        })
        .collect();
    if property.nullable {
        choices.push(Choice {
            label: "None".into(),
            text: String::new(),
            none: true,
        });
    }
    Some(choices)
}

/// The choice `text` asks for, judged as a save reads it, so a draft typed `TRUE` before the
/// select was its editor is the `true` row.
pub(crate) fn chosen(property: &Property, choices: &[Choice], text: &str) -> Option<usize> {
    let value = parse_input(property, text).ok()?;
    choices.iter().position(|choice| {
        parse_input(property, &choice.text).is_ok_and(|other| same_value(&other, &value))
    })
}

/// What runs when a row is clicked, with its index.
pub(crate) type OnPick = Rc<dyn Fn(usize, &mut Window, &mut App)>;

pub(crate) struct CellSelect {
    choices: Vec<Choice>,
    /// The row the cell holds, checked; none while it holds a value the schema no longer lists.
    chosen: Option<usize>,
    highlighted: usize,
    /// Enum values read as chips in the cell, as the column shows them.
    chips: bool,
    /// The cell's width, which the list is at least.
    width: f32,
    /// Why the edit's last save did not land, and the branch it writes to, for the keys line.
    notice: Option<Notice>,
    branch: SharedString,
    typed: String,
    typed_at: Option<Instant>,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    on_pick: OnPick,
}

impl CellSelect {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        property: &Property,
        choices: Vec<Choice>,
        text: &str,
        width: f32,
        notice: Option<Notice>,
        branch: SharedString,
        focus_handle: FocusHandle,
        on_pick: OnPick,
    ) -> Self {
        let chosen = chosen(property, &choices, text);
        let scroll = UniformListScrollHandle::new();
        let highlighted = chosen.unwrap_or(0);
        if choices.get(highlighted).is_some_and(|choice| !choice.none) {
            scroll.scroll_to_item(highlighted, ScrollStrategy::Center);
        }
        Self {
            choices,
            chosen,
            highlighted,
            chips: property.enum_values.is_some(),
            width,
            notice,
            branch,
            typed: String::new(),
            typed_at: None,
            focus_handle,
            scroll,
            on_pick,
        }
    }

    /// The edit's text the highlighted row writes.
    pub(crate) fn highlighted_text(&self) -> Option<String> {
        self.choices
            .get(self.highlighted)
            .map(|choice| choice.text.clone())
    }

    pub(crate) fn set_notice(&mut self, notice: Option<Notice>, cx: &mut Context<Self>) {
        self.notice = notice;
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn highlighted(&self) -> usize {
        self.highlighted
    }

    pub(crate) fn highlight(&mut self, index: usize, cx: &mut Context<Self>) {
        if index != self.highlighted {
            self.highlighted = index;
            // None stands under the values' list, outside its scroll.
            if !self.choices[index].none {
                self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
            }
            cx.notify();
        }
    }

    fn step(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.choices.len() as isize;
        if count > 0 {
            let next = (self.highlighted as isize + step).rem_euclid(count) as usize;
            self.highlight(next, cx);
        }
    }

    /// A letter typed: the highlight moves to the next row beginning with what was typed since
    /// the last pause, the row highlighted first, so typing it again moves on to the next.
    fn type_ahead(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        let Some(typed) = keystroke
            .key_char
            .as_deref()
            .filter(|typed| typed.chars().all(|c| !c.is_whitespace() && !c.is_control()))
            .filter(|typed| !typed.is_empty())
        else {
            return;
        };
        let now = cx.background_executor().now();
        if self
            .typed_at
            .is_none_or(|at| now.duration_since(at) > TYPE_AHEAD)
        {
            self.typed.clear();
        }
        self.typed_at = Some(now);
        self.typed.push_str(&typed.to_lowercase());
        let count = self.choices.len();
        // A letter repeated looks for the next row it begins; a word for the row it begins,
        // from the one highlighted.
        let repeated =
            self.typed.chars().count() > 1 && self.typed.chars().all(|c| self.typed.starts_with(c));
        let (needle, from) = if repeated {
            (typed.to_lowercase(), self.highlighted + 1)
        } else if self.typed.chars().count() > 1 {
            (self.typed.clone(), self.highlighted)
        } else {
            (self.typed.clone(), self.highlighted + 1)
        };
        let found = (0..count)
            .map(|offset| (from + offset) % count)
            .find(|&index| {
                self.choices[index]
                    .label
                    .to_lowercase()
                    .starts_with(&needle)
            });
        if let Some(index) = found {
            self.highlight(index, cx);
        }
        cx.stop_propagation();
    }

    /// The cell while it is edited: the value it holds, as the column shows it, and the
    /// select's chevron at its end.
    fn render_value(&self, cx: &App) -> AnyElement {
        match self.chosen.and_then(|index| self.choices.get(index)) {
            Some(choice) if !choice.none && self.chips => {
                studio_ui::ds::chip(choice.label.clone(), cx).into_any_element()
            }
            Some(choice) if !choice.none => {
                studio_ui::cell_text(choice.label.clone(), cx).into_any_element()
            }
            _ => studio_ui::cell_text("—", cx)
                .color(Color::Disabled.color(cx))
                .into_any_element(),
        }
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let choice = &self.choices[index];
        let on_pick = self.on_pick.clone();
        let check = div()
            .flex_none()
            .child(studio_ui::ds::icon::lead(IconName::Check))
            .when(self.chosen != Some(index), |check| check.invisible());
        let row = MenuRow::new(("cell-select-choice", index))
            .aria_role(Role::MenuItemCheckBox)
            .aria_checked(self.chosen == Some(index))
            .aria_label(choice.label.clone())
            .toggle_state(index == self.highlighted)
            .start_slot(check)
            .child(
                studio_ui::ds::text::body(choice.label.clone())
                    .color(if choice.none {
                        Color::Muted
                    } else {
                        Color::Default
                    })
                    .single_line()
                    .truncate(),
            )
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.highlight(index, cx);
                }
            }))
            .on_click(move |_, window, cx| on_pick(index, window, cx));
        div()
            .debug_selector(move || format!("cell-select-choice-{index}"))
            .child(row)
            .into_any_element()
    }

    /// The list under the cell: the values, None after a rule, then why the last save did not
    /// land and the keys, as a typed edit's callout says them.
    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let values = self.choices.iter().filter(|choice| !choice.none).count();
        let list = uniform_list(
            "cell-select-values",
            values,
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                range.map(|index| this.render_row(index, cx)).collect()
            }),
        )
        .track_scroll(&self.scroll)
        .h(px(size::MENU_ROW * values.min(ROWS_SHOWN) as f32));
        let none = (values < self.choices.len()).then(|| self.render_row(values, cx));
        let keys = format!("↵ save · esc cancel · write · {}", self.branch);
        v_flex()
            .id("cell-select")
            .occlude()
            .min_w(px(self.width.max(size::MENU_MIN_WIDTH)))
            .max_w(px(size::POPOVER_MAX))
            .p(px(space::XS))
            .map(|list| menu_surface(list, cx))
            // A press in the list keeps the focus in the select: a click takes its value.
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .child(list)
            .when_some(none, |list, none| {
                list.child(menu_separator(cx)).child(none)
            })
            .child(menu_separator(cx))
            .child(
                v_flex()
                    .px(px(space::MD))
                    .py(px(space::XS))
                    .gap(px(space::XS))
                    .children(self.notice.clone().map(|notice| {
                        h_flex()
                            .gap(px(space::SM))
                            .child(studio_ui::ds::dot(notice.tone.color()))
                            .child(
                                studio_ui::ds::text::body(notice.message)
                                    .color(notice.tone.color()),
                            )
                            .when(notice.tone == Tone::Unknown, |row| {
                                row.child(
                                    studio_ui::ds::button("open-history", "Open history")
                                        .style(ButtonStyle::Subtle)
                                        .on_click(|_, window, cx| {
                                            window.dispatch_action(
                                                Box::new(studio_actions::graph::OpenHistory),
                                                cx,
                                            )
                                        }),
                                )
                            })
                    }))
                    .child(studio_ui::ds::text::meta(keys)),
            )
            .into_any_element()
    }
}

impl Focusable for CellSelect {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CellSelect {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list = self.render_list(cx);
        h_flex()
            .key_context("CellSelect")
            .track_focus(&self.focus_handle)
            .size_full()
            .gap(px(space::XS))
            .on_action(cx.listener(|this, _: &SelectNextSuggestion, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPreviousSuggestion, _, cx| this.step(-1, cx)))
            .on_key_down(cx.listener(Self::type_ahead))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(self.render_value(cx)),
            )
            .child(studio_ui::ds::icon::meta(IconName::ChevronDown))
            // Under the cell, its edge on the cell's, a hairline's gap below it, where a typed
            // edit's callout stands.
            .child(
                div().absolute().top_0().left_0().child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(
                                px(-(space::LG + size::HAIRLINE)),
                                px(ROW_HEIGHT + space::XXS),
                            ))
                            .snap_to_window_with_margin(px(space::MD))
                            .child(list),
                    )
                    .with_priority(2),
                ),
            )
    }
}
