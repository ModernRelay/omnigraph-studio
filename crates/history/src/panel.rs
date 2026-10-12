//! History in the bottom dock (spec 009): the window's branch, rows only, rebuilt when the
//! window opens another graph. A commit opens in its own tab.

use gpui::{AppContext as _, Context, Entity, Render, Subscription, WeakEntity, Window};
use graph_store::GraphSession;
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;

use crate::view::HistoryView;

pub(crate) struct HistoryPanel {
    view: Option<Entity<HistoryView>>,
    _subscription: Option<Subscription>,
}

impl HistoryPanel {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        session: Option<Entity<GraphSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = workspace.upgrade().map(|workspace| {
            cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, window, cx| {
                    let WorkspaceEvent::SessionChanged = event;
                    let session = workspace.read(cx).session().cloned();
                    this.view =
                        session.map(|session| cx.new(|cx| HistoryView::panel(session, window, cx)));
                    cx.notify();
                },
            )
        });
        Self {
            view: session.map(|session| cx.new(|cx| HistoryView::panel(session, window, cx))),
            _subscription: subscription,
        }
    }
}

impl Render for HistoryPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.view {
            Some(view) => div().size_full().child(view.clone()).into_any_element(),
            None => {
                studio_ui::ds::empty_state(IconName::GitGraph, "No graph open").into_any_element()
            }
        }
    }
}
