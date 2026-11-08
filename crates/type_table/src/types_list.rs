//! The sidebar's Data panel (spec 020): the schema, the graph's saved views (spec 024), every node
//! type, then every edge type, each type with its color and its row count at the branch head.
//! Every row is a thing: a click opens it, and the tab's view strip offers its other shapes. A
//! saved view is a thing ("active customers"); Table, Tree and Canvas have no row.

use std::sync::Arc;

use editor::{Editor, EditorEvent};
use gpui::{
    Context, Entity, Focusable as _, ListAlignment, ListState, Render, Subscription, WeakEntity,
    Window, list,
};
use gq_language::SchemaModel;
use graph_store::{GraphSession, Loadable, SessionEvent};
use studio_actions::Thing;
use studio_actions::graph::{OpenMap, OpenSavedView, OpenType, RestoreSavedView};
use studio_settings::StudioSettings;
use studio_ui::ds::Prose as _;
use studio_ui::ds::Tipped;
use studio_ui::menu::ContextMenu;
use studio_workspace::{ToastKind, Workspace, WorkspaceEvent};
use ui::prelude::*;
use ui::right_click_menu;

use crate::table::TypeTable;

/// What the rows were made from: the schema's text and the views' ids and names.
type RowsFor = (Arc<str>, Vec<(String, String)>);

/// One row of the sidebar's list.
#[derive(Debug, PartialEq)]
enum Row {
    Schema,
    Section(&'static str),
    /// The Views section's label, with its "+".
    ViewsSection,
    /// A saved view, by id and name.
    View {
        id: String,
        name: String,
    },
    Type {
        name: String,
        is_edge: bool,
    },
}

pub struct TypesList {
    workspace: WeakEntity<Workspace>,
    session: Option<Entity<GraphSession>>,
    /// The rows for the schema they were made from: a schema can hold hundreds of types, so
    /// the list draws only the rows in view.
    rows: Vec<Row>,
    rows_for: Option<RowsFor>,
    /// The view being renamed, and its name field.
    renaming: Option<(String, Entity<Editor>, Subscription)>,
    /// What the active tab shows as this frame draws the rows, taken once for all of them.
    frame_active: Option<Thing>,
    /// The saved view the active tab is, as this frame draws the rows.
    frame_view: Option<String>,
    list: ListState,
    _workspace_subscription: Option<Subscription>,
    _session_subscription: Option<Subscription>,
    /// Views saved, renamed or deleted anywhere: the list follows.
    _settings_subscription: Subscription,
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
            renaming: None,
            frame_active: None,
            frame_view: None,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            _workspace_subscription: workspace_subscription,
            _session_subscription: None,
            _settings_subscription: cx.observe_global::<StudioSettings>(|_, cx| cx.notify()),
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

    /// The type table the active tab is, if it is one.
    fn active_table(&self, cx: &App) -> Option<Entity<TypeTable>> {
        let workspace = self.workspace.upgrade()?;
        workspace
            .read(cx)
            .active_item()?
            .to_any()
            .downcast::<TypeTable>()
            .ok()
    }

    /// The graph's settings key, which its views are kept under.
    fn graph(&self, cx: &App) -> Option<String> {
        Some(self.session.as_ref()?.read(cx).overrides_key())
    }

    // ---- views ----

    fn start_rename(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(graph) = self.graph(cx) else { return };
        let Some(name) = StudioSettings::get(cx)
            .view(&graph, &id)
            .map(|v| v.name.clone())
        else {
            return;
        };
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(name, window, cx);
            editor.select_all(&Default::default(), window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        // Leaving the field keeps what was typed, as renaming in a list does.
        let subscription =
            cx.subscribe_in(&editor, window, |this, _, event: &EditorEvent, _, cx| {
                if let EditorEvent::Blurred = event {
                    this.commit_rename(cx);
                }
            });
        self.renaming = Some((id, editor, subscription));
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some((id, editor, _)) = self.renaming.take() else {
            return;
        };
        cx.notify();
        let Some(graph) = self.graph(cx) else { return };
        let name = editor.read(cx).text(cx).trim().to_owned();
        let settings = StudioSettings::get(cx);
        if settings
            .view(&graph, &id)
            .is_none_or(|view| view.name == name)
            || name.is_empty()
        {
            return;
        }
        if settings.view_name_taken(&graph, &name, Some(&id)) {
            self.toast(
                ToastKind::Error,
                format!("Not renamed: another view is named “{name}”."),
                cx,
            );
            return;
        }
        studio_settings::update(cx, move |settings| {
            settings.rename_view(&graph, &id, &name);
        });
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        cx.notify();
    }

    fn duplicate(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(graph) = self.graph(cx) else { return };
        studio_settings::update(cx, move |settings| {
            settings.duplicate_view(&graph, &id);
        });
    }

    /// Deletes a view, with Undo on the toast: a view is a setting, so Undo puts it back.
    fn delete(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(graph) = self.graph(cx) else { return };
        let views = StudioSettings::get(cx).graph_views(&graph);
        let Some(at) = views.iter().position(|view| view.key() == id) else {
            return;
        };
        let view = views[at].clone();
        let name = view.name.clone();
        let restore = RestoreSavedView {
            graph: graph.clone(),
            at,
            view: serde_json::to_value(&view).unwrap_or_default(),
        };
        studio_settings::update(cx, move |settings| {
            settings.remove_view(&graph, &id);
        });
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast_with_action(
                    ToastKind::Success,
                    format!("Deleted the view “{name}”."),
                    "Undo",
                    Box::new(restore),
                    cx,
                )
            });
        }
    }

    /// Saves what the active table shows as a new view.
    fn new_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(table) = self.active_table(cx) {
            table.update(cx, |table, cx| table.open_save_view(window, cx));
        }
    }

    fn toast(&self, kind: ToastKind, message: String, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.show_toast(kind, message, cx));
        }
    }
}

/// The rows for a schema and the graph's views: the schema itself, the views by name, then the
/// types.
fn rows_of(schema: &SchemaModel, views: &[(String, String)]) -> Vec<Row> {
    let mut rows = vec![Row::Schema];
    if !views.is_empty() {
        rows.push(Row::ViewsSection);
        rows.extend(views.iter().map(|(id, name)| Row::View {
            id: id.clone(),
            name: name.clone(),
        }));
    }
    rows.push(Row::Section("Node types"));
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
        let views: Vec<(String, String)> = StudioSettings::get(cx)
            .graph_views(&s.overrides_key())
            .iter()
            .map(|view| (view.key(), view.name.clone()))
            .collect();
        let made_for = (schema.source.clone(), views);
        if self.rows_for.as_ref() != Some(&made_for) {
            let rows = rows_of(&schema, &made_for.1);
            self.list.reset(rows.len());
            self.rows = rows;
            self.rows_for = Some(made_for);
        }
        self.frame_active = self.active_thing(cx);
        self.frame_view = self
            .active_table(cx)
            .and_then(|table| table.read(cx).view_id().map(str::to_owned));
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
            Row::ViewsSection => {
                let table = self.active_table(cx);
                let from = table
                    .as_ref()
                    .map(|table| table.read(cx).type_name().to_owned());
                let tip = match &from {
                    Some(type_name) => format!("New view from this table of {type_name}"),
                    None => "Open a type's table to save a view of it".to_owned(),
                };
                h_flex()
                    .justify_between()
                    .items_center()
                    .pr(px(studio_ui::ds::space::SM))
                    .child(studio_ui::ds::section_label("Views").pt(px(studio_ui::ds::space::LG)))
                    .child(
                        div().pt(px(studio_ui::ds::space::LG)).child(
                            studio_ui::ds::row_icon_button("new-view", IconName::Plus)
                                .disabled(from.is_none())
                                .tip(studio_ui::ds::tooltip(tip))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.new_view(window, cx)),
                                ),
                        ),
                    )
                    .into_any_element()
            }
            Row::View { id, name } => self.render_view_row(ix, id, name, cx),
            Row::Type { name, is_edge } => {
                let s = session.read(cx);
                let entity = if *is_edge {
                    graph_store::EntityKind::Edge
                } else {
                    graph_store::EntityKind::Node
                };
                let count = match s.count_of(entity, name) {
                    Some(n) => studio_ui::group_count(n),
                    None if matches!(s.counts, Loadable::Loading | Loadable::NotLoaded) => {
                        "…".into()
                    }
                    None => "—".into(),
                };
                let tooltip = match (&s.counts, s.counts_stale()) {
                    (Loadable::Failed(error), _) => format!("Counts could not be read: {error}"),
                    (_, Some(error)) => format!(
                        "Rows as last read on {}: reading them again failed ({error}). They \
                         are read again shortly.",
                        s.branch
                    ),
                    _ => format!("Rows at the head of {}", s.branch),
                };
                // A view's tab lights its view's row, not its type's.
                let selected = self.frame_view.is_none()
                    && matches!(&self.frame_active, Some(Thing::Type(active)) if active == name);
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

impl TypesList {
    /// A saved view's row: its name, lit while the active tab is it; a click previews it, a
    /// double click keeps it, and a right click opens, renames, duplicates or deletes it.
    fn render_view_row(
        &self,
        ix: usize,
        id: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some((_, editor, _)) = self.renaming.as_ref().filter(|(r, _, _)| r == id) {
            return h_flex()
                .h(px(studio_ui::ds::size::SIDEBAR_ROW))
                .mx(px(studio_ui::ds::space::SM))
                .px(px(studio_ui::ds::space::SM))
                .items_center()
                .rounded(px(studio_ui::ds::radii(cx).sm))
                .bg(studio_ui::ds::surface::selected_chrome(cx))
                .key_context("ViewName")
                .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.commit_rename(cx)))
                .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.cancel_rename(cx)))
                .child(div().flex_1().prose().child(editor.clone()))
                .into_any_element();
        }
        let selected = self.frame_view.as_deref() == Some(id);
        let open = OpenSavedView {
            id: id.to_owned(),
            preview: true,
        };
        let row = {
            let (name, open) = (name.to_owned(), open.clone());
            move |_: &mut Window, _: &mut App| {
                let open = open.clone();
                studio_ui::ds::SidebarRow::new(("view", ix), name.clone())
                    .icon(IconName::Table)
                    .selected(selected)
                    .tip(studio_ui::ds::tooltip(
                        "A saved view: right-click to rename, duplicate or delete it",
                    ))
                    .on_click(move |event, window, cx| {
                        let mut action = open.clone();
                        action.preview = event.click_count() < 2;
                        window.dispatch_action(Box::new(action), cx)
                    })
            }
        };
        let list = cx.entity().downgrade();
        let id = id.to_owned();
        right_click_menu(SharedString::from(format!("view-menu-{id}")))
            .trigger(move |_, window, cx| row(window, cx))
            .menu(move |window, cx| {
                let (list, id) = (list.clone(), id.clone());
                ContextMenu::build(window, cx, move |menu, _, _| {
                    let keep = OpenSavedView {
                        id: id.clone(),
                        preview: false,
                    };
                    let on =
                        |f: fn(&mut TypesList, String, &mut Window, &mut Context<TypesList>)| {
                            let (list, id) = (list.clone(), id.clone());
                            move |window: &mut Window, cx: &mut App| {
                                list.update(cx, |this, cx| f(this, id.clone(), window, cx))
                                    .ok();
                            }
                        };
                    menu.entry("Open as Table", None, move |window, cx| {
                        window.dispatch_action(Box::new(keep.clone()), cx)
                    })
                    .separator()
                    .entry(
                        "Rename",
                        None,
                        on(|this, id, window, cx| this.start_rename(id, window, cx)),
                    )
                    .entry(
                        "Duplicate",
                        None,
                        on(|this, id, _, cx| this.duplicate(id, cx)),
                    )
                    .separator()
                    .entry(
                        "Delete",
                        None,
                        on(|this, id, _, cx| this.delete(id, cx)),
                    )
                })
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A graph's saved views list first, by name, under their own label; with none, the label
    /// does not show (spec 024).
    #[test]
    fn saved_views_list_before_the_types() {
        let schema = SchemaModel::parse("node Person {\n  slug: String @key\n}\n")
            .expect("the schema parses");
        let views = [("v1".to_owned(), "Founders".to_owned())];
        assert_eq!(
            rows_of(&schema, &views),
            vec![
                Row::Schema,
                Row::ViewsSection,
                Row::View {
                    id: "v1".into(),
                    name: "Founders".into()
                },
                Row::Section("Node types"),
                Row::Type {
                    name: "Person".into(),
                    is_edge: false
                },
            ]
        );
    }

    /// Every row names a thing: the schema, or a type. Table, Tree and Canvas
    /// have no row (spec 020).
    #[test]
    fn every_row_is_a_thing() {
        let schema = SchemaModel::parse(
            "node Person {\n  slug: String @key\n}\nedge Knows: Person -> Person\n",
        )
        .expect("the schema parses");
        assert_eq!(
            rows_of(&schema, &[]),
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
