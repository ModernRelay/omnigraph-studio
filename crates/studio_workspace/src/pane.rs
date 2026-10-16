//! Panes and the split tree of the document area (spec 002, "Frame"), after Zed's `Pane` and
//! `PaneGroup` at `v1.23.2`: a pane is a row of tabs with one active; pinned tabs come first and
//! are kept by "close others"; one tab may be a preview, which the next preview replaces in
//! place until it is kept. Each pane keeps its own back and forward trail, as Zed's
//! `NavHistory` does. Panes sit in a tree of horizontal and vertical splits.
//!
//! The logic here is plain data, generic over the tab type, so it is tested without a window.

use gpui::EntityId;
use serde::{Deserialize, Serialize};

pub type PaneId = usize;

/// How many tabs a pane's back trail remembers.
const HISTORY: usize = 100;

/// What a pane needs from a tab.
pub trait Tab {
    fn tab_id(&self) -> EntityId;
}

pub struct Pane<T> {
    pub id: PaneId,
    pub items: Vec<T>,
    pub active: usize,
    /// The first `pinned` items are pinned.
    pub pinned: usize,
    /// The preview tab, replaced by the next preview opened in this pane.
    pub preview: Option<EntityId>,
    /// Tabs this pane showed before its active one, most recent last (Back), and those
    /// stepped back from (Forward). A tab closed since is skipped when stepping.
    pub back: Vec<EntityId>,
    pub forward: Vec<EntityId>,
}

impl<T: Tab> Pane<T> {
    pub fn new(id: PaneId) -> Self {
        Self {
            id,
            items: Vec::new(),
            active: 0,
            pinned: 0,
            preview: None,
            back: Vec::new(),
            forward: Vec::new(),
        }
    }

    /// Notes that the pane leaves the tab `left` for another: Back returns to it, and the
    /// Forward trail is forgotten.
    pub fn record(&mut self, left: EntityId) {
        self.back.retain(|visited| *visited != left);
        self.back.push(left);
        if self.back.len() > HISTORY {
            self.back.remove(0);
        }
        self.forward.clear();
    }

    /// Whether Back (or Forward) has a tab of this pane to go to.
    pub fn can_step(&self, back: bool) -> bool {
        let active = self.active_item().map(Tab::tab_id);
        let trail = if back { &self.back } else { &self.forward };
        trail
            .iter()
            .any(|id| Some(*id) != active && self.position(*id).is_some())
    }

    /// Steps back (or forward): the index of the tab to show, with the active tab moved to the
    /// other trail; tabs closed since are dropped. `None` when there is nowhere to go.
    pub fn step(&mut self, back: bool) -> Option<usize> {
        let active = self.active_item().map(Tab::tab_id);
        loop {
            let next = if back {
                self.back.pop()
            } else {
                self.forward.pop()
            }?;
            let Some(index) = self.position(next) else {
                continue;
            };
            if Some(next) == active {
                continue;
            }
            if let Some(active) = active {
                if back {
                    self.forward.push(active);
                } else {
                    self.back.push(active);
                }
            }
            return Some(index);
        }
    }

    /// Forgets a tab that will not come back (a preview replaced in place).
    pub fn forget(&mut self, id: EntityId) {
        self.back.retain(|visited| *visited != id);
        self.forward.retain(|visited| *visited != id);
    }

    pub fn position(&self, id: EntityId) -> Option<usize> {
        self.items.iter().position(|item| item.tab_id() == id)
    }

    pub fn active_item(&self) -> Option<&T> {
        self.items.get(self.active)
    }

    pub fn is_pinned(&self, index: usize) -> bool {
        index < self.pinned
    }

    pub fn is_preview(&self, index: usize) -> bool {
        self.preview.is_some() && self.items.get(index).map(Tab::tab_id) == self.preview
    }

    /// Adds `item` and returns its index. A preview takes the place of the current preview tab,
    /// which is returned to be dropped.
    pub fn add(&mut self, item: T, preview: bool) -> (usize, Option<T>) {
        let id = item.tab_id();
        if preview && let Some(index) = self.preview.and_then(|p| self.position(p)) {
            let replaced = std::mem::replace(&mut self.items[index], item);
            self.preview = Some(id);
            return (index, Some(replaced));
        }
        self.items.push(item);
        if preview {
            self.preview = Some(id);
        }
        (self.items.len() - 1, None)
    }

    /// Keeps a preview tab: the next preview opens beside it.
    pub fn keep(&mut self, id: EntityId) -> bool {
        if self.preview == Some(id) {
            self.preview = None;
            return true;
        }
        false
    }

    /// Pins or unpins the tab at `index`, moving it to the end of the pinned tabs or the start
    /// of the others. Returns its new index; the active tab stays active.
    pub fn toggle_pin(&mut self, index: usize) -> usize {
        if index >= self.items.len() {
            return index;
        }
        let active = self.active_item().map(Tab::tab_id);
        let item = self.items.remove(index);
        let id = item.tab_id();
        let to = if index < self.pinned {
            self.pinned -= 1;
            self.pinned
        } else {
            self.keep(id);
            self.pinned += 1;
            self.pinned - 1
        };
        self.items.insert(to, item);
        if let Some(active) = active.and_then(|a| self.position(a)) {
            self.active = active;
        }
        to
    }

    /// Removes the tab at `index`. The tab to its right becomes active, else the one to its left.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.items.len() {
            return None;
        }
        let item = self.items.remove(index);
        if index < self.pinned {
            self.pinned -= 1;
        }
        if self.preview == Some(item.tab_id()) {
            self.preview = None;
        }
        if index < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.items.len().saturating_sub(1));
        Some(item)
    }

    /// The unpinned tabs other than the one at `keep`, for "Close others".
    pub fn others(&self, keep: usize) -> Vec<EntityId> {
        self.unpinned(|index| index != keep)
    }

    /// The unpinned tabs right of `index`, for "Close to the right".
    pub fn to_the_right(&self, index: usize) -> Vec<EntityId> {
        self.unpinned(|i| i > index)
    }

    /// Every unpinned tab, for "Close all".
    pub fn all_unpinned(&self) -> Vec<EntityId> {
        self.unpinned(|_| true)
    }

    fn unpinned(&self, take: impl Fn(usize) -> bool) -> Vec<EntityId> {
        self.items
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= self.pinned && take(*index))
            .map(|(_, item)| item.tab_id())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    /// Side by side.
    Horizontal,
    /// One above the other.
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Left,
    Right,
    Up,
    Down,
}

impl SplitDirection {
    fn axis(self) -> SplitAxis {
        match self {
            SplitDirection::Left | SplitDirection::Right => SplitAxis::Horizontal,
            SplitDirection::Up | SplitDirection::Down => SplitAxis::Vertical,
        }
    }

    /// Whether the new pane goes after the one split.
    fn after(self) -> bool {
        matches!(self, SplitDirection::Right | SplitDirection::Down)
    }
}

/// A node of the split tree: a pane, or panes side by side or stacked, each with its share.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Member {
    Pane(PaneId),
    Split {
        axis: SplitAxis,
        members: Vec<Member>,
        flexes: Vec<f32>,
    },
}

impl Member {
    /// The panes in reading order: left to right, top to bottom.
    pub fn panes(&self) -> Vec<PaneId> {
        match self {
            Member::Pane(id) => vec![*id],
            Member::Split { members, .. } => members.iter().flat_map(Member::panes).collect(),
        }
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            Member::Pane(id) => *id == pane,
            Member::Split { members, .. } => members.iter().any(|m| m.contains(pane)),
        }
    }

    /// Puts `new` beside `at`. Returns false when `at` is not in the tree.
    pub fn split(&mut self, at: PaneId, new: PaneId, direction: SplitDirection) -> bool {
        let axis = direction.axis();
        let pair = |at, new| {
            let (first, second) = if direction.after() {
                (at, new)
            } else {
                (new, at)
            };
            Member::Split {
                axis,
                members: vec![Member::Pane(first), Member::Pane(second)],
                flexes: vec![1., 1.],
            }
        };
        match self {
            Member::Pane(id) if *id == at => {
                *self = pair(at, new);
                true
            }
            Member::Pane(_) => false,
            Member::Split {
                axis: own_axis,
                members,
                flexes,
            } => {
                if let Some(index) = members.iter().position(|m| *m == Member::Pane(at)) {
                    if *own_axis == axis {
                        let to = if direction.after() { index + 1 } else { index };
                        // The new pane takes half of the split one's share.
                        let half = flexes[index] / 2.;
                        flexes[index] = half;
                        members.insert(to, Member::Pane(new));
                        flexes.insert(to, half);
                    } else {
                        members[index] = pair(at, new);
                    }
                    return true;
                }
                members.iter_mut().any(|m| m.split(at, new, direction))
            }
        }
    }

    /// Takes `pane` out of the tree; a split left with one member becomes that member. Returns
    /// false for the last pane, which stays.
    pub fn remove(&mut self, pane: PaneId) -> bool {
        let Member::Split {
            members, flexes, ..
        } = self
        else {
            return false;
        };
        let removed = if let Some(index) = members.iter().position(|m| *m == Member::Pane(pane)) {
            members.remove(index);
            flexes.remove(index);
            true
        } else {
            members.iter_mut().any(|m| m.remove(pane))
        };
        if removed && members.len() == 1 {
            *self = members.remove(0);
        }
        removed
    }

    /// The pane at the top right, whose tab strip carries the dock toggles.
    pub fn top_right(&self) -> PaneId {
        match self {
            Member::Pane(id) => *id,
            Member::Split {
                axis: SplitAxis::Horizontal,
                members,
                ..
            } => members.last().map_or(0, Member::top_right),
            Member::Split { members, .. } => members.first().map_or(0, Member::top_right),
        }
    }

    /// The pane at the top left, whose tab strip makes room for the traffic lights.
    pub fn top_left(&self) -> PaneId {
        match self {
            Member::Pane(id) => *id,
            Member::Split { members, .. } => members.first().map_or(0, Member::top_left),
        }
    }

    /// The same tree with each pane id replaced by `f(id)`.
    pub fn map(&self, f: &impl Fn(PaneId) -> PaneId) -> Member {
        match self {
            Member::Pane(id) => Member::Pane(f(*id)),
            Member::Split {
                axis,
                members,
                flexes,
            } => Member::Split {
                axis: *axis,
                members: members.iter().map(|m| m.map(f)).collect(),
                flexes: flexes.clone(),
            },
        }
    }

    /// The shares of the split at `path`, if there is one.
    pub fn flexes_at(&self, path: &[usize]) -> Option<&[f32]> {
        match (self, path.split_first()) {
            (Member::Split { flexes, .. }, None) => Some(flexes),
            (Member::Split { members, .. }, Some((first, rest))) => {
                members.get(*first)?.flexes_at(rest)
            }
            _ => None,
        }
    }

    /// Sets the shares of the members of the split at `path` (child indices from the root).
    pub fn set_flexes(&mut self, path: &[usize], new: Vec<f32>) {
        match (self, path.split_first()) {
            (Member::Split { flexes, .. }, None) if flexes.len() == new.len() => *flexes = new,
            (Member::Split { members, .. }, Some((first, rest))) => {
                if let Some(member) = members.get_mut(*first) {
                    member.set_flexes(rest, new);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct T(u64);

    impl Tab for T {
        fn tab_id(&self) -> EntityId {
            EntityId::from(self.0)
        }
    }

    /// A pane's trail, as Zed's per-pane history: Back returns to the tabs it showed, Forward
    /// undoes that, a new visit forgets Forward, and closed tabs are skipped.
    #[test]
    fn a_pane_steps_back_and_forward_through_the_tabs_it_showed() {
        let mut pane = Pane::new(0);
        for id in 1..=3 {
            pane.add(T(id), false);
        }
        let id = |n: u64| EntityId::from(n);
        // Showed 1, then 2, then 3.
        pane.active = 0;
        pane.record(id(1));
        pane.active = 1;
        pane.record(id(2));
        pane.active = 2;
        assert!(pane.can_step(true) && !pane.can_step(false));

        let to = pane.step(true).unwrap();
        assert_eq!(to, 1, "back to 2");
        pane.active = to;
        let to = pane.step(true).unwrap();
        assert_eq!(to, 0, "back to 1");
        pane.active = to;
        assert!(!pane.can_step(true), "nothing before 1");
        assert_eq!(pane.step(true), None);
        let to = pane.step(false).unwrap();
        assert_eq!(to, 1, "forward to 2");
        pane.active = to;

        // A new visit forgets Forward.
        pane.record(id(2));
        pane.active = 2;
        assert!(!pane.can_step(false));

        // A tab closed since is skipped.
        pane.remove(1);
        let to = pane.step(true).unwrap();
        assert_eq!(pane.items[to], T(1), "2 is gone, so back goes to 1");
    }

    fn ids(pane: &Pane<T>) -> Vec<u64> {
        pane.items.iter().map(|t| t.0).collect()
    }

    #[test]
    fn a_preview_is_replaced_in_place_until_kept() {
        let mut pane = Pane::new(0);
        pane.add(T(1), false);
        assert_eq!(pane.add(T(2), true), (1, None));
        pane.add(T(3), false);
        let (index, replaced) = pane.add(T(4), true);
        assert_eq!(
            (index, replaced),
            (1, Some(T(2))),
            "same place, old preview out"
        );
        assert_eq!(ids(&pane), [1, 4, 3]);
        assert!(pane.is_preview(1));
        assert!(pane.keep(EntityId::from(4)));
        assert_eq!(
            pane.add(T(5), true).0,
            3,
            "a kept preview stays; the next opens beside"
        );
    }

    #[test]
    fn pinned_tabs_come_first_and_survive_close_others() {
        let mut pane = Pane::new(0);
        for id in 1..=4 {
            pane.add(T(id), false);
        }
        pane.active = 2;
        assert_eq!(pane.toggle_pin(2), 0);
        assert_eq!(ids(&pane), [3, 1, 2, 4]);
        assert_eq!(pane.active, 0, "the active tab moves with its pin");
        assert_eq!(pane.toggle_pin(3), 1);
        assert_eq!(ids(&pane), [3, 4, 1, 2]);
        assert_eq!(pane.others(2), [EntityId::from(2)]);
        assert_eq!(pane.to_the_right(0), [EntityId::from(1), EntityId::from(2)]);
        assert_eq!(
            pane.toggle_pin(0),
            1,
            "unpinning puts it first among the others"
        );
        assert_eq!(ids(&pane), [4, 3, 1, 2]);
        assert_eq!(pane.pinned, 1);
    }

    #[test]
    fn removing_a_tab_activates_its_right_neighbour() {
        let mut pane = Pane::new(0);
        for id in 1..=3 {
            pane.add(T(id), false);
        }
        pane.toggle_pin(0);
        pane.active = 1;
        pane.remove(1);
        assert_eq!(pane.active_item(), Some(&T(3)));
        pane.remove(0);
        assert_eq!(pane.pinned, 0);
        assert_eq!(pane.active_item(), Some(&T(3)));
        pane.remove(0);
        assert_eq!(pane.active_item(), None);
    }

    #[test]
    fn splits_nest_by_axis_and_collapse_when_emptied() {
        let mut tree = Member::Pane(0);
        assert!(tree.split(0, 1, SplitDirection::Right));
        assert!(tree.split(1, 2, SplitDirection::Right));
        assert_eq!(tree.panes(), [0, 1, 2]);
        let Member::Split { flexes, .. } = &tree else {
            panic!("a split");
        };
        assert_eq!(
            flexes,
            &[1., 0.5, 0.5],
            "the new pane halves the one it split"
        );
        assert!(tree.split(2, 3, SplitDirection::Down));
        assert_eq!(tree.panes(), [0, 1, 2, 3]);
        assert_eq!(tree.top_right(), 2, "the upper of the stacked right panes");
        assert_eq!(tree.top_left(), 0);
        assert!(tree.split(0, 4, SplitDirection::Left));
        assert_eq!(tree.panes(), [4, 0, 1, 2, 3]);
        assert!(tree.remove(2));
        assert!(tree.remove(1));
        assert!(tree.remove(4));
        assert_eq!(tree.panes(), [0, 3]);
        assert!(tree.remove(0));
        assert_eq!(tree, Member::Pane(3), "a split of one becomes that pane");
        assert!(!tree.remove(3), "the last pane stays");
        assert!(!tree.split(9, 10, SplitDirection::Right));
    }

    #[test]
    fn a_tree_round_trips_through_json() {
        let mut tree = Member::Pane(0);
        tree.split(0, 1, SplitDirection::Down);
        let text = serde_json::to_string(&tree).unwrap();
        assert_eq!(serde_json::from_str::<Member>(&text).unwrap(), tree);
    }
}
