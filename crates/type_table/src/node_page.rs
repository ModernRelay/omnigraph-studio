//! A node's page (spec 014): the node view at full size in a tab of its own, read at the
//! branch head or held at a commit.

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    Window,
};
use graph_store::GraphSession;
use serde_json::json;
use studio_actions::{ViewKind, ViewLink};
use studio_ui::ds::Tipped as _;
use studio_workspace::{Inspected, InspectedNode, Item, ItemEvent, SavedItem};
use ui::prelude::*;

use crate::peek::NodePeek;

pub struct NodePage {
    view: Entity<NodePeek>,
    session: Entity<GraphSession>,
    type_name: String,
    id: String,
    at_commit: Option<String>,
    /// The branch the page reads and edits whatever the window shows (spec 038); `None`
    /// follows the window's.
    branch: Option<String>,
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
    _session: Subscription,
}

impl NodePage {
    pub fn new(
        session: Entity<GraphSession>,
        type_name: String,
        id: String,
        at_commit: Option<String>,
        branch: Option<String>,
        saved_label: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        // A page held at a commit reads no branch.
        let branch = branch.filter(|_| at_commit.is_none());
        let view = {
            let (session, type_name, id) = (session.clone(), type_name.clone(), id.clone());
            let (at_commit, branch) = (at_commit.clone(), branch.clone());
            cx.new(|cx| NodePeek::page(session, type_name, id, at_commit, branch, cx))
        };
        // A page on a branch of its own says so while the window shows another.
        let session_subscription = cx.subscribe(&session, |this, _, event, cx| {
            if this.branch.is_some()
                && matches!(
                    event,
                    graph_store::SessionEvent::BranchChanged
                        | graph_store::SessionEvent::ProposingChanged
                )
            {
                cx.notify();
            }
        });
        // The tab's title is the node's label once it is read, and its dot says newer data:
        // the tab is told when either changes, not at each change of the view, which draws
        // itself.
        let subscription = cx.observe(&view, |this, _, cx| {
            // The label read last names the tab while the view reads the node again, as it
            // does after a branch switch once the tab is shown.
            if let Some(label) = this.view.read(cx).label() {
                this.saved_label = Some(label);
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
            session,
            type_name,
            id,
            at_commit,
            branch,
            saved_label,
            tab: (String::new(), false),
            toolbar: (true, false),
            selected: None,
            _subscription: subscription,
            _session: session_subscription,
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
        self.view
            .read(cx)
            .label()
            .or_else(|| self.saved_label.clone())
            .unwrap_or_else(|| self.id.clone())
    }

    fn toolbar_state(&self, cx: &App) -> (bool, bool) {
        let view = self.view.read(cx);
        (view.hides_empty(), view.any_empty())
    }

    /// One page per node at the window's branch head, one per node and branch of its own, and
    /// one per node and commit.
    pub fn key(type_name: &str, id: &str, at_commit: Option<&str>, branch: Option<&str>) -> String {
        match (at_commit, branch) {
            (Some(commit), _) => format!("node:{type_name}:{id}@{commit}"),
            (None, Some(branch)) => format!("node:{type_name}:{id}#{branch}"),
            (None, None) => format!("node:{type_name}:{id}"),
        }
    }

    /// The branch the page is on when it is not the window's: what its toolbar names.
    fn other_branch(&self, cx: &App) -> Option<String> {
        let own = self.branch.as_ref()?;
        (*own != self.session.read(cx).branch).then(|| own.clone())
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
        ]
    }

    fn view_action(&self, kind: ViewKind, cx: &App) -> Option<Box<dyn gpui::Action>> {
        // A tree and a canvas read the window's branch: from a page on another branch they
        // hold the commit the page shows, read-only (spec 038).
        let at_commit = match self.other_branch(cx) {
            Some(_) => self
                .at_commit
                .clone()
                .or_else(|| self.view.read(cx).shown_commit()),
            None => self.at_commit.clone(),
        };
        let (type_name, id) = (self.type_name.clone(), self.id.clone());
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
        Some(
            Self::key(
                &self.type_name,
                &self.id,
                self.at_commit.as_deref(),
                self.branch.as_deref(),
            )
            .into(),
        )
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
                "branch": self.branch,
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
        let other = self.other_branch(cx);
        let writes_to = other
            .as_ref()
            .map(|branch| self.session.read(cx).writes_to(branch));
        studio_ui::ds::toolbar(cx)
            .children(studio_workspace::view_strip(Item::views(self, cx), cx))
            .child(div().flex_1())
            // The branch the page reads and edits, when the window shows another (spec 038).
            .children(other.zip(writes_to).map(|(branch, writes_to)| {
                let tip = if writes_to == branch {
                    format!("This page reads and edits {branch}, whatever the window shows.")
                } else {
                    format!(
                        "This page reads {branch}, whatever the window shows; edits go to \
                         {writes_to}."
                    )
                };
                studio_ui::ds::meta(IconName::GitBranch, format!("on {branch}"))
                    .id("page-branch")
                    .flex_none()
                    .tip(studio_ui::ds::tooltip(tip))
            }))
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
