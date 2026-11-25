//! The column model every grid uses (spec 049): a column's width is the one the person gave it,
//! else the grid's own; the grid's cells wrap or are cut, by the person's choice, else the
//! grid's own. What the person chose is a [`GridLayout`], which the grid keeps where its table
//! is kept (a type's in settings, a query tab's with the tab). The grid draws its rows through
//! GPUI's `list`, which measures a row when it draws it, so a wrapped row is as tall as
//! [`wrapped_row_height`] and the rows stay virtualized.

use std::rc::Rc;

use gpui::{Action, App, Div, Pixels, Styled as _, Window, px};
pub use studio_settings::{CellWrap, GridLayout};
use ui::IconPosition;

use crate::ds::menu::{ContextMenu, ContextMenuEntry};
use crate::{CellText, clamp_width};

/// A change a person makes to a grid's columns, from a key, a column's menu or its edge: the
/// same few for every grid, which each grid's keys and menu name ([`GridColumns::change`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnChange {
    /// The column [`COLUMN_STEP`] wider.
    Widen,
    /// The column [`COLUMN_STEP`] narrower.
    Narrow,
    /// The column as wide as its widest text.
    Fit,
    /// The column back to its own width.
    ResetWidth,
    /// Every column back to its own width, and the grid to its own wrap.
    Reset,
    /// Cut cells wrapped, or wrapped ones cut.
    ToggleWrap,
}

impl ColumnChange {
    /// Whether the change is to one column, which a grid names by its cursor or its menu.
    pub fn is_to_a_column(self) -> bool {
        !matches!(self, ColumnChange::Reset | ColumnChange::ToggleWrap)
    }
}

/// How much wider or narrower one Widen or Narrow makes a column.
pub const COLUMN_STEP: f32 = 16.;
/// The most rows a fit measures: those first read, as a grid holds them.
pub const FIT_ROWS: usize = 1_000;
/// The most lines a wrapped cell shows; the last is cut with "…".
pub const WRAP_LINES: usize = 6;

/// See the module documentation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GridColumns {
    layout: GridLayout,
    /// The grid's own wrap, which the person's choice is over.
    own_wrap: CellWrap,
}

impl GridColumns {
    /// The model of a grid whose own wrap is `own_wrap`, with what the person chose: widths
    /// held within the bounds, and a choice that is the grid's own forgotten.
    pub fn new(layout: GridLayout, own_wrap: CellWrap) -> Self {
        let mut columns = Self {
            layout: GridLayout::default(),
            own_wrap,
        };
        for (key, width) in layout.widths {
            columns.set_width(&key, Some(width as f32));
        }
        columns.layout.wrap = layout.wrap.filter(|wrap| *wrap != own_wrap);
        columns
    }

    /// What the person chose, to keep.
    pub fn layout(&self) -> &GridLayout {
        &self.layout
    }

    /// A column's width: the one the person gave it, else the grid's own (`own`).
    pub fn width(&self, key: &str, own: f32) -> f32 {
        self.chosen_width(key).unwrap_or(own)
    }

    /// The width the person gave a column, if they did.
    pub fn chosen_width(&self, key: &str) -> Option<f32> {
        self.layout.widths.get(key).map(|width| *width as f32)
    }

    /// Gives a column `width`, within [`crate::COLUMN_MIN_WIDTH`] and
    /// [`crate::COLUMN_MAX_WIDTH`] in whole pixels; `None` gives it back its own. Whether
    /// anything changed.
    pub fn set_width(&mut self, key: &str, width: Option<f32>) -> bool {
        let before = self.layout.widths.get(key).copied();
        match width {
            Some(width) => {
                self.layout
                    .widths
                    .insert(key.to_owned(), clamp_width(width) as u32);
            }
            None => {
                self.layout.widths.remove(key);
            }
        }
        self.layout.widths.get(key).copied() != before
    }

    /// Makes a column `steps` × [`COLUMN_STEP`] wider (narrower when negative), from the width
    /// it has now (`own` when the person gave it none).
    pub fn step_width(&mut self, key: &str, own: f32, steps: i32) -> bool {
        let width = self.width(key, own) + steps as f32 * COLUMN_STEP;
        self.set_width(key, Some(width))
    }

    /// Makes a column as wide as `widest`, the widest of its texts (its header's among them),
    /// with a cell's inset on both sides ([`fitted_width`]).
    pub fn fit(&mut self, key: &str, widest: Pixels) -> bool {
        self.set_width(key, Some(fitted_width(widest)))
    }

    /// Forgets every width and the wrap: the grid as it was made.
    pub fn reset(&mut self) -> bool {
        let changed = !self.layout.is_default();
        self.layout = GridLayout::default();
        changed
    }

    /// Whether the grid's cells wrap.
    pub fn wrap(&self) -> CellWrap {
        self.layout.wrap.unwrap_or(self.own_wrap)
    }

    /// Wraps cut cells, or cuts wrapped ones. The grid's own choice is kept as none.
    pub fn toggle_wrap(&mut self) {
        let wrap = self.wrap().toggled();
        self.layout.wrap = (wrap != self.own_wrap).then_some(wrap);
    }

    /// Makes `change`, to `column` (its key and own width) when it is to a column; `widest`
    /// measures the column's texts, for a fit only. Whether anything changed: a change to a
    /// column with none named changes nothing.
    pub fn change(
        &mut self,
        change: ColumnChange,
        column: Option<(&str, f32)>,
        widest: impl FnOnce() -> Pixels,
    ) -> bool {
        match (change, column) {
            (ColumnChange::Reset, _) => self.reset(),
            (ColumnChange::ToggleWrap, _) => {
                self.toggle_wrap();
                true
            }
            (_, None) => false,
            (ColumnChange::Widen, Some((key, own))) => self.step_width(key, own, 1),
            (ColumnChange::Narrow, Some((key, own))) => self.step_width(key, own, -1),
            (ColumnChange::Fit, Some((key, _))) => self.fit(key, widest()),
            (ColumnChange::ResetWidth, Some((key, _))) => self.set_width(key, None),
        }
    }

    /// A column's menu's entries for its width and the grid's wrap, after what the grid puts
    /// first: Fit width, Reset width, then Wrap text and Reset widths and wrap. `key` is the column the
    /// menu is for; `action` names the action whose key an entry shows, if it has one; `apply`
    /// makes a change.
    pub fn menu_entries(
        &self,
        menu: ContextMenu,
        key: &str,
        action: impl Fn(ColumnChange) -> Option<Box<dyn Action>>,
        apply: impl Fn(ColumnChange, &mut Window, &mut App) + 'static,
    ) -> ContextMenu {
        let apply = Rc::new(apply);
        let entry = |label: &'static str, change: ColumnChange| {
            let apply = apply.clone();
            let entry = ContextMenuEntry::new(label)
                .toggleable(IconPosition::Start, false)
                .handler(move |window, cx| apply(change, window, cx));
            // A column's own entries show no key: the key acts on the selected cell's column,
            // which may be another than the menu's.
            match action(change).filter(|_| !change.is_to_a_column()) {
                Some(action) => entry.action(action),
                None => entry,
            }
        };
        let wrapped = self.wrap() == CellWrap::Wrap;
        let toggle = apply.clone();
        menu.separator()
            .item(entry("Fit width", ColumnChange::Fit))
            .item(
                entry("Reset width", ColumnChange::ResetWidth)
                    .disabled(self.chosen_width(key).is_none()),
            )
            .separator()
            .toggleable_entry(
                "Wrap text",
                wrapped,
                IconPosition::Start,
                action(ColumnChange::ToggleWrap),
                move |window, cx| toggle(ColumnChange::ToggleWrap, window, cx),
            )
            .item(
                entry("Reset widths and wrap", ColumnChange::Reset)
                    .disabled(self.layout.is_default()),
            )
    }
}

/// How far past the view a wrapped grid lays its rows out, above and below: GPUI's `list`
/// learns a row's height only by drawing it, so this is how far a frame's scroll can reach
/// past the rows measured so far.
pub const WRAPPED_DRAW_AHEAD: f32 = 600.;

/// Where a grid's rows are scrolled, whichever way they are drawn (spec 049). Cut, every row
/// is one height and GPUI's `uniform_list` draws them, its scroll range exact. Wrapped, GPUI's
/// `list` measures each row as it draws it: it forgets every height when its width changes and
/// counts a row it has not drawn as none, so a cut grid, which has one height, does not use it.
/// A grid asks this for the line under the pointer, the first line in view, and to scroll, and
/// never divides by a row's height itself.
#[derive(Clone)]
pub struct GridRows {
    /// The cut rows' scroll.
    pub uniform: gpui::UniformListScrollHandle,
    /// The wrapped rows, as GPUI's `list` laid them out.
    pub measured: gpui::ListState,
    /// How many lines the grid holds, as last told ([`GridRows::sync`]).
    count: Rc<std::cell::Cell<usize>>,
}

impl GridRows {
    /// The rows of a grid whose cut rows scroll by `uniform`.
    pub fn new(uniform: gpui::UniformListScrollHandle) -> Self {
        Self {
            uniform,
            measured: gpui::ListState::new(0, gpui::ListAlignment::Top, px(WRAPPED_DRAW_AHEAD)),
            count: Rc::default(),
        }
    }

    /// The grid holds other lines now, `count` of them, from the first: every height the
    /// wrapped list measured is forgotten.
    pub fn reset(&self, count: usize) {
        self.count.set(count);
        self.measured.reset(count);
    }

    /// The grid holds `count` lines. Lines added or taken are at the end; nothing in view
    /// moves.
    pub fn sync(&self, count: usize) {
        self.count.set(count);
        let old = self.measured.item_count();
        if count > old {
            // GPUI moves a scroll held at the old end past what is added.
            let top = self.measured.logical_scroll_top();
            self.measured.splice(old..old, count - old);
            if top.item_ix >= old {
                self.measured.scroll_to(top);
            }
        } else if count < old {
            self.measured.splice(count..old, 0);
        }
    }

    /// A cut line's height, as the list last laid its lines out: GPUI's `last_item_size` holds
    /// the list's own size as `item` and its lines' as `contents`.
    pub fn cut_row_height(&self) -> Pixels {
        let count = self.count.get();
        self.uniform
            .0
            .borrow()
            .last_item_size
            .filter(|_| count > 0)
            .map(|size| size.contents.height / count as f32)
            .filter(|height| *height > Pixels::ZERO)
            .unwrap_or(px(crate::ROW_HEIGHT))
    }

    /// The first line in view, and how far into it the view starts.
    pub fn first(&self, wrap: CellWrap) -> (usize, Pixels) {
        match wrap {
            CellWrap::Wrap => {
                let top = self.measured.logical_scroll_top();
                (top.item_ix, top.offset_in_item)
            }
            CellWrap::Truncate => {
                let line = self.cut_row_height();
                let down = (-self.uniform.0.borrow().base_handle.offset().y).max(Pixels::ZERO);
                let first = (down / line).floor();
                (first as usize, down - line * first)
            }
        }
    }

    /// Scrolls so `line` is first in view, `offset` into it.
    pub fn scroll_to(&self, wrap: CellWrap, line: usize, offset: Pixels) {
        match wrap {
            CellWrap::Wrap => {
                self.sync(self.count.get());
                self.measured.scroll_to(gpui::ListOffset {
                    item_ix: line,
                    offset_in_item: offset,
                });
            }
            CellWrap::Truncate => {
                let y = self.cut_row_height() * line as f32 + offset;
                let state = self.uniform.0.borrow();
                let x = state.base_handle.offset().x;
                state.base_handle.set_offset(gpui::point(x, -y));
            }
        }
    }

    /// Scrolls as little as shows all of `line`.
    pub fn reveal(&self, wrap: CellWrap, line: usize) {
        match wrap {
            CellWrap::Wrap => self.measured.scroll_to_reveal_item(line),
            CellWrap::Truncate => self
                .uniform
                .scroll_to_item(line, gpui::ScrollStrategy::Nearest),
        }
    }

    /// Keeps the first line in view first when the grid turns from `from` to the other way of
    /// drawing its rows (constitution V).
    pub fn switch(&self, from: CellWrap) {
        let (line, offset) = self.first(from);
        let to = from.toggled();
        // A line's height differs between the two; its start stays.
        let offset = if to == CellWrap::Wrap {
            Pixels::ZERO
        } else {
            offset.min(self.cut_row_height())
        };
        self.scroll_to(to, line, offset);
    }

    /// The line under `position`, as the rows were last laid out and are scrolled now; `None`
    /// off the rows.
    pub fn line_at(&self, wrap: CellWrap, position: gpui::Point<Pixels>) -> Option<usize> {
        match wrap {
            CellWrap::Wrap => {
                let view = self.measured.viewport_bounds();
                if !view.contains(&position) {
                    return None;
                }
                let top = self.measured.logical_scroll_top().item_ix;
                (top..self.measured.item_count())
                    .map_while(|line| Some((line, self.measured.bounds_for_item(line)?)))
                    .take_while(|(_, bounds)| bounds.top() < view.bottom())
                    .find(|(_, bounds)| bounds.top() <= position.y && position.y < bounds.bottom())
                    .map(|(line, _)| line)
            }
            CellWrap::Truncate => {
                let (rows, offset) = {
                    let state = self.uniform.0.borrow();
                    (state.base_handle.bounds(), state.base_handle.offset())
                };
                if !rows.contains(&position) {
                    return None;
                }
                let line = (position.y - rows.origin.y - offset.y) / self.cut_row_height();
                (line >= 0.).then_some(line as usize)
            }
        }
    }
}

/// How wide a column fitted to text `widest` wide is: the text, rounded up, with a cell's inset
/// on both sides, within the bounds every column keeps.
pub fn fitted_width(widest: Pixels) -> f32 {
    clamp_width(f32::from(widest).ceil() + 2. * crate::ds::space::LG)
}

/// The widest of `texts` as cells draw them, measuring at most [`FIT_ROWS`]: what a fit is
/// fitted to.
pub fn widest_text(texts: impl IntoIterator<Item = CellText>, window: &gpui::Window) -> Pixels {
    texts
        .into_iter()
        .take(FIT_ROWS)
        .map(|text| text.line_width(window))
        .fold(Pixels::ZERO, Pixels::max)
}

/// How many lines a wrapped row takes: as many as its text cell that takes the most, each
/// wrapped at its room. A grid counts every column's, drawn or not, so a row keeps its height
/// while it is scrolled sideways, and keeps the count until a width or the wrap changes.
pub fn row_lines(
    cells: impl IntoIterator<Item = (CellText, Pixels)>,
    window: &Window,
    cx: &App,
) -> usize {
    cells
        .into_iter()
        .map(|(text, room)| text.wrapped_lines(room, window, cx))
        .max()
        .unwrap_or(1)
}

/// How tall a row is whose tallest cell takes `lines` lines of `line_height`, in a grid whose
/// cut rows are `row` tall: a cut row, and a line more for each line more, up to
/// [`WRAP_LINES`]. Its first line sits where a cut row's one line does, so turning the wrap on
/// moves no row of one line.
pub fn wrapped_row_height(lines: usize, line_height: Pixels, row: Pixels) -> Pixels {
    let more = lines.clamp(1, WRAP_LINES) - 1;
    row + line_height * more as f32
}

/// The height of a line of a cell's text ([`crate::cell_text`]) where it is drawn.
pub fn cell_line_height(window: &Window, cx: &App) -> Pixels {
    use gpui::Refineable as _;

    let mut text_style = window.text_style();
    text_style.refine(&crate::cell_text("", cx).style);
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    crate::ds::cell_text::line_height(&text_style, font_size, window)
}

/// `cell` made to hold wrapped text: as tall as its lines, the first where a cut cell's one
/// line is, stretched to its row's height so a fill covers it. `cut` is how tall the grid's cut
/// cells are, so a row of one line is as tall wrapped as cut ([`wrapped_row_height`]).
pub fn wrapping_cell(cell: Div, line_height: Pixels, cut: Pixels) -> Div {
    let room = cut;
    let inset = ((room - line_height) / 2.).max(Pixels::ZERO);
    cell.h_auto()
        .min_h(room)
        .items_start()
        .py(inset)
        .self_stretch()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{COLUMN_MAX_WIDTH, COLUMN_MIN_WIDTH};

    fn layout(widths: &[(&str, u32)], wrap: Option<CellWrap>) -> GridLayout {
        GridLayout {
            widths: widths
                .iter()
                .map(|(key, width)| (key.to_string(), *width))
                .collect(),
            wrap,
        }
    }

    /// A width the person gives is held within the bounds, in whole pixels, and wins over
    /// the grid's own until it is given back.
    #[test]
    fn a_width_is_clamped_and_wins_until_given_back() {
        let mut columns = GridColumns::default();
        assert_eq!(columns.width("title", 200.), 200.);
        assert!(columns.set_width("title", Some(320.4)));
        assert_eq!(columns.width("title", 200.), 320.);
        assert!(!columns.set_width("title", Some(320.)), "unchanged");
        columns.set_width("title", Some(3.));
        assert_eq!(columns.width("title", 200.), COLUMN_MIN_WIDTH);
        columns.set_width("title", Some(90_000.));
        assert_eq!(columns.width("title", 200.), COLUMN_MAX_WIDTH);
        assert!(columns.set_width("title", None));
        assert_eq!(columns.width("title", 200.), 200.);
        assert!(columns.layout().is_default());
    }

    /// A width read back from settings or a tab is held within the bounds too, and a wrap
    /// that is the grid's own is no choice.
    #[test]
    fn a_kept_layout_is_held_within_bounds() {
        let columns = GridColumns::new(
            layout(
                &[("a", 1), ("b", 99_999), ("c", 250)],
                Some(CellWrap::Truncate),
            ),
            CellWrap::Truncate,
        );
        assert_eq!(
            columns.layout(),
            &layout(
                &[
                    ("a", COLUMN_MIN_WIDTH as u32),
                    ("b", COLUMN_MAX_WIDTH as u32),
                    ("c", 250)
                ],
                None
            )
        );
    }

    /// Widen and Narrow step from the width shown, within the bounds.
    #[test]
    fn steps_start_from_the_width_shown() {
        let mut columns = GridColumns::default();
        columns.step_width("title", 200., 1);
        assert_eq!(columns.width("title", 200.), 200. + COLUMN_STEP);
        columns.step_width("title", 200., -2);
        assert_eq!(columns.width("title", 200.), 200. - COLUMN_STEP);
        for _ in 0..100 {
            columns.step_width("title", 200., -1);
        }
        assert_eq!(columns.width("title", 200.), COLUMN_MIN_WIDTH);
        assert!(!columns.step_width("title", 200., -1), "held at the least");
    }

    /// A fit is the widest text with the cell's inset, rounded up, within the bounds.
    #[test]
    fn a_fit_is_the_widest_text_and_its_inset() {
        let inset = 2. * crate::ds::space::LG;
        let mut columns = GridColumns::default();
        assert!(columns.fit("title", px(100.2)));
        assert_eq!(columns.width("title", 0.), 101. + inset);
        columns.fit("title", px(0.));
        assert_eq!(columns.width("title", 0.), COLUMN_MIN_WIDTH);
        columns.fit("title", px(10_000.));
        assert_eq!(columns.width("title", 0.), COLUMN_MAX_WIDTH);
    }

    /// The wrap toggles over the grid's own, which is kept as no choice; Reset forgets the
    /// widths and the wrap together.
    #[test]
    fn the_wrap_toggles_and_reset_forgets_everything() {
        let mut columns = GridColumns::new(GridLayout::default(), CellWrap::Truncate);
        assert_eq!(columns.wrap(), CellWrap::Truncate);
        columns.toggle_wrap();
        assert_eq!(columns.wrap(), CellWrap::Wrap);
        assert_eq!(columns.layout().wrap, Some(CellWrap::Wrap));
        columns.toggle_wrap();
        assert_eq!(columns.layout().wrap, None, "the grid's own is no choice");
        // A grid that wraps of its own keeps "truncate" as the choice.
        let mut wrapping = GridColumns::new(GridLayout::default(), CellWrap::Wrap);
        wrapping.toggle_wrap();
        assert_eq!(wrapping.layout().wrap, Some(CellWrap::Truncate));

        columns.toggle_wrap();
        columns.set_width("title", Some(300.));
        assert!(columns.reset());
        assert!(columns.layout().is_default());
        assert_eq!(columns.wrap(), CellWrap::Truncate);
        assert!(!columns.reset(), "nothing left to forget");
    }

    /// Every change goes through one door: a change to a column with none named does nothing,
    /// and the grid-wide ones need no column.
    #[test]
    fn changes_name_their_column_or_the_grid() {
        let mut columns = GridColumns::default();
        let column = Some(("title", 200.));
        let unmeasured = || -> Pixels { panic!("only a fit measures") };
        assert!(!columns.change(ColumnChange::Widen, None, unmeasured));
        assert!(columns.change(ColumnChange::Widen, column, unmeasured));
        assert_eq!(columns.width("title", 200.), 200. + COLUMN_STEP);
        assert!(columns.change(ColumnChange::Narrow, column, unmeasured));
        assert_eq!(columns.width("title", 200.), 200.);
        assert!(columns.change(ColumnChange::Fit, column, || px(300.)));
        assert_eq!(columns.width("title", 200.), fitted_width(px(300.)));
        assert!(columns.change(ColumnChange::ResetWidth, column, unmeasured));
        assert!(columns.layout().is_default());
        assert!(columns.change(ColumnChange::ToggleWrap, None, unmeasured));
        assert_eq!(columns.wrap(), CellWrap::Wrap);
        assert!(columns.change(ColumnChange::Reset, None, unmeasured));
        assert_eq!(columns.wrap(), CellWrap::Truncate);
        assert!(ColumnChange::Fit.is_to_a_column() && !ColumnChange::Reset.is_to_a_column());
    }

    /// A row of one line is a row's height; more lines grow it by a line each, up to the most
    /// a cell shows.
    #[test]
    fn a_wrapped_row_grows_by_its_lines_up_to_the_most() {
        let line = px(18.);
        let row = px(crate::ROW_HEIGHT);
        assert_eq!(wrapped_row_height(0, line, row), row);
        assert_eq!(wrapped_row_height(1, line, row), row);
        assert_eq!(wrapped_row_height(2, line, row), row + line);
        assert_eq!(wrapped_row_height(3, line, row), row + line * 2.);
        assert_eq!(
            wrapped_row_height(WRAP_LINES + 10, line, row),
            row + line * (WRAP_LINES - 1) as f32
        );
    }

    /// Drawn, a wrapped cell is as tall as its lines: one line a row, a sentence in a narrow
    /// column a row and a line more for each line more, and a long text the most lines, no more.
    /// Cut, every cell is a row.
    #[gpui::test]
    fn a_wrapped_cell_is_as_tall_as_its_lines(cx: &mut gpui::TestAppContext) {
        use gpui::{InteractiveElement as _, ParentElement as _, div};
        use ui::FluentBuilder as _;

        const SENTENCE: &str = "a value long enough to need three or four lines here";
        struct Probe;
        impl gpui::Render for Probe {
            fn render(
                &mut self,
                window: &mut Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let line = cell_line_height(window, cx);
                let width = px(160.);
                let cell = |name: &'static str, text: String, wrap: CellWrap| {
                    // In a row with the rule a grid's rows have below them.
                    let cell = crate::grid_cell(width, false);
                    let cell = match wrap {
                        CellWrap::Wrap => wrapping_cell(
                            cell,
                            line,
                            px(crate::ROW_HEIGHT - crate::ds::size::HAIRLINE),
                        )
                        .child(crate::cell_text(text, cx).wrap(crate::cell_room(width))),
                        CellWrap::Truncate => cell.child(crate::cell_text(text, cx)),
                    };
                    div()
                        .flex()
                        .border_b_1()
                        .debug_selector(move || name.into())
                        .when(wrap == CellWrap::Truncate, |row| {
                            row.h(px(crate::ROW_HEIGHT))
                        })
                        .child(cell)
                };
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .child(cell("short", "Ada".into(), CellWrap::Wrap))
                    .child(cell("sentence", SENTENCE.into(), CellWrap::Wrap))
                    .child(cell("long", SENTENCE.repeat(20), CellWrap::Wrap))
                    .child(cell("cut", SENTENCE.repeat(20), CellWrap::Truncate))
            }
        }
        cx.update(studio_theme::init_test);
        let (_, cx) = cx.add_window_view(|_, _| Probe);
        let line = cx.update(|window, cx| cell_line_height(window, cx));
        let mut height = |name: &'static str| cx.debug_bounds(name).expect("drawn").size.height;
        // Layout puts edges on whole pixels, so a height is the formula's within one. The
        // probe's rows are the query results' kind: a row high, its rule within it.
        let row = px(crate::ROW_HEIGHT);
        let near = |drawn: Pixels, lines: usize| {
            (drawn - wrapped_row_height(lines, line, row)).abs() < px(1.)
        };
        assert!(line > px(0.));
        assert!(near(height("short"), 1), "{:?}", height("short"));
        let sentence = height("sentence");
        let lines = (1..=WRAP_LINES).find(|&lines| near(sentence, lines));
        assert!(
            lines.is_some_and(|lines| (2..WRAP_LINES).contains(&lines)),
            "{sentence:?} at a line of {line:?}"
        );
        assert!(near(height("long"), WRAP_LINES), "{:?}", height("long"));
        assert_eq!(height("cut"), px(crate::ROW_HEIGHT));
    }
}
