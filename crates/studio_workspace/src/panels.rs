//! Panels and docks (spec 002, "Panels"), after Zed's `Panel`/`Dock` and Obsidian's views in
//! side docks: a panel is a view with an id, a title and an icon; a dock at the left, right or
//! bottom holds panels, shows one at a time, and opens or closes as a whole. A dock draws its
//! active panel under a header strip with that panel's title and buttons; panels are switched
//! from their icons in the status bar, as Zed's `PanelButtons`.
//!
//! Documents (type tables, queries, history) are items in the center, not panels.

use std::rc::Rc;

use gpui::{AnyView, App, FocusHandle, SharedString};
use serde::{Deserialize, Serialize};
use ui::IconName;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelPosition {
    Left,
    Right,
    Bottom,
}

impl PanelPosition {
    pub const ALL: [PanelPosition; 3] = [Self::Left, Self::Bottom, Self::Right];

    /// As the menu that moves a panel names it.
    pub fn label(self) -> &'static str {
        match self {
            PanelPosition::Left => "Dock Left",
            PanelPosition::Right => "Dock Right",
            PanelPosition::Bottom => "Dock Bottom",
        }
    }

    pub fn side(self) -> studio_settings::DockSide {
        match self {
            PanelPosition::Left => studio_settings::DockSide::Left,
            PanelPosition::Right => studio_settings::DockSide::Right,
            PanelPosition::Bottom => studio_settings::DockSide::Bottom,
        }
    }

    pub fn from_side(side: studio_settings::DockSide) -> Self {
        match side {
            studio_settings::DockSide::Left => PanelPosition::Left,
            studio_settings::DockSide::Right => PanelPosition::Right,
            studio_settings::DockSide::Bottom => PanelPosition::Bottom,
        }
    }

    pub fn default_size(self) -> f32 {
        match self {
            PanelPosition::Left => studio_ui::ds::size::SIDEBAR_WIDTH,
            PanelPosition::Right => studio_ui::ds::size::INSPECTOR_WIDTH,
            PanelPosition::Bottom => 300.,
        }
    }
}

/// One panel: what it is called, its icon in the status bar, and its view.
pub struct PanelEntry<V = AnyView> {
    pub id: SharedString,
    pub title: SharedString,
    pub icon: IconName,
    /// The dock its feature put it in; it goes back there unless the person moved it.
    pub home: PanelPosition,
    pub view: V,
    /// Small buttons on the right of the panel's title line (search, new), as Delta and Zed
    /// put a list's actions in its header.
    pub actions: Option<V>,
    /// What takes the keyboard when the panel is shown from its button or its key, as Zed
    /// focuses a dock's panel; `None` for a panel with no keys of its own.
    pub focus: Option<PanelFocus>,
}

/// The panel's focus as it is now (its view may be replaced, as when the graph changes).
pub type PanelFocus = Rc<dyn Fn(&App) -> Option<FocusHandle>>;

/// The panels at one position.
pub struct Dock<V = AnyView> {
    pub position: PanelPosition,
    pub panels: Vec<PanelEntry<V>>,
    pub active: usize,
    pub open: bool,
    /// Width of a side dock or height of the bottom dock, in pixels; dragged by its edge.
    pub size: f32,
}

/// The least a side dock is dragged to, and the most of the window's width it takes.
const SIDE_MIN: f32 = 200.;
const SIDE_SHARE: f32 = 0.45;
/// The least the bottom dock is dragged to, and the most of the window's height it takes.
const BOTTOM_MIN: f32 = 120.;
const BOTTOM_SHARE: f32 = 0.6;
/// The width the document keeps between the side docks, and the height it keeps above the
/// bottom dock, however small the window: a dock gives way rather than squeezing it out.
const DOCUMENT_MIN_WIDTH: f32 = 360.;
const DOCUMENT_MIN_HEIGHT: f32 = 160.;

/// The widths the side docks are drawn at in a window `width` wide, from their sizes (`left`
/// and `right`, 0 for one not drawn): each at most its share of the window, and together leaving
/// the document [`DOCUMENT_MIN_WIDTH`], past which they give way in proportion. The sizes held
/// stay as they are, so a window widened again draws the docks at them.
pub fn side_widths(left: f32, right: f32, width: f32) -> (f32, f32) {
    let shared = |size: f32| size.min(width * SIDE_SHARE).max(0.);
    let (left, right) = (shared(left), shared(right));
    let room = (width - DOCUMENT_MIN_WIDTH).max(0.);
    let taken = left + right;
    if taken <= room {
        return (left, right);
    }
    let scale = room / taken;
    (left * scale, right * scale)
}

/// The height the bottom dock is drawn at in a window `height` tall, from its size: at most its
/// share of the window, leaving the document [`DOCUMENT_MIN_HEIGHT`].
pub fn bottom_height(size: f32, height: f32) -> f32 {
    size.min(height * BOTTOM_SHARE)
        .min(height - DOCUMENT_MIN_HEIGHT)
        .max(0.)
}

impl<V> Dock<V> {
    pub fn new(position: PanelPosition, open: bool) -> Self {
        Self {
            position,
            panels: Vec::new(),
            active: 0,
            open,
            size: position.default_size(),
        }
    }

    /// Sets the size within what keeps the dock and the document usable in `available` pixels
    /// (the window's width for a side dock, its height for the bottom dock).
    pub fn resize(&mut self, size: f32, available: f32) {
        let (min, share) = match self.position {
            PanelPosition::Left | PanelPosition::Right => (SIDE_MIN, SIDE_SHARE),
            PanelPosition::Bottom => (BOTTOM_MIN, BOTTOM_SHARE),
        };
        self.size = size.min(available * share).max(min);
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

    /// Adds a panel, or replaces the one with its id, keeping that one's header actions and
    /// focus handle when the new one brings none.
    pub fn insert(&mut self, mut panel: PanelEntry<V>) {
        match self.position_of(&panel.id) {
            Some(index) => {
                if panel.actions.is_none() {
                    panel.actions = self.panels[index].actions.take();
                }
                if panel.focus.is_none() {
                    panel.focus = self.panels[index].focus.take();
                }
                self.panels[index] = panel;
            }
            None => self.panels.push(panel),
        }
    }

    /// Sets the header actions of the panel with `id`; false when there is none.
    pub fn set_actions(&mut self, id: &str, actions: V) -> bool {
        match self.position_of(id) {
            Some(index) => {
                self.panels[index].actions = Some(actions);
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.take(id);
    }

    /// Takes a panel out, to put it in another dock. The active panel stays active, or its
    /// neighbor becomes so.
    pub fn take(&mut self, id: &str) -> Option<PanelEntry<V>> {
        let index = self.position_of(id)?;
        let active = self.active_id();
        let panel = self.panels.remove(index);
        self.active = match active.and_then(|active| self.position_of(&active)) {
            Some(still) => still,
            None => index.min(self.panels.len().saturating_sub(1)),
        };
        Some(panel)
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
            home: PanelPosition::Left,
            view: (),
            actions: None,
            focus: None,
        }
    }

    #[test]
    fn a_taken_panel_leaves_the_others_as_they_were() {
        let mut dock = Dock::new(PanelPosition::Bottom, true);
        for id in ["nodes", "history", "activity"] {
            dock.insert(panel(id));
        }
        dock.activate("activity");
        let taken = dock.take("history").expect("history is there");
        assert_eq!(taken.id.as_ref(), "history");
        assert_eq!(
            dock.active_id().as_deref(),
            Some("activity"),
            "still active"
        );
        dock.take("activity");
        assert_eq!(dock.active_id().as_deref(), Some("nodes"), "its neighbor");
        assert!(dock.take("activity").is_none());
    }

    /// The side docks keep their sizes in a wide window; in a narrow one each stays within its
    /// share, and together they give way in proportion so the document keeps its width.
    #[test]
    fn docks_give_way_to_a_small_window() {
        assert_eq!(side_widths(260., 320., 1600.), (260., 320.));
        assert_eq!(side_widths(260., 0., 1600.), (260., 0.));
        // Half a 1280 px screen: 640 px, of which the document keeps 360. The inspector is
        // held within its share first (288 px), then both give way in proportion.
        let (left, right) = side_widths(260., 320., 640.);
        assert!((left + right - 280.).abs() < 0.01, "{left} + {right}");
        assert!((left / right - 260. / 288.).abs() < 0.01, "in proportion");
        // Each within its share of the window.
        let (wide, none) = side_widths(900., 0., 1600.);
        assert!((wide - 720.).abs() < 0.01 && none == 0., "{wide}");
        // Too small for any dock: the document has it all.
        assert_eq!(side_widths(260., 320., 300.), (0., 0.));
        assert_eq!(bottom_height(300., 1000.), 300.);
        assert_eq!(bottom_height(300., 400.), 240., "within its share");
        assert_eq!(
            bottom_height(300., 250.),
            90.,
            "the document keeps its height"
        );
        assert_eq!(bottom_height(300., 100.), 0.);
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

    #[test]
    fn a_dock_resizes_within_bounds() {
        let mut side = Dock::<()>::new(PanelPosition::Left, true);
        assert_eq!(side.size, 264.);
        side.resize(120., 1440.);
        assert_eq!(side.size, 200., "never narrower than a row of the sidebar");
        side.resize(1000., 1440.);
        assert_eq!(
            side.size,
            1440. * 0.45,
            "the document keeps most of the window"
        );
        let mut bottom = Dock::<()>::new(PanelPosition::Bottom, false);
        bottom.resize(400., 900.);
        assert_eq!(bottom.size, 400.);
    }
}
