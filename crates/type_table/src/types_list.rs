//! The sidebar's "Types" section (spec 004): every node type, then every edge type, with its
//! color and its row count at the branch head. A click opens the type's table.

use std::sync::Arc;

use gpui::{
    Context, Entity, ListAlignment, ListState, Render, SharedString, Subscription, WeakEntity,
    Window, list,
};
use graph_store::{GraphSession, Loadable, SessionEvent};
use studio_actions::graph::{OpenCanvas, OpenOntology, OpenSchema, OpenType};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::Tooltip;
use ui::prelude::*;

use crate::TABLE_KEY_PREFIX;

/// One row of the sidebar's list.
enum Row {
    Ontology,
    Canvas,
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
    /// The active tab's key as this frame draws the rows, taken once for all of them.
    frame_active: Option<SharedString>,
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

    fn active_key(&self, cx: &App) -> Option<SharedString> {
        let workspace = self.workspace.upgrade()?;
        workspace.read(cx).active_item()?.dedupe_key(cx)
    }

    /// Whether the active tab shows what `action` opens: the canvas's ontology and its walk
    /// from a node are one tab, which only the tab can tell apart.
    fn active_shows(&self, action: &dyn gpui::Action, cx: &App) -> bool {
        self.workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .active_item()
                .is_some_and(|item| item.opened_by(action, cx))
        })
    }
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
        // The graph as a whole first: its ontology, the explorer and the schema, then its types.
        if self.rows_for.as_deref() != Some(&*schema.source) {
            let mut rows = vec![
                Row::Ontology,
                Row::Canvas,
                Row::Schema,
                Row::Section("Types"),
            ];
            rows.extend(schema.nodes.iter().map(|node| Row::Type {
                name: node.name.clone(),
                is_edge: false,
            }));
            if !schema.edges.is_empty() {
                rows.push(Row::Section("Relationships"));
            }
            rows.extend(schema.edges.iter().map(|edge| Row::Type {
                name: edge.name.clone(),
                is_edge: true,
            }));
            self.list.reset(rows.len());
            self.rows = rows;
            self.rows_for = Some(schema.source.clone());
        }
        self.frame_active = self.active_key(cx);
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
        let active = self.frame_active.as_deref();
        let view_row = |id: &'static str, label: &'static str, icon: IconName| {
            studio_ui::ds::SidebarRow::new(id, label).icon(icon)
        };
        match row {
            Row::Ontology => view_row("view-ontology", "Ontology", IconName::GitGraph)
                .selected(self.active_shows(&OpenOntology, cx))
                .tooltip_fn(|_, cx| {
                    Tooltip::for_action(
                        "The types by domain, and each type's relationships",
                        &OpenOntology,
                        cx,
                    )
                })
                .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenOntology), cx))
                .into_any_element(),
            Row::Canvas => view_row("view-canvas", "Canvas", IconName::Crosshair)
                .selected(self.active_shows(&OpenCanvas, cx))
                .tooltip_fn(|_, cx| {
                    Tooltip::for_action(
                        "Walk the graph from a node: ↵ on a row opens one",
                        &OpenCanvas,
                        cx,
                    )
                })
                .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenCanvas), cx))
                .into_any_element(),
            Row::Schema => view_row("view-schema", "Schema", IconName::Code)
                .selected(active == Some("schema"))
                .tooltip_fn(|_, cx| {
                    Tooltip::for_action(
                        "The schema's source, read-only",
                        &OpenSchema::default(),
                        cx,
                    )
                })
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(OpenSchema::default()), cx)
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
                // The type's table at the head is the active tab ([`crate::table_key`]).
                let selected = active.and_then(|key| key.strip_prefix(TABLE_KEY_PREFIX))
                    == Some(name.as_str());
                // A click previews the type in the pane's preview tab; a double click keeps it.
                let action = OpenType {
                    type_name: name.clone(),
                    at_commit: None,
                    filter_id: None,
                    via: None,
                    preview: true,
                };
                studio_ui::ds::SidebarRow::new(("type", ix), name.clone())
                    .start(studio_ui::kind_swatch(name, *is_edge, cx))
                    .end(studio_ui::ds::sidebar_count(count))
                    .muted(*is_edge)
                    .selected(selected)
                    .tooltip(tooltip)
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
