//! The "Storage" inspector (spec 010): server health and readiness, every graph's state, and the
//! session branch's snapshot with its datasets. Every value is copyable; a value the server did
//! not send reads "Not reported by this server".

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, Window,
};
use graph_client::ServerHealth;
use graph_client::api::{
    EntityKind, GraphInfo, GraphListResponse, ReadinessOutput, SnapshotOutput,
};
use graph_store::{GraphSession, Loadable, SessionEvent};
use studio_ui::CopyableValue;
use studio_ui::ds::space;
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;

pub const STORAGE_KEY: &str = "inspector:storage";

/// The storage tables' columns, sized to the values they hold; a header and its rows share one.
mod col {
    pub const GRAPH: f32 = 140.;
    pub const STATE: f32 = 110.;
    pub const FLAG: f32 = 60.;
    pub const ACTION: f32 = 180.;
    pub const FAILURE: f32 = 160.;
    pub const KIND: f32 = 56.;
    pub const TYPE: f32 = 150.;
    pub const ROWS: f32 = 90.;
    pub const VERSION: f32 = 80.;
    pub const NATIVE: f32 = 130.;
}

pub struct StorageInspector {
    session: Entity<GraphSession>,
    branch: String,
    focus_handle: FocusHandle,
    health: Loadable<ServerHealth>,
    readiness: Loadable<ReadinessOutput>,
    graphs: Loadable<GraphListResponse>,
    /// Its datasets nodes first, each kind by type name, as the table lists them.
    snapshot: Loadable<SnapshotOutput>,
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<ItemEvent> for StorageInspector {}

impl Focusable for StorageInspector {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for StorageInspector {
    fn tab_title(&self, _cx: &App) -> SharedString {
        "Storage".into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::Server)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(STORAGE_KEY.into())
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "storage".into(),
            data: serde_json::Value::Null,
        })
    }
}

impl StorageInspector {
    pub fn new(session: Entity<GraphSession>, cx: &mut Context<Self>) -> Self {
        let branch = session.read(cx).branch.clone();
        // What it shows is the session branch's: another branch is read at once.
        let subscription = cx.subscribe(&session, |this: &mut Self, _, event, cx| {
            if let SessionEvent::BranchChanged = event {
                this.reload(cx);
            }
        });
        let mut view = Self {
            session,
            branch,
            focus_handle: cx.focus_handle(),
            health: Loadable::NotLoaded,
            readiness: Loadable::NotLoaded,
            graphs: Loadable::NotLoaded,
            snapshot: Loadable::NotLoaded,
            _task: None,
            _subscription: subscription,
        };
        view.reload(cx);
        view
    }

    /// Reads everything again, at the session's current branch.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.branch = self.session.read(cx).branch.clone();
        let branch = self.branch.clone();
        self.health = Loadable::Loading;
        self.readiness = Loadable::Loading;
        self.graphs = Loadable::Loading;
        self.snapshot = Loadable::Loading;
        let read = self
            .session
            .update(cx, |session, cx| session.server_state(&branch, cx));
        self._task = Some(cx.spawn(async move |this, cx| {
            let state = read.await;
            this.update(cx, |this, cx| {
                this.health = loadable(state.health);
                this.readiness = loadable(state.readiness);
                this.graphs = loadable(state.graphs);
                this.snapshot = loadable(state.snapshot.map(|mut snapshot| {
                    snapshot.datasets.sort_by(|a, b| {
                        (matches!(a.entity_kind, EntityKind::Edge), &a.type_name)
                            .cmp(&(matches!(b.entity_kind, EntityKind::Edge), &b.type_name))
                    });
                    snapshot
                }));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_server(&self, cx: &mut Context<Self>) -> AnyElement {
        let url = self.session.read(cx).server_url(cx);
        let health = self.health.loaded();
        let readiness = self.readiness.loaded();
        let mut rows = vec![
            kv("Address", CopyableValue::new("server-url", Some(url))),
            kv(
                "Version",
                CopyableValue::new("server-version", health.map(|h| h.health.version.clone())),
            ),
            kv(
                "Source",
                CopyableValue::new(
                    "server-source",
                    health.and_then(|h| h.health.source_version.clone()),
                ),
            ),
            kv(
                "HTTP contract",
                CopyableValue::new("contract", health.and_then(|h| h.contract.clone())),
            ),
            kv(
                "Storage format",
                CopyableValue::new(
                    "server-schema-version",
                    health.map(|h| h.health.internal_schema_version.to_string()),
                ),
            ),
        ];
        rows.extend([
            kv(
                "Ready",
                CopyableValue::new(
                    "ready",
                    readiness
                        .map(|r| format!("{} ({})", if r.ready { "yes" } else { "no" }, r.status)),
                )
                .plain(),
            ),
            kv(
                "Graphs",
                CopyableValue::new(
                    "graph-counts",
                    readiness.map(|r| {
                        format!(
                            "{} served · {} ready · {} loading · {} blocked",
                            r.served_graph_count,
                            r.ready_graph_count,
                            r.loading_graph_count,
                            r.blocked_graph_count
                        )
                    }),
                )
                .plain(),
            ),
            kv(
                "State revision",
                CopyableValue::new(
                    "state-revision",
                    readiness.map(|r| r.state_revision.to_string()),
                ),
            ),
            kv(
                "State CAS",
                CopyableValue::new("state-cas", readiness.and_then(|r| r.state_cas.clone())),
            ),
            kv(
                "Serving digest",
                CopyableValue::new(
                    "serving-digest",
                    readiness.and_then(|r| r.booted_serving_digest.clone()),
                ),
            ),
            kv(
                "Shutdown grace",
                CopyableValue::new(
                    "grace",
                    readiness.map(|r| format!("{} s", r.shutdown_grace_seconds)),
                )
                .plain(),
            ),
        ]);
        section(
            "Server",
            error_of(&[&self.health.failed(), &self.readiness.failed()]),
            rows,
            cx,
        )
    }

    fn render_graphs(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.session.read(cx).graph_id.clone();
        let rows = match self.graphs.loaded() {
            Some(list) => list
                .graphs
                .iter()
                .enumerate()
                .map(|(ix, graph)| graph_row(ix, graph, graph.graph_id == current))
                .collect(),
            None => Vec::new(),
        };
        let header = h_flex()
            .gap(px(space::MD))
            .child(column("Graph", col::GRAPH))
            .child(column("State", col::STATE))
            .child(column("Reads", col::FLAG))
            .child(column("Writes", col::FLAG))
            .child(column("Action", col::ACTION))
            .child(column("Failure", col::FAILURE))
            .child(div().flex_1().child(heading("Storage root")))
            .into_any_element();
        let mut all = vec![header];
        all.extend(rows);
        section("Graphs", error_of(&[&self.graphs.failed()]), all, cx)
    }

    fn render_snapshot(&self, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = self.snapshot.loaded();
        let mut rows = vec![
            kv(
                "Branch",
                CopyableValue::new("snapshot-branch", snapshot.map(|s| s.graph_branch.clone())),
            ),
            kv(
                "Manifest version",
                CopyableValue::new(
                    "manifest-version",
                    snapshot.map(|s| s.graph_manifest_version.to_string()),
                ),
            ),
            kv(
                "Storage format",
                CopyableValue::new(
                    "snapshot-schema-version",
                    snapshot.map(|s| s.internal_schema_version.to_string()),
                ),
            ),
        ];
        if let Some(snapshot) = snapshot {
            rows.push(
                h_flex()
                    .gap(px(space::MD))
                    .pt(px(space::MD))
                    .child(column("Kind", col::KIND))
                    .child(column("Type", col::TYPE))
                    .child(column("Rows", col::ROWS))
                    .child(column("Version", col::VERSION))
                    .child(column("Native branch", col::NATIVE))
                    .child(div().flex_1().child(heading("Dataset path")))
                    .into_any_element(),
            );
            for (ix, dataset) in snapshot.datasets.iter().enumerate() {
                rows.push(
                    h_flex()
                        .gap(px(space::MD))
                        .min_h(px(studio_ui::ds::size::SIDEBAR_ROW))
                        .child(
                            div().w(px(col::KIND)).flex_none().child(
                                Label::new(match dataset.entity_kind {
                                    EntityKind::Node => "node",
                                    EntityKind::Edge => "edge",
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                        )
                        .child(
                            h_flex()
                                .w(px(col::TYPE))
                                .flex_none()
                                .gap(px(space::SM))
                                .child(studio_ui::kind_swatch(
                                    &dataset.type_name,
                                    dataset.entity_kind == graph_client::api::EntityKind::Edge,
                                    cx,
                                ))
                                .child(
                                    CopyableValue::new(
                                        ("type", ix),
                                        Some(dataset.type_name.clone()),
                                    )
                                    .plain(),
                                ),
                        )
                        .child(div().w(px(col::ROWS)).flex_none().child(CopyableValue::new(
                            ("rows", ix),
                            Some(dataset.entity_count.to_string()),
                        )))
                        .child(div().w(px(col::VERSION)).flex_none().child(CopyableValue::new(
                            ("version", ix),
                            Some(dataset.published_dataset_version.to_string()),
                        )))
                        .child(
                            div().w(px(col::NATIVE)).flex_none().child(CopyableValue::new(
                                ("native", ix),
                                Some(
                                    dataset
                                        .native_dataset_branch
                                        .clone()
                                        .unwrap_or_else(|| "none".into()),
                                ),
                            )),
                        )
                        .child(div().flex_1().min_w_0().child(CopyableValue::new(
                            ("path", ix),
                            Some(dataset.dataset_path.clone()),
                        )))
                        .into_any_element(),
                );
            }
        }
        section(
            &format!("Snapshot of {}", self.branch),
            error_of(&[&self.snapshot.failed()]),
            rows,
            cx,
        )
    }
}

trait FailedMessage {
    fn failed(&self) -> Option<String>;
}

impl<T> FailedMessage for Loadable<T> {
    fn failed(&self) -> Option<String> {
        match self {
            Loadable::Failed(message) => Some(message.clone()),
            _ => None,
        }
    }
}

fn loadable<T, E: std::fmt::Display>(result: Result<T, E>) -> Loadable<T> {
    match result {
        Ok(value) => Loadable::Loaded(value),
        Err(error) => Loadable::Failed(error.to_string()),
    }
}

fn error_of(errors: &[&Option<String>]) -> Option<String> {
    let messages: Vec<&str> = errors.iter().filter_map(|e| e.as_deref()).collect();
    (!messages.is_empty()).then(|| messages.join(" · "))
}

fn heading(text: &'static str) -> Label {
    Label::new(text).size(LabelSize::XSmall).color(Color::Muted)
}

fn column(text: &'static str, width: f32) -> Div {
    div().w(px(width)).flex_none().child(heading(text))
}

fn kv(label: &'static str, value: CopyableValue) -> AnyElement {
    h_flex()
        .gap(px(space::LG))
        .min_h(px(studio_ui::ds::size::SIDEBAR_ROW))
        .child(
            div()
                .w(px(studio_ui::ds::size::FIELD_LABEL))
                .flex_none()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(div().flex_1().min_w_0().child(value))
        .into_any_element()
}

fn graph_row(ix: usize, graph: &GraphInfo, current: bool) -> AnyElement {
    let state = format!("{:?}", graph.state).to_lowercase();
    let action = words(&format!("{:?}", graph.action));
    let failure = graph.failure.map(|f| words(&format!("{f:?}")));
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    h_flex()
        .gap(px(space::MD))
        .min_h(px(studio_ui::ds::size::SIDEBAR_ROW))
        .child(
            h_flex()
                .w(px(col::GRAPH))
                .flex_none()
                .gap(px(space::XS))
                .child(CopyableValue::new(
                    ("graph-id", ix),
                    Some(graph.graph_id.clone()),
                ))
                .when(current, |d| {
                    d.child(
                        Label::new("open")
                            .size(LabelSize::XSmall)
                            .color(Color::Accent),
                    )
                }),
        )
        .child(
            div()
                .w(px(col::STATE))
                .flex_none()
                .child(CopyableValue::new(("graph-state", ix), Some(state)).plain()),
        )
        .child(
            div()
                .w(px(col::FLAG))
                .flex_none()
                .child(Label::new(yes_no(graph.read_available)).size(LabelSize::Small)),
        )
        .child(
            div()
                .w(px(col::FLAG))
                .flex_none()
                .child(Label::new(yes_no(graph.write_available)).size(LabelSize::Small)),
        )
        .child(
            div()
                .w(px(col::ACTION))
                .flex_none()
                .child(CopyableValue::new(("graph-action", ix), Some(action)).plain()),
        )
        .child(
            div().w(px(col::FAILURE)).flex_none().child(
                Label::new(failure.unwrap_or_else(|| "none".into()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
        )
        .child(div().flex_1().min_w_0().child(CopyableValue::new(
            ("graph-uri", ix),
            Some(graph.uri.clone()),
        )))
        .into_any_element()
}

/// `WaitForStartup` → `wait for startup`.
fn words(variant: &str) -> String {
    let mut out = String::new();
    for (i, c) in variant.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    out
}

fn section(title: &str, error: Option<String>, rows: Vec<AnyElement>, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .gap(px(space::XS))
        .py(px(space::LG))
        .border_b_1()
        .border_color(studio_ui::ds::surface::rule(cx))
        .child(
            Label::new(title.to_owned())
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .children(error.map(|e| {
            Label::new(format!("Could not read: {e}"))
                .size(LabelSize::Small)
                .color(Color::Error)
        }))
        .children(rows)
        .into_any_element()
}

impl Render for StorageInspector {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = studio_ui::ds::surface::rule(cx);
        let graph_id = self.session.read(cx).graph_id.clone();
        let loading = self.snapshot.is_loading();
        let server = self.render_server(cx);
        let graphs = self.render_graphs(cx);
        let snapshot = self.render_snapshot(cx);
        v_flex()
            .key_context("StorageInspector")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                h_flex()
                    .flex_none()
                    .px(px(space::XL))
                    .py(px(space::MD))
                    .gap(px(space::MD))
                    .border_b_1()
                    .border_color(border)
                    .child(Icon::new(IconName::Server).color(Color::Muted))
                    .child(
                        Headline::new(format!("Storage · {graph_id} @ {}", self.branch))
                            .size(HeadlineSize::XSmall),
                    )
                    .when(loading, |d| {
                        d.child(Label::new("Reading…").size(LabelSize::Small).color(Color::Muted))
                    })
                    .child(div().flex_1())
                    .child(
                        studio_ui::ds::button("refresh-storage", "Refresh")
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::RotateCw).size(IconSize::Small))
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            )
            .child(
                v_flex()
                    .id("storage-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(space::XXL))
                    .py(px(space::XL))
                    .gap(px(space::XL))
                    .child(server)
                    .child(graphs)
                    .child(snapshot)
                    .child(
                        div()
                            .id("index-status")
                            .tooltip(ui::Tooltip::text(
                                "OmniGraph does not serve index status yet (E13); `explain` in the query editor shows a type's index facts",
                            ))
                            .child(studio_ui::ds::meta(IconName::Info, "Index status not served")),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::words;

    #[test]
    fn variants_read_as_words() {
        assert_eq!(words("WaitForStartup"), "wait for startup");
        assert_eq!(words("None"), "none");
    }
}
