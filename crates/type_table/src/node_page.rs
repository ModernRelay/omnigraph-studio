//! A node's page (spec 014): the node view at full size in a tab of its own, read at the
//! branch head or held at a commit.

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    Window,
};
use graph_store::{GraphSession, SessionEvent};
use serde_json::json;
use studio_actions::graph::RunNodeAction;
use studio_actions::node::ShowActions;
use studio_actions::{ViewKind, ViewLink};
use studio_ui::ds::Tipped as _;
use studio_ui::menu::{ContextMenu, ContextMenuEntry};
use studio_workspace::{Inspected, InspectedNode, Item, ItemEvent, SavedItem};
use ui::prelude::*;
use ui::{PopoverMenu, PopoverMenuHandle};

use crate::peek::NodePeek;

/// A stored mutation that acts on the page's node, and the action that runs it on the node.
type NodeActionEntry = (RunNodeAction, graph_store::verbs::NodeAction);

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
    session: Entity<GraphSession>,
    /// The toolbar's Actions menu: the stored mutations that act on the node (spec 041).
    actions_menu: PopoverMenuHandle<ContextMenu>,
    _subscription: Subscription,
    _subscriptions: Vec<Subscription>,
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
            let session = session.clone();
            cx.new(|cx| NodePeek::page(session, type_name, id, at_commit, cx))
        };
        // The Actions menu follows the catalog and what the person says of it (spec 041).
        session.update(cx, |session, cx| session.want_stored_queries(cx));
        let subscriptions = vec![
            cx.subscribe(&session, |_, _, event: &SessionEvent, cx| {
                if let SessionEvent::StoredQueriesChanged = event {
                    cx.notify();
                }
            }),
            cx.observe_global::<studio_settings::StudioSettings>(|_, cx| cx.notify()),
        ];
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
            type_name,
            id,
            at_commit,
            saved_label,
            tab: (String::new(), false),
            toolbar: (true, false),
            selected: None,
            session,
            actions_menu: PopoverMenuHandle::default(),
            _subscription: subscription,
            _subscriptions: subscriptions,
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

impl NodePage {
    /// The stored mutations that act on the page's node (spec 041), each as the action that
    /// runs it, and why none can run here when the page is held at a commit.
    fn node_actions(&self, cx: &App) -> (Vec<NodeActionEntry>, Option<String>) {
        let actions = self
            .session
            .read(cx)
            .node_actions(&self.type_name, cx)
            .into_iter()
            .map(|action| {
                (
                    RunNodeAction {
                        query: action.query.clone(),
                        param: action.param.clone(),
                        type_name: self.type_name.clone(),
                        id: self.id.clone(),
                    },
                    action,
                )
            })
            .collect();
        let held = self.at_commit.as_ref().map(|commit| {
            format!(
                "Read at commit {}; an action writes to a branch's head",
                studio_ui::short_commit_id(commit)
            )
        });
        (actions, held)
    }

    /// Opens the Actions menu, from ⌥↵; not on a page held at a commit, whose button is
    /// disabled: an action writes to the branch's head.
    fn show_actions(&mut self, _: &ShowActions, window: &mut Window, cx: &mut Context<Self>) {
        let (actions, held) = self.node_actions(cx);
        if held.is_none() && !actions.is_empty() {
            self.actions_menu.show(window, cx);
        }
    }

    /// The toolbar's Actions menu, when a stored mutation acts on the node's type: each by its
    /// name in words, its description as its tooltip, and the branch it writes to.
    fn render_actions(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (actions, held) = self.node_actions(cx);
        if actions.is_empty() {
            return None;
        }
        let branch = self.session.read(cx).branch.clone();
        let open = self.actions_menu.is_deployed();
        let focus = self.view.read(cx).focus_handle(cx);
        let tip = match &held {
            Some(reason) => reason.clone(),
            None => format!("The graph's stored mutations for this node, run on {branch}"),
        };
        Some(
            PopoverMenu::new("node-actions")
                .anchor(gpui::Anchor::TopRight)
                .with_handle(self.actions_menu.clone())
                .trigger(
                    studio_ui::ds::button("node-actions-button", "Actions")
                        .style(ButtonStyle::Subtle)
                        .end_icon(Icon::new(IconName::ChevronDown))
                        .disabled(held.is_some())
                        .when(!open, |button| {
                            button.tip(studio_ui::ds::tooltip_for_action_in(
                                tip,
                                &ShowActions,
                                &focus,
                            ))
                        }),
                )
                .menu(move |window, cx| {
                    let actions = actions.clone();
                    let branch = branch.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        menu = menu.header(format!("Stored mutations · on {branch}"));
                        for (run, action) in &actions {
                            let run = run.clone();
                            menu = menu.item(
                                ContextMenuEntry::new(format!("{}…", action.label)).handler(
                                    move |window, cx| {
                                        window.dispatch_action(Box::new(run.clone()), cx)
                                    },
                                ),
                            );
                        }
                        menu
                    }))
                }),
        )
    }

    /// The tab's toolbar: its views, then the node's actions, the tree's eye over the
    /// relationships with none and Collapse all.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (hide_empty, any_empty) = self.toolbar;
        studio_ui::ds::toolbar(cx)
            .children(studio_workspace::view_strip(Item::views(self, cx), cx))
            .child(div().flex_1())
            .child(
                studio_ui::ds::toolbar_actions()
                    .children(self.render_actions(cx))
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
            .on_action(cx.listener(Self::show_actions))
            .size_full()
            .bg(studio_ui::ds::surface::document(cx))
            .child(self.render_toolbar(cx))
            // The page scrolls as a list of its own, drawing only what is in view.
            .child(div().flex_1().min_h_0().child(self.view.clone()))
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;
    use crate::test_support::{PEOPLE, session};

    /// Spec 041: a stored mutation whose parameter the person says names a Person is in a
    /// person's page's Actions menu, which ⌥↵ opens; a page held at a commit offers it with
    /// the reason it cannot run there; a type no mutation acts on has no menu.
    #[gpui::test]
    async fn a_pages_actions_are_the_mutations_said_to_act_on_its_type(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
            studio_settings::init_test(cx);
            studio_settings::keep_changes_in_memory(cx);
            cx.bind_keys([gpui::KeyBinding::new(
                "alt-enter",
                ShowActions,
                Some("NodePage"),
            )]);
        });
        let session = session(PEOPLE, cx);
        cx.run_until_parked();
        fn page<'a>(
            session: &Entity<GraphSession>,
            at_commit: Option<&str>,
            cx: &'a mut TestAppContext,
        ) -> (Entity<NodePage>, &'a mut gpui::VisualTestContext) {
            let (session, at_commit) = (session.clone(), at_commit.map(str::to_owned));
            cx.add_window_view(move |_, cx| {
                NodePage::new(session, "Person".into(), "p1".into(), at_commit, None, cx)
            })
        }
        let (head, cx) = page(&session, None, cx);
        cx.run_until_parked();
        head.read_with(cx, |page, cx| {
            assert!(page.node_actions(cx).0.is_empty(), "nothing said yet")
        });
        let graph = session.read_with(cx, |session, _| session.overrides_key());
        cx.update(|_, cx| {
            studio_settings::update(cx, |settings| {
                settings.set_node_param(&graph, "verify_person", "slug", Some("Person".into()))
            })
        });
        cx.run_until_parked();
        head.read_with(cx, |page, cx| {
            let (actions, held) = page.node_actions(cx);
            assert_eq!(held, None);
            let [(run, action)] = actions.as_slice() else {
                panic!("one action: {actions:?}");
            };
            assert_eq!(action.label, "Verify person");
            assert_eq!(
                (run.query.as_str(), run.param.as_str(), run.id.as_str()),
                ("verify_person", "slug", "p1")
            );
        });
        cx.update(|window, cx| {
            let focus = head.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("alt-enter");
        cx.run_until_parked();
        head.read_with(cx, |page, _| {
            assert!(page.actions_menu.is_deployed(), "⌥↵ opens the menu")
        });
        let (held, cx) = page(&session, Some("c1"), cx);
        cx.run_until_parked();
        held.read_with(cx, |page, cx| {
            let (actions, held) = page.node_actions(cx);
            assert_eq!(actions.len(), 1);
            assert!(held.is_some_and(|reason| reason.contains("commit c1")));
        });
        cx.update(|window, cx| {
            let focus = held.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("alt-enter");
        cx.run_until_parked();
        held.read_with(cx, |page, _| {
            assert!(
                !page.actions_menu.is_deployed(),
                "a held page runs no action"
            )
        });
    }
}
