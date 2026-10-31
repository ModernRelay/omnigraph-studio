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
    /// The node's label as the tab last named it, saved with the tab: a page restored behind
    /// another tab reads nothing until it is shown, and its tab is named by this until then.
    saved_label: Option<String>,
    /// The tab as it was last told: its title, and whether it says newer data.
    tab: (String, bool),
    _subscription: Subscription,
}

impl NodePage {
    pub fn new(
        session: Entity<GraphSession>,
        type_name: String,
        id: String,
        at_commit: Option<String>,
        saved_label: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = {
            let (type_name, id, at_commit) = (type_name.clone(), id.clone(), at_commit.clone());
            cx.new(|cx| NodePeek::page(session, type_name, id, at_commit, cx))
        };
        // The tab's title is the node's label once it is read, and its dot says newer data:
        // the tab is told when either changes, not at each change of the view, which draws
        // itself.
        let subscription = cx.observe(&view, |this, _, cx| {
            let tab = (this.title(cx), this.view.read(cx).is_newer());
            if tab != this.tab {
                this.tab = tab;
                cx.emit(ItemEvent::UpdateTab);
            }
        });
        let mut page = Self {
            view,
            type_name,
            id,
            at_commit,
            saved_label,
            tab: (String::new(), false),
            _subscription: subscription,
        };
        page.tab = (page.title(cx), page.view.read(cx).is_newer());
        page
    }

    /// What names the tab: the node's label once the page reads it, else the one the tab was
    /// saved with, else the node's id.
    fn title(&self, cx: &App) -> String {
        self.view
            .read(cx)
            .label()
            .or_else(|| self.saved_label.clone())
            .unwrap_or_else(|| self.id.clone())
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

/// The page's keyboard is its view's, so the view's keys (⌘S saves the drafts) reach it when the
/// tab is shown, with no field open.
impl Focusable for NodePage {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).focus_handle(cx)
    }
}

impl Item for NodePage {
    fn tab_title(&self, cx: &App) -> SharedString {
        self.title(cx).into()
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

    fn serialize(&self, cx: &App) -> Option<SavedItem> {
        let label = self
            .view
            .read(cx)
            .label()
            .or_else(|| self.saved_label.clone());
        Some(SavedItem {
            kind: "node".into(),
            data: json!({
                "type_name": self.type_name,
                "id": self.id,
                "at_commit": self.at_commit,
                "label": label,
            }),
        })
    }
}

impl Render for NodePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("node-page")
            .key_context("NodePage")
            .size_full()
            .bg(studio_ui::ds::surface::document(cx))
            // The page scrolls as a list of its own, drawing only what is in view.
            .child(self.view.clone())
    }
}
