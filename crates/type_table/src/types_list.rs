//! The sidebar's "Types" section (spec 004): every node type, then every edge type, with its
//! color and its row count at the branch head. A click opens the type's table.

use gpui::{Context, Entity, Render, SharedString, Subscription, WeakEntity, Window};
use graph_store::{GraphSession, Loadable, SessionEvent};
use studio_actions::graph::{OpenCanvas, OpenOntology, OpenSchema, OpenType};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::Tooltip;
use ui::prelude::*;

use crate::model::format_count;

pub struct TypesList {
    workspace: WeakEntity<Workspace>,
    session: Option<Entity<GraphSession>>,
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
                    .px_3()
                    .py_1()
                    .gap_1()
                    .child(
                        Label::new("Studio could not read the schema.")
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        studio_ui::ds::button("reload-schema", "Try again")
                            .label_size(LabelSize::Small)
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
        let counts_error = match &s.datasets {
            Loadable::Failed(error) => Some(error.clone()),
            _ => None,
        };
        let counts_loading = matches!(s.datasets, Loadable::Loading | Loadable::NotLoaded);
        let branch = s.branch.clone();
        let count_label = |name: &str| -> String {
            match s.count_of(name) {
                Some(n) => format_count(n),
                None if counts_loading => "…".into(),
                None => "—".into(),
            }
        };
        let active = self.active_key(cx);
        let entries: Vec<(String, String, bool)> = schema
            .nodes
            .iter()
            .map(|node| (node.name.clone(), count_label(&node.name), false))
            .chain(
                schema
                    .edges
                    .iter()
                    .map(|edge| (edge.name.clone(), count_label(&edge.name), true)),
            )
            .collect();
        let first_edge = entries.iter().position(|(_, _, edge)| *edge);
        // The graph as a whole first: its ontology, the explorer and the schema, then its types.
        let canvas_open = active
            .as_deref()
            .is_some_and(|key| key.starts_with("canvas:"));
        let view_row = |id: &'static str, label: &'static str, icon: IconName| {
            studio_ui::ds::SidebarRow::new(id, label)
                .start(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
        };
        let mut list = v_flex()
            .w_full()
            .child(
                view_row("view-ontology", "Ontology", IconName::GitGraph)
                    .selected(canvas_open && !self.exploring(cx))
                    .tooltip_fn(|_, cx| {
                        Tooltip::for_action(
                            "The types and relationships as a map",
                            &OpenOntology,
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenOntology), cx)),
            )
            .child(
                view_row("view-explorer", "Explorer", IconName::Crosshair)
                    .selected(canvas_open && self.exploring(cx))
                    .tooltip_fn(|_, cx| {
                        Tooltip::for_action(
                            "Walk the graph from a node: ↵ on a row opens one",
                            &OpenCanvas,
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenCanvas), cx)),
            )
            .child(
                view_row("view-schema", "Schema", IconName::Code)
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
                    }),
            )
            .child(studio_ui::ds::section_label("Types").pt_3());
        for (index, (name, count, is_edge)) in entries.into_iter().enumerate() {
            if Some(index) == first_edge {
                list = list.child(studio_ui::ds::section_label("Relationships").pt_3());
            }
            let selected = active.as_deref() == Some(format!("type:{name}").as_str());
            // A click previews the type in the pane's preview tab; a double click keeps it.
            let action = OpenType {
                type_name: name.clone(),
                at_commit: None,
                filter_id: None,
                via: None,
                preview: true,
            };
            let tooltip = match &counts_error {
                Some(error) => format!("Counts could not be read: {error}"),
                None => format!("Rows at the head of {branch}"),
            };
            list = list.child(
                studio_ui::ds::SidebarRow::new(("type", index), name.clone())
                    .start(studio_ui::kind_swatch(&name, is_edge))
                    .end(studio_ui::ds::sidebar_count(count))
                    .muted(is_edge)
                    .selected(selected)
                    .tooltip(tooltip)
                    .on_click(move |event, window, cx| {
                        let mut action = action.clone();
                        action.preview = event.click_count() < 2;
                        window.dispatch_action(Box::new(action), cx)
                    }),
            );
        }
        list.into_any_element()
    }
}
