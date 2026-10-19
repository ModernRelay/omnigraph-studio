//! Studio components on Zed's `ui`: the data grid pieces shared by type tables and query
//! results, value formatting, empty and error states.
//!
//! Layer 2 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.

use gpui::{AnyElement, App, Div, Hsla, Pixels, SharedString};
use serde_json::Value;
use ui::prelude::*;

pub use studio_theme::type_color;

mod button;
mod cell_text;
mod changes;
pub mod ds;
mod grid_places;
pub mod menu;
mod modal;
pub mod motion;
mod overlay_slot;
mod resize;
pub mod rows;
pub mod text;
mod values;

pub use cell_text::{CellText, cell_text};
pub use changes::{
    ChangeCard, ChangeKind, PropertyRow, RelationshipLine, change_counts, change_group_header,
};
pub use grid_places::{GridPlace, columns_in_view, grid_places, scrolled_within};
pub use overlay_slot::{OverlaySlot, OverlaySlotElement};
pub use resize::{
    COLUMN_MAX_WIDTH, COLUMN_MIN_WIDTH, ColumnDrag, ColumnResize, clamp_width, column_resize_handle,
};
pub use values::{
    CopyableValue, format_bytes, group_digits, now_micros, relative_time, short_commit_id,
    utc_date_time, value_text,
};

/// A column in a data grid.
#[derive(Debug, Clone)]
pub struct GridColumn {
    pub key: SharedString,
    pub title: SharedString,
    /// The declared type, shown under the title.
    pub detail: Option<SharedString>,
    pub width: Pixels,
    pub align_right: bool,
}

impl GridColumn {
    pub fn new(key: impl Into<SharedString>, title: impl Into<SharedString>) -> Self {
        Self {
            key: key.into(),
            title: title.into(),
            detail: None,
            width: px(180.),
            align_right: false,
        }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    pub fn align_right(mut self, align_right: bool) -> Self {
        self.align_right = align_right;
        self
    }
}

pub const ROW_HEIGHT: f32 = ds::size::ROW;
pub const HEADER_HEIGHT: f32 = ds::size::HEADER;

/// The header row of a grid: one line per column, the name then its type, muted, with a
/// hairline below (spec 002, "Visual language").
pub fn grid_header(columns: &[GridColumn], leading: Pixels, cx: &App) -> Div {
    resizable_grid_header(columns, None, leading, |_| None, cx)
}

/// [`grid_header`] at a frame's `places` (every column when `None`; see [`grid_places`]),
/// with what `handle` gives for a column at its right edge: a [`column_resize_handle`].
pub fn resizable_grid_header(
    columns: &[GridColumn],
    places: Option<&[GridPlace]>,
    leading: Pixels,
    mut handle: impl FnMut(usize) -> Option<AnyElement>,
    cx: &App,
) -> Div {
    let every: Vec<GridPlace>;
    let places = match places {
        Some(places) => places,
        None => {
            every = (0..columns.len()).map(GridPlace::Column).collect();
            &every
        }
    };
    h_flex()
        .h(px(HEADER_HEIGHT))
        .flex_none()
        .border_b_1()
        .border_color(ds::surface::rule(cx))
        .child(div().w(leading).flex_none())
        .children(places.iter().map(|place| {
            let index = match *place {
                GridPlace::Column(index) => index,
                GridPlace::Gap(width) => return div().w(width).flex_none().into_any_element(),
            };
            let column = &columns[index];
            h_flex()
                .relative()
                .w(column.width)
                .flex_none()
                .px(px(crate::ds::space::LG))
                .gap(px(crate::ds::space::SM))
                .overflow_hidden()
                .when(column.align_right, |d| d.justify_end())
                .child(
                    Label::new(column.title.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .single_line(),
                )
                .children(column.detail.clone().map(|detail| {
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Disabled)
                        .single_line()
                }))
                .children(handle(index))
                .into_any_element()
        }))
}

/// One cell's container: fixed width, clipped, single line.
pub fn grid_cell(width: Pixels, align_right: bool) -> Div {
    h_flex()
        .w(width)
        .h(px(ROW_HEIGHT))
        .flex_none()
        .px(px(crate::ds::space::LG))
        .overflow_hidden()
        .when(align_right, |d| d.justify_end())
}

/// How a JSON value from a query result reads in a cell. Null is an em dash; lists show the
/// first items then "+N"; objects show their `@id` when they have one.
pub fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "—".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let shown: Vec<String> = items.iter().take(3).map(format_value).collect();
            let mut text = shown.join(", ");
            if items.len() > 3 {
                text.push_str(&format!(" +{}", items.len() - 3));
            }
            text
        }
        Value::Object(map) => match map.get("@id") {
            Some(Value::String(id)) => id.clone(),
            _ => serde_json::to_string(value).unwrap_or_default(),
        },
    }
}

/// Whether a value is a number, for right alignment.
pub fn is_numeric(value: &Value) -> bool {
    matches!(value, Value::Number(_))
}

/// A small colored square for a node or edge type.
pub fn type_swatch(type_name: &str, cx: &App) -> Div {
    type_mark(type_name, type_name, cx)
}

/// A node type's tinted mark, or for a relationship a neutral arrow: a relationship is a line,
/// not a thing, so only node types carry a colour.
pub fn kind_swatch(type_name: &str, is_edge: bool, cx: &App) -> Div {
    if is_edge {
        div()
            .size(px(ds::size::ROW_ICON))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .child(
                Icon::new(IconName::ArrowRight)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
    } else {
        type_swatch(type_name, cx)
    }
}

/// The type mark: the initial of `label` (a node's label, or the type's name) in white on
/// the type's tint, a 16-pixel rounded square. The tint says what kind of thing it is, the
/// letter which one.
pub fn type_mark(type_name: &str, label: &str, cx: &App) -> Div {
    let color: Hsla = type_color(type_name);
    let initial = label
        .trim()
        .chars()
        .next()
        .or_else(|| type_name.chars().next())
        .unwrap_or('?')
        .to_uppercase()
        .collect::<String>();
    div()
        .size(px(ds::size::ROW_ICON))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(ds::radii(cx).xs))
        .bg(color)
        .text_color(ds::surface::on_tint(cx))
        .text_size(px(ds::size::MARK_TEXT))
        .font_weight(ds::text::weight::HEADING)
        .line_height(px(ds::size::MARK_TEXT))
        .child(initial)
}

/// The keys bound to `action`, written as this platform writes a shortcut in text: "⌘⇧A" on
/// macOS, "Ctrl+Shift+A" elsewhere, modifiers in the order Zed's key hints draw them. For text
/// that names a shortcut, so it follows the keymap and the platform rather than a constant. The
/// binding added last wins, as in dispatch, whatever context it applies in; `None` when nothing
/// binds the action.
pub fn shortcut_text(action: &dyn gpui::Action, cx: &App) -> Option<String> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let binding = keymap.bindings_for_action(action).last()?;
    let style = ui::PlatformStyle::platform();
    Some(
        binding
            .keystrokes()
            .iter()
            .map(|stroke| keystroke_text(stroke.modifiers(), stroke.key(), style))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn keystroke_text(modifiers: &gpui::Modifiers, key: &str, style: ui::PlatformStyle) -> String {
    let mac = style == ui::PlatformStyle::Mac;
    let platform = match style {
        ui::PlatformStyle::Mac => "⌘",
        ui::PlatformStyle::Linux => "Super",
        ui::PlatformStyle::Windows => "Win",
    };
    let held = [
        (modifiers.function, "Fn", "Fn"),
        (modifiers.control, "⌃", "Ctrl"),
        (modifiers.alt, "⌥", "Alt"),
        (modifiers.platform, platform, platform),
        (modifiers.shift, "⇧", "Shift"),
    ];
    let key = match (key, mac) {
        ("enter" | "return", true) => "↵".to_owned(),
        ("backspace", true) => "⌫".to_owned(),
        ("delete", true) => "⌦".to_owned(),
        ("escape", true) => "⎋".to_owned(),
        ("tab", true) => "⇥".to_owned(),
        ("up", _) => "↑".to_owned(),
        ("down", _) => "↓".to_owned(),
        ("left", _) => "←".to_owned(),
        ("right", _) => "→".to_owned(),
        ("escape", false) => "Esc".to_owned(),
        ("pageup", _) => "PageUp".to_owned(),
        ("pagedown", _) => "PageDown".to_owned(),
        (key, _) => {
            let mut chars = key.chars();
            chars.next().map_or(String::new(), |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    };
    let mut parts: Vec<&str> = held
        .iter()
        .filter(|(on, _, _)| *on)
        .map(|(_, symbol, word)| if mac { *symbol } else { *word })
        .collect();
    parts.push(&key);
    parts.join(if mac { "" } else { "+" })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A shortcut reads as the platform writes one: symbols run together on macOS, words joined
    /// by "+" elsewhere, modifiers in the order of Zed's key hints.
    #[test]
    fn shortcuts_are_written_as_the_platform_writes_them() {
        let held = |control, alt, platform, shift| gpui::Modifiers {
            control,
            alt,
            platform,
            shift,
            function: false,
        };
        let mac = ui::PlatformStyle::Mac;
        let linux = ui::PlatformStyle::Linux;
        assert_eq!(
            keystroke_text(&held(false, false, true, true), "a", mac),
            "⌘⇧A"
        );
        assert_eq!(
            keystroke_text(&held(false, false, true, false), "enter", mac),
            "⌘↵"
        );
        assert_eq!(
            keystroke_text(&held(true, false, true, false), "]", mac),
            "⌃⌘]"
        );
        assert_eq!(
            keystroke_text(&held(false, false, false, false), "g", mac),
            "G"
        );
        assert_eq!(
            keystroke_text(&held(true, false, false, true), "a", linux),
            "Ctrl+Shift+A"
        );
        assert_eq!(
            keystroke_text(&held(true, false, false, false), "enter", linux),
            "Ctrl+Enter"
        );
        assert_eq!(
            keystroke_text(&held(false, false, false, false), "escape", linux),
            "Esc"
        );
    }

    #[test]
    fn formats_values_for_cells() {
        assert_eq!(format_value(&Value::Null), "—");
        assert_eq!(
            format_value(&json!(["a", "b", "c", "d", "e"])),
            "a, b, c +2"
        );
        assert_eq!(
            format_value(&json!({"@id": "person-1", "name": "Ada"})),
            "person-1"
        );
        assert_eq!(format_value(&json!(42)), "42");
    }
}
