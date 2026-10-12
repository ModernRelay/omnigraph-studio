//! The left dock's "Queries" panel (spec 007): this graph's saved `.gq` files, the most
//! recently changed first. A click opens one; ⌘S in a new query saves it here.

use std::path::PathBuf;
use std::time::SystemTime;

use gpui::{Context, Entity, Render, Subscription, WeakEntity, Window};
use graph_store::GraphSession;
use studio_actions::graph::{NewQuery, OpenQueryFile};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::Tooltip;
use ui::prelude::*;

use crate::SavedQueriesChanged;

pub const QUERIES_PANEL: &str = "queries";

pub struct QueriesPanel {
    session: Option<Entity<GraphSession>>,
    files: Vec<(PathBuf, SystemTime)>,
    _subscriptions: Vec<Subscription>,
}

impl QueriesPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        session: Option<Entity<GraphSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions =
            vec![cx.observe_global::<SavedQueriesChanged>(|this, cx| this.read_folder(cx))];
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, _, cx| match event {
                    WorkspaceEvent::SessionChanged => {
                        this.session = workspace.read(cx).session().cloned();
                        this.read_folder(cx);
                    }
                },
            ));
        }
        let mut this = Self {
            session,
            files: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.read_folder(cx);
        this
    }

    fn graph_key(&self, cx: &App) -> Option<String> {
        Some(self.session.as_ref()?.read(cx).settings_key(cx))
    }

    /// Lists the graph's queries folder again, off the main thread.
    fn read_folder(&mut self, cx: &mut Context<Self>) {
        let Some(graph) = self.graph_key(cx) else {
            self.files.clear();
            cx.notify();
            return;
        };
        let listing = cx.background_spawn(async move { studio_settings::saved_queries(&graph) });
        cx.spawn(async move |this, cx| {
            let files = listing.await;
            this.update(cx, |this, cx| {
                this.files = files;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for QueriesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let folder = self
            .graph_key(cx)
            .map(|graph| studio_settings::queries_dir(&graph));
        let now = SystemTime::now();
        // The dock titles the panel; what it offers besides the files sits under them, quiet.
        let actions = v_flex()
            .pt(px(studio_ui::ds::space::SM))
            .gap(px(studio_ui::ds::space::WITHIN))
            .child(
                studio_ui::ds::SidebarRow::new("queries-new", "New query")
                    .muted(true)
                    .start(
                        Icon::new(IconName::Plus)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip_fn(|_, cx| Tooltip::for_action("New query", &NewQuery, cx))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(NewQuery), cx)),
            )
            .child(
                studio_ui::ds::SidebarRow::new("queries-open", "Open a file…")
                    .muted(true)
                    .start(
                        Icon::new(IconName::FolderOpen)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip_fn(|_, cx| {
                        Tooltip::for_action("Open a .gq file", &OpenQueryFile::default(), cx)
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(OpenQueryFile::default()), cx)
                    }),
            )
            .children(folder.clone().map(|folder| {
                studio_ui::ds::SidebarRow::new("queries-reveal", "Show the folder")
                    .muted(true)
                    .start(
                        Icon::new(IconName::Folder)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(folder.display().to_string())
                    .on_click(move |_, _, cx| {
                        std::fs::create_dir_all(&folder).ok();
                        cx.reveal_path(&folder)
                    })
            }));
        let list = if self.files.is_empty() {
            studio_ui::ds::sidebar_placeholder(
                "queries-empty",
                IconName::FileCode,
                "No saved queries",
                "⌘S in a query saves it here, for this graph",
            )
            .into_any_element()
        } else {
            v_flex()
                .gap(px(studio_ui::ds::space::WITHIN))
                .children(
                    self.files
                        .iter()
                        .enumerate()
                        .map(|(index, (path, modified))| {
                            let name = path
                                .file_stem()
                                .map(|stem| stem.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let age = now
                                .duration_since(*modified)
                                .map(|age| age.as_micros() as i64)
                                .unwrap_or(0);
                            let when = studio_ui::relative_time(
                                studio_ui::now_micros() - age,
                                studio_ui::now_micros(),
                            );
                            let open = OpenQueryFile {
                                path: Some(path.to_string_lossy().into_owned()),
                            };
                            studio_ui::ds::SidebarRow::new(("saved-query", index), name)
                                .start(
                                    Icon::new(IconName::FileCode)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .end(studio_ui::ds::sidebar_count(when))
                                .tooltip(path.display().to_string())
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(Box::new(open.clone()), cx)
                                })
                        }),
                )
                .into_any_element()
        };
        v_flex().size_full().child(list).child(actions)
    }
}
