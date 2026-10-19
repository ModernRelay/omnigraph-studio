//! Resizing a grid's columns from their header's right edge (spec 002), shared by type tables
//! and query results. A line shows at the edge only on hover and while dragging, so grids keep
//! no rules between columns.
//!
//! The grid owns the state: it records a [`ColumnResize`] when the pointer goes down on a
//! handle, follows [`ColumnDrag`] moves over the whole grid with `on_drag_move`, and forgets the
//! resize on drop. Clicking a handle twice fits its column again.

use gpui::{
    App, ClickEvent, Div, DragMoveEvent, MouseButton, MouseDownEvent, Pixels, SharedString,
    Stateful, Window,
};
use ui::prelude::*;

/// The narrowest and widest a dragged column can be.
pub const COLUMN_MIN_WIDTH: f32 = 48.;
pub const COLUMN_MAX_WIDTH: f32 = 1_200.;

/// A column's right edge being dragged; what follows the pointer draws nothing.
#[derive(Clone, Copy)]
pub struct ColumnDrag;

impl Render for ColumnDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// A column resize in progress: the column, where the pointer went down and the width then.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnResize<K> {
    pub column: K,
    start_x: Pixels,
    start_width: f32,
}

impl<K> ColumnResize<K> {
    pub fn new(column: K, event: &MouseDownEvent, width: f32) -> Self {
        Self {
            column,
            start_x: event.position.x,
            start_width: width,
        }
    }

    /// The width a drag now asks for: where the pointer is, relative to where it went down,
    /// within [`COLUMN_MIN_WIDTH`] and [`COLUMN_MAX_WIDTH`], in whole pixels.
    pub fn width(&self, event: &DragMoveEvent<ColumnDrag>) -> f32 {
        self.width_at(event.event.position.x)
    }

    fn width_at(&self, x: Pixels) -> f32 {
        clamp_width(self.start_width + f32::from(x - self.start_x))
    }
}

/// `width` within the bounds a dragged column keeps, in whole pixels.
pub fn clamp_width(width: f32) -> f32 {
    width.clamp(COLUMN_MIN_WIDTH, COLUMN_MAX_WIDTH).round()
}

/// The handle at a column header's right edge. Place it in a header cell that is `relative()`.
/// `on_start` runs when the pointer goes down on it, `on_reset` on a double click; neither
/// reaches the header behind it. `active` keeps the line shown while this column is dragged.
pub fn column_resize_handle(
    index: usize,
    active: bool,
    on_start: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    on_reset: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    let line = crate::ds::surface::focus(cx);
    let group = SharedString::from(format!("column-resize-{index}"));
    div()
        .id(("column-resize", index))
        .group(group.clone())
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(px(crate::ds::size::RESIZE_HANDLE))
        .cursor_col_resize()
        .flex()
        .justify_end()
        .child(
            div()
                .w(px(crate::ds::size::DRAG_LINE))
                .h_full()
                .when(active, |d| d.bg(line))
                .group_hover(group, |d| d.bg(line)),
        )
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            cx.stop_propagation();
            on_start(event, window, cx);
        })
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            if event.click_count() >= 2 {
                on_reset(event, window, cx);
            }
        })
        .on_drag(ColumnDrag, |drag, _, _, cx| cx.new(|_| *drag))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_follows_the_pointer_within_bounds() {
        let resize = ColumnResize {
            column: "name",
            start_x: px(300.),
            start_width: 120.,
        };
        assert_eq!(resize.width_at(px(300.)), 120.);
        assert_eq!(resize.width_at(px(340.4)), 160.);
        assert_eq!(resize.width_at(px(250.)), 70.);
        assert_eq!(resize.width_at(px(0.)), COLUMN_MIN_WIDTH);
        assert_eq!(resize.width_at(px(9_000.)), COLUMN_MAX_WIDTH);
    }
}
