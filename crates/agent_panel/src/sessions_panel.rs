//! The Conversations panel of the left sidebar (spec 011): the graph's conversations with
//! agents, as Delta's sidebar lists threads and Zed's lists its agent threads. Pinned ones
//! first, then by the person's last message; each row is the conversation's title with what it
//! is doing (running, waiting for the person, failed, finished unseen), when it was last active
//! in its tooltip, and pin and archive on hover. A right click renames, pins, archives or deletes. The
//! Filter button narrows the list by title or agent; archived ones fold away at the end; the agent's
//! own conversations in the graph's directory can be imported. A row opens the conversation in
//! its own tab, or shows the tab it is in.

use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol::schema::v1::SessionInfo;
use chrono::{DateTime, Utc};
use editor::{Editor, EditorEvent};
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ListAlignment, ListState, ParentElement, Render, SharedString, Styled,
    Subscription, WeakEntity, Window, div, list,
};
use studio_actions::agent::{NewConversation, OpenAgent, OpenConversation};
use studio_settings::StudioSettings;
use studio_ui::ds;
use studio_ui::ds::Prose as _;
use studio_ui::menu::ContextMenu;
use studio_workspace::{ToastKind, Workspace, WorkspaceEvent};
use ui::prelude::*;
use ui::{Indicator, PopoverMenu, Tooltip, right_click_menu};

use crate::connection::{self, AgentConnection};
use crate::host::AgentLaunch;
use crate::sessions::{AgentSessions, Live, SessionRecord, graph_cwd};
use studio_workspace::ModalView;

pub const PANEL_ID: &str = "agent-conversations";

/// How long an import or a delete waits for the agent's answer: past it the answer is given up,
/// and the connection with it, so an agent that never answers keeps no process for it.
const AGENT_ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(120);

/// `answer`, or `None` when the agent has not answered within [`AGENT_ANSWER_WITHIN`].
async fn within_time<T>(
    answer: futures::channel::oneshot::Receiver<T>,
    cx: &gpui::AsyncApp,
) -> Option<Result<T, futures::channel::oneshot::Canceled>> {
    let late = cx.background_executor().timer(AGENT_ANSWER_WITHIN);
    match futures::future::select(answer, late).await {
        futures::future::Either::Left((answer, _)) => Some(answer),
        futures::future::Either::Right(_) => None,
    }
}

pub struct SessionsPanel {
    workspace: WeakEntity<Workspace>,
    sessions: Entity<AgentSessions>,
    show_archived: bool,
    search: Entity<Editor>,
    /// The conversation being renamed, and its title field.
    renaming: Option<(String, Entity<Editor>, Subscription)>,
    /// The search field shows: asked for in the header, or holding text.
    search_open: bool,
    /// The list drawn, as descriptors: only the visible rows are built (constitution IX).
    list_state: ListState,
    rows: Vec<ListRow>,
    /// Something the rows are made from changed (the conversations, the search, the graph,
    /// what shows): they are made again at the next draw, not at every frame.
    rows_stale: bool,
    /// The conversation of the tab shown as last seen, so the list draws again when another
    /// is shown, not whenever the workspace does.
    shown_conversation: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// One row of the list, as what it shows rather than its element, so the list is told only of
/// the rows that changed and keeps its scroll position.
#[derive(Clone, PartialEq)]
enum ListRow {
    Search,
    Note(Note),
    /// A section's label; the first sits under the panel's own title, not a section's gap.
    Label {
        title: &'static str,
        first: bool,
    },
    Archived {
        count: usize,
        open: bool,
    },
    /// A conversation's row, with its record as the list was made: the list is made again
    /// when any record changes, so a frame does not look each one up.
    Conversation(Rc<SessionRecord>),
    End,
}

/// The note a list with no conversation row shows, or one above every row.
#[derive(Clone, Copy, PartialEq)]
enum Note {
    NoMatch,
    NoConversations,
    /// Studio could neither read the list's file nor set it aside, so it does not save the
    /// list over it.
    NotSaved,
}

/// What a row shows at its end.
#[derive(Clone)]
enum RowState {
    Running,
    Waiting,
    Failed,
    Unread,
    Idle,
}

impl SessionsPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sessions = AgentSessions::global(cx);
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter by title or agent", window, cx);
            editor.set_text_style_refinement(ds::field_text(cx));
            editor
        });
        let mut subscriptions = vec![
            cx.observe(&sessions, |this, _, cx| this.rows_changed(cx)),
            cx.subscribe(&search, |this, _, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.rows_changed(cx);
                }
            }),
        ];
        // The graph decides which are listed, so a graph shown or closed makes the rows again.
        // The shown tab marks its conversation, drawn as the rows draw: the workspace redraws
        // for many things (a spinner's frame, a toast), and only a change of the tab shown
        // draws the list again.
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(
                cx.subscribe(
                    &workspace,
                    |this, _, event: &WorkspaceEvent, cx| match event {
                        WorkspaceEvent::SessionChanged => this.rows_changed(cx),
                    },
                ),
            );
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                let shown = this.shown(cx);
                if shown != this.shown_conversation {
                    this.shown_conversation = shown;
                    cx.notify();
                }
            }));
        }
        // Made while its workspace is being made (`observe_new`), so the workspace is read once
        // that is done.
        cx.defer_in(window, |this, _, cx| {
            this.shown_conversation = this.shown(cx);
            cx.notify();
        });
        Self {
            workspace,
            sessions,
            show_archived: false,
            search,
            renaming: None,
            search_open: false,
            list_state: ListState::new(0, ListAlignment::Top, px(ds::size::SIDEBAR_ROW * 8.)),
            rows: Vec::new(),
            rows_stale: true,
            shown_conversation: None,
            _subscriptions: subscriptions,
        }
    }

    /// The rows are made again at the next draw.
    fn rows_changed(&mut self, cx: &mut Context<Self>) {
        self.rows_stale = true;
        cx.notify();
    }

    fn graph(&self, cx: &App) -> Option<String> {
        let workspace = self.workspace.upgrade()?;
        let session = workspace.read(cx).session()?.clone();
        Some(session.read(cx).overrides_key())
    }

    /// The conversation in the shown tab; a new one has none until the agent names it.
    fn shown(&self, cx: &App) -> Option<String> {
        shown_in(&self.workspace, cx)
    }

    /// Whether the settings name an agent, asked without making its launch.
    fn has_agent(cx: &App) -> bool {
        !StudioSettings::get(cx).agent.command.is_empty()
    }

    /// The agent the settings name, as this panel runs it to import or delete conversations.
    fn agent(cx: &App) -> Option<AgentLaunch> {
        let settings = &StudioSettings::get(cx).agent;
        Self::has_agent(cx)
            .then(|| AgentLaunch::new(settings.command.clone(), settings.env.clone()))
    }

    // ---- rename ----

    fn start_rename(
        &mut self,
        record: &SessionRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = record.shown_title().to_owned();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(title, window, cx);
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
        self.renaming = Some((record.session.clone(), editor, subscription));
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some((id, editor, _)) = self.renaming.take() else {
            return;
        };
        let title = editor.read(cx).text(cx);
        // The conversation may have been rebound since the field opened (its agent could not
        // load it): the name goes to it under the id it has now.
        self.sessions.update(cx, |sessions, cx| {
            let id = sessions.current(&id);
            sessions.rename(&id, &title, cx)
        });
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        cx.notify();
    }

    // ---- delete and import, at the agent ----

    fn confirm_delete(
        &mut self,
        record: SessionRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let panel = cx.entity().downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                DeleteConversation::new(record, panel, window, cx)
            });
        });
    }

    /// Forgets the conversation here and asks the agent to delete it, when it can.
    fn delete(&mut self, record: &SessionRecord, cx: &mut Context<Self>) {
        // Under the id it has now: it may have been rebound while the confirmation was open.
        let id = self.sessions.read(cx).current(&record.session);
        self.sessions
            .update(cx, |sessions, cx| sessions.remove(&id, cx));
        let Some(agent) = Self::agent(cx).filter(|agent| agent.command.join(" ") == record.agent)
        else {
            self.toast(
                ToastKind::Info,
                "Removed. Another agent holds the conversation, so it stays there.",
                cx,
            );
            return;
        };
        let connection = connection::connection(agent, cx);
        let answer = connection.delete(id);
        cx.spawn(async move |this, cx| {
            let answer = within_time(answer, cx)
                .await
                .unwrap_or_else(|| Ok(Err("it did not answer in two minutes".to_owned())));
            let (kind, note) = match answer {
                Ok(Ok(())) => (
                    ToastKind::Success,
                    "Deleted, here and at the agent.".to_owned(),
                ),
                Ok(Err(reason)) => (
                    ToastKind::Info,
                    format!("Removed here; the agent did not delete it ({reason})."),
                ),
                Err(_) => (
                    ToastKind::Info,
                    "Removed here; the agent stopped before deleting it.".to_owned(),
                ),
            };
            drop(connection);
            this.update(cx, |this, cx| this.toast(kind, note, cx)).ok();
        })
        // An import's or a delete's outcome, said when the agent answers: a second one does not
        // cancel the first, and the connection stays until then.
        .detach();
    }

    /// Says how an import or a delete went, as Studio reports work that finishes away from where
    /// it started.
    fn toast(&self, kind: ToastKind, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let message = message.into();
        self.workspace
            .update(cx, |workspace, cx| workspace.show_toast(kind, message, cx))
            .ok();
    }

    fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_search_open(!self.search_open, window, cx);
    }

    /// Opens the search field with the keyboard in it, or closes it and lets its text go.
    fn set_search_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = open;
        if open {
            window.focus(&self.search.focus_handle(cx), cx);
        } else {
            self.search
                .update(cx, |editor, cx| editor.clear(window, cx));
        }
        self.rows_changed(cx);
    }

    /// Adds the agent's conversations in this graph's directory that the list does not have.
    fn import(&mut self, cx: &mut Context<Self>) {
        let (Some(graph), Some(agent)) = (self.graph(cx), Self::agent(cx)) else {
            return;
        };
        let cwd = graph_cwd(&graph);
        let agent_name = agent.command.join(" ");
        let connection: Arc<AgentConnection> = connection::connection(agent, cx);
        let answer = connection.list(cwd.clone());
        cx.spawn(async move |this, cx| {
            let result = within_time(answer, cx)
                .await
                .unwrap_or_else(|| Ok(Err("The agent did not answer in two minutes.".to_owned())));
            drop(connection);
            // The list is asked for this graph's folder, a filter an agent may not keep: one
            // elsewhere is no conversation of this graph, and opened here it would be handed the
            // graph's gateway. Compared off the main thread, links resolved.
            let result = match result {
                Ok(Ok(found)) => {
                    let cwd = cwd.clone();
                    Ok(Ok(cx
                        .background_spawn(async move {
                            found
                                .into_iter()
                                .filter(|info| same_folder(&info.cwd, &cwd))
                                .collect::<Vec<_>>()
                        })
                        .await))
                }
                other => other,
            };
            this.update(cx, |this, cx| {
                let (kind, note) = match result {
                    Ok(Ok(found)) => {
                        let records = found
                            .into_iter()
                            .map(|info| imported(info, &graph, &agent_name, &cwd))
                            .collect();
                        let added = this
                            .sessions
                            .update(cx, |sessions, cx| sessions.import(records, cx));
                        (
                            ToastKind::Success,
                            match added {
                                0 => "No conversations to import: the list has them all.".into(),
                                1 => "Imported 1 conversation.".into(),
                                n => format!("Imported {n} conversations."),
                            },
                        )
                    }
                    Ok(Err(reason)) => (ToastKind::Info, reason),
                    Err(_) => (
                        ToastKind::Info,
                        "The agent stopped before answering.".to_owned(),
                    ),
                };
                this.toast(kind, note, cx);
            })
            .ok();
        })
        // An import's or a delete's outcome, said when the agent answers: a second one does not
        // cancel the first, and the connection stays until then.
        .detach();
    }

    // ---- rows ----

    fn row(&self, record: &SessionRecord, shown: bool, cx: &mut Context<Self>) -> AnyElement {
        let id = record.session.clone();
        if let Some((_, editor, _)) = self.renaming.as_ref().filter(|(r, _, _)| *r == id) {
            return h_flex()
                .h(px(ds::size::SIDEBAR_ROW))
                .mx(px(ds::space::SM))
                .px(px(ds::space::SM))
                .items_center()
                .rounded(px(ds::radii(cx).sm))
                .bg(ds::surface::selected_chrome(cx))
                .key_context("ConversationTitle")
                .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.commit_rename(cx)))
                .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.cancel_rename(cx)))
                .child(div().flex_1().prose().child(editor.clone()))
                .into_any_element();
        }
        let sessions = self.sessions.read(cx);
        let state = match sessions.live(&id) {
            Some(Live::Running) => RowState::Running,
            Some(Live::Waiting) => RowState::Waiting,
            Some(Live::Failed) => RowState::Failed,
            None if sessions.is_unread(&id) => RowState::Unread,
            None => RowState::Idle,
        };
        let panel = cx.entity().downgrade();
        let store = self.sessions.clone();
        let record = record.clone();
        right_click_menu(SharedString::from(format!("conversation-menu-{id}")))
            .trigger({
                let (record, store) = (record.clone(), store.clone());
                move |_, _, _| conversation_row(&record, shown, &state, &store)
            })
            .menu(move |window, cx| {
                let (record, panel, store) = (record.clone(), panel.clone(), store.clone());
                ContextMenu::build(window, cx, move |menu, _, _| {
                    let rename = {
                        let (record, panel) = (record.clone(), panel.clone());
                        move |window: &mut Window, cx: &mut App| {
                            panel
                                .update(cx, |panel, cx| panel.start_rename(&record, window, cx))
                                .ok();
                        }
                    };
                    let pin = {
                        let (id, pinned, store) =
                            (record.session.clone(), record.pinned, store.clone());
                        move |_: &mut Window, cx: &mut App| {
                            store.update(cx, |s, cx| s.set_pinned(&id, !pinned, cx));
                        }
                    };
                    let archive = {
                        let (id, archived, store) =
                            (record.session.clone(), record.archived, store.clone());
                        move |_: &mut Window, cx: &mut App| {
                            store.update(cx, |s, cx| s.set_archived(&id, !archived, cx));
                        }
                    };
                    let delete = {
                        let (record, panel) = (record.clone(), panel.clone());
                        move |window: &mut Window, cx: &mut App| {
                            panel
                                .update(cx, |panel, cx| {
                                    panel.confirm_delete(record.clone(), window, cx)
                                })
                                .ok();
                        }
                    };
                    menu.entry("Rename", None, rename)
                        .when(!record.archived, |menu| {
                            menu.entry(if record.pinned { "Unpin" } else { "Pin" }, None, pin)
                        })
                        .entry(
                            if record.archived {
                                "Restore"
                            } else {
                                "Archive"
                            },
                            None,
                            archive,
                        )
                        .separator()
                        .entry("Delete…", None, delete)
                })
            })
            .into_any_element()
    }
}

/// A conversation's row, from what it holds: the title, its state or time at the end, Pin and
/// Archive on hover, the details in the tooltip; a click opens it.
fn conversation_row(
    record: &SessionRecord,
    shown: bool,
    state: &RowState,
    store: &Entity<AgentSessions>,
) -> ds::SidebarRow {
    let id = record.session.clone();
    let (end, state_text): (AnyElement, Option<&str>) = match state {
        RowState::Running => (studio_ui::ds::spinner().into_any_element(), Some("Running")),
        RowState::Waiting => (
            studio_ui::ds::icon::meta(IconName::Warning)
                .color(Color::Warning)
                .into_any_element(),
            Some("Waiting for your answer"),
        ),
        RowState::Failed => (
            studio_ui::ds::icon::meta(IconName::XCircle)
                .color(Color::Error)
                .into_any_element(),
            Some("The agent stopped"),
        ),
        RowState::Unread => (
            Indicator::dot().color(Color::Accent).into_any_element(),
            Some("Finished while you were away"),
        ),
        // No time on the row, as Delta's threads: it is in the tooltip.
        RowState::Idle => (div().into_any_element(), None),
    };
    let when = match state {
        RowState::Idle => studio_ui::relative_time(
            record.updated_at.timestamp_micros(),
            studio_ui::now_micros(),
        ),
        _ => record
            .updated_at
            .with_timezone(&chrono::Local)
            .format("%-d %b %H:%M")
            .to_string(),
    };
    let mut about = format!(
        "{} · {} · you last wrote {when}",
        record.shown_title(),
        agent_name(&record.agent),
    );
    if let Some(state) = state_text {
        about = format!("{state} · {about}");
    }
    let pin = {
        let (id, pinned, store) = (id.clone(), record.pinned, store.clone());
        ds::row_icon_button(
            SharedString::from(format!("pin-{id}")),
            if pinned {
                IconName::Unpin
            } else {
                IconName::Pin
            },
        )
        .icon_color(Color::Muted)
        .tooltip(Tooltip::text(if pinned { "Unpin" } else { "Pin" }))
        .on_click(move |_, _, cx| {
            store.update(cx, |store, cx| store.set_pinned(&id, !pinned, cx));
        })
    };
    let archive = {
        let (id, archived, store) = (id.clone(), record.archived, store.clone());
        ds::row_icon_button(
            SharedString::from(format!("archive-{id}")),
            if archived {
                IconName::Undo
            } else {
                IconName::Archive
            },
        )
        .icon_color(Color::Muted)
        .tooltip(Tooltip::text(if archived {
            "Restore to the list"
        } else {
            "Archive (the agent keeps the conversation)"
        }))
        .on_click(move |_, _, cx| {
            store.update(cx, |store, cx| store.set_archived(&id, !archived, cx));
        })
    };
    ds::SidebarRow::new(
        SharedString::from(format!("conversation-{id}")),
        record.shown_title().to_owned(),
    )
    .selected(shown)
    .muted(record.archived)
    .end(end)
    .actions(
        h_flex()
            .gap(px(ds::space::XXS))
            .when(!record.archived, |row| row.child(pin))
            .child(archive),
    )
    .tooltip(about)
    .on_click(move |_, window, cx| {
        window.dispatch_action(
            Box::new(OpenConversation {
                session: id.clone(),
            }),
            cx,
        )
    })
}

/// Whether `a` and `b` name one folder: the same path, or the same once links are resolved.
fn same_folder(a: &Path, b: &Path) -> bool {
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .is_some_and(|a| std::fs::canonicalize(b).is_ok_and(|b| a == b))
}

/// A conversation the agent knows and the list does not, as a record: the agent's title (one
/// line, cut as a title Studio makes is) and time, a branch named for when it was last active.
fn imported(info: SessionInfo, graph: &str, agent: &str, cwd: &Path) -> SessionRecord {
    let updated = info
        .updated_at
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    SessionRecord {
        session: info.session_id.0.to_string(),
        graph: graph.to_owned(),
        agent: agent.to_owned(),
        title: info.title.as_deref().and_then(crate::sessions::title_of),
        title_override: None,
        created_at: updated,
        updated_at: updated,
        branch: format!(
            "agent/{}",
            updated
                .with_timezone(&chrono::Local)
                .format("%Y%m%d-%H%M%S")
        ),
        branch_created: false,
        pinned: false,
        archived: false,
        cwd: cwd.to_owned(),
    }
}

impl SessionsPanel {
    /// The rows to draw for `graph`: the search field when open, pinned conversations, then
    /// recent ones, then the archive folded at the end. A search looks through the archive
    /// too.
    fn list_rows(&self, graph: &str, cx: &App) -> Vec<ListRow> {
        let sessions = self.sessions.read(cx);
        let query = self.search.read(cx).text(cx).trim().to_lowercase();
        let searching = !query.is_empty();
        let matches = |record: &&SessionRecord| {
            query.is_empty()
                || record.shown_title().to_lowercase().contains(&query)
                || agent_name(&record.agent).to_lowercase().contains(&query)
        };
        let (pinned, recent): (Vec<&SessionRecord>, Vec<&SessionRecord>) = sessions
            .for_graph(graph)
            .into_iter()
            .filter(matches)
            .partition(|record| record.pinned);
        let archived: Vec<&SessionRecord> = sessions
            .archived(graph)
            .into_iter()
            .filter(matches)
            .collect();
        let id = |record: &&SessionRecord| ListRow::Conversation(Rc::new((*record).clone()));
        let mut rows = Vec::new();
        if sessions.not_saved().is_some() {
            rows.push(ListRow::Note(Note::NotSaved));
        }
        if self.search_open || searching {
            rows.push(ListRow::Search);
        }
        if pinned.is_empty() && recent.is_empty() && archived.is_empty() {
            rows.push(ListRow::Note(if searching {
                Note::NoMatch
            } else {
                Note::NoConversations
            }));
        }
        if !pinned.is_empty() {
            rows.push(ListRow::Label {
                title: "Pinned",
                first: true,
            });
            rows.extend(pinned.iter().map(id));
            if !recent.is_empty() {
                rows.push(ListRow::Label {
                    title: "Recent",
                    first: false,
                });
            }
        }
        rows.extend(recent.iter().map(id));
        if !archived.is_empty() {
            let open = self.show_archived || searching;
            rows.push(ListRow::Archived {
                count: archived.len(),
                open,
            });
            if open {
                rows.extend(archived.iter().map(id));
            }
        }
        rows.push(ListRow::End);
        rows
    }

    fn render_list_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        match row {
            // The field's text sits on the rows' edge, as their titles do; ⎋ closes it.
            ListRow::Search => ds::sidebar_search(self.search.clone(), cx)
                .key_context("ConversationSearch")
                .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                    this.set_search_open(false, window, cx)
                }))
                .into_any_element(),
            ListRow::Note(Note::NoMatch) => {
                ds::sidebar_note("No conversation matches.").into_any_element()
            }
            // The panel's foot asks the agent, with its keys.
            ListRow::Note(Note::NoConversations) => {
                ds::sidebar_note("No conversations yet.").into_any_element()
            }
            // By its path only, which holds no conversation.
            ListRow::Note(Note::NotSaved) => {
                let file = self
                    .sessions
                    .read(cx)
                    .not_saved()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                ds::sidebar_note(format!(
                    "Studio could not read {file} or move it aside, so this list is not saved: \
                     changes last until Studio quits. Close any program that has the file \
                     open, then restart Studio."
                ))
                .into_any_element()
            }
            ListRow::Label { title, first } => ds::section_label(title)
                .when(first, |label| label.pt(px(ds::space::XS)))
                .into_any_element(),
            ListRow::Archived { count, open } => {
                ds::section_disclosure("archived", "Archived", count, open)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_archived = !this.show_archived;
                        this.rows_changed(cx);
                    }))
                    .into_any_element()
            }
            ListRow::Conversation(record) => {
                // As last seen ([`Self::shown_conversation`]): a running conversation's spinner
                // draws the list at every frame, and the workspace is not asked at each.
                let shown = self.shown_conversation.as_deref() == Some(record.session.as_str());
                self.row(&record, shown, cx)
            }
            ListRow::End => div().h(px(ds::space::SM)).into_any_element(),
        }
    }
}

impl Render for SessionsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(graph) = self.graph(cx) else {
            return v_flex().into_any_element();
        };
        if self.rows_stale {
            let rows = self.list_rows(&graph, cx);
            studio_ui::rows::sync(&self.list_state, &mut self.rows, rows);
            self.rows_stale = false;
        }
        // The conversations, then at the panel's foot the way to ask the agent, as the
        // reference sidebar ends with its "Ask" launcher: an outlined pill with its keys.
        v_flex()
            .size_full()
            .child(
                list(
                    self.list_state.clone(),
                    // A row's root takes the list's width, so a note wraps at it.
                    cx.processor(|this, ix, _window, cx| {
                        div()
                            .w_full()
                            .child(this.render_list_row(ix, cx))
                            .into_any_element()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(ds::space::SM))
                    .pb(px(ds::space::SM))
                    .child(
                        ds::button("ask-agent", "Ask the agent")
                            .style(ButtonStyle::Outlined)
                            .full_width()
                            .start_icon(Icon::new(IconName::ZedAssistant))
                            .key_caps(ds::KeyCaps::for_action(&OpenAgent))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenAgent), cx)
                            }),
                    ),
            )
            .into_any_element()
    }
}

/// The Conversations panel's header buttons: search, More (import), and a new conversation.
/// The conversation of the tab `workspace` shows, if it shows one.
fn shown_in(workspace: &WeakEntity<Workspace>, cx: &App) -> Option<String> {
    let workspace = workspace.upgrade()?;
    let key = workspace.read(cx).active_item()?.dedupe_key(cx)?;
    key.strip_prefix("agent:").map(str::to_owned)
}

pub struct SessionsActions {
    panel: WeakEntity<SessionsPanel>,
    _observe: Subscription,
}

impl SessionsActions {
    pub fn new(panel: &Entity<SessionsPanel>, cx: &mut Context<Self>) -> Self {
        Self {
            panel: panel.downgrade(),
            _observe: cx.observe(panel, |_, _, cx| cx.notify()),
        }
    }
}

impl Render for SessionsActions {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = self
            .panel
            .upgrade()
            .is_some_and(|panel| panel.read(cx).search_open);
        let has_agent = SessionsPanel::has_agent(cx);
        let panel = self.panel.clone();
        h_flex()
            .gap(px(ds::space::XXS))
            .child({
                let panel = panel.clone();
                // A filter, not a search: the sidebar's title line ends with the global one.
                ds::row_icon_button("search-conversations", IconName::Filter)
                    .icon_color(Color::Muted)
                    .toggle_state(searching)
                    .tooltip(Tooltip::text("Filter conversations"))
                    .on_click(move |_, window, cx| {
                        panel
                            .update(cx, |panel, cx| panel.toggle_search(window, cx))
                            .ok();
                    })
            })
            .when(has_agent, |row| {
                let panel = panel.clone();
                row.child(
                    PopoverMenu::new("conversations-more")
                        .trigger(
                            ds::row_icon_button("conversations-more-button", IconName::Ellipsis)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("More")),
                        )
                        .menu(move |window, cx| {
                            let panel = panel.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                menu.entry("Import from the agent", None, move |_, cx| {
                                    panel.update(cx, |panel, cx| panel.import(cx)).ok();
                                })
                            }))
                        })
                        .anchor(gpui::Anchor::TopRight),
                )
            })
            .child(
                ds::row_icon_button("new-conversation", IconName::Plus)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("New conversation"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(NewConversation), cx)
                    }),
            )
    }
}

/// Asks before a conversation is deleted: Studio forgets it, and the agent deletes it when it
/// can. Nothing on the graph changes. Neither can be put back, so it is confirmed by typing the
/// conversation's title (constitution IV).
struct DeleteConversation {
    record: SessionRecord,
    panel: WeakEntity<SessionsPanel>,
    /// Where the title is typed.
    confirm: Entity<Editor>,
    /// Delete was asked for with other words in the field.
    refused: bool,
    _subscription: Subscription,
}

/// Whether `typed` names the conversation to delete: its title as the list shows it, typed
/// whole.
fn title_typed(typed: &str, record: &SessionRecord) -> bool {
    typed.trim() == record.shown_title().trim()
}

impl DeleteConversation {
    fn new(
        record: SessionRecord,
        panel: WeakEntity<SessionsPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title = record.shown_title().trim().to_owned();
        let confirm = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&format!("Type {title} to delete it"), window, cx);
            editor.set_text_style_refinement(ds::field_text(cx));
            editor
        });
        let subscription = cx.subscribe(&confirm, |this: &mut Self, _, event: &EditorEvent, cx| {
            if let EditorEvent::BufferEdited = event {
                this.refused = false;
                cx.notify();
            }
        });
        Self {
            record,
            panel,
            confirm,
            refused: false,
            _subscription: subscription,
        }
    }

    fn confirmed(&self, cx: &App) -> bool {
        title_typed(&self.confirm.read(cx).text(cx), &self.record)
    }

    /// Deletes once the title is typed; otherwise the field says what to type.
    fn delete(&mut self, cx: &mut Context<Self>) {
        if !self.confirmed(cx) {
            self.refused = true;
            cx.notify();
            return;
        }
        let record = self.record.clone();
        self.panel
            .update(cx, |panel, cx| panel.delete(&record, cx))
            .ok();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for DeleteConversation {}

impl ModalView for DeleteConversation {}

impl Focusable for DeleteConversation {
    /// The field the title is typed in.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.confirm.focus_handle(cx)
    }
}

impl Render for DeleteConversation {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.record.shown_title().trim().to_owned();
        let confirmed = self.confirmed(cx);
        ds::modal(cx)
            .key_context("DeleteConversation")
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.delete(cx)))
            .child(
                ds::ModalHeader::new(format!("Delete “{title}”"))
                    .description(SharedString::from(format!(
                        "Studio forgets this conversation and its open tab closes; the agent \
                         deletes it too when it can. This cannot be undone. The graph and the \
                         agent's branch are not touched. Type {title} to confirm."
                    )))
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            // The body holds only the field the title is typed in.
            .child(ds::modal_body().child(
                ds::field_frame(self.refused, cx).child(ds::field_input(self.confirm.clone())),
            ))
            .child(
                ds::modal_footer(cx)
                    .child(
                        ds::cancel_button("cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        ds::action_button("delete", "Delete")
                            .variant(ds::Variant::Destructive)
                            .disabled(!confirmed)
                            .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
                    ),
            )
    }
}

/// The agent's name from its command: the package or program, without a version or path.
fn agent_name(command: &str) -> String {
    let program = command
        .split_whitespace()
        .rfind(|part| !part.starts_with('-'))
        .unwrap_or(command);
    // A Windows path parts its folders with `\` too.
    let base = program.rsplit(['/', '\\']).next().unwrap_or(program);
    base.split('@')
        .find(|part| !part.is_empty())
        .unwrap_or(base)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversation is deleted only with its title typed whole, as the list shows it: the
    /// person's name for it over the agent's, and "New conversation" before either.
    #[test]
    fn a_conversation_is_deleted_only_with_its_title_typed() {
        let at = Utc::now();
        let record = |title: Option<&str>, title_override: Option<&str>| SessionRecord {
            session: "s1".into(),
            graph: "server/crm".into(),
            agent: "claude".into(),
            title: title.map(str::to_owned),
            title_override: title_override.map(str::to_owned),
            created_at: at,
            updated_at: at,
            branch: "agent/s1".into(),
            branch_created: false,
            pinned: false,
            archived: false,
            cwd: std::path::PathBuf::from("/tmp/agent"),
        };
        let titled = record(Some("Count people"), None);
        assert!(title_typed("Count people", &titled));
        assert!(title_typed("  Count people ", &titled));
        for typed in ["", "count people", "Count", "Count people!", "\n"] {
            assert!(!title_typed(typed, &titled), "{typed:?}");
        }
        let renamed = record(Some("Count people"), Some("People count"));
        assert!(title_typed("People count", &renamed));
        assert!(!title_typed("Count people", &renamed));
        assert!(title_typed("New conversation", &record(None, None)));
    }

    #[test]
    fn agents_are_named_by_their_package() {
        assert_eq!(
            agent_name("npx -y @agentclientprotocol/claude-agent-acp@latest"),
            "claude-agent-acp"
        );
        assert_eq!(agent_name("/usr/local/bin/my-agent --acp"), "my-agent");
        assert_eq!(agent_name(r"C:\Tools\my-agent --acp"), "my-agent");
    }
}
