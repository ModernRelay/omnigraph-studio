//! A Cards tab's grid (spec 048): columns of one width and cards of one size, placed in row
//! order, left to right, then down. Card `i` of `c` columns is in line `i ÷ c`, column
//! `i mod c`: a card's place is its index alone, so nothing is measured before it is placed and
//! no card moves when later rows are read. Pure: no GPUI, no reads. Lengths are in pixels.

use std::ops::Range;

/// A cover's shape, height over width: a box 4 wide by 3 high across the card.
pub const COVER_RATIO: f32 = 3. / 4.;

/// One arrow key's move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Left,
    Right,
    Up,
    Down,
}

/// The columns of a grid and the one size of its cards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardGrid {
    columns: usize,
    width: f32,
    height: f32,
    gap: f32,
}

impl CardGrid {
    /// As many columns of at least `at_least` as fit `inner` (the tab's width less its edges),
    /// `gap` apart, each grown so they fill it; one column when `inner` is narrower than a
    /// card. `height` gives a card's height for its width.
    pub fn fit(inner: f32, at_least: f32, gap: f32, height: impl FnOnce(f32) -> f32) -> Self {
        let inner = inner.max(at_least);
        let columns = ((inner + gap) / (at_least + gap)).floor().max(1.) as usize;
        let width = (inner - gap * (columns as f32 - 1.)) / columns as f32;
        Self {
            columns,
            width,
            height: height(width),
            gap,
        }
    }

    #[cfg(test)]
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// A card's width.
    pub fn width(&self) -> f32 {
        self.width
    }

    /// A card's height: every card's.
    pub fn card_height(&self) -> f32 {
        self.height
    }

    /// From one line's top to the next.
    fn pitch(&self) -> f32 {
        self.height + self.gap
    }

    /// The line card `card` is in: `card ÷ columns`.
    pub fn line_of(&self, card: usize) -> usize {
        card / self.columns
    }

    /// The column card `card` is in: `card mod columns`.
    pub fn column_of(&self, card: usize) -> usize {
        card % self.columns
    }

    /// Where card `card` is, from the grid's top-left: its left and its top.
    pub fn place_of(&self, card: usize) -> (f32, f32) {
        (
            self.column_of(card) as f32 * (self.width + self.gap),
            self.line_of(card) as f32 * self.pitch(),
        )
    }

    /// The middle of card `card`'s line, down the grid.
    pub fn middle_of(&self, card: usize) -> f32 {
        self.place_of(card).1 + self.height / 2.
    }

    /// How many lines `cards` cards take.
    pub fn lines(&self, cards: usize) -> usize {
        cards.div_ceil(self.columns)
    }

    /// How tall `cards` cards are: their lines and the gaps between them.
    pub fn height(&self, cards: usize) -> f32 {
        match self.lines(cards) {
            0 => 0.,
            lines => lines as f32 * self.pitch() - self.gap,
        }
    }

    /// The cards, of `cards`, in the lines that meet `span` of the grid's height: found from
    /// the span alone, so it costs nothing a card.
    pub fn meeting(&self, span: Range<f32>, cards: usize) -> Range<usize> {
        if span.end <= span.start || span.end <= 0. {
            return cards..cards;
        }
        let pitch = self.pitch();
        // The first line whose bottom is past the span's start, and the first whose top is at
        // or past its end.
        let first = if span.start < self.height {
            0
        } else {
            ((span.start - self.height) / pitch).floor() as usize + 1
        };
        let end = (span.end / pitch).ceil() as usize;
        let from = first.saturating_mul(self.columns).min(cards);
        let to = end.saturating_mul(self.columns).min(cards);
        from..to.max(from)
    }

    /// Where an arrow takes the cursor from card `from` of `cards`: left and right to the card
    /// before or after in row order, across line ends; up and down by a line. Down onto a last
    /// line too short to hold a card below goes to the last card. At an edge it stays.
    pub fn step(&self, from: usize, step: Step, cards: usize) -> usize {
        let Some(last) = cards.checked_sub(1) else {
            return from;
        };
        let from = from.min(last);
        match step {
            Step::Left => from.saturating_sub(1),
            Step::Right => (from + 1).min(last),
            Step::Up => from.checked_sub(self.columns).unwrap_or(from),
            Step::Down if from + self.columns <= last => from + self.columns,
            Step::Down if self.line_of(from) < self.line_of(last) => last,
            Step::Down => from,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three columns of cards 100 by 80, 10 apart.
    fn grid() -> CardGrid {
        CardGrid::fit(320., 100., 10., |_| 80.)
    }

    /// The columns fill the width: as many of at least the card's as fit, the gaps kept, and
    /// one when the width is narrower than a card. A card's height is asked for its width.
    #[test]
    fn the_columns_fill_the_width() {
        let grid = CardGrid::fit(1168., 260., 12., |width| width / 2.);
        assert_eq!(grid.columns(), 4);
        let used = 4. * grid.width() + 3. * 12.;
        assert!((used - 1168.).abs() < 0.01, "{used}");
        assert!(grid.width() >= 260.);
        assert_eq!(grid.card_height(), grid.width() / 2.);
        // One short of a fifth column: four, each wider.
        assert_eq!(CardGrid::fit(1347., 260., 12., |_| 1.).columns(), 4);
        assert_eq!(CardGrid::fit(1348., 260., 12., |_| 1.).columns(), 5);
        let narrow = CardGrid::fit(100., 260., 12., |_| 1.);
        assert_eq!((narrow.columns(), narrow.width()), (1, 260.));
    }

    /// Card `i` of `c` columns is in line `i ÷ c`, column `i mod c`, whatever else is placed:
    /// a place is the index's alone, so a card read later moves none.
    #[test]
    fn a_card_is_placed_by_its_index_alone() {
        let grid = grid();
        assert_eq!(grid.columns(), 3);
        assert_eq!(grid.width(), 100.);
        for card in 0..40 {
            assert_eq!(grid.line_of(card), card / 3);
            assert_eq!(grid.column_of(card), card % 3);
            let (left, top) = grid.place_of(card);
            assert_eq!(left, (card % 3) as f32 * 110.);
            assert_eq!(top, (card / 3) as f32 * 90.);
        }
        // Every line is one height: 7 cards are 3 lines, the last short.
        assert_eq!(grid.lines(7), 3);
        assert_eq!(grid.height(7), 3. * 80. + 2. * 10.);
        assert_eq!(grid.height(6), 2. * 80. + 10.);
        assert_eq!((grid.lines(0), grid.height(0)), (0, 0.));
        assert_eq!(grid.middle_of(4), 90. + 40.);
    }

    /// The cards of the lines meeting a span are found from the span: a line counts while any
    /// of it is inside, the gap under it does not, and nothing past the cards read.
    #[test]
    fn the_cards_of_the_lines_meeting_a_span_are_found() {
        let grid = grid();
        // Lines at 0-80, 90-170, 180-260, …
        assert_eq!(grid.meeting(0.0..50., 100), 0..3);
        assert_eq!(grid.meeting(0.0..90., 100), 0..3);
        assert_eq!(grid.meeting(0.0..90.5, 100), 0..6);
        assert_eq!(grid.meeting(79.0..181., 100), 0..9);
        assert_eq!(grid.meeting(80.0..180., 100), 3..6, "the gaps meet no line");
        assert_eq!(grid.meeting(85.0..88., 100), 3..3, "inside a gap");
        assert_eq!(grid.meeting(100.0..260., 7), 3..7, "a short last line");
        assert_eq!(grid.meeting(5000.0..6000., 7), 7..7);
        assert_eq!(grid.meeting(-300.0..-10., 7), 7..7);
        assert_eq!(grid.meeting(-300.0..10., 7), 0..3);
        assert_eq!(grid.meeting(0.0..1000., 0), 0..0);
        // What a frame lays out does not grow with what is read.
        assert_eq!(grid.meeting(900.0..1700., 1_000_000).len(), 27);
    }

    /// ← and → go to the card before or after in row order, across line ends; ↑ and ↓ by a
    /// line; ↓ onto a short last line goes to the last card; at an edge the cursor stays.
    #[test]
    fn the_arrows_move_in_row_order_and_by_lines() {
        let grid = grid();
        // 8 cards: lines 0 1 2 / 3 4 5 / 6 7.
        let cards = 8;
        assert_eq!(grid.step(2, Step::Right, cards), 3, "across a line's end");
        assert_eq!(grid.step(3, Step::Left, cards), 2, "and back");
        assert_eq!(grid.step(0, Step::Left, cards), 0);
        assert_eq!(grid.step(7, Step::Right, cards), 7);
        assert_eq!(grid.step(4, Step::Up, cards), 1);
        assert_eq!(
            grid.step(1, Step::Up, cards),
            1,
            "nothing above the first line"
        );
        assert_eq!(grid.step(1, Step::Down, cards), 4);
        assert_eq!(grid.step(4, Step::Down, cards), 7);
        assert_eq!(grid.step(3, Step::Down, cards), 6);
        assert_eq!(
            grid.step(5, Step::Down, cards),
            7,
            "no card below in the short last line: the last card"
        );
        assert_eq!(
            grid.step(6, Step::Down, cards),
            6,
            "nothing below the last line"
        );
        assert_eq!(grid.step(7, Step::Down, cards), 7);
        // A full last line has a card below every card of the line before.
        assert_eq!(grid.step(5, Step::Down, 9), 8);
        // One line: up and down stay.
        assert_eq!(grid.step(1, Step::Down, 3), 1);
        assert_eq!(grid.step(1, Step::Up, 3), 1);
        // No cards, or a cursor past them: held to what there is.
        assert_eq!(grid.step(0, Step::Right, 0), 0);
        assert_eq!(grid.step(40, Step::Left, 8), 6);
    }
}
