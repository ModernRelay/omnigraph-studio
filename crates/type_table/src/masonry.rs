//! A gallery's masonry (spec 035): columns of one width, each tile as tall as its file's shape
//! needs, placed strictly in row order into the shortest column. A tile is placed once its shape
//! is known and every tile before it is placed, so a placed tile never moves when a later shape
//! arrives. Pure: no GPUI, no reads. Lengths are in pixels.

use std::ops::Range;

/// The narrowest and tallest shapes a tile takes, as height over width: a panorama is not drawn
/// as a sliver, nor a long screenshot as a column's whole height. The file is fitted inside.
pub const MIN_RATIO: f32 = 0.4;
pub const MAX_RATIO: f32 = 2.2;

/// Where one tile is: its column and its top, and how tall it is with its caption.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Place {
    pub column: usize,
    pub top: f32,
    pub height: f32,
}

impl Place {
    pub fn bottom(&self) -> f32 {
        self.top + self.height
    }

    pub fn middle(&self) -> f32 {
        self.top + self.height / 2.
    }
}

/// The columns and what is placed in them, for one column count and width.
#[derive(Debug, Clone, PartialEq)]
pub struct Masonry {
    columns: usize,
    width: f32,
    gap: f32,
    /// What a tile adds to its preview's height: its frame and its caption.
    chrome: f32,
    /// By row: the first `places.len()` rows are placed.
    places: Vec<Place>,
    /// Each column's rows, top to bottom.
    by_column: Vec<Vec<usize>>,
    /// Where each column's next tile goes.
    bottoms: Vec<f32>,
}

impl Masonry {
    /// Empty columns: `columns` of `width`, `gap` apart and between tiles; a tile is `chrome`
    /// taller than its preview.
    pub fn new(columns: usize, width: f32, gap: f32, chrome: f32) -> Self {
        let columns = columns.max(1);
        Self {
            columns,
            width,
            gap,
            chrome,
            places: Vec::new(),
            by_column: vec![Vec::new(); columns],
            bottoms: vec![0.; columns],
        }
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    /// Whether these columns are laid out as `columns` of `width`.
    pub fn fits(&self, columns: usize, width: f32) -> bool {
        self.columns == columns.max(1) && (self.width - width).abs() < 0.5
    }

    /// How many rows are placed: rows `0..placed()`.
    pub fn placed(&self) -> usize {
        self.places.len()
    }

    pub fn place_of(&self, row: usize) -> Option<&Place> {
        self.places.get(row)
    }

    /// A preview's height in a column, for a shape (height over width).
    pub fn preview_height(&self, ratio: f32) -> f32 {
        self.width * ratio.clamp(MIN_RATIO, MAX_RATIO)
    }

    /// Places the next row, of shape `ratio`, in the shortest column (the first of equals).
    pub fn place(&mut self, ratio: f32) -> Place {
        let column = (0..self.columns)
            .min_by(|a, b| self.bottoms[*a].total_cmp(&self.bottoms[*b]))
            .unwrap_or(0);
        let place = Place {
            column,
            top: self.bottoms[column],
            height: self.preview_height(ratio) + self.chrome,
        };
        self.bottoms[column] = place.bottom() + self.gap;
        self.by_column[column].push(self.places.len());
        self.places.push(place);
        place
    }

    /// Places rows from the next one on while their shapes are known (`shape(row)`), up to
    /// `rows`; stops at the first row whose shape is not.
    pub fn place_known(&mut self, rows: usize, shape: impl Fn(usize) -> Option<f32>) -> usize {
        let before = self.places.len();
        while self.places.len() < rows {
            let Some(ratio) = shape(self.places.len()) else {
                break;
            };
            self.place(ratio);
        }
        self.places.len() - before
    }

    /// The left edge of a column, inside the grid's edge `inset`.
    pub fn left(&self, column: usize, inset: f32) -> f32 {
        inset + column as f32 * (self.width + self.gap)
    }

    /// The bottom of the tallest column's last tile.
    pub fn height(&self) -> f32 {
        self.by_column
            .iter()
            .filter_map(|column| column.last())
            .map(|&row| self.places[row].bottom())
            .fold(0., f32::max)
    }

    /// Where each column's next tile would go.
    pub fn bottoms(&self) -> &[f32] {
        &self.bottoms
    }

    /// The bottom of the shortest column: where the next row goes.
    pub fn shortest(&self) -> f32 {
        self.bottoms.iter().copied().fold(f32::INFINITY, f32::min)
    }

    /// The rows whose tiles meet `top..bottom`, in row order: a search in each column, so it
    /// costs what is shown, not what is placed.
    pub fn meeting(&self, span: Range<f32>) -> Vec<usize> {
        let mut rows: Vec<usize> = self
            .by_column
            .iter()
            .flat_map(|column| {
                let first = column.partition_point(|&row| self.places[row].bottom() <= span.start);
                column[first..]
                    .iter()
                    .copied()
                    .take_while(|&row| self.places[row].top < span.end)
            })
            .collect();
        rows.sort_unstable();
        rows
    }

    /// The tile above or below `row` in its column (`by` −1 or 1).
    pub fn vertical(&self, row: usize, by: isize) -> Option<usize> {
        let place = self.places.get(row)?;
        let column = &self.by_column[place.column];
        let at = column.iter().position(|&r| r == row)?;
        let next = at.checked_add_signed(by)?;
        column.get(next).copied()
    }

    /// The tile in the next column left or right (`by` −1 or 1) whose vertical middle is
    /// nearest `row`'s, the higher of two as near.
    pub fn across(&self, row: usize, by: isize) -> Option<usize> {
        let place = self.places.get(row)?;
        let column = place.column.checked_add_signed(by)?;
        let middle = place.middle();
        self.by_column.get(column)?.iter().copied().min_by(|a, b| {
            let da = (self.places[*a].middle() - middle).abs();
            let db = (self.places[*b].middle() - middle).abs();
            da.total_cmp(&db).then(a.cmp(b))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three columns 100 wide, 10 apart, a tile 20 taller than its preview.
    fn masonry() -> Masonry {
        Masonry::new(3, 100., 10., 20.)
    }

    /// Rows go in row order into the shortest column, the first of equals.
    #[test]
    fn rows_go_into_the_shortest_column_in_their_order() {
        let mut m = masonry();
        // Tall, square, wide: then the wide column is shortest, then the square one.
        for ratio in [2., 1., 0.5, 1., 1.] {
            m.place(ratio);
        }
        let columns: Vec<usize> = (0..5).map(|row| m.place_of(row).unwrap().column).collect();
        assert_eq!(columns, [0, 1, 2, 2, 1]);
        assert_eq!(m.place_of(0).unwrap().height, 220.);
        assert_eq!(m.place_of(3).unwrap().top, 80.);
        assert_eq!(m.height(), 250.);
        // A shape past the bounds is held to them.
        assert_eq!(m.preview_height(10.), 100. * MAX_RATIO);
        assert_eq!(m.preview_height(0.01), 100. * MIN_RATIO);
    }

    /// A placed tile keeps its place when later shapes arrive: placing stops at the first row
    /// whose shape is unknown, and placing the rest leaves the earlier places as they were, the
    /// same as placing them all at once.
    #[test]
    fn a_placed_tile_keeps_its_place_when_later_shapes_arrive() {
        let shapes = [1.5, 0.75, 1., 2., 0.6, 1.2, 0.9];
        let mut known: Vec<Option<f32>> = shapes.iter().map(|&s| Some(s)).collect();
        known[3] = None;
        let mut m = masonry();
        assert_eq!(m.place_known(shapes.len(), |row| known[row]), 3);
        let early: Vec<Place> = (0..3).map(|row| *m.place_of(row).unwrap()).collect();
        // Shapes 4 to 6 landed first, then 3: nothing past 3 was placed meanwhile.
        assert_eq!(m.place_known(shapes.len(), |row| known[row]), 0);
        known[3] = Some(shapes[3]);
        assert_eq!(m.place_known(shapes.len(), |row| known[row]), 4);
        for (row, place) in early.iter().enumerate() {
            assert_eq!(m.place_of(row), Some(place), "row {row} moved");
        }
        let mut all = masonry();
        all.place_known(shapes.len(), |row| Some(shapes[row]));
        assert_eq!(m, all);
    }

    /// A tile is shown while it meets the span, whatever its column.
    #[test]
    fn the_tiles_meeting_a_span_are_found_in_each_column() {
        let mut m = Masonry::new(2, 100., 10., 0.);
        for _ in 0..20 {
            m.place(1.);
        }
        // Rows 2k and 2k+1 sit side by side at 110k.
        assert_eq!(m.meeting(0.0..50.), [0, 1]);
        assert_eq!(m.meeting(105.0..225.), [2, 3, 4, 5]);
        assert!(m.meeting(5000.0..6000.).is_empty());
    }

    /// Up and down stay in a column; left and right go to the next column's tile whose middle
    /// is nearest, and nowhere past the edge.
    #[test]
    fn the_arrows_move_within_and_across_columns() {
        let mut m = masonry();
        for ratio in [2., 1., 0.5, 1., 1.] {
            m.place(ratio);
        }
        // Columns: 0 holds row 0 (0..220); 1 rows 1 (0..120) and 4 (130..250); 2 rows 2
        // (0..70) and 3 (80..200).
        assert_eq!(m.vertical(1, 1), Some(4));
        assert_eq!(m.vertical(4, -1), Some(1));
        assert_eq!(m.vertical(0, 1), None);
        assert_eq!(m.vertical(0, -1), None);
        // Row 0's middle is 110: row 1 (middle 60) and row 4 (190) — row 1 is nearer.
        assert_eq!(m.across(0, 1), Some(1));
        // Row 4's middle is 190: in column 2, row 3 (middle 140) is nearer than row 2 (35).
        assert_eq!(m.across(4, 1), Some(3));
        assert_eq!(m.across(3, -1), Some(4));
        assert_eq!(m.across(0, -1), None);
        assert_eq!(m.across(2, 1), None);
    }
}
