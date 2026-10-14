//! What a field offers while it is typed in (spec 004): the list, ↑/↓ moving its highlight
//! around the ends, and its rows. The filter's value field and the edge dialog's endpoint
//! fields share it; each draws its own container.

use std::rc::Rc;

use gpui::{App, Div, MouseButton, Stateful, Window};
use ui::prelude::*;

use crate::suggest::Suggestion;

/// What runs when an offer is pressed, with its index.
pub type OnPick = Rc<dyn Fn(usize, &mut Window, &mut App)>;

#[derive(Debug, Default)]
pub struct Offers {
    items: Vec<Suggestion>,
    highlighted: Option<usize>,
}

impl Offers {
    /// Offers `items`, with the first highlighted when `highlight`, so ↵ picks it.
    pub fn set(&mut self, items: Vec<Suggestion>, highlight: bool) {
        self.highlighted = (highlight && !items.is_empty()).then_some(0);
        self.items = items;
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.highlighted = None;
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn get(&self, index: usize) -> Option<&Suggestion> {
        self.items.get(index)
    }

    pub fn highlighted(&self) -> Option<usize> {
        self.highlighted
    }

    /// ↑/↓: moves the highlight by `step` around the ends; with none highlighted, ↓ takes the
    /// first and ↑ the last. False when there is nothing to move to.
    pub fn step(&mut self, step: isize) -> bool {
        let count = self.items.len();
        if count == 0 {
            return false;
        }
        self.highlighted = Some(match self.highlighted {
            None if step > 0 => 0,
            None => count - 1,
            Some(at) => (at as isize + step).rem_euclid(count as isize) as usize,
        });
        true
    }

    /// The rows: a node by its label with its id after it, a value as itself. `on_pick` runs
    /// on press, before the field loses focus to the click.
    pub fn rows(&self, id: &'static str, on_pick: OnPick, cx: &App) -> Vec<Stateful<Div>> {
        let highlight = cx.theme().colors().element_selected;
        self.items
            .iter()
            .enumerate()
            .map(|(index, offer)| {
                let (primary, secondary) = match &offer.label {
                    Some(label) => (label.clone(), Some(offer.value.clone())),
                    None => (offer.value.clone(), None),
                };
                let on_pick = on_pick.clone();
                h_flex()
                    .id((id, index))
                    .h(px(studio_ui::ds::size::SIDEBAR_ROW))
                    .px(px(studio_ui::ds::space::MD))
                    .gap(px(studio_ui::ds::space::MD))
                    // A row of a `ds::row_list`, which is concentric around this radius.
                    .rounded(px(studio_ui::ds::radius::XS))
                    .cursor_pointer()
                    .when(self.highlighted == Some(index), |row| row.bg(highlight))
                    .hover(|row| row.bg(highlight))
                    .child(
                        div().min_w_0().flex_shrink(1.).child(
                            Label::new(primary)
                                .size(LabelSize::Small)
                                .single_line()
                                .truncate(),
                        ),
                    )
                    .children(secondary.map(|id| {
                        div().min_w_0().flex_shrink(1.).child(
                            Label::new(id)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line()
                                .truncate_start(),
                        )
                    }))
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        cx.stop_propagation();
                        on_pick(index, window, cx);
                    })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(names: &[&str]) -> Vec<Suggestion> {
        names
            .iter()
            .map(|name| Suggestion {
                value: (*name).to_owned(),
                label: None,
            })
            .collect()
    }

    #[test]
    fn the_highlight_moves_around_the_ends() {
        let mut offers = Offers::default();
        assert!(!offers.step(1), "nothing to move to");
        offers.set(values(&["a", "b", "c"]), false);
        assert_eq!(offers.highlighted(), None);
        offers.step(-1);
        assert_eq!(offers.highlighted(), Some(2), "↑ from none takes the last");
        offers.step(1);
        assert_eq!(offers.highlighted(), Some(0), "↓ past the last wraps");
        offers.set(values(&["x"]), true);
        assert_eq!(offers.highlighted(), Some(0));
        offers.set(Vec::new(), true);
        assert_eq!(offers.highlighted(), None);
        offers.set(values(&["a"]), true);
        offers.clear();
        assert!(offers.is_empty() && offers.highlighted().is_none());
    }
}
