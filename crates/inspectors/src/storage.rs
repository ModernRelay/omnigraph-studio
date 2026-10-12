//! The "Storage" inspector (spec 010): server health and readiness, every graph's state, and the
//! session branch's snapshot with its datasets. Every value is copyable; a value the server did
//! not send reads "Not reported by this server".

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Task, Window,
};
use graph_client::ServerHealth;
use graph_client::api::{
    EntityKind, GraphInfo, GraphListResponse, ReadinessOutput, SnapshotOutput,
};
use graph_store::{GraphSession, Loadable};
use studio_ui::{CopyableValue, type_swatch};
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;

pub const STORAGE_KEY: &str = "inspector:storage";

pub struct StorageInspector {
    session: Entity<GraphSession>,
    branch: String,
    focus_handle: FocusHandle,
    health: Loadable<ServerHealth>,
    readiness: Loadable<ReadinessOutput>,
    graphs: Loadable<GraphListResponse>,
    snapshot: Loadable<SnapshotOutput>,
    _task: Option<Task<()>>,
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
        let mut view = Self {
            session,
            branch,
            focus_handle: cx.focus_handle(),
            health: Loadable::NotLoaded,
            readiness: Loadable::NotLoaded,
            graphs: Loadable::NotLoaded,
            snapshot: Loadable::NotLoaded,
            _task: None,
        };
        view.reload(cx);
        view
    }

    /// Reads everything again, at the session's current branch.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        self.branch = session.branch.clone();
        let client = session.client(cx);
        let graph = session.graph_id.clone();
        let branch = self.branch.clone();
        self.health = Loadable::Loading;
        self.readiness = Loadable::Loading;
        self.graphs = Loadable::Loading;
        self.snapshot = Loadable::Loading;
        self._task = Some(cx.spawn(async move |this, cx| {
            let health = loadable(client.health().await);
            let readiness = loadable(client.readiness().await);
            let graphs = loadable(client.graphs().await);
            let snapshot = loadable(client.snapshot(&graph, &branch).await);
            this.update(cx, |this, cx| {
                this.health = health;
                this.readiness = readiness;
                this.graphs = graphs;
                this.snapshot = snapshot;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_server(&self, cx: &mut Context<Self>) -> AnyElement {
        let url = self.session.read(cx).client(cx).base_url().to_owned();
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
            .gap_2()
            .child(column("Graph", 140.))
            .child(column("State", 110.))
            .child(column("Reads", 60.))
            .child(column("Writes", 60.))
            .child(column("Action", 180.))
            .child(column("Failure", 160.))
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
                    .gap_2()
                    .pt_2()
                    .child(column("Kind", 56.))
                    .child(column("Type", 150.))
                    .child(column("Rows", 90.))
                    .child(column("Version", 80.))
                    .child(column("Native branch", 130.))
                    .child(div().flex_1().child(heading("Dataset path")))
                    .into_any_element(),
            );
            let mut datasets = snapshot.datasets.clone();
            datasets.sort_by(|a, b| {
                (matches!(a.entity_kind, EntityKind::Edge), &a.type_name)
                    .cmp(&(matches!(b.entity_kind, EntityKind::Edge), &b.type_name))
            });
            for (ix, dataset) in datasets.iter().enumerate() {
                rows.push(
                    h_flex()
                        .gap_2()
                        .min_h(px(24.))
                        .child(
                            div().w(px(56.)).flex_none().child(
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
                                .w(px(150.))
                                .flex_none()
                                .gap_1p5()
                                .child(type_swatch(&dataset.type_name))
                                .child(
                                    CopyableValue::new(
                                        ("type", ix),
                                        Some(dataset.type_name.clone()),
                                    )
                                    .plain(),
                                ),
                        )
                        .child(div().w(px(90.)).flex_none().child(CopyableValue::new(
                            ("rows", ix),
                            Some(dataset.entity_count.to_string()),
                        )))
                        .child(div().w(px(80.)).flex_none().child(CopyableValue::new(
                            ("version", ix),
                            Some(dataset.published_dataset_version.to_string()),
                        )))
                        .child(
                            div().w(px(130.)).flex_none().child(CopyableValue::new(
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
        .gap_3()
        .min_h(px(24.))
        .child(
            div()
                .w(px(140.))
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
        .gap_2()
        .min_h(px(24.))
        .child(
            h_flex()
                .w(px(140.))
                .flex_none()
                .gap_1()
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
                .w(px(110.))
                .flex_none()
                .child(CopyableValue::new(("graph-state", ix), Some(state)).plain()),
        )
        .child(
            div()
                .w(px(60.))
                .flex_none()
                .child(Label::new(yes_no(graph.read_available)).size(LabelSize::Small)),
        )
        .child(
            div()
                .w(px(60.))
                .flex_none()
                .child(Label::new(yes_no(graph.write_available)).size(LabelSize::Small)),
        )
        .child(
            div()
                .w(px(180.))
                .flex_none()
                .child(CopyableValue::new(("graph-action", ix), Some(action)).plain()),
        )
        .child(
            div().w(px(160.)).flex_none().child(
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
        .gap_1()
        .py_3()
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
        let border = cx.theme().colors().border_variant;
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
                    .px_4()
                    .py_2()
                    .gap_2()
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
                        Button::new("refresh-storage", "Refresh")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
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
                    .px_6()
                    .py_4()
                    .gap_4()
                    .child(server)
                    .child(graphs)
                    .child(snapshot)
                    .child(
                        div()
                            .id("index-status")
                            .tooltip(ui::Tooltip::text(
                                "OmniGraph 0.13 does not serve index status (E13); `explain` in the query editor shows a type's index facts",
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
