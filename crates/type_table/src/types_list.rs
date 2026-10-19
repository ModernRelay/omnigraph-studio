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

use crate::model::format_count;

/// One row of the sidebar's list.
enum Row {
    Ontology,
    Explorer,
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
                },
            )
        });
        let mut this = Self {
            workspace,
            session: None,
            rows: Vec::new(),
            rows_for: None,
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

    /// Whether the active tab is the canvas showing an exploration rather than the ontology.
    fn exploring(&self, cx: &App) -> bool {
        self.workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .active_item()
                .is_some_and(|item| item.tab_title(cx) == "Explorer")
        })
    }
}

impl Render for TypesList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session) = self.session.clone() else {
            return v_flex().into_any_element();
        };
        let s = session.read(cx);
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
                Row::Explorer,
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
        let active = self.active_key(cx);
        let canvas_open = active
            .as_deref()
            .is_some_and(|key| key.starts_with("canvas:"));
        let view_row = |id: &'static str, label: &'static str, icon: IconName| {
            studio_ui::ds::SidebarRow::new(id, label).icon(icon)
        };
        match row {
            Row::Ontology => view_row("view-ontology", "Ontology", IconName::GitGraph)
                .selected(canvas_open && !self.exploring(cx))
                .tooltip_fn(|_, cx| {
                    Tooltip::for_action("The types and relationships as a map", &OpenOntology, cx)
                })
                .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenOntology), cx))
                .into_any_element(),
            Row::Explorer => view_row("view-explorer", "Explorer", IconName::Crosshair)
                .selected(canvas_open && self.exploring(cx))
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
                .selected(active.as_deref() == Some("schema"))
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
                    Some(n) => format_count(n),
                    None if matches!(s.datasets, Loadable::Loading | Loadable::NotLoaded) => {
                        "…".into()
                    }
                    None => "—".into(),
                };
                let tooltip = match &s.datasets {
                    Loadable::Failed(error) => format!("Counts could not be read: {error}"),
                    _ => format!("Rows at the head of {}", s.branch),
                };
                let selected = active.as_deref() == Some(crate::table_key(name, None).as_str());
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
