//! A node's page (spec 014): the node view at full size in a tab of its own, read at the
//! branch head or held at a commit.

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    Window,
};
use graph_store::GraphSession;
use serde_json::json;
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;

use crate::peek::NodePeek;

pub struct NodePage {
    view: Entity<NodePeek>,
    type_name: String,
    id: String,
    at_commit: Option<String>,
    focus_handle: FocusHandle,
    _subscription: Subscription,
}

impl NodePage {
    pub fn new(
        session: Entity<GraphSession>,
        type_name: String,
        id: String,
        at_commit: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = {
            let (type_name, id, at_commit) = (type_name.clone(), id.clone(), at_commit.clone());
            cx.new(|cx| NodePeek::page(session, type_name, id, at_commit, cx))
        };
        // The tab's title is the node's label once it is read, and its dot says newer data.
        let subscription = cx.observe(&view, |_, _, cx| {
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        });
        Self {
            view,
            type_name,
            id,
            at_commit,
            focus_handle: cx.focus_handle(),
            _subscription: subscription,
        }
    }

    /// One page per node at the head, and one per node and commit.
    pub fn key(type_name: &str, id: &str, at_commit: Option<&str>) -> String {
        match at_commit {
            Some(commit) => format!("node:{type_name}:{id}@{commit}"),
            None => format!("node:{type_name}:{id}"),
        }
    }
}

impl EventEmitter<ItemEvent> for NodePage {}

impl Focusable for NodePage {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for NodePage {
    fn tab_title(&self, cx: &App) -> SharedString {
        self.view.read(cx).title().into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::Reader)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(Self::key(&self.type_name, &self.id, self.at_commit.as_deref()).into())
    }

    fn is_stale(&self, cx: &App) -> bool {
        self.view.read(cx).is_newer()
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "node".into(),
            data: json!({
                "type_name": self.type_name,
                "id": self.id,
                "at_commit": self.at_commit,
            }),
        })
    }
}

impl Render for NodePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("node-page")
            .key_context("NodePage")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            // The page scrolls as a list of its own, drawing only what is in view.
            .child(self.view.clone())
    }
}
