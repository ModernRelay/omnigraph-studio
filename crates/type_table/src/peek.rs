//! The peek in the inspector (spec 004): one row's properties, copyable, and for a node its
//! relationship groups with counts and first neighbours. Every read is pinned to the commit the
//! row was read at, or to the first read when the node was opened by id (constitution II).

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, ClipboardItem, Context, Entity, Render, SharedString, Task, Window};
use gq_language::{Direction, RelationshipGroup, SchemaModel, builder};
use graph_store::{GraphSession, Loadable};
use serde_json::{Map, Value};
use studio_actions::graph::OpenNode;
use ui::Tooltip;
use ui::prelude::*;

use crate::model::{FROM_COLUMN, TO_COLUMN, display_value, format_count, node_rows, short_commit};

/// First neighbours shown under an expanded relationship group.
pub const NEIGHBOUR_LIMIT: u32 = 8;

/// Debounce before relationship counts are read, so moving the cursor quickly through rows
/// does not send a burst of reads.
const COUNT_DELAY: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, PartialEq)]
pub enum PeekSubject {
    Node {
        type_name: String,
        id: String,
    },
    Edge {
        edge_type: String,
        id: String,
        from_type: String,
        to_type: String,
    },
}

impl PeekSubject {
    fn type_name(&self) -> &str {
        match self {
            PeekSubject::Node { type_name, .. } => type_name,
            PeekSubject::Edge { edge_type, .. } => edge_type,
        }
    }

    fn id(&self) -> &str {
        match self {
            PeekSubject::Node { id, .. } | PeekSubject::Edge { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Neighbour {
    pub id: String,
    pub label: Option<String>,
}

struct Group {
    group: RelationshipGroup,
    count: Loadable<i64>,
    expanded: bool,
    neighbours: Loadable<Vec<Neighbour>>,
    _task: Option<Task<()>>,
}

pub struct NodePeek {
    session: Entity<GraphSession>,
    schema: Option<Arc<SchemaModel>>,
    subject: PeekSubject,
    snapshot: Option<String>,
    values: Loadable<Map<String, Value>>,
    groups: Vec<Group>,
    copied: Option<String>,
    _tasks: Vec<Task<()>>,
}

impl NodePeek {
    /// A peek of a row the caller already holds (`values`) read at `snapshot`, or of a node
    /// opened by id (`values` is `None`), which is read first. `debounce` delays the
    /// relationship counts, for a peek that follows a moving cursor.
    pub fn new(
        session: Entity<GraphSession>,
        subject: PeekSubject,
        values: Option<Map<String, Value>>,
        snapshot: Option<String>,
        debounce: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let schema = session.read(cx).schema_model();
        let mut this = Self {
            session,
            schema,
            subject,
            snapshot,
            values: Loadable::NotLoaded,
            groups: Vec::new(),
            copied: None,
            _tasks: Vec::new(),
        };
        match values {
            Some(values) => {
                this.values = Loadable::Loaded(values);
                this.load_groups(debounce, cx);
            }
            None => this.load_node(cx),
        }
        this
    }

    pub fn subject(&self) -> &PeekSubject {
        &self.subject
    }

    fn load_node(&mut self, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, id } = &self.subject else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            self.values = Loadable::Failed("The schema is still loading.".into());
            return;
        };
        let query = match builder::node_by_id(&schema, type_name, id) {
            Ok(query) => query,
            Err(error) => {
                self.values = Loadable::Failed(error.to_string());
                return;
            }
        };
        self.values = Loadable::Loading;
        let snapshot = self.snapshot.clone();
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot, cx));
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(output) => {
                        if this.snapshot.is_none() {
                            this.snapshot = output.graph_commit_id.clone();
                        }
                        match node_rows(&output.rows).into_iter().next() {
                            Some(row) => {
                                this.values = Loadable::Loaded(row.values);
                                this.load_groups(false, cx);
                            }
                            None => {
                                this.values = Loadable::Failed(
                                    "This node does not exist at this commit.".into(),
                                );
                            }
                        }
                    }
                    Err(error) => this.values = Loadable::Failed(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn load_groups(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, id } = self.subject.clone() else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            return;
        };
        self.groups = schema
            .relationships_of(&type_name)
            .into_iter()
            .map(|group| Group {
                group,
                count: Loadable::Loading,
                expanded: false,
                neighbours: Loadable::NotLoaded,
                _task: None,
            })
            .collect();
        for index in 0..self.groups.len() {
            let query = match builder::degree(&schema, &type_name, &self.groups[index].group, &id) {
                Ok(query) => query,
                Err(error) => {
                    self.groups[index].count = Loadable::Failed(error.to_string());
                    continue;
                }
            };
            let session = self.session.downgrade();
            let snapshot = self.snapshot.clone();
            self.groups[index]._task = Some(cx.spawn(async move |this, cx| {
                if debounce {
                    cx.background_executor().timer(COUNT_DELAY).await;
                }
                let Ok(read) = session.update(cx, |session, cx| session.read(&query, snapshot, cx))
                else {
                    return;
                };
                let result = read.await;
                this.update(cx, |this, cx| {
                    if let Some(group) = this.groups.get_mut(index) {
                        group.count = match result {
                            Ok(output) => match crate::model::count_of(&output.rows) {
                                Some(n) => Loadable::Loaded(n),
                                None => Loadable::Failed("No count in the response".into()),
                            },
                            Err(error) => Loadable::Failed(error.to_string()),
                        };
                        // A small group shows its neighbours without a click.
                        let small = matches!(group.count, Loadable::Loaded(n)
                            if n > 0 && n <= i64::from(NEIGHBOUR_LIMIT));
                        if small && !group.expanded {
                            this.toggle_group(index, cx);
                        }
                    }
                    cx.notify();
                })
                .ok();
            }));
        }
        cx.notify();
    }

    fn toggle_group(&mut self, index: usize, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, id } = self.subject.clone() else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let Some(group) = self.groups.get_mut(index) else {
            return;
        };
        group.expanded = !group.expanded;
        if !group.expanded || !matches!(group.neighbours, Loadable::NotLoaded) {
            cx.notify();
            return;
        }
        let query = match builder::neighbours(
            &schema,
            &type_name,
            &group.group,
            &id,
            None,
            NEIGHBOUR_LIMIT,
        ) {
            Ok(query) => query,
            Err(error) => {
                group.neighbours = Loadable::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        group.neighbours = Loadable::Loading;
        let snapshot = self.snapshot.clone();
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot, cx));
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                if let Some(group) = this.groups.get_mut(index) {
                    group.neighbours = match result {
                        Ok(output) => Loadable::Loaded(neighbours_from(&output.rows)),
                        Err(error) => Loadable::Failed(error.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(group) = self.groups.get_mut(index) {
            group._task = Some(task);
        }
        cx.notify();
    }

    fn copy(&mut self, name: String, value: &Value, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(crate::model::copy_text(value)));
        self.copied = Some(name);
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            this.update(cx, |this, cx| {
                this.copied = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// The properties to list, in schema order, with each one's declared type.
    fn property_rows(&self) -> Vec<(String, Option<gq_language::Property>, String)> {
        let Some(schema) = &self.schema else {
            return Vec::new();
        };
        match &self.subject {
            PeekSubject::Node { type_name, .. } => schema
                .node(type_name)
                .map(|node| {
                    node.properties
                        .iter()
                        .filter(|p| p.kind != gq_language::ScalarKind::Vector)
                        .map(|p| (p.name.clone(), Some(p.clone()), p.display_type()))
                        .collect()
                })
                .unwrap_or_default(),
            PeekSubject::Edge { edge_type, .. } => {
                let mut rows = Vec::new();
                if let Some(edge) = schema.edge(edge_type) {
                    rows.extend(
                        edge.properties
                            .iter()
                            .filter(|p| p.projectable())
                            .map(|p| (p.name.clone(), Some(p.clone()), p.display_type())),
                    );
                }
                rows
            }
        }
    }

    fn render_properties(&self, cx: &mut Context<Self>) -> AnyElement {
        let values = match &self.values {
            Loadable::Loaded(values) => values.clone(),
            Loadable::Failed(error) => {
                return Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element();
            }
            _ => {
                return Label::new("Reading…")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element();
            }
        };
        let hover = cx.theme().colors().element_hover;
        let rows = self.property_rows();
        v_flex()
            .gap_0p5()
            .children(
                rows.into_iter()
                    .enumerate()
                    .map(|(index, (name, property, ty))| {
                        let value = values.get(&name).cloned().unwrap_or(Value::Null);
                        let title = match name.as_str() {
                            FROM_COLUMN => "from".to_owned(),
                            TO_COLUMN => "to".to_owned(),
                            _ => name.clone(),
                        };
                        let copied = self.copied.as_deref() == Some(name.as_str());
                        let shown = display_value(property.as_ref(), &value);
                        let copy_name = name.clone();
                        let copy_value = value.clone();
                        h_flex()
                            .id(("peek-property", index))
                            .px_1()
                            .py_0p5()
                            .gap_2()
                            .items_start()
                            .rounded_sm()
                            .hover(move |s| s.bg(hover))
                            .cursor_pointer()
                            .tooltip(Tooltip::text(format!("{ty} · click to copy")))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy(copy_name.clone(), &copy_value, cx)
                            }))
                            .child(
                                div().w(px(96.)).flex_none().child(
                                    Label::new(title)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                            )
                            .child(div().flex_1().min_w_0().child(
                                Label::new(shown).size(LabelSize::Small).color(
                                    if value.is_null() {
                                        Color::Disabled
                                    } else {
                                        Color::Default
                                    },
                                ),
                            ))
                            .when(copied, |d| {
                                d.child(
                                    Label::new("Copied")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Success),
                                )
                            })
                    }),
            )
            .into_any_element()
    }

    fn render_relationships(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.groups.is_empty() {
            return Label::new("This type takes part in no relationships.")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        }
        let hover = cx.theme().colors().element_hover;
        v_flex()
            .gap_0p5()
            .children(self.groups.iter().enumerate().map(|(index, group)| {
                let icon = match group.group.direction {
                    Direction::Outgoing => IconName::ArrowRight,
                    Direction::Incoming => IconName::ArrowLeft,
                };
                let count = match &group.count {
                    Loadable::Loaded(n) => format_count(*n),
                    Loadable::Failed(_) => "?".into(),
                    _ => "…".into(),
                };
                let failed = match &group.count {
                    Loadable::Failed(error) => Some(error.clone()),
                    _ => None,
                };
                let header = h_flex()
                    .id(("peek-group", index))
                    .px_1()
                    .py_0p5()
                    .gap_1p5()
                    .rounded_sm()
                    .hover(move |s| s.bg(hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(index, cx)))
                    .when_some(failed, |d, error| d.tooltip(Tooltip::text(error)))
                    .child(
                        Icon::new(if group.expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                    .child(
                        Label::new(gq_language::traversal_name(&group.group.edge))
                            .size(LabelSize::Small),
                    )
                    .child(
                        Label::new(group.group.other_type.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(count)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    );
                let body = group
                    .expanded
                    .then(|| self.render_neighbours(index, group, cx));
                v_flex().child(header).children(body)
            }))
            .into_any_element()
    }

    fn render_neighbours(&self, index: usize, group: &Group, cx: &mut Context<Self>) -> AnyElement {
        let hover = cx.theme().colors().element_hover;
        match &group.neighbours {
            Loadable::Loaded(neighbours) if neighbours.is_empty() => div()
                .pl(px(28.))
                .child(
                    Label::new("None at this commit")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            Loadable::Loaded(neighbours) => {
                let more = match &group.count {
                    Loadable::Loaded(n) if *n > neighbours.len() as i64 => {
                        Some(*n - neighbours.len() as i64)
                    }
                    _ => None,
                };
                let other = group.group.other_type.clone();
                v_flex()
                    .pl(px(22.))
                    .children(neighbours.iter().enumerate().map(|(n, neighbour)| {
                        let action = OpenNode {
                            type_name: other.clone(),
                            id: neighbour.id.clone(),
                        };
                        h_flex()
                            .id(SharedString::from(format!("peek-neighbour-{index}-{n}")))
                            .px_1()
                            .py_0p5()
                            .gap_2()
                            .rounded_sm()
                            .hover(move |s| s.bg(hover))
                            .cursor_pointer()
                            .tooltip(Tooltip::text(format!(
                                "Open {} in the inspector",
                                neighbour.id
                            )))
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(Box::new(action.clone()), cx)
                            })
                            .child(studio_ui::type_swatch(&other))
                            .child(
                                Label::new(
                                    neighbour
                                        .label
                                        .clone()
                                        .unwrap_or_else(|| neighbour.id.clone()),
                                )
                                .size(LabelSize::Small)
                                .truncate(),
                            )
                            .when(neighbour.label.is_some(), |d| {
                                d.child(
                                    Label::new(neighbour.id.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            })
                    }))
                    .children(more.map(|more| {
                        div().px_1().child(
                            Label::new(format!("+{} more", format_count(more)))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }))
                    .into_any_element()
            }
            Loadable::Failed(error) => div()
                .pl(px(28.))
                .child(
                    Label::new(error.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
                .into_any_element(),
            _ => div()
                .pl(px(28.))
                .child(
                    Label::new("Reading…")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
        }
    }

    fn render_endpoints(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let PeekSubject::Edge {
            from_type, to_type, ..
        } = &self.subject
        else {
            return None;
        };
        let values = self.values.loaded()?;
        let hover = cx.theme().colors().element_hover;
        let endpoint = |label: &'static str, ty: &String, key: &str| {
            let id = values.get(key).and_then(Value::as_str)?.to_owned();
            let action = OpenNode {
                type_name: ty.clone(),
                id: id.clone(),
            };
            Some(
                h_flex()
                    .id(label)
                    .px_1()
                    .py_0p5()
                    .gap_2()
                    .rounded_sm()
                    .hover(move |s| s.bg(hover))
                    .cursor_pointer()
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(Box::new(action.clone()), cx)
                    })
                    .child(
                        div()
                            .w(px(40.))
                            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                    )
                    .child(studio_ui::type_swatch(ty))
                    .child(Label::new(id).size(LabelSize::Small))
                    .child(
                        Label::new(ty.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
        };
        Some(
            v_flex()
                .gap_0p5()
                .children(endpoint("from", from_type, FROM_COLUMN))
                .children(endpoint("to", to_type, TO_COLUMN))
                .into_any_element(),
        )
    }
}

fn neighbours_from(rows: &Value) -> Vec<Neighbour> {
    rows.as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(Neighbour {
                        id: row.get("id")?.as_str()?.to_owned(),
                        label: row.get("label").and_then(Value::as_str).map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn section(title: &'static str) -> impl IntoElement {
    div().pt_2().pb_1().child(
        Label::new(title)
            .size(LabelSize::XSmall)
            .color(Color::Muted),
    )
}

impl Render for NodePeek {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let type_name = self.subject.type_name().to_owned();
        let id = self.subject.id().to_owned();
        let label = self.values.loaded().and_then(|values| {
            if let PeekSubject::Edge { .. } = &self.subject {
                let from = values.get(FROM_COLUMN)?.as_str()?;
                let to = values.get(TO_COLUMN)?.as_str()?;
                return Some(format!("{from} → {to}"));
            }
            let schema = self.schema.as_ref()?;
            let node = schema.node(&type_name)?;
            let label = node.label_property()?;
            (Some(label) != node.key.as_deref())
                .then(|| values.get(label)?.as_str().map(str::to_owned))
                .flatten()
        });
        let at = self.snapshot.clone();
        let has_properties = !self.property_rows().is_empty();
        let properties = self.render_properties(cx);
        let relationships =
            matches!(self.subject, PeekSubject::Node { .. }).then(|| self.render_relationships(cx));
        let endpoints = self.render_endpoints(cx);
        v_flex()
            .id("node-peek")
            .w_full()
            .p_4()
            .gap_1p5()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(studio_ui::type_swatch(&type_name))
                    .child(
                        Label::new(type_name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Label::new(label.unwrap_or_else(|| id.clone()))
                    .size(LabelSize::Large)
                    .weight(gpui::FontWeight::MEDIUM),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(studio_ui::ds::meta_code(IconName::Hash, id, cx))
                    .children(at.map(|commit| {
                        div()
                            .id("peek-commit")
                            .tooltip(Tooltip::text(format!("Read at commit {commit}")))
                            .child(studio_ui::ds::meta_code(
                                IconName::GitCommit,
                                short_commit(&commit),
                                cx,
                            ))
                    })),
            )
            .children(
                endpoints.map(|endpoints| v_flex().child(section("Endpoints")).child(endpoints)),
            )
            .when(has_properties, |d| {
                d.child(section("Properties")).child(properties)
            })
            .children(relationships.map(|relationships| {
                v_flex()
                    .child(section("Relationships"))
                    .child(relationships)
            }))
    }
}

impl NodePeek {
    /// Whether this peek shows `id` of `type_name`.
    pub fn shows(&self, id: &str, _cx: &App) -> bool {
        self.subject.id() == id
    }
}
