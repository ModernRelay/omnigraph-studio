//! The contract every tab in the center pane implements (spec 002). A small version of Zed's
//! `workspace::Item`.

use gpui::{
    Action, AnyElement, AnyView, App, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Render, SharedString, Window,
};
use ui::IconName;

use crate::persistence::SavedItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemEvent {
    /// The tab's title, icon or dirty state changed.
    UpdateTab,
    /// The item asks to be closed.
    Close,
}

/// What a tab adds to the status bar (spec 002): a few facts after the graph and branch, as
/// Zed's status items, and the keys it answers, which the bar lists in a panel behind one
/// button rather than spelling them out. A tab's controls belong in the tab.
#[derive(Default)]
pub struct ItemStatus {
    pub start: Vec<AnyElement>,
    pub end: Vec<AnyElement>,
    pub keys: Vec<KeyHint>,
}

/// One key a tab answers, as its keys panel lists it: what it does, and the action whose
/// binding is shown, looked up from `focus` so the tab's own context decides it.
pub struct KeyHint {
    pub label: SharedString,
    pub action: Box<dyn Action>,
    pub focus: FocusHandle,
}

impl KeyHint {
    pub fn new(label: impl Into<SharedString>, action: &dyn Action, focus: &FocusHandle) -> Self {
        Self {
            label: label.into(),
            action: action.boxed_clone(),
            focus: focus.clone(),
        }
    }
}

impl Clone for KeyHint {
    fn clone(&self) -> Self {
        Self {
            label: self.label.clone(),
            action: self.action.boxed_clone(),
            focus: self.focus.clone(),
        }
    }
}

pub trait Item: Render + Focusable + EventEmitter<ItemEvent> {
    fn tab_title(&self, cx: &App) -> SharedString;

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        None
    }

    /// Items with the same key are one tab: opening the item again activates the existing tab.
    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        None
    }

    /// Whether the tab shows what `action` opens, so a panel row that dispatches it shows as
    /// selected while the tab is active. One tab can show what different actions open (the
    /// canvas's ontology and its walk from a node), which its dedupe key does not tell apart.
    fn opened_by(&self, _action: &dyn Action, _cx: &App) -> bool {
        false
    }

    /// The item has unsaved work; closing asks first.
    fn is_dirty(&self, _cx: &App) -> bool {
        false
    }

    /// The item reads an older commit than the branch head.
    fn is_stale(&self, _cx: &App) -> bool {
        false
    }

    /// What reopens this tab after a restart; `None` is not remembered. The kind must be
    /// registered with `register_item_kind`.
    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        None
    }

    /// The tab's part of the status bar; `None` shows only the graph and branch.
    fn status(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<ItemStatus> {
        None
    }

    /// The item draws its costly part as a cached view of its own, so its pane draws it
    /// uncached. A cached view that draws again draws every cached view inside it again too
    /// (GPUI's `prepaint_view`), so a keystroke in one of the item's fields would lay that part
    /// out again if the pane cached the whole item.
    fn caches_its_parts(&self) -> bool {
        false
    }

    /// The tab shows what the window's graph reads from its server, so while the server asks
    /// for a sign-in nobody holds its pane shows the sign-in instead ([`crate::sign_in_prompt`]).
    /// A tab that reads nothing from it (settings, the agent) is drawn as it is.
    fn reads_graph(&self) -> bool {
        true
    }
}

pub trait ItemHandle: 'static {
    fn item_id(&self) -> EntityId;
    fn to_any(&self) -> AnyView;
    fn tab_title(&self, cx: &App) -> SharedString;
    fn tab_icon(&self, cx: &App) -> Option<IconName>;
    fn dedupe_key(&self, cx: &App) -> Option<SharedString>;
    fn opened_by(&self, action: &dyn Action, cx: &App) -> bool;
    fn is_dirty(&self, cx: &App) -> bool;
    fn is_stale(&self, cx: &App) -> bool;
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn serialize(&self, cx: &App) -> Option<SavedItem>;
    fn status(&self, window: &mut Window, cx: &mut App) -> Option<ItemStatus>;
    fn caches_its_parts(&self, cx: &App) -> bool;
    fn reads_graph(&self, cx: &App) -> bool;
}

impl<T: Item> ItemHandle for Entity<T> {
    fn item_id(&self) -> EntityId {
        self.entity_id()
    }

    fn to_any(&self) -> AnyView {
        self.clone().into()
    }

    fn tab_title(&self, cx: &App) -> SharedString {
        self.read(cx).tab_title(cx)
    }

    fn tab_icon(&self, cx: &App) -> Option<IconName> {
        self.read(cx).tab_icon(cx)
    }

    fn dedupe_key(&self, cx: &App) -> Option<SharedString> {
        self.read(cx).dedupe_key(cx)
    }

    fn opened_by(&self, action: &dyn Action, cx: &App) -> bool {
        self.read(cx).opened_by(action, cx)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty(cx)
    }

    fn is_stale(&self, cx: &App) -> bool {
        self.read(cx).is_stale(cx)
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        Focusable::focus_handle(self.read(cx), cx)
    }

    fn serialize(&self, cx: &App) -> Option<SavedItem> {
        self.read(cx).serialize(cx)
    }

    fn status(&self, window: &mut Window, cx: &mut App) -> Option<ItemStatus> {
        self.update(cx, |item, cx| item.status(window, cx))
    }

    fn caches_its_parts(&self, cx: &App) -> bool {
        self.read(cx).caches_its_parts()
    }

    fn reads_graph(&self, cx: &App) -> bool {
        self.read(cx).reads_graph()
    }
}
