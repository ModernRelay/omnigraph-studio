//! Which columns of a wide grid a frame draws (`docs/drawing.md`). A line lays out each cell it
//! draws, so a grid wider than its view would otherwise pay for every column on every scroll.
//! A frame draws the columns in view, one either side; each run of the others is one empty gap
//! as wide as they are, so the line keeps its width and the grid scrolls as far as before.

use std::ops::Range;

use gpui::Pixels;

/// One place across a line as a frame draws it: a column, or the columns between two drawn
/// ones, which a line leaves as one empty space at their width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridPlace {
    Column(usize),
    Gap(Pixels),
}

/// The columns whose span meets `shown`, laid side by side from 0 at `widths`, and one either
/// side of those, so a quick scroll shows no empty column.
pub fn columns_in_view(widths: &[Pixels], shown: Range<Pixels>) -> Vec<bool> {
    let mut left = Pixels::ZERO;
    let in_view: Vec<bool> = widths
        .iter()
        .map(|&width| {
            let right = left + width;
            let meets = right > shown.start && left < shown.end;
            left = right;
            meets
        })
        .collect();
    (0..in_view.len())
        .map(|col| {
            in_view[col]
                || in_view.get(col + 1).copied().unwrap_or(false)
                || (col > 0 && in_view[col - 1])
        })
        .collect()
}

/// The places a frame draws, from the columns it draws and every column's width.
pub fn grid_places(drawn: &[bool], widths: &[Pixels]) -> Vec<GridPlace> {
    let mut places = Vec::new();
    for (col, (&drawn, &width)) in drawn.iter().zip(widths).enumerate() {
        match places.last_mut() {
            _ if drawn => places.push(GridPlace::Column(col)),
            Some(GridPlace::Gap(gap)) => *gap += width,
            _ => places.push(GridPlace::Gap(width)),
        }
    }
    places
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;

    #[test]
    fn the_columns_a_frame_leaves_out_are_one_gap_per_run() {
        let widths: Vec<Pixels> = [10., 20., 30., 40., 50., 60.].map(px).to_vec();
        assert_eq!(
            grid_places(&[true, false, false, true, false, false], &widths),
            vec![
                GridPlace::Column(0),
                GridPlace::Gap(px(50.)),
                GridPlace::Column(3),
                GridPlace::Gap(px(110.)),
            ]
        );
        assert_eq!(
            grid_places(&[true, true], &widths[..2]),
            vec![GridPlace::Column(0), GridPlace::Column(1)]
        );
    }

    #[test]
    fn a_frame_draws_the_columns_in_view_and_one_either_side() {
        // Columns at 0-100, 100-200, ... 900-1000; the view shows 350 to 550.
        let widths = vec![px(100.); 10];
        let drawn = columns_in_view(&widths, px(350.)..px(550.));
        let drawn: Vec<usize> = (0..10).filter(|&col| drawn[col]).collect();
        assert_eq!(drawn, vec![2, 3, 4, 5, 6]);
        // At the start, the first columns and one after.
        let drawn = columns_in_view(&widths, px(0.)..px(150.));
        assert_eq!(
            (0..10).filter(|&col| drawn[col]).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }
}
