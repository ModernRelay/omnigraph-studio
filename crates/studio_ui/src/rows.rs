//! A virtualized list's rows (constitution IX): a view keeps the rows it last drew as plain
//! descriptors, and tells GPUI's `ListState` only which of them changed, so the list keeps its
//! scroll position and measured heights for the rest.

use std::ops::Range;

use gpui::ListState;

/// Replaces `rows` with `next`, splicing `state` over the rows that differ. Nothing changes
/// when they are equal.
pub fn sync<T: PartialEq>(state: &ListState, rows: &mut Vec<T>, next: Vec<T>) {
    if let Some((old, count)) = changed(rows, &next) {
        state.splice(old, count);
    }
    *rows = next;
}

/// The range of `old` that `new` replaces, and how many rows replace it: what lies between
/// the rows both start with and the rows both end with. `None` when they are equal.
fn changed<T: PartialEq>(old: &[T], new: &[T]) -> Option<(Range<usize>, usize)> {
    let start = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    if start == old.len() && start == new.len() {
        return None;
    }
    let end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    Some((start..old.len() - end, new.len() - start - end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_rows_between_a_shared_start_and_end_change() {
        assert_eq!(changed(&[1, 2, 3], &[1, 2, 3]), None);
        assert_eq!(changed(&[1, 2, 3], &[1, 9, 3]), Some((1..2, 1)));
        assert_eq!(
            changed(&[1, 2, 3], &[0, 1, 2, 3]),
            Some((0..0, 1)),
            "one added first"
        );
        assert_eq!(changed(&[1, 2, 3], &[1, 3]), Some((1..2, 0)), "one removed");
        assert_eq!(
            changed(&[1, 2], &[1, 2, 3, 4]),
            Some((2..2, 2)),
            "two added last"
        );
        assert_eq!(changed::<i32>(&[], &[1]), Some((0..0, 1)));
        assert_eq!(changed(&[1, 1], &[1]), Some((1..2, 0)), "repeated rows");
    }
}
