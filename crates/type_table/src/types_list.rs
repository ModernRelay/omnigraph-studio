//! The sidebar's Data panel (spec 020): the schema, every node type, then every edge type, each
//! type with its color and its row count at the branch head. Every row is a thing, never a view:
//! a click opens the thing, and the tab's view strip offers its other views.

use std::sync::Arc;

use gpui::{
    Context, Entity, ListAlignment, ListState, Render, Subscription, WeakEntity, Window, list,
};
use gq_language::SchemaModel;
use graph_store::{GraphSession, Loadable, SessionEvent};
use studio_actions::Thing;
use studio_actions::graph::{OpenMap, OpenType};
use studio_ui::ds::Tipped;
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;

/// One row of the sidebar's list.
#[derive(Debug, PartialEq)]
enum Row {
    Schema,
    Section(&'static str),
    Type { name: String, is_edge: bool },
}

pub struct TypesList {
    workspace: WeakEntity<Workspace>,
    session: Option<Entity<GraphSession>>,
    /// The rows for the schema they were made from: a schema can hold hundreds of types, so
    /// the list draws only the rows in view.
    rows: Vec<Row>,
    rows_for: Option<Arc<str>>,
    /// What the active tab shows as this frame draws the rows, taken once for all of them.
    frame_active: Option<Thing>,
    list: ListState,
    _workspace_subscription: Option<Subscription>,
    _session_subscription: Option<Subscription>,
}

impl TypesList {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        session: Option<Entity<GraphSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_subscription = workspace.upgrade().map(|workspace| {
            cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, _, cx| match event {
                    WorkspaceEvent::SessionChanged => {
                        let session = workspace.read(cx).session().cloned();
                        this.watch(session, cx);
                    }
                    WorkspaceEvent::ActiveItemChanged => cx.notify(),
                },
            )
        });
        let mut this = Self {
            workspace,
            session: None,
            rows: Vec::new(),
            rows_for: None,
            frame_active: None,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            _workspace_subscription: workspace_subscription,
            _session_subscription: None,
        };
        this.watch(session, cx);
        this
    }

    fn watch(&mut self, session: Option<Entity<GraphSession>>, cx: &mut Context<Self>) {
        self._session_subscription = session.as_ref().map(|session| {
            cx.subscribe(session, |_, _, event: &SessionEvent, cx| match event {
                SessionEvent::SchemaChanged
                | SessionEvent::SchemaMoved
                | SessionEvent::CountsChanged
                | SessionEvent::BranchChanged => cx.notify(),
                _ => {}
            })
        });
        self.session = session;
        cx.notify();
    }

    fn active_thing(&self, cx: &App) -> Option<Thing> {
        let workspace = self.workspace.upgrade()?;
        workspace.read(cx).active_item()?.thing(cx)
    }
}

/// The rows for a schema: the schema itself, then its types.
fn rows_of(schema: &SchemaModel) -> Vec<Row> {
    let mut rows = vec![Row::Schema, Row::Section("Node types")];
    rows.extend(schema.nodes.iter().map(|node| Row::Type {
        name: node.name.clone(),
        is_edge: false,
    }));
    if !schema.edges.is_empty() {
        rows.push(Row::Section("Edge types"));
    }
    rows.extend(schema.edges.iter().map(|edge| Row::Type {
        name: edge.name.clone(),
        is_edge: true,
    }));
    rows
}

impl Render for TypesList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session) = self.session.clone() else {
            return v_flex().into_any_element();
        };
        let s = session.read(cx);
        // The window's tab says where to sign in and offers it; this says why there are no
        // types, rather than the refused read's 401 and a Try again that would be refused too.
        if !matches!(s.schema, Loadable::Loaded(_))
            && let Some(prompt) = s.connection.read(cx).sign_in_prompt()
        {
            return studio_ui::ds::sidebar_note(format!(
                "Sign in to {} to see its types.",
                prompt.server
            ))
            .into_any_element();
        }
        let schema = match &s.schema {
            Loadable::Loaded(schema) => schema.clone(),
            Loadable::Failed(error) => {
                let error = error.clone();
                return v_flex()
                    .px(px(studio_ui::ds::space::LG))
                    .py(px(studio_ui::ds::space::XS))
                    .gap(px(studio_ui::ds::space::XS))
                    .child(studio_ui::ds::text::error(
                        "Studio could not read the schema.",
                    ))
                    .child(studio_ui::ds::text::meta(error))
                    .child(
                        studio_ui::ds::button("reload-schema", "Try again")
                            .style(ButtonStyle::Subtle)
                            .on_click(move |_, _, cx| {
                                session.update(cx, |session, cx| session.reload_schema(cx))
                            }),
                    )
                    .into_any_element();
            }
            _ => {
                return studio_ui::ds::sidebar_note("Reading types…").into_any_element();
            }
        };
        if self.rows_for.as_deref() != Some(&*schema.source) {
            let rows = rows_of(&schema);
            self.list.reset(rows.len());
            self.rows = rows;
            self.rows_for = Some(schema.source.clone());
        }
        self.frame_active = self.active_thing(cx);
        list(
            self.list.clone(),
            // Each row in a column of the list's width, as the rows sat in one before.
            cx.processor(|this, ix: usize, _, cx| {
                v_flex()
                    .w_full()
                    .child(this.render_row(ix, cx))
                    .into_any_element()
            }),
        )
        .size_full()
        .pb(px(studio_ui::ds::space::MD))
        .into_any_element()
    }
}

impl TypesList {
    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let (Some(row), Some(session)) = (self.rows.get(ix), &self.session) else {
            return div().into_any_element();
        };
        match row {
            Row::Schema => studio_ui::ds::SidebarRow::new("schema", "Schema")
                .icon(IconName::Code)
                .selected(self.frame_active == Some(Thing::Schema))
                .tip_fn(|_, cx| {
                    studio_ui::ds::tooltip_for_action_view(
                        "The graph's types and how they relate",
                        &OpenMap::default(),
                        cx,
                    )
                })
                // A click previews the map in the pane's preview tab; a double click keeps it.
                .on_click(|event, window, cx| {
                    let preview = event.click_count() < 2;
                    window.dispatch_action(Box::new(OpenMap { preview }), cx)
                })
                .into_any_element(),
            Row::Section(title) => studio_ui::ds::section_label(*title)
                .pt(px(studio_ui::ds::space::LG))
                .into_any_element(),
            Row::Type { name, is_edge } => {
                let s = session.read(cx);
                let entity = if *is_edge {
                    graph_store::EntityKind::Edge
                } else {
                    graph_store::EntityKind::Node
                };
                let count = match s.count_of(entity, name) {
                    Some(n) => studio_ui::group_count(n),
                    None if matches!(s.snapshot, Loadable::Loading | Loadable::NotLoaded) => {
                        "…".into()
                    }
                    None => "—".into(),
                };
                let tooltip = match (&s.snapshot, s.counts_stale()) {
                    (Loadable::Failed(error), _) => format!("Counts could not be read: {error}"),
                    (_, Some(error)) => format!(
                        "Rows as last read on {}: reading them again failed ({error}). They \
                         are read again shortly.",
                        s.branch
                    ),
                    _ => format!("Rows at the head of {}", s.branch),
                };
                let selected =
                    matches!(&self.frame_active, Some(Thing::Type(active)) if active == name);
                // A click previews the type in the pane's preview tab; a double click keeps it.
                let action = OpenType {
                    type_name: name.clone(),
                    at_commit: None,
                    filter_id: None,
                    via: None,
                    preview: true,
                    filters: None,
                };
                studio_ui::ds::SidebarRow::new(("type", ix), name.clone())
                    .start(studio_ui::kind_swatch(name, *is_edge, cx))
                    .end(studio_ui::ds::sidebar_count(count))
                    .muted(*is_edge)
                    .selected(selected)
                    .tip(studio_ui::ds::tooltip(tooltip))
                    .on_click(move |event, window, cx| {
                        let mut action = action.clone();
                        action.preview = event.click_count() < 2;
                        window.dispatch_action(Box::new(action), cx)
                    })
                    .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row names a thing: the schema, or a type. No row is a view (spec 020).
    #[test]
    fn every_row_is_a_thing() {
        let schema = SchemaModel::parse(
            "node Person {\n  slug: String @key\n}\nedge Knows: Person -> Person\n",
        )
        .expect("the schema parses");
        assert_eq!(
            rows_of(&schema),
            vec![
                Row::Schema,
                Row::Section("Node types"),
                Row::Type {
                    name: "Person".into(),
                    is_edge: false
                },
                Row::Section("Edge types"),
                Row::Type {
                    name: "Knows".into(),
                    is_edge: true
                },
            ]
        );
    }
}
