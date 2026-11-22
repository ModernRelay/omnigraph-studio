//! A node's page (spec 014): the node view at full size in a tab of its own, read at the
//! branch head or held at a commit.

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    Window,
};
use graph_store::GraphSession;
use serde_json::json;
use studio_actions::{ViewKind, ViewLink};
use studio_workspace::{Inspected, InspectedNode, Item, ItemEvent, SavedItem};
use ui::prelude::*;

use crate::peek::NodePeek;

pub struct NodePage {
    view: Entity<NodePeek>,
    type_name: String,
    id: String,
    at_commit: Option<String>,
    /// The node's label as the tab last named it, saved with the tab: a page restored behind
    /// another tab, or one whose branch changed, reads nothing until it is shown, and its tab
    /// is named by this until then.
    saved_label: Option<String>,
    /// The tab as it was last told: its title, and whether it says newer data.
    tab: (String, bool),
    /// The toolbar as it was last drawn: whether the relationships with none are hidden, and
    /// whether there are any.
    toolbar: (bool, bool),
    /// The related node selected on the page as the inspector was last told of it.
    selected: Option<InspectedNode>,
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
            // The label read last names the tab while the view reads the node again, as it
            // does after a branch switch once the tab is shown; a node read with no name (an
            // empty or cleared one) forgets the old name.
            if let Some(label) = this.view.read(cx).read_label() {
                this.saved_label = label;
            }
            let tab = (this.title(cx), this.view.read(cx).is_newer());
            if tab != this.tab {
                this.tab = tab;
                cx.emit(ItemEvent::UpdateTab);
            }
            let toolbar = this.toolbar_state(cx);
            if toolbar != this.toolbar {
                this.toolbar = toolbar;
                cx.notify();
            }
            // A related node selected on the page is what the inspector shows (spec 020).
            let selected = this.view.read(cx).selected_node();
            if selected != this.selected {
                this.selected = selected;
                cx.emit(ItemEvent::Selected);
            }
        });
        let mut page = Self {
            view,
            type_name,
            id,
            at_commit,
            saved_label,
            tab: (String::new(), false),
            toolbar: (true, false),
            selected: None,
            _subscription: subscription,
        };
        page.tab = (page.title(cx), page.view.read(cx).is_newer());
        page.toolbar = page.toolbar_state(cx);
        page
    }

    /// The page's view of its node.
    #[cfg(test)]
    pub(crate) fn view(&self) -> &Entity<NodePeek> {
        &self.view
    }

    /// What names the tab: the node's label once the page reads it, else the one the tab was
    /// saved with, else the node's id.
    fn title(&self, cx: &App) -> String {
        match self.view.read(cx).read_label() {
            Some(label) => label,
            None => self.saved_label.clone(),
        }
        .unwrap_or_else(|| self.id.clone())
    }

    fn toolbar_state(&self, cx: &App) -> (bool, bool) {
        let view = self.view.read(cx);
        (view.hides_empty(), view.any_empty())
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

    fn inspected(&mut self, _cx: &mut Context<Self>) -> Option<Inspected> {
        self.selected.clone().map(Inspected::Node)
    }

    fn views(&self, _cx: &App) -> Vec<ViewLink> {
        vec![
            ViewLink::current(ViewKind::Page),
            ViewLink::to(ViewKind::Tree),
            ViewLink::to(ViewKind::Canvas),
            // Read at the branch head, also from a page held at a commit (spec 046).
            ViewLink::to(ViewKind::History),
        ]
    }

    fn view_action(&self, kind: ViewKind, _cx: &App) -> Option<Box<dyn gpui::Action>> {
        let (type_name, id, at_commit) = (
            self.type_name.clone(),
            self.id.clone(),
            self.at_commit.clone(),
        );
        match kind {
            ViewKind::Tree => Some(Box::new(studio_actions::graph::ShowNodeInTree {
                type_name,
                id,
                at_commit,
                preview: true,
            })),
            ViewKind::Canvas => Some(Box::new(studio_actions::graph::ShowNodeOnCanvas {
                type_name,
                id,
                at_commit,
                preview: true,
            })),
            ViewKind::History => Some(Box::new(studio_actions::graph::ShowNodeHistory {
                type_name,
                id,
                preview: true,
            })),
            _ => None,
        }
    }

    fn held_at(&self, _cx: &App) -> Option<String> {
        self.at_commit.clone()
    }

    /// The page's node (spec 032).
    fn selected_ids(&self, _cx: &App) -> Option<studio_workspace::SelectedIds> {
        Some(studio_workspace::SelectedIds {
            type_name: Some(self.type_name.clone()),
            ids: vec![self.id.clone()],
            root: None,
        })
    }

    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        Some(studio_actions::Thing::Node {
            type_name: self.type_name.clone(),
            id: self.id.clone(),
        })
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
        let label = match self.view.read(cx).read_label() {
            Some(label) => label,
            None => self.saved_label.clone(),
        };
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

impl NodePage {
    /// The tab's toolbar: its views, then the tree's eye over the relationships with none and
    /// Collapse all.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (hide_empty, any_empty) = self.toolbar;
        studio_ui::ds::toolbar(cx)
            .children(studio_workspace::view_strip(Item::views(self, cx), cx))
            .child(div().flex_1())
            .child(
                studio_ui::ds::toolbar_actions()
                    // The eye shows only when it changes something, as on a tree.
                    .when(hide_empty || any_empty, |row| {
                        row.child(
                            studio_workspace::empty_groups_toggle(hide_empty, false).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.view.update(cx, |view, cx| view.toggle_empty(cx))
                                }),
                            ),
                        )
                    })
                    .child(
                        studio_workspace::collapse_all_button().on_click(cx.listener(
                            |this, _, _, cx| this.view.update(cx, |view, cx| view.collapse_all(cx)),
                        )),
                    ),
            )
    }
}

impl Render for NodePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("node-page")
            .key_context("NodePage")
            .size_full()
            .bg(studio_ui::ds::surface::document(cx))
            .child(self.render_toolbar(cx))
            // The page scrolls as a list of its own, drawing only what is in view.
            .child(div().flex_1().min_h_0().child(self.view.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;

    /// A page saved under a name whose node now has none (cleared to empty) names its tab by
    /// the id once the node is read, as its title does, not by the old name (Bugbot, #161).
    #[gpui::test]
    async fn a_cleared_name_leaves_no_stale_tab_title(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let schema = "node Person {\n  slug: String @key\n  name: String\n}\n";
        let rows = json!([{"n": {"@id": "p1", "slug": "p1", "name": ""}}]);
        let session = crate::test_support::session_reading(schema, rows, cx);
        let (page, cx) = cx.add_window_view(move |_, cx| {
            NodePage::new(
                session,
                "Person".into(),
                "p1".into(),
                None,
                Some("Old name".into()),
                cx,
            )
        });
        cx.run_until_parked();
        page.read_with(cx, |page, cx| assert_eq!(page.title(cx), "p1"));
    }
}
