//! Branches (spec 008): the sidebar list, switching, and the create and delete dialogs.
//!
//! OmniGraph's `GET /branches` returns names only (E9 waits on branch details), so each
//! branch's last commit comes from its own `GET /commits?branch=`, for up to
//! [`DETAIL_LIMIT`] branches.

use std::collections::HashMap;

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use graph_client::WriteOutcome;
use graph_client::api::{BranchStartRefusalOutput, BranchStartRefusalReason, CommitOutput};
use graph_store::{BranchStart, GraphSession, SessionEvent};
use studio_actions::graph::{
    CreateBranch, CreateBranchAt, DeleteBranch, MergeBranch, ReviewBranch,
};
use studio_ui::menu::{ContextMenu, ContextMenuEntry};
use studio_ui::{ds, now_micros, relative_time, short_commit_id};
use studio_workspace::{Workspace, WorkspaceEvent};
use ui::prelude::*;
use ui::{Tooltip, right_click_menu};

use crate::view::open_review;
use studio_workspace::ModalView;

/// Branches whose last commit is read for the list.
const DETAIL_LIMIT: usize = 20;

/// The branch every review and merge targets.
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
        // The same graph shown again (another opened behind it): its details hold.
        if self.session.as_ref().map(Entity::entity_id) == session.as_ref().map(Entity::entity_id) {
            return;
        }
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

    /// Each branch's head commit, read alone (or from a commit list the session holds): after
    /// a commit, only the branch it landed on is read again.
    fn load_details(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(branches) = session.read(cx).branches.loaded().cloned() else {
            return;
        };
        let reads: Vec<_> = branches
            .into_iter()
            .take(DETAIL_LIMIT)
            .map(|branch| {
                let read =
                    session.update(cx, |session, cx| session.head_commit(&branch, false, cx));
                (branch, read)
            })
            .collect();
        self._details_task = Some(cx.spawn(async move |this, cx| {
            let mut details = HashMap::new();
            for (branch, read) in reads {
                if let Ok(Some(head)) = read.await {
                    details.insert(
                        branch,
                        BranchDetail {
                            head: head.graph_commit_id,
                            created_at: head.created_at,
                            actor: head.actor_id,
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
                move |_, _, _| {
                    let (when, tooltip) = detail.clone().unzip();
                    let row = studio_ui::ds::SidebarRow::new(("branch", index), name.clone())
                        .icon(IconName::GitBranch)
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
                                    open_review(w, name.clone(), TARGET.into(), window, cx)
                                })
                                .ok();
                        }
                    };
                    // A merge is made from the review, which holds it.
                    let merge = review.clone();
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
            .icon(IconName::Plus)
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
                list = list.child(div().px(px(ds::space::MD)).child(
                    studio_ui::ds::text::error_note(format!("Branches could not be read: {error}")),
                ))
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
    studio_ui::ds::field_frame(false, cx).child(studio_ui::ds::field_input(editor.clone()))
}

/// Where the new branch starts, as the dialog shows it.
enum Start {
    /// The current branch's head.
    Head(String),
    /// A published commit, read as the dialog opens to say who made it and when.
    Commit { id: String, read: CommitRead },
}

enum CommitRead {
    Reading,
    Read(CommitOutput),
    /// Not read (a token whose read grant names branches is refused commits by id): the commit
    /// is named by its id alone, and the server still decides.
    Failed,
}

/// "New branch": a name, then the branch is created where it starts and switched to: the
/// current branch's head, or a published commit (History's "New branch here").
pub struct CreateBranchModal {
    session: Entity<GraphSession>,
    start: Start,
    name: Entity<Editor>,
    error: Option<SharedString>,
    /// Why the server would not start a branch at the commit. It is the commit's, not the
    /// name's, so it stays while the name changes, and Create with it: another name would be
    /// refused the same.
    refused_start: Option<SharedString>,
    running: bool,
    focus_handle: FocusHandle,
    _read: Option<Task<()>>,
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for CreateBranchModal {}

impl ModalView for CreateBranchModal {
    /// A write still running keeps the dialog open: its outcome shows here.
    fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        !self.running
    }
}

impl Focusable for CreateBranchModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl CreateBranchModal {
    /// At the current branch's head.
    pub fn new(session: Entity<GraphSession>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let from = session.read(cx).branch.clone();
        Self::starting(session, Start::Head(from), window, cx)
    }

    /// At a published commit, which is read for who made it and when.
    pub fn at_commit(
        session: Entity<GraphSession>,
        commit_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let read = session.update(cx, |session, cx| session.commit(&commit_id, cx));
        let start = Start::Commit {
            id: commit_id,
            read: CommitRead::Reading,
        };
        let mut this = Self::starting(session, start, window, cx);
        this._read = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| {
                if let Start::Commit { read: shown, .. } = &mut this.start {
                    *shown = read.map_or(CommitRead::Failed, CommitRead::Read);
                }
                cx.notify();
            })
            .ok();
        }));
        this
    }

    fn starting(
        session: Entity<GraphSession>,
        start: Start,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
            start,
            name,
            error: None,
            refused_start: None,
            running: false,
            focus_handle: cx.focus_handle(),
            _read: None,
            _task: None,
            _subscription: subscription,
        }
    }

    fn branch_start(&self) -> BranchStart {
        match &self.start {
            Start::Head(branch) => BranchStart::Head(branch.clone()),
            Start::Commit { id, .. } => BranchStart::Commit(id.clone()),
        }
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        if self.running || self.refused_start.is_some() {
            return;
        }
        let name = self.name.read(cx).text(cx).trim().to_owned();
        if name.is_empty() {
            self.error = Some("Enter a name for the branch.".into());
            cx.notify();
            return;
        }
        self.running = true;
        let start = self.branch_start();
        let task = self
            .session
            .update(cx, |s, cx| s.create_branch(name.clone(), start, cx));
        self._task = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| {
                this.running = false;
                match outcome {
                    WriteOutcome::Committed(_) => {
                        this.session.update(cx, |s, cx| s.set_branch(name, cx));
                        cx.emit(DismissEvent);
                    }
                    WriteOutcome::Refused(error) => match &error.body.branch_start_refusal {
                        Some(refusal) => {
                            this.refused_start =
                                Some(start_refusal_notice(refusal, &error.body.error).into())
                        }
                        None => {
                            this.error = Some(format!("Not created: {}", error.body.error).into())
                        }
                    },
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

/// Why the server would not start a branch at a commit, in plain words, with what to do
/// instead; nothing was created. A reason this build does not know reads as the server's own
/// message, as any other refusal does.
fn start_refusal_notice(refusal: &BranchStartRefusalOutput, message: &str) -> String {
    let commit = short_commit_id(&refusal.graph_commit_id);
    match refusal.reason {
        BranchStartRefusalReason::NotRetained => format!(
            "Not created: cleanup no longer keeps {} as it was at {commit}. Start the branch at \
             a newer commit.",
            refusal.type_name.as_deref().unwrap_or("the graph")
        ),
        BranchStartRefusalReason::SchemaChanged => format!(
            "Not created: {commit} was written under an earlier schema. Start the branch at a \
             commit made since the schema last changed."
        ),
        BranchStartRefusalReason::EarlierStorageFormat => format!(
            "Not created: {commit} was written before the graph's storage was upgraded. Start \
             the branch at a commit made since the upgrade."
        ),
        BranchStartRefusalReason::BranchDeleted => format!(
            "Not created: {}, the branch that wrote {commit}, was deleted. Start the branch at a \
             commit of a branch that still exists.",
            refusal.branch.as_deref().unwrap_or("the branch")
        ),
        BranchStartRefusalReason::UnpublishedCommit => format!(
            "Not created: {commit} is not published yet. A branch can start at it once it is \
             published."
        ),
        BranchStartRefusalReason::UnattachedSnapshot => format!(
            "Not created: {commit} is an unattached snapshot, which never joins a branch's \
             history."
        ),
        BranchStartRefusalReason::Unknown | BranchStartRefusalReason::Other => {
            format!("Not created: {message}")
        }
    }
}

impl Render for CreateBranchModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.name.read(cx).text(cx);
        let name = if name.trim().is_empty() {
            "<name>"
        } else {
            name.trim()
        };
        // A commit start is headed by its short id, then who made it and when, as History
        // shows a commit; the line keeps its place while the commit is read.
        let (title, about, cli) = match &self.start {
            Start::Head(from) => (
                format!("New branch from {from}"),
                None,
                format!("omnigraph branch create {name} --from {from}"),
            ),
            Start::Commit { id, read } => {
                let about = match read {
                    CommitRead::Reading => "Reading the commit…".to_owned(),
                    CommitRead::Read(commit) => format!(
                        "{} · {}",
                        commit.actor_id.as_deref().unwrap_or("—"),
                        relative_time(commit.created_at, now_micros())
                    ),
                    CommitRead::Failed => {
                        "Who made the commit, and when, could not be read.".to_owned()
                    }
                };
                (
                    format!("New branch at {}", short_commit_id(id)),
                    Some(SharedString::from(about)),
                    format!("omnigraph branch create {name} --from-commit {id}"),
                )
            }
        };
        let refused = self.refused_start.is_some();
        ds::modal(cx)
            .key_context("CreateBranchModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.create(cx)))
            .child(
                ds::ModalHeader::new(title)
                    .description(about)
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                ds::modal_body()
                    .child(field(&self.name, cx))
                    .child(studio_ui::ds::text::meta(cli).buffer_font(cx))
                    .children(self.error.clone().map(studio_ui::ds::text::error))
                    .children(self.refused_start.clone().map(studio_ui::ds::text::error)),
            )
            .child(
                ds::modal_footer(cx)
                    .child(
                        ds::cancel_button("cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        ds::action_button(
                            "create",
                            if self.running {
                                "Creating…"
                            } else {
                                "Create"
                            },
                        )
                        .disabled(self.running || refused)
                        .when(refused, |button| {
                            button.tooltip(Tooltip::text("A branch cannot start at this commit"))
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.create(cx))),
                    ),
            )
    }
}

/// What deleting a branch would lose, read from its merge base with [`TARGET`] when the
/// dialog opens.
#[derive(Debug, Clone, PartialEq)]
enum Holds {
    Reading,
    /// Its head is in the target: deleting it loses no commit.
    Merged,
    /// It has commits the target does not.
    Unmerged,
    /// The check could not be read: treated as unmerged.
    Unknown(String),
}

/// "Delete branch" (constitution IV). A branch whose every commit is already in `main` loses
/// nothing with it, so ↵ deletes it; one with commits only it holds, or one the check could not
/// read, is deleted by typing its name. `main` cannot be deleted.
pub struct DeleteBranchModal {
    session: Entity<GraphSession>,
    branch: String,
    holds: Holds,
    confirm: Entity<Editor>,
    error: Option<SharedString>,
    running: bool,
    focus_handle: FocusHandle,
    _check: Task<()>,
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for DeleteBranchModal {}

impl ModalView for DeleteBranchModal {
    /// A write still running keeps the dialog open: its outcome shows here.
    fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        !self.running
    }
}

impl Focusable for DeleteBranchModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
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
            editor.set_placeholder_text(&format!("Type {branch} to delete it"), window, cx);
            editor
        });
        let subscription = cx.subscribe(&confirm, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                this.error = None;
                cx.notify();
            }
        });
        let read = session.update(cx, |session, cx| session.merge_base(&branch, TARGET, cx));
        let check = cx.spawn_in(window, async move |this, cx| {
            let holds = match read.await {
                Ok(base) if base.merge_base.graph_commit_id == base.source_commit_id => {
                    Holds::Merged
                }
                Ok(_) => Holds::Unmerged,
                Err(error) => Holds::Unknown(error.to_string()),
            };
            this.update_in(cx, |this, window, cx| {
                if holds != Holds::Merged {
                    window.focus(&this.confirm.focus_handle(cx), cx);
                }
                this.holds = holds;
                cx.notify();
            })
            .ok();
        });
        Self {
            session,
            branch,
            holds: Holds::Reading,
            confirm,
            error: None,
            running: false,
            focus_handle: cx.focus_handle(),
            _check: check,
            _task: None,
            _subscription: subscription,
        }
    }

    /// Whether Delete may run: a merged branch at once, any other once its name is typed.
    fn confirmed(&self, cx: &App) -> bool {
        self.branch != TARGET
            && match self.holds {
                Holds::Reading => false,
                Holds::Merged => true,
                Holds::Unmerged | Holds::Unknown(_) => {
                    self.confirm.read(cx).text(cx).trim() == self.branch
                }
            }
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if self.running || !self.confirmed(cx) {
            return;
        }
        self.running = true;
        let branch = self.branch.clone();
        // Merged when the dialog opened, and asked again now: a commit landed on the branch
        // meanwhile is the branch's alone, and then the branch waits for its name.
        let recheck = matches!(self.holds, Holds::Merged).then(|| {
            self.session
                .update(cx, |session, cx| session.merge_base(&branch, TARGET, cx))
        });
        self._task =
            Some(cx.spawn(async move |this, cx| {
                if let Some(read) = recheck {
                    let holds = match read.await {
                        Ok(base) if base.merge_base.graph_commit_id == base.source_commit_id => {
                            Holds::Merged
                        }
                        Ok(_) => Holds::Unmerged,
                        Err(error) => Holds::Unknown(error.to_string()),
                    };
                    if !matches!(holds, Holds::Merged) {
                        this.update(cx, |this, cx| {
                            this.running = false;
                            this.error = Some(
                                match &holds {
                                    Holds::Unknown(error) => format!(
                                        "Studio could not check {} again: {error}. Type its \
                                         name to delete it.",
                                        this.branch
                                    ),
                                    _ => format!(
                                        "{} has a commit {TARGET} does not since this opened; \
                                         type its name to delete it.",
                                        this.branch
                                    ),
                                }
                                .into(),
                            );
                            this.holds = holds;
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                }
                let Ok(task) = this.update(cx, |this, cx| {
                    this.session.update(cx, |s, cx| s.delete_branch(branch, cx))
                }) else {
                    return;
                };
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
        let branch = &self.branch;
        let (what, typing) = match &self.holds {
            Holds::Reading => (
                format!("Checking whether {branch} is merged into {TARGET}…"),
                false,
            ),
            Holds::Merged => (
                format!("Everything on {branch} is in {TARGET}, so deleting it loses nothing."),
                false,
            ),
            Holds::Unmerged => (
                format!(
                    "{branch} has commits that are not in {TARGET}. Deleting it loses them, and \
                     Studio cannot bring them back."
                ),
                true,
            ),
            Holds::Unknown(error) => (
                format!(
                    "Studio could not tell whether {branch} is merged into {TARGET} ({error}). \
                     Deleting it may lose commits."
                ),
                true,
            ),
        };
        let cli = format!("omnigraph branch delete {branch}");
        let running = self.running;
        ds::modal(cx)
            .key_context("DeleteBranchModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.delete(cx)))
            // What it loses is the description; the body holds only the field that asks for
            // the branch's name when it may lose commits.
            .child(
                ds::ModalHeader::new(format!("Delete {branch}?"))
                    .description(SharedString::from(what))
                    .close_disabled(running)
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .when(typing || self.error.is_some(), |dialog| {
                dialog.child(
                    ds::modal_body()
                        .when(typing, |body| body.child(field(&self.confirm, cx)))
                        .children(self.error.clone().map(studio_ui::ds::text::error)),
                )
            })
            .child(
                ds::modal_footer(cx)
                    .child(
                        ds::cancel_button("cancel")
                            .disabled(running)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        ds::action_button(
                            "delete",
                            if self.running {
                                "Deleting…"
                            } else {
                                "Delete"
                            },
                        )
                        .variant(ds::Variant::Destructive)
                        .disabled(!confirmed || self.running)
                        .tooltip(move |_, cx| {
                            Tooltip::with_meta("Delete the branch", None, cli.clone(), cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
                    ),
            )
    }
}

/// Registers the branch actions on a workspace.
pub fn register(workspace: &mut Workspace) {
    // A merge is made from the review, which holds it: both open the review of the branch.
    fn review_current(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let Some(session) = workspace.session() else {
            return;
        };
        let branch = session.read(cx).branch.clone();
        open_review(workspace, branch, TARGET.into(), window, cx);
    }
    workspace.register_action(|workspace, _: &ReviewBranch, window, cx| {
        review_current(workspace, window, cx)
    });
    workspace.register_action(|workspace, _: &MergeBranch, window, cx| {
        review_current(workspace, window, cx)
    });
    workspace.register_action(|workspace, _: &CreateBranch, window, cx| {
        let Some(session) = workspace.session().cloned() else {
            return;
        };
        workspace.toggle_modal(window, cx, |window, cx| {
            CreateBranchModal::new(session, window, cx)
        });
    });
    // A start at a commit replaces whatever dialog is open, rather than closing one.
    workspace.register_action(|workspace, action: &CreateBranchAt, window, cx| {
        let Some(session) = workspace.session().cloned() else {
            return;
        };
        let commit = action.commit_id.clone();
        workspace.show_modal(window, cx, |window, cx| {
            CreateBranchModal::at_commit(session, commit, window, cx)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::view::tests::fake_server;

    /// A merged branch is deleted with ↵; one with commits only it holds waits for its name.
    #[gpui::test]
    async fn a_merged_branch_deletes_at_once_and_an_unmerged_one_by_its_name(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let commit = |id: &str| json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": "published", "created_at": 0 });
        let late_reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let late = late_reads.clone();
        let (session, asked) = fake_server(
            move |path, query, _| {
                if path.ends_with("/branches/merge-base") {
                    // `feature`'s head is in main; `spike` has a commit of its own; `late` is
                    // merged when its dialog opens and has a commit of its own at the delete.
                    let late_merged = query.contains("source=late")
                        && late.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                    let (base, head) = if query.contains("source=feature") || late_merged {
                        ("f1", "f1")
                    } else {
                        ("m1", "s1")
                    };
                    (
                        200,
                        json!({ "merge_base": commit(base), "source": "x", "source_commit_id": head, "target": "main", "target_commit_id": "m2" }),
                    )
                } else if path.contains("/branches/") {
                    (200, json!({}))
                } else {
                    (200, json!({ "branches": ["main", "feature", "spike"] }))
                }
            },
            cx,
        );
        let deletes = || {
            asked
                .lock()
                .unwrap()
                .iter()
                .filter(|request| {
                    request.contains("/branches/feature?")
                        || request.contains("/branches/spike?")
                        || request.contains("/branches/late?")
                })
                .count()
        };
        let (merged, cx) = cx.add_window_view(|window, cx| {
            DeleteBranchModal::new(session.clone(), "feature".into(), window, cx)
        });
        cx.run_until_parked();
        merged.update(cx, |modal, cx| {
            assert_eq!(modal.holds, Holds::Merged);
            modal.delete(cx);
        });
        cx.run_until_parked();
        assert_eq!(deletes(), 1, "deleted with no name typed");

        let (unmerged, cx) = cx.add_window_view(|window, cx| {
            DeleteBranchModal::new(session.clone(), "spike".into(), window, cx)
        });
        cx.run_until_parked();
        unmerged.update(cx, |modal, cx| {
            assert_eq!(modal.holds, Holds::Unmerged);
            modal.delete(cx);
        });
        cx.run_until_parked();
        assert_eq!(deletes(), 1, "not without its name");
        unmerged.update_in(cx, |modal, window, cx| {
            modal
                .confirm
                .update(cx, |editor, cx| editor.set_text("spike", window, cx));
            modal.delete(cx);
        });
        cx.run_until_parked();
        assert_eq!(deletes(), 2);

        let (late, cx) = cx.add_window_view(|window, cx| {
            DeleteBranchModal::new(session.clone(), "late".into(), window, cx)
        });
        cx.run_until_parked();
        late.update(cx, |modal, cx| {
            assert_eq!(modal.holds, Holds::Merged);
            modal.delete(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            deletes(),
            2,
            "a commit landed meanwhile: not without its name"
        );
        late.read_with(cx, |modal, _| {
            assert_eq!(modal.holds, Holds::Unmerged);
            assert!(modal.error.is_some());
        });
    }

    /// A branch at a commit is asked for with the commit alone, never a branch too, and the
    /// dialog names who made the commit and when. Each refusal to start there says why in
    /// words, naming the type or the branch it is about, and nothing is sent again: not by
    /// itself, and not by Create, which stays off while the name changes. A reason this build
    /// does not know reads as the server's message.
    #[gpui::test]
    async fn a_branch_at_a_commit_says_why_it_was_refused_and_is_not_sent_again(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        // What each commit's start is refused with: the reason, its detail and its status.
        let refusals = [
            ("c1", "not_retained", json!({ "type_name": "Person" }), 410),
            ("c2", "schema_changed", json!({}), 409),
            ("c3", "earlier_storage_format", json!({}), 409),
            ("c4", "branch_deleted", json!({ "branch": "spike" }), 409),
            ("c5", "unpublished_commit", json!({}), 409),
            ("c6", "unattached_snapshot", json!({}), 409),
            ("c7", "unknown", json!({}), 409),
            ("c8", "a_later_reason", json!({}), 409),
        ];
        let notices = [
            "Not created: cleanup no longer keeps Person as it was at c1. Start the branch at a \
             newer commit.",
            "Not created: c2 was written under an earlier schema. Start the branch at a commit \
             made since the schema last changed.",
            "Not created: c3 was written before the graph's storage was upgraded. Start the \
             branch at a commit made since the upgrade.",
            "Not created: spike, the branch that wrote c4, was deleted. Start the branch at a \
             commit of a branch that still exists.",
            "Not created: c5 is not published yet. A branch can start at it once it is \
             published.",
            "Not created: c6 is an unattached snapshot, which never joins a branch's history.",
            "Not created: cannot start a branch at commit 'c7'",
            "Not created: cannot start a branch at commit 'c8'",
        ];
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let creates = sent.clone();
        let served = refusals.clone();
        // The branches the server lists: those it made, once it makes them.
        let made = std::sync::Arc::new(std::sync::Mutex::new(vec![
            "main".to_owned(),
            "feature".to_owned(),
        ]));
        let (session, _) = fake_server(
            move |path, _, body| {
                if let Some(id) = path.rsplit_once("/commits/").map(|(_, id)| id) {
                    let commit = json!({ "graph_commit_id": id, "graph_manifest_version": 1, "kind": "published", "created_at": 0, "actor_id": "act-alice" });
                    return (200, commit);
                }
                if !path.ends_with("/branches") || body.is_empty() {
                    return (200, json!({ "branches": *made.lock().unwrap() }));
                }
                let asked: serde_json::Value = serde_json::from_str(body).unwrap();
                creates.lock().unwrap().push(asked.clone());
                let at = asked["from_commit"].as_str().unwrap_or_default().to_owned();
                match served.iter().find(|(commit, ..)| *commit == at) {
                    Some((commit, reason, detail, status)) => {
                        let mut refusal = json!({ "reason": reason, "graph_commit_id": commit });
                        for (key, value) in detail.as_object().unwrap() {
                            refusal[key] = value.clone();
                        }
                        let error = format!("cannot start a branch at commit '{commit}'");
                        (
                            *status,
                            json!({ "error": error, "code": "conflict", "branch_start_refusal": refusal }),
                        )
                    }
                    None => {
                        let name = asked["name"].as_str().unwrap_or_default().to_owned();
                        made.lock().unwrap().push(name.clone());
                        (
                            200,
                            json!({ "uri": "u", "from": "main", "name": name, "from_commit_id": at }),
                        )
                    }
                }
            },
            cx,
        );
        for ((commit, ..), notice) in refusals.iter().zip(notices) {
            let (modal, cx) = cx.add_window_view(|window, cx| {
                CreateBranchModal::at_commit(session.clone(), (*commit).to_owned(), window, cx)
            });
            cx.run_until_parked();
            modal.update_in(cx, |modal, window, cx| {
                assert!(matches!(
                    &modal.start,
                    Start::Commit { read: CommitRead::Read(read), .. }
                        if read.actor_id.as_deref() == Some("act-alice")
                ));
                modal
                    .name
                    .update(cx, |editor, cx| editor.set_text("then", window, cx));
                modal.create(cx);
            });
            cx.run_until_parked();
            modal.update_in(cx, |modal, window, cx| {
                assert_eq!(modal.refused_start.as_deref(), Some(notice), "{commit}");
                assert_eq!(modal.error, None);
                // The refusal is the commit's: another name keeps it, and sends nothing.
                modal
                    .name
                    .update(cx, |editor, cx| editor.set_text("again", window, cx));
                modal.create(cx);
            });
            cx.run_until_parked();
            modal.read_with(cx, |modal, _| {
                assert_eq!(modal.refused_start.as_deref(), Some(notice));
                assert!(!modal.running);
            });
        }
        assert_eq!(
            sent.lock().unwrap().len(),
            refusals.len(),
            "each start was sent once"
        );

        // A start the server takes: the commit alone is sent, and the session moves there.
        let (modal, cx) = cx.add_window_view(|window, cx| {
            CreateBranchModal::at_commit(session.clone(), "c9".into(), window, cx)
        });
        cx.run_until_parked();
        modal.update_in(cx, |modal, window, cx| {
            modal
                .name
                .update(cx, |editor, cx| editor.set_text("then", window, cx));
            modal.create(cx);
        });
        cx.run_until_parked();
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .all(|asked| asked.get("from").is_none())
        );
        assert_eq!(
            sent.lock().unwrap().last(),
            Some(&json!({ "name": "then", "from_commit": "c9" }))
        );
        session.read_with(cx, |session, _| {
            assert_eq!(session.branch, "then");
            assert_eq!(session.head("then"), Some("c9"));
        });
    }
}
