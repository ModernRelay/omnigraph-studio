//! The Nodes panel (spec 004, after Tower's explore view): one type's table in the bottom dock,
//! under the canvas. A click on a type in the canvas browses it here; the table is the ordinary
//! type table, editing included.

use gpui::{AppContext as _, Context, Entity, Render, Subscription, WeakEntity, Window};
use graph_store::GraphSession;
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;

use crate::table::TypeTable;

pub const NODES_PANEL: &str = "nodes";

pub struct NodesPanel {
    workspace: WeakEntity<Workspace>,
    session: Option<Entity<GraphSession>>,
    table: Option<Entity<TypeTable>>,
    _subscription: Option<Subscription>,
}

impl NodesPanel {
    pub fn new(
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
                    this.table = None;
                    cx.notify();
                },
            )
        });
        Self {
            workspace,
            session,
            table: None,
            _subscription: subscription,
        }
    }

    /// Shows `type_name`'s rows, keeping the table when it already shows that type.
    pub fn browse(&mut self, type_name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if self
            .table
            .as_ref()
            .is_some_and(|table| table.read(cx).type_name() == type_name)
        {
            return;
        }
        let workspace = self.workspace.clone();
        let name = type_name.to_owned();
        self.table = Some(cx.new(|cx| TypeTable::new(workspace, session, name, None, window, cx)));
        cx.notify();
    }
}

impl Render for NodesPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.table {
            Some(table) => div().size_full().child(table.clone()).into_any_element(),
            None => studio_ui::ds::empty_state(IconName::Table, "Click a type to browse it")
                .into_any_element(),
        }
    }
}
