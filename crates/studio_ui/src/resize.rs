//! Resizing a grid's columns from their header's right edge (spec 002), shared by type tables
//! and query results. A line shows at the edge only on hover and while dragging, so grids keep
//! no rules between columns.
//!
//! The grid owns the state: it records a [`ColumnResize`] when the pointer goes down on a
//! handle, follows its own [`ColumnDrag`] moves over the whole grid with `on_drag_move`, and
//! forgets the resize when the button comes up, by a drop or a click that dragged nothing.
//! Clicking a handle twice fits its column again.

use gpui::{
    App, ClickEvent, Div, DragMoveEvent, EntityId, MouseButton, MouseDownEvent, Pixels,
    SharedString, Stateful, Window,
};
use ui::prelude::*;

/// The narrowest and widest a dragged column can be.
pub const COLUMN_MIN_WIDTH: f32 = 48.;
pub const COLUMN_MAX_WIDTH: f32 = 1_200.;

/// A column's right edge being dragged, by the grid's view that owns it: every grid hears
/// every drag's moves, and follows its own only. What follows the pointer draws nothing.
#[derive(Clone, Copy)]
pub struct ColumnDrag {
    pub owner: EntityId,
}

impl ColumnDrag {
    /// Whether the drag `event` moves is one of `owner`'s columns.
    pub fn of(event: &DragMoveEvent<ColumnDrag>, owner: EntityId, cx: &App) -> bool {
        event.drag(cx).owner == owner
    }
}

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

/// Which side of a column's rule a handle is on: the column's own end, in its header cell, or
/// the start of the column after it, in that header cell. A grid gives every column it lets a
/// person resize both, the start one only where that column is drawn against it (not where a
/// scrolled strip has moved another column there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnEdge {
    /// The column's end. `ruled` when the grid draws a rule between its columns: the handle
    /// draws it, at the cell's last pixel, in place of a border. GPUI clips a cell's children
    /// inside its border (`Style::overflow_mask`), so a rule drawn as the cell's border could
    /// never be the handle's.
    End {
        ruled: bool,
    },
    Start,
}

/// A handle on column `index`'s rule, of the grid `owner` draws: the rule is the edge between a
/// column and the next, and the edge belongs to the handle. Every grid follows one rule for who
/// owns a pointer there (spec 049):
///
/// - The edge is [`crate::ds::size::COLUMN_EDGE`] either side of the rule: the end of column
///   `index` ([`ColumnEdge::End`], over its rule) and the start of the column after it
///   ([`ColumnEdge::Start`]). Each half is placed in its own header cell, which is
///   `relative()`, as the cell's **last** child: drawn over what the cell holds, so its line
///   shows over a hover slot's fill. A press on it stops there, before the header cell around
///   it hears it, so no header click (sort) or reorder drag starts; it blocks nothing else, so
///   a column dragged onto the header to move it still drops there.
/// - Each half lights the line on its side of the rule, half of [`crate::ds::size::DRAG_LINE`]:
///   hovering either half lights one, a drag lights both, the whole line centred on the rule.
/// - Controls stay out: a header cell's end control is a [`crate::ds::header_slot`], which
///   stops short of the edge, so no control's hit area meets the handle's.
/// - A press on the handle starts a resize and goes no further; a click on a control never
///   starts one, since the control is not on the edge. A drag that starts on the handle is the
///   handle's to its end: the button under the pointer when it is released was not pressed.
///
/// `on_start` runs when the pointer goes down on it, `on_reset` on a double click. `active`
/// keeps the line shown while this column is dragged.
pub fn column_resize_handle(
    owner: EntityId,
    index: usize,
    edge: ColumnEdge,
    active: bool,
    on_start: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    on_reset: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    use crate::ds::size::{COLUMN_EDGE, DRAG_LINE, HAIRLINE};
    let rule = crate::ds::surface::rule(cx);
    let line = crate::ds::surface::focus(cx);
    let slot = index * 2 + usize::from(edge == ColumnEdge::Start);
    let ruled = matches!(edge, ColumnEdge::End { ruled: true });
    let group = SharedString::from(format!("column-resize-{slot}"));
    let handle = div()
        .id(("column-resize", slot))
        .group(group.clone())
        .absolute()
        .top_0()
        .bottom_0();
    let handle = match edge {
        ColumnEdge::End { .. } => handle.right_0().w(px(COLUMN_EDGE)).flex().justify_end(),
        ColumnEdge::Start => handle.left_0().w(px(COLUMN_EDGE)).flex().justify_start(),
    };
    handle
        .debug_selector(move || format!("column-edge-{slot}"))
        .cursor_col_resize()
        .child(
            div()
                .w(px(DRAG_LINE / 2.))
                .h_full()
                .flex()
                .justify_end()
                // At rest, the rule between the columns, where a border would be.
                .when(ruled && !active, |d| {
                    d.child(div().w(px(HAIRLINE)).h_full().bg(rule))
                })
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
        .on_drag(ColumnDrag { owner }, |drag, _, _, cx| cx.new(|_| *drag))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grid header's edges follow the rule (`column_resize_handle`): a press within
    /// `COLUMN_EDGE` either side of a rule resizes the column on its left, the last column's
    /// own half too; further in, it resizes nothing.
    #[gpui::test]
    fn a_press_at_a_rule_resizes_the_column_on_its_left(cx: &mut gpui::TestAppContext) {
        use crate::ds::size::COLUMN_EDGE;
        use std::{cell::RefCell, rc::Rc};

        struct Header(Rc<RefCell<Vec<usize>>>, Pixels);
        impl Render for Header {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let columns: Vec<crate::GridColumn> = ["a", "b", "c"]
                    .map(|key| crate::GridColumn::new(key, key).width(px(100.)))
                    .to_vec();
                let owner = cx.entity_id();
                let pressed = self.0.clone();
                crate::resizable_grid_header(
                    &columns,
                    None,
                    px(0.),
                    self.1,
                    |index, edge| {
                        let pressed = pressed.clone();
                        Some(
                            column_resize_handle(
                                owner,
                                index,
                                edge,
                                false,
                                move |_, _, _| pressed.borrow_mut().push(index),
                                |_, _, _| {},
                                cx,
                            )
                            .into_any_element(),
                        )
                    },
                    cx,
                )
            }
        }
        cx.update(studio_theme::init_test);
        let pressed: Rc<RefCell<Vec<usize>>> = Rc::default();
        let (header, cx) = cx.add_window_view({
            let pressed = pressed.clone();
            |_, _| Header(pressed, px(0.))
        });
        let mut press = |x: f32| {
            pressed.borrow_mut().clear();
            let at = gpui::point(px(x), px(crate::HEADER_HEIGHT / 2.));
            cx.simulate_mouse_down(at, MouseButton::Left, Default::default());
            cx.simulate_mouse_up(at, MouseButton::Left, Default::default());
            pressed.borrow().first().copied()
        };
        // Rules at 100, 200 and the last column's end at 300.
        for (rule, column) in [(100., 0), (200., 1)] {
            for dx in [-COLUMN_EDGE + 0.5, -0.5, 0.5, COLUMN_EDGE - 0.5] {
                assert_eq!(press(rule + dx), Some(column), "{dx} from {rule}");
            }
            assert_eq!(press(rule - COLUMN_EDGE - 1.), None);
            assert_eq!(press(rule + COLUMN_EDGE + 1.), None);
        }
        assert_eq!(press(300. - 0.5), Some(2), "the last column's own half");
        // Scrolled so the view starts at b's start, a is out of view: b's start resizes
        // nothing, and its end still resizes b.
        header.update(cx, |header, cx| {
            header.1 = px(100.);
            cx.notify();
        });
        cx.run_until_parked();
        let mut press = |x: f32| {
            pressed.borrow_mut().clear();
            let at = gpui::point(px(x), px(crate::HEADER_HEIGHT / 2.));
            cx.simulate_mouse_down(at, MouseButton::Left, Default::default());
            cx.simulate_mouse_up(at, MouseButton::Left, Default::default());
            pressed.borrow().first().copied()
        };
        assert_eq!(press(100.5), None, "a's edge with a out of view");
        assert_eq!(press(199.5), Some(1));
    }

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
