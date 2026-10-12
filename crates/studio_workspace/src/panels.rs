//! Panels and docks (spec 002, "Panels"), after Zed's `Panel`/`Dock` and Obsidian's views in
//! side docks: a panel is a view with an id, a title and an icon; a dock at the left, right or
//! bottom holds panels, shows one at a time, and opens or closes as a whole. Every dock draws
//! the same bounded chrome: a header strip with its panels' tabs, then the active panel.
//!
//! Documents (type tables, queries, history) are items in the center, not panels.

use gpui::{AnyView, SharedString};
use serde::{Deserialize, Serialize};
use ui::IconName;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelPosition {
    Left,
    Right,
    Bottom,
}

/// One panel: what it is called, its icon in the dock's tabs, and its view.
pub struct PanelEntry<V = AnyView> {
    pub id: SharedString,
    pub title: SharedString,
    pub icon: IconName,
    pub view: V,
}

/// The panels at one position.
pub struct Dock<V = AnyView> {
    pub position: PanelPosition,
    pub panels: Vec<PanelEntry<V>>,
    pub active: usize,
    pub open: bool,
}

impl<V> Dock<V> {
    pub fn new(position: PanelPosition, open: bool) -> Self {
        Self {
            position,
            panels: Vec::new(),
            active: 0,
            open,
        }
    }

    pub fn active_panel(&self) -> Option<&PanelEntry<V>> {
        self.panels.get(self.active)
    }

    pub fn active_id(&self) -> Option<SharedString> {
        self.active_panel().map(|panel| panel.id.clone())
    }

    /// Whether the dock is drawn: open and holding a panel.
    pub fn visible(&self) -> bool {
        self.open && !self.panels.is_empty()
    }

    pub fn position_of(&self, id: &str) -> Option<usize> {
        self.panels.iter().position(|panel| panel.id.as_ref() == id)
    }

    /// Adds a panel, or replaces the view of the panel with the same id.
    pub fn insert(&mut self, panel: PanelEntry<V>) {
        match self.position_of(&panel.id) {
            Some(index) => self.panels[index] = panel,
            None => self.panels.push(panel),
        }
    }

    pub fn remove(&mut self, id: &str) {
        if let Some(index) = self.position_of(id) {
            self.panels.remove(index);
            if self.active >= self.panels.len() {
                self.active = self.panels.len().saturating_sub(1);
            }
        }
    }

    /// Makes `id` the active panel and opens the dock; false when the dock has no such panel.
    pub fn activate(&mut self, id: &str) -> bool {
        match self.position_of(id) {
            Some(index) => {
                self.active = index;
                self.open = true;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(id: &str) -> PanelEntry<()> {
        PanelEntry {
            id: id.to_owned().into(),
            title: id.to_owned().into(),
            icon: IconName::Table,
            view: (),
        }
    }

    #[test]
    fn a_dock_shows_one_panel_and_hides_when_empty() {
        let mut dock = Dock::<()>::new(PanelPosition::Left, true);
        assert!(!dock.visible());
        assert!(!dock.activate("types"));
        assert_eq!(dock.active_id(), None);
        assert_eq!(dock.position_of("types"), None);
        dock.remove("types");
        assert_eq!(dock.active, 0);
    }

    #[test]
    fn panels_are_replaced_by_id_activated_and_removed() {
        let mut dock = Dock::new(PanelPosition::Left, false);
        dock.insert(panel("types"));
        dock.insert(panel("explore"));
        dock.insert(panel("types"));
        assert_eq!(dock.panels.len(), 2, "same id replaces");
        assert!(!dock.visible(), "a closed dock stays hidden");
        assert!(dock.activate("explore"));
        assert!(dock.visible(), "activating opens the dock");
        assert_eq!(dock.active_id().as_deref(), Some("explore"));
        dock.remove("explore");
        assert_eq!(dock.active_id().as_deref(), Some("types"));
    }
}
