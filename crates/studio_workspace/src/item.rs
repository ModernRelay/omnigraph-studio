//! The contract every tab in the center pane implements (spec 002). A small version of Zed's
//! `workspace::Item`.

use gpui::{
    AnyView, App, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Render, SharedString,
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

pub trait Item: Render + Focusable + EventEmitter<ItemEvent> {
    fn tab_title(&self, cx: &App) -> SharedString;

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        None
    }

    /// Items with the same key are one tab: opening the item again activates the existing tab.
    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        None
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
}

pub trait ItemHandle: 'static {
    fn item_id(&self) -> EntityId;
    fn to_any(&self) -> AnyView;
    fn tab_title(&self, cx: &App) -> SharedString;
    fn tab_icon(&self, cx: &App) -> Option<IconName>;
    fn dedupe_key(&self, cx: &App) -> Option<SharedString>;
    fn is_dirty(&self, cx: &App) -> bool;
    fn is_stale(&self, cx: &App) -> bool;
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn boxed_clone(&self) -> Box<dyn ItemHandle>;
    fn serialize(&self, cx: &App) -> Option<SavedItem>;
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

    fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty(cx)
    }

    fn is_stale(&self, cx: &App) -> bool {
        self.read(cx).is_stale(cx)
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        Focusable::focus_handle(self.read(cx), cx)
    }

    fn boxed_clone(&self) -> Box<dyn ItemHandle> {
        Box::new(self.clone())
    }

    fn serialize(&self, cx: &App) -> Option<SavedItem> {
        self.read(cx).serialize(cx)
    }
}
