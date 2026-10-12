//! Branches (spec 008): the sidebar list, switching, and the create and delete dialogs.
//!
//! OmniGraph 0.13's `GET /branches` returns names only (E9 waits on branch details), so each
//! branch's last commit comes from its own `GET /commits?branch=`, for up to
//! [`DETAIL_LIMIT`] branches.

use std::collections::HashMap;

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use graph_client::WriteOutcome;
use graph_store::{GraphSession, SessionEvent};
use studio_actions::graph::{CreateBranch, DeleteBranch, MergeBranch, ReviewBranch};
use studio_ui::{now_micros, relative_time, short_commit_id};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;
use ui::{ContextMenu, ContextMenuEntry, Tooltip, right_click_menu};

use crate::view::open_review;

/// Branches whose last commit is read for the list.
const DETAIL_LIMIT: usize = 20;

/// The branch every review and merge targets until branch details exist (E9).
pub const TARGET: &str = "main";

#[derive(Debug, Clone)]
struct BranchDetail {
    head: String,
    created_at: i64,
    actor: Option<String>,
}

/// The "Branches" sidebar section.
pub struct BranchList {
    workspace: WeakEntity<Workspace>,
    session: Option<Entity<GraphSession>>,
    details: HashMap<String, BranchDetail>,
    _details_task: Option<Task<()>>,
    _workspace_subscription: Option<Subscription>,
    _session_subscription: Option<Subscription>,
}

impl BranchList {
    /// `session` is the workspace's session when the list is created; later sessions arrive
    /// through [`WorkspaceEvent::SessionChanged`].
    pub fn new(
        workspace: WeakEntity<Workspace>,
        session: Option<Entity<GraphSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            workspace: workspace.clone(),
            session: None,
            details: HashMap::new(),
            _details_task: None,
            _workspace_subscription: None,
            _session_subscription: None,
        };
        if let Some(workspace) = workspace.upgrade() {
            this._workspace_subscription = Some(cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, _, cx| match event {
                    WorkspaceEvent::SessionChanged => {
                        this.watch(workspace.read(cx).session().cloned(), cx)
                    }
                },
            ));
        }
        this.watch(session, cx);
        this
    }

    fn watch(&mut self, session: Option<Entity<GraphSession>>, cx: &mut Context<Self>) {
        self._session_subscription = session.as_ref().map(|session| {
            cx.subscribe(session, |this, _, event: &SessionEvent, cx| match event {
                SessionEvent::BranchesChanged | SessionEvent::Committed { .. } => {
                    this.load_details(cx)
                }
                SessionEvent::BranchChanged => cx.notify(),
                _ => {}
            })
        });
        self.session = session;
        self.details.clear();
        self.load_details(cx);
        cx.notify();
    }

    fn load_details(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let session = session.read(cx);
        let Some(branches) = session.branches.loaded().cloned() else {
            return;
        };
        let client = session.client(cx);
        let graph = session.graph_id.clone();
        self._details_task = Some(cx.spawn(async move |this, cx| {
            let mut details = HashMap::new();
            for branch in branches.into_iter().take(DETAIL_LIMIT) {
                if let Ok(list) = client.commits(&graph, &branch).await
                    && let Some(head) = list.commits.first()
                {
                    details.insert(
                        branch,
                        BranchDetail {
                            head: head.graph_commit_id.clone(),
                            created_at: head.created_at,
                            actor: head.actor_id.clone(),
                        },
                    );
                }
            }
            this.update(cx, |this, cx| {
                this.details = details;
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_branch(
        &self,
        index: usize,
        branch: String,
        current: bool,
    ) -> impl IntoElement + use<> {
        let detail = self.details.get(&branch).map(|d| {
            let actor = d.actor.clone().unwrap_or_else(|| "unknown actor".into());
            let when = relative_time(d.created_at, now_micros());
            (
                when.clone(),
                format!("Head {} · {actor} · {when}", short_commit_id(&d.head)),
            )
        });
        let session = self.session.clone();
        let workspace = self.workspace.clone();
        let name = branch.clone();
        let is_target = branch == TARGET;
        right_click_menu(("branch-menu", index))
            .trigger({
                let session = session.clone();
                let name = name.clone();
                move |_, _, _| {
                    let (when, tooltip) = detail.clone().unzip();
                    let row = studio_ui::ds::SidebarRow::new(("branch", index), name.clone())
                        .start(
                            Icon::new(IconName::GitBranch)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .selected(current)
                        .when_some(when, |row, when| {
                            row.end(studio_ui::ds::sidebar_count(when))
                        });
                    let row = match tooltip {
                        Some(tooltip) => row.tooltip(tooltip),
                        None => row,
                    };
                    row.on_click({
                        let session = session.clone();
                        let name = name.clone();
                        move |_, _, cx| {
                            if let Some(session) = &session {
                                session.update(cx, |s, cx| s.set_branch(name.clone(), cx));
                            }
                        }
                    })
                }
            })
            .menu(move |window, cx| {
                let session = session.clone();
                let workspace = workspace.clone();
                let name = branch.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    let switch = {
                        let session = session.clone();
                        let name = name.clone();
                        move |_: &mut Window, cx: &mut App| {
                            if let Some(session) = &session {
                                session.update(cx, |s, cx| s.set_branch(name.clone(), cx));
                            }
                        }
                    };
                    let review = {
                        let workspace = workspace.clone();
                        let name = name.clone();
                        move |window: &mut Window, cx: &mut App| {
                            workspace
                                .update(cx, |w, cx| {
                                    open_review(w, name.clone(), TARGET.into(), false, window, cx)
                                })
                                .ok();
                        }
                    };
                    let merge = {
                        let workspace = workspace.clone();
                        let name = name.clone();
                        move |window: &mut Window, cx: &mut App| {
                            workspace
                                .update(cx, |w, cx| {
                                    open_review(w, name.clone(), TARGET.into(), true, window, cx)
                                })
                                .ok();
                        }
                    };
                    let delete = {
                        let workspace = workspace.clone();
                        let session = session.clone();
                        let name = name.clone();
                        move |window: &mut Window, cx: &mut App| {
                            let Some(session) = session.clone() else {
                                return;
                            };
                            workspace
                                .update(cx, |w, cx| {
                                    w.toggle_modal(window, cx, |window, cx| {
                                        DeleteBranchModal::new(session, name.clone(), window, cx)
                                    })
                                })
                                .ok();
                        }
                    };
                    menu.header(name.clone())
                        .entry("Switch to this branch", None, switch)
                        .item(
                            ContextMenuEntry::new(format!("Review against {TARGET}"))
                                .disabled(is_target)
                                .handler(review),
                        )
                        .item(
                            ContextMenuEntry::new(format!("Merge into {TARGET}…"))
                                .disabled(is_target)
                                .handler(merge),
                        )
                        .separator()
                        .item(
                            ContextMenuEntry::new(if is_target {
                                format!("Delete ({TARGET} cannot be deleted)")
                            } else {
                                "Delete…".to_owned()
                            })
                            .icon(IconName::Trash)
                            .disabled(is_target)
                            .handler(delete),
                        )
                })
            })
    }
}

impl Render for BranchList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session) = self.session.clone() else {
            return v_flex().into_any_element();
        };
        let s = session.read(cx);
        let current = s.branch.clone();
        let state = match &s.branches {
            graph_store::Loadable::Loaded(branches) => Ok(branches.clone()),
            graph_store::Loadable::Failed(error) => Err(Some(error.clone())),
            _ => Err(None),
        };
        let new_button = studio_ui::ds::SidebarRow::new("new-branch", "New branch…")
            .muted(true)
            .start(
                Icon::new(IconName::Plus)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .tooltip_fn({
                let current = current.clone();
                move |_, cx| {
                    Tooltip::with_meta(
                        format!("New branch from {current}"),
                        Some(&CreateBranch),
                        format!("omnigraph branch create <name> --from {current}"),
                        cx,
                    )
                }
            })
            .on_click(|_, window, cx| window.dispatch_action(Box::new(CreateBranch), cx));
        let mut list = v_flex();
        match state {
            Ok(branches) => {
                for (index, branch) in branches.into_iter().enumerate() {
                    let is_current = branch == current;
                    list = list.child(self.render_branch(index, branch, is_current));
                }
            }
            Err(Some(error)) => {
                list = list.child(
                    div().px_2().child(
                        Label::new(format!("Branches could not be read: {error}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    ),
                )
            }
            Err(None) => list = list.child(studio_ui::ds::sidebar_note("Reading branches…")),
        }
        v_flex()
            .w_full()
            .child(list)
            .child(new_button)
            .into_any_element()
    }
}

// ---- dialogs ----

fn field(editor: &Entity<Editor>, cx: &App) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border)
        .child(editor.clone())
}

fn dialog(title: impl Into<SharedString>, cx: &App) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .w(px(440.))
        .p_4()
        .gap_3()
        .rounded_lg()
        .bg(colors.elevated_surface_background)
        .border_1()
        .border_color(colors.border)
        .shadow_lg()
        .child(Headline::new(title.into()).size(HeadlineSize::Small))
}

/// "New branch": a name, created from the current branch, then switched to.
pub struct CreateBranchModal {
    session: Entity<GraphSession>,
    from: String,
    name: Entity<Editor>,
    error: Option<SharedString>,
    running: bool,
    focus_handle: FocusHandle,
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for CreateBranchModal {}

impl Focusable for CreateBranchModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl CreateBranchModal {
    pub fn new(session: Entity<GraphSession>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let from = session.read(cx).branch.clone();
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Branch name, for example fix-titles", window, cx);
            editor
        });
        let subscription = cx.subscribe(&name, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                this.error = None;
                cx.notify();
            }
        });
        Self {
            session,
            from,
            name,
            error: None,
            running: false,
            focus_handle: cx.focus_handle(),
            _task: None,
            _subscription: subscription,
        }
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let name = self.name.read(cx).text(cx).trim().to_owned();
        if name.is_empty() {
            self.error = Some("Enter a name for the branch.".into());
            cx.notify();
            return;
        }
        self.running = true;
        let from = self.from.clone();
        let task = self
            .session
            .update(cx, |s, cx| s.create_branch(name.clone(), Some(from), cx));
        self._task = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| {
                this.running = false;
                match outcome {
                    WriteOutcome::Committed(_) => {
                        this.session.update(cx, |s, cx| s.set_branch(name, cx));
                        cx.emit(DismissEvent);
                    }
                    WriteOutcome::Refused(error) => {
                        this.error = Some(format!("Not created: {}", error.body.error).into());
                    }
                    WriteOutcome::Unknown { .. } => {
                        this.error = Some(
                            "The outcome is unknown. Check the branch list before trying again."
                                .into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for CreateBranchModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.name.read(cx).text(cx);
        let cli = format!(
            "omnigraph branch create {} --from {}",
            if name.trim().is_empty() {
                "<name>"
            } else {
                name.trim()
            },
            self.from
        );
        dialog(format!("New branch from {}", self.from), cx)
            .key_context("CreateBranchModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.create(cx)))
            .child(field(&self.name, cx))
            .child(
                Label::new(cli)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .buffer_font(cx),
            )
            .children(
                self.error
                    .clone()
                    .map(|e| Label::new(e).size(LabelSize::Small).color(Color::Error)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        studio_ui::ds::button("cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        studio_ui::ds::button(
                            "create",
                            if self.running {
                                "Creating…"
                            } else {
                                "Create"
                            },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(self.running)
                        .on_click(cx.listener(|this, _, _, cx| this.create(cx))),
                    ),
            )
    }
}

/// "Delete branch": confirmed by typing the branch's name (constitution IV). `main` cannot be
/// deleted.
pub struct DeleteBranchModal {
    session: Entity<GraphSession>,
    branch: String,
    confirm: Entity<Editor>,
    error: Option<SharedString>,
    running: bool,
    focus_handle: FocusHandle,
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for DeleteBranchModal {}

impl Focusable for DeleteBranchModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.confirm.focus_handle(cx)
    }
}

impl DeleteBranchModal {
    pub fn new(
        session: Entity<GraphSession>,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let confirm = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&format!("Type {branch} to confirm"), window, cx);
            editor
        });
        let subscription = cx.subscribe(&confirm, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                this.error = None;
                cx.notify();
            }
        });
        Self {
            session,
            branch,
            confirm,
            error: None,
            running: false,
            focus_handle: cx.focus_handle(),
            _task: None,
            _subscription: subscription,
        }
    }

    fn confirmed(&self, cx: &App) -> bool {
        self.branch != TARGET && self.confirm.read(cx).text(cx).trim() == self.branch
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if self.running || !self.confirmed(cx) {
            return;
        }
        self.running = true;
        let branch = self.branch.clone();
        let task = self.session.update(cx, |s, cx| s.delete_branch(branch, cx));
        self._task =
            Some(cx.spawn(async move |this, cx| {
                let outcome = task.await;
                this.update(cx, |this, cx| {
                    this.running = false;
                    match outcome {
                        WriteOutcome::Committed(_) => cx.emit(DismissEvent),
                        WriteOutcome::Refused(error) => {
                            this.error = Some(format!("Not deleted: {}", error.body.error).into())
                        }
                        WriteOutcome::Unknown { .. } => this.error = Some(
                            "The outcome is unknown. Check the branch list before trying again."
                                .into(),
                        ),
                    }
                    cx.notify();
                })
                .ok();
            }));
        cx.notify();
    }
}

impl Render for DeleteBranchModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let confirmed = self.confirmed(cx);
        let is_target = self.branch == TARGET;
        let reason = if is_target {
            Some(format!("{TARGET} cannot be deleted."))
        } else if !confirmed {
            Some(format!("Type {} to enable Delete.", self.branch))
        } else {
            None
        };
        dialog(format!("Delete branch {}", self.branch), cx)
            .key_context("DeleteBranchModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.delete(cx)))
            .child(
                Label::new(format!(
                    "Its commits stay reachable only if merged. Type {} to confirm.",
                    self.branch
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(field(&self.confirm, cx))
            .child(
                Label::new(format!("omnigraph branch delete {}", self.branch))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .buffer_font(cx),
            )
            .children(
                self.error
                    .clone()
                    .map(|e| Label::new(e).size(LabelSize::Small).color(Color::Error)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .children(
                        reason.map(|r| Label::new(r).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                    .child(
                        studio_ui::ds::button("cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        studio_ui::ds::button(
                            "delete",
                            if self.running {
                                "Deleting…"
                            } else {
                                "Delete"
                            },
                        )
                        .style(ButtonStyle::Filled)
                        .color(Color::Error)
                        .disabled(!confirmed || self.running)
                        .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
                    ),
            )
    }
}

/// Registers the branch actions on a workspace.
pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ReviewBranch, window, cx| {
        let Some(session) = workspace.session() else {
            return;
        };
        let branch = session.read(cx).branch.clone();
        open_review(workspace, branch, TARGET.into(), false, window, cx);
    });
    workspace.register_action(|workspace, _: &MergeBranch, window, cx| {
        let Some(session) = workspace.session() else {
            return;
        };
        let branch = session.read(cx).branch.clone();
        open_review(workspace, branch, TARGET.into(), true, window, cx);
    });
    workspace.register_action(|workspace, _: &CreateBranch, window, cx| {
        let Some(session) = workspace.session().cloned() else {
            return;
        };
        workspace.toggle_modal(window, cx, |window, cx| {
            CreateBranchModal::new(session, window, cx)
        });
    });
    workspace.register_action(|workspace, _: &DeleteBranch, window, cx| {
        let Some(session) = workspace.session().cloned() else {
            return;
        };
        let branch = session.read(cx).branch.clone();
        if branch == TARGET {
            workspace.show_toast(
                studio_workspace::ToastKind::Info,
                format!("{TARGET} cannot be deleted. Switch to another branch first."),
                cx,
            );
            return;
        }
        workspace.toggle_modal(window, cx, |window, cx| {
            DeleteBranchModal::new(session, branch, window, cx)
        });
    });
}
