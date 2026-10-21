//! The "Storage" inspector (spec 010): server health and readiness, every graph's state, and the
//! session branch's snapshot with its datasets. Every value is copyable; a value the server did
//! not send reads "Not reported by this server". The snapshot shown stays when the branch moves:
//! the tab offers the newer one (constitution II, V), and reads again on Refresh or on another
//! branch.

use std::ops::Range;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, Window, uniform_list,
};
use graph_client::ServerHealth;
use graph_client::api::{
    EntityKind, GraphInfo, GraphListResponse, ReadinessOutput, SnapshotDatasetOutput,
    SnapshotOutput,
};
use graph_store::{GraphSession, Loadable, SessionEvent};
use studio_actions::workspace::Refresh;
use studio_ui::CopyableValue;
use studio_ui::ds::{Dotted as _, space};
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;
use ui::{TintColor, Tooltip};

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

/// The least height of the snapshot's part, in rows (its title, its three facts, the tables'
/// header and some tables), when the server's and the graphs' facts above it would take more:
/// those then scroll in what is left.
const SNAPSHOT_MIN_ROWS: f32 = 12.;

pub struct StorageInspector {
    session: Entity<GraphSession>,
    branch: String,
    focus_handle: FocusHandle,
    health: Loadable<ServerHealth>,
    readiness: Loadable<ReadinessOutput>,
    graphs: Loadable<GraphListResponse>,
    /// Its datasets nodes first, each kind by type name, as the table lists them.
    snapshot: Loadable<SnapshotOutput>,
    /// Counts the reads of the snapshot begun: only the latest one's answer is shown, so an
    /// older read that finishes later does not put back the storage it saw.
    snapshot_reads: u64,
    /// The branch head the session knew when the snapshot shown was asked for. The snapshot
    /// route names no commit, so a head move is measured against this.
    read_at: Option<String>,
    /// The branch moved since the snapshot shown was asked for.
    newer: bool,
    _task: Option<Task<()>>,
    _snapshot_task: Option<Task<()>>,
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

    fn is_stale(&self, _cx: &App) -> bool {
        self.newer
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "storage".into(),
            data: serde_json::Value::Null,
        })
    }
}

/// A table's row count as its Rows column shows it: grouped, as Studio writes every count.
fn rows_text(count: i64) -> String {
    studio_ui::group_count(count)
}

/// A snapshot's datasets, nodes first, each kind by type name, as the table lists them.
fn tables_in_order(mut snapshot: SnapshotOutput) -> SnapshotOutput {
    snapshot.datasets.sort_by(|a, b| {
        (matches!(a.entity_kind, EntityKind::Edge), &a.type_name)
            .cmp(&(matches!(b.entity_kind, EntityKind::Edge), &b.type_name))
    });
    snapshot
}

impl StorageInspector {
    pub fn new(session: Entity<GraphSession>, cx: &mut Context<Self>) -> Self {
        let branch = session.read(cx).branch.clone();
        // Its storage is the session branch's: another branch's is read at once. The server's
        // health, readiness and graphs are the same on every branch, and stay.
        let subscription = cx.subscribe(&session, |this: &mut Self, _, event, cx| match event {
            SessionEvent::BranchChanged => this.reload_snapshot(true, cx),
            // A new token may see less than the one the server's state was read with: none of
            // that shows while it is read again under the new one.
            SessionEvent::CredentialChanged => this.reload(cx),
            // A commit, Studio's or another writer's, moves the tables' versions and rows: the
            // storage shown stays, and the tab offers the newer.
            SessionEvent::HeadMoved { branch, head } if *branch == this.branch => {
                this.head_moved(head, cx)
            }
            _ => {}
        });
        let mut view = Self {
            session,
            branch,
            focus_handle: cx.focus_handle(),
            health: Loadable::NotLoaded,
            readiness: Loadable::NotLoaded,
            graphs: Loadable::NotLoaded,
            snapshot: Loadable::NotLoaded,
            snapshot_reads: 0,
            read_at: None,
            newer: false,
            _task: None,
            _snapshot_task: None,
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
        let snapshot_read = self.begin_snapshot_read(cx);
        let read = self
            .session
            .update(cx, |session, cx| session.server_state(&branch, cx));
        self._snapshot_task = None;
        self._task = Some(cx.spawn(async move |this, cx| {
            let state = read.await;
            this.update(cx, |this, cx| {
                this.health = loadable(state.health);
                this.readiness = loadable(state.readiness);
                this.graphs = loadable(state.graphs);
                // A branch moved to, or a commit, meanwhile is read on its own.
                if this.snapshot_reads == snapshot_read {
                    this.snapshot = loadable(state.snapshot.map(tables_in_order));
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Reads the storage of the session's branch again, the server's facts kept; `clear` shows
    /// it reading meanwhile, as for another branch, rather than the storage read last.
    fn reload_snapshot(&mut self, clear: bool, cx: &mut Context<Self>) {
        self.branch = self.session.read(cx).branch.clone();
        let branch = self.branch.clone();
        if clear {
            self.snapshot = Loadable::Loading;
        }
        let snapshot_read = self.begin_snapshot_read(cx);
        let read = self
            .session
            .update(cx, |session, cx| session.branch_snapshot(&branch, cx));
        self._snapshot_task = Some(cx.spawn(async move |this, cx| {
            let snapshot = read.await;
            this.update(cx, |this, cx| {
                if this.snapshot_reads == snapshot_read {
                    this.snapshot = loadable(snapshot.map(tables_in_order));
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Counts a read of the branch's snapshot begun, at the head the session knows now, and
    /// takes down the offer of a newer one.
    fn begin_snapshot_read(&mut self, cx: &mut Context<Self>) -> u64 {
        self.snapshot_reads += 1;
        self.read_at = self.session.read(cx).head(&self.branch).map(str::to_owned);
        if self.newer {
            self.newer = false;
            cx.emit(ItemEvent::UpdateTab);
        }
        self.snapshot_reads
    }

    /// The branch's head is now `head`. A head other than the one the snapshot was asked at
    /// offers the newer; the first head the session learns, while the snapshot was read, is
    /// taken as the one it was read at.
    fn head_moved(&mut self, head: &str, cx: &mut Context<Self>) {
        match &self.read_at {
            None => self.read_at = Some(head.to_owned()),
            Some(read_at) if read_at == head || self.newer => {}
            Some(_) => {
                self.newer = true;
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            }
        }
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

    /// The snapshot's facts over its tables. A graph can hold hundreds of tables (653 on one
    /// seen), so only the rows in view are built, each with its copy buttons, as every long
    /// list is drawn (constitution IX); the list scrolls in the room the facts above leave.
    fn render_snapshot(&self, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = self.snapshot.loaded();
        let facts = [
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
        let tables = snapshot.map(|snapshot| {
            v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    h_flex()
                        .gap(px(space::MD))
                        .pt(px(space::MD))
                        .child(column("Kind", col::KIND))
                        .child(column("Type", col::TYPE))
                        .child(column("Rows", col::ROWS))
                        .child(column("Version", col::VERSION))
                        .child(column("Native branch", col::NATIVE))
                        .child(div().flex_1().child(heading("Dataset path"))),
                )
                .child(
                    uniform_list(
                        "storage-tables",
                        snapshot.datasets.len(),
                        cx.processor(|this, range: Range<usize>, _window, cx| {
                            this.dataset_rows(range, cx)
                        }),
                    )
                    .flex_1()
                    .min_h_0(),
                )
        });
        let error = error_of(&[&self.snapshot.failed()]);
        v_flex()
            .w_full()
            .flex_1()
            .min_h(px(studio_ui::ds::size::SIDEBAR_ROW * SNAPSHOT_MIN_ROWS))
            .gap(px(space::XS))
            .py(px(space::LG))
            .border_b_1()
            .dotted(cx)
            .child(studio_ui::ds::text::muted(format!(
                "Snapshot of {}",
                self.branch
            )))
            .children(error.map(|e| studio_ui::ds::text::error(format!("Could not read: {e}"))))
            .children(facts)
            .children(tables)
            .into_any_element()
    }

    /// The rows of the snapshot's tables in `range`, as the list shows them.
    fn dataset_rows(&self, range: Range<usize>, cx: &App) -> Vec<AnyElement> {
        let Some(snapshot) = self.snapshot.loaded() else {
            return Vec::new();
        };
        range
            .filter_map(|ix| Some(dataset_row(ix, snapshot.datasets.get(ix)?, cx)))
            .collect()
    }
}

/// One table of the snapshot: its kind, type, rows, version, native branch and path, each
/// copyable. One line tall whatever it holds, as the list it is drawn in requires.
fn dataset_row(ix: usize, dataset: &SnapshotDatasetOutput, cx: &App) -> AnyElement {
    h_flex()
        .gap(px(space::MD))
        .h(px(studio_ui::ds::size::SIDEBAR_ROW))
        .child(
            div()
                .w(px(col::KIND))
                .flex_none()
                .child(studio_ui::ds::text::muted(match dataset.entity_kind {
                    EntityKind::Node => "node",
                    EntityKind::Edge => "edge",
                })),
        )
        .child(
            h_flex()
                .w(px(col::TYPE))
                .flex_none()
                .gap(px(space::SM))
                .child(studio_ui::kind_swatch(
                    &dataset.type_name,
                    dataset.entity_kind == EntityKind::Edge,
                    cx,
                ))
                .child(CopyableValue::new(("type", ix), Some(dataset.type_name.clone())).plain()),
        )
        .child(div().w(px(col::ROWS)).flex_none().child(CopyableValue::new(
            ("rows", ix),
            Some(rows_text(dataset.entity_count)),
        )))
        .child(
            div()
                .w(px(col::VERSION))
                .flex_none()
                .child(CopyableValue::new(
                    ("version", ix),
                    Some(dataset.published_dataset_version.to_string()),
                )),
        )
        .child(
            div()
                .w(px(col::NATIVE))
                .flex_none()
                .child(CopyableValue::new(
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
        .into_any_element()
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
    studio_ui::ds::text::meta(text)
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
                .child(studio_ui::ds::text::muted(label)),
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
                    d.child(studio_ui::ds::text::meta("open").color(Color::Accent))
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
                .child(studio_ui::ds::text::body(yes_no(graph.read_available))),
        )
        .child(
            div()
                .w(px(col::FLAG))
                .flex_none()
                .child(studio_ui::ds::text::body(yes_no(graph.write_available))),
        )
        .child(
            div()
                .w(px(col::ACTION))
                .flex_none()
                .child(CopyableValue::new(("graph-action", ix), Some(action)).plain()),
        )
        .child(
            div()
                .w(px(col::FAILURE))
                .flex_none()
                .child(studio_ui::ds::text::muted(
                    failure.unwrap_or_else(|| "none".into()),
                )),
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
        .dotted(cx)
        .child(studio_ui::ds::text::muted(title.to_owned()))
        .children(error.map(|e| studio_ui::ds::text::error(format!("Could not read: {e}"))))
        .children(rows)
        .into_any_element()
}

impl Render for StorageInspector {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let graph_id = self.session.read(cx).graph_id.clone();
        let loading = self.snapshot.is_loading();
        let server = self.render_server(cx);
        let graphs = self.render_graphs(cx);
        let snapshot = self.render_snapshot(cx);
        v_flex()
            .key_context("StorageInspector")
            .track_focus(&self.focus_handle)
            // ⌘R reads the inspector again, and goes on to the workspace, which reads the
            // graph's schema, branches and counts again.
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.reload(cx);
                cx.propagate();
            }))
            .size_full()
            .child(
                studio_ui::ds::toolbar(cx)
                    .child(
                        studio_ui::ds::icon::lead(IconName::Server),
                    )
                    .child(studio_ui::ds::text::title(format!(
                        "Storage · {graph_id} @ {}",
                        self.branch
                    )))
                    .when(loading, |d| d.child(studio_ui::ds::text::meta("Reading…")))
                    .when(self.newer, |d| {
                        d.child(
                            studio_ui::ds::toolbar_button(
                                "newer-storage",
                                "Newer snapshot · Refresh",
                            )
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .tooltip(Tooltip::text(
                                "The branch moved since its storage was read; read it again",
                            ))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.reload_snapshot(false, cx)),
                            ),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        studio_ui::ds::toolbar_icon_button("refresh-storage", IconName::RotateCw)
                            .tooltip(|_, cx| {
                                Tooltip::for_action("Read the server's state again", &Refresh, cx)
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .px(px(space::XXL))
                    .py(px(space::XL))
                    .gap(px(space::XL))
                    // The facts give way to the snapshot's tables, and scroll when they do.
                    .child(
                        v_flex()
                            .id("storage-facts")
                            .flex_shrink_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .gap(px(space::XL))
                            .child(server)
                            .child(graphs),
                    )
                    .child(snapshot)
                    .child(
                        div()
                            .id("index-status")
                            .tooltip(ui::Tooltip::text(
                                "OmniGraph does not serve index status yet; `explain` in the query editor shows a type's index facts",
                            ))
                            .child(studio_ui::ds::meta(IconName::Info, "Index status not served")),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use serde_json::json;

    use super::*;

    #[test]
    fn variants_read_as_words() {
        assert_eq!(words("WaitForStartup"), "wait for startup");
        assert_eq!(words("None"), "none");
    }

    /// Another writer's commit leaves the storage shown and offers the newer; Refresh reads it
    /// again, and another branch is read at once.
    #[gpui::test]
    async fn a_head_move_offers_the_newer_snapshot_and_reads_it_only_on_refresh(
        cx: &mut TestAppContext,
    ) {
        let (session, served) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n}\n",
            |_| json!([]),
            cx,
        );
        served.lock().unwrap().branches.push("feature".into());
        cx.run_until_parked();
        let inspector = cx.new(|cx| StorageInspector::new(session.clone(), cx));
        cx.run_until_parked();
        let state = |cx: &mut TestAppContext| {
            inspector.read_with(cx, |inspector, cx| {
                (
                    inspector.snapshot_reads,
                    inspector.newer,
                    inspector.is_stale(cx),
                )
            })
        };
        assert_eq!(state(cx), (1, false, false));

        served.lock().unwrap().commit("c2", Vec::new());
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(state(cx), (1, true, true), "the storage shown stays");

        inspector.update(cx, |inspector, cx| inspector.reload_snapshot(false, cx));
        cx.run_until_parked();
        assert_eq!(state(cx), (2, false, false));
        // The head the snapshot was read at, seen again, offers nothing.
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(state(cx), (2, false, false));

        session.update(cx, |session, cx| session.set_branch("feature".into(), cx));
        cx.run_until_parked();
        assert_eq!(
            state(cx),
            (3, false, false),
            "another branch is read at once"
        );
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.branch.clone()),
            "feature"
        );
    }

    /// A new token on the connection reads the server's state again under it, showing none of
    /// the old token's meanwhile: it may see less.
    #[gpui::test]
    async fn a_new_token_reads_the_storage_again(cx: &mut TestAppContext) {
        let (session, _) = graph_store::test_server::followed_session(
            "node Person {\n  slug: String @key\n}\n",
            |_| json!([]),
            cx,
        );
        cx.run_until_parked();
        let inspector = cx.new(|cx| StorageInspector::new(session.clone(), cx));
        cx.run_until_parked();
        let reads = |cx: &mut TestAppContext| {
            inspector.read_with(cx, |inspector, _| inspector.snapshot_reads)
        };
        assert_eq!(reads(cx), 1);
        let connection = session.read_with(cx, |session, _| session.connection.clone());
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("another".into()), cx)
        });
        inspector.read_with(cx, |inspector, _| {
            assert!(
                inspector.graphs.is_loading(),
                "the old token's graphs do not show"
            );
            assert!(inspector.snapshot.is_loading());
        });
        cx.run_until_parked();
        assert_eq!(reads(cx), 2, "read again under the new token");
        inspector.read_with(cx, |inspector, _| {
            assert!(!inspector.graphs.is_loading());
        });
    }

    /// A table's row count is grouped as every count Studio writes.
    #[test]
    fn row_counts_are_grouped() {
        assert_eq!(rows_text(12_480), "12,480");
        assert_eq!(rows_text(7), "7");
    }
}
