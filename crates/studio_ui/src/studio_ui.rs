//! Studio components on Zed's `ui`: the data grid pieces shared by type tables and query
//! results, value formatting, empty and error states.
//!
//! Layer 2 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.

use gpui::{App, Div, Hsla, Pixels, SharedString};
use serde_json::Value;
use ui::prelude::*;

pub use studio_theme::type_color;

mod changes;
pub mod ds;
pub mod motion;
mod values;

pub use changes::{ChangeCard, ChangeKind, PropertyRow, change_counts, change_group_header};
pub use values::{
    CopyableValue, format_bytes, group_digits, now_micros, relative_time, short_commit_id,
    utc_date_time,
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
    h_flex()
        .h(px(HEADER_HEIGHT))
        .flex_none()
        .border_b_1()
        .border_color(ds::surface::rule(cx))
        .child(div().w(leading).flex_none())
        .children(columns.iter().map(|column| {
            h_flex()
                .w(column.width)
                .flex_none()
                .px_3()
                .gap_1p5()
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
        }))
}

/// One cell's container: fixed width, clipped, single line.
pub fn grid_cell(width: Pixels, align_right: bool) -> Div {
    h_flex()
        .w(width)
        .h(px(ROW_HEIGHT))
        .flex_none()
        .px_3()
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

/// A centered message with an optional detail line, for empty, loading and error states.
pub fn empty_state(title: impl Into<SharedString>, detail: Option<SharedString>) -> Div {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_1()
        .p_6()
        .child(Label::new(title.into()).color(Color::Muted))
        .children(detail.map(|detail| {
            Label::new(detail)
                .size(LabelSize::Small)
                .color(Color::Disabled)
        }))
}

/// A small colored square for a node or edge type.
pub fn type_swatch(type_name: &str) -> Div {
    type_mark(type_name, type_name)
}

/// A node type's tinted mark, or for a relationship a neutral arrow: in Tower a relationship is
/// a line, not a thing, so only node types carry a colour.
pub fn kind_swatch(type_name: &str, is_edge: bool) -> Div {
    if is_edge {
        div()
            .size(px(16.))
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
        type_swatch(type_name)
    }
}

/// Tower's type mark: the initial of `label` (a node's label, or the type's name) in white on
/// the type's tint, a 16-pixel rounded square. The tint says what kind of thing it is, the
/// letter which one.
pub fn type_mark(type_name: &str, label: &str) -> Div {
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
        .size(px(16.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .bg(color)
        .text_color(gpui::white())
        .text_size(px(9.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .line_height(px(9.))
        .child(initial)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
