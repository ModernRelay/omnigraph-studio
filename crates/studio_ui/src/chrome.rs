//! Studio's visual rules as functions (spec 002, "Visual language"), so every surface draws
//! the same section labels, sidebar rows, toolbars and table rules.

use gpui::{App, Div, ElementId, Hsla, SharedString};
use ui::prelude::*;
use ui::{ListItem, ListItemSpacing};

/// Height of a sidebar row.
pub const SIDEBAR_ROW_HEIGHT: f32 = 26.;
/// Height of a surface's toolbar.
pub const TOOLBAR_HEIGHT: f32 = 40.;
/// Height of every column's header and footer strip, so their rules line up across the
/// window (spec 002, after Delta).
pub const STRIP_HEIGHT: f32 = 32.;
/// Height of a document's footer strip (its own status line, as Delta's composer bar).
pub const FOOTER_HEIGHT: f32 = STRIP_HEIGHT;

/// The rule between regions and rows: `border_variant`, lighter.
pub fn hairline(cx: &App) -> Hsla {
    cx.theme().colors().border_variant.opacity(0.6)
}

/// A quiet section label, as in the sidebar's "Types" or "Branches".
pub fn section_label(title: impl Into<SharedString>) -> Div {
    div().px_3().pt_4().pb_1().child(
        Label::new(title.into())
            .size(LabelSize::XSmall)
            .color(Color::Muted),
    )
}

/// A sidebar row: inset, rounded, a grey pill when selected.
pub fn sidebar_item(id: impl Into<ElementId>, selected: bool) -> ListItem {
    ListItem::new(id)
        .inset(true)
        .spacing(ListItemSpacing::Dense)
        .toggle_state(selected)
}

/// A count or other trailing number in a sidebar row.
pub fn sidebar_count(text: impl Into<SharedString>) -> Label {
    Label::new(text.into())
        .size(LabelSize::XSmall)
        .color(Color::Muted)
}

/// A surface's toolbar: fixed height, a hairline below.
pub fn toolbar(cx: &App) -> Div {
    h_flex()
        .h(px(TOOLBAR_HEIGHT))
        .flex_none()
        .px_3()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(hairline(cx))
}

/// A one-sentence empty or waiting state inside a sidebar section.
pub fn sidebar_note(text: impl Into<SharedString>) -> Div {
    div().px_3().py_1().child(
        Label::new(text.into())
            .size(LabelSize::Small)
            .color(Color::Muted),
    )
}
