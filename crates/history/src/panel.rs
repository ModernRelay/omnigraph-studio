//! History in the bottom dock (spec 009): the window's branch, rows only, rebuilt when the
//! window opens another graph. A commit opens in its own tab.

use gpui::{
    AppContext as _, Context, Entity, FocusHandle, Focusable as _, Render, Subscription,
    WeakEntity, Window,
};
use graph_store::GraphSession;
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;

use crate::view::HistoryView;

pub(crate) struct HistoryPanel {
    session: Option<Entity<GraphSession>>,
    /// Made when the panel is first drawn: a window whose History is never shown reads no
    /// history.
    view: Option<Entity<HistoryView>>,
    /// The keyboard's place until the list exists, handed to it when it does.
    focus_handle: FocusHandle,
    _subscription: Option<Subscription>,
}

impl HistoryPanel {
    /// The list's focus, which takes the keyboard when the panel is shown from its button.
    pub(crate) fn focus(&self, cx: &App) -> Option<FocusHandle> {
        Some(
            self.view
                .as_ref()
                .map_or_else(|| self.focus_handle.clone(), |view| view.focus_handle(cx)),
        )
    }
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
                |this, workspace, event: &WorkspaceEvent, _window, cx| {
                    let WorkspaceEvent::SessionChanged = event;
                    this.session = workspace.read(cx).session().cloned();
                    this.view = None;
                    cx.notify();
                },
            )
        });
        Self {
            session,
            view: None,
            focus_handle: cx.focus_handle(),
            _subscription: subscription,
        }
    }
}

impl Render for HistoryPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.view.is_none()
            && let Some(session) = self.session.clone()
        {
            let view = cx.new(|cx| HistoryView::panel(session, window, cx));
            if self.focus_handle.is_focused(window) {
                window.focus(&view.focus_handle(cx), cx);
            }
            self.view = Some(view);
        }
        match &self.view {
            Some(view) => div().size_full().child(view.clone()).into_any_element(),
            None => div()
                .size_full()
                .track_focus(&self.focus_handle)
                .child(studio_ui::ds::empty_state(
                    IconName::HistoryRerun,
                    "No graph open",
                ))
                .into_any_element(),
        }
    }
}
