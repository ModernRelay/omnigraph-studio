//! The Conversations panel of the left sidebar (spec 011): the graph's conversations with
//! agents, as Delta's sidebar lists threads and Zed's lists its agent threads. Pinned ones
//! first, then by the person's last message; each row is the conversation's title with what it
//! is doing (running, waiting for the person, failed, finished unseen) or when it was last
//! active, and pin and archive on hover. A right click renames, pins, archives or deletes. The
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
use studio_actions::agent::{NewConversation, OpenConversation};
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

/// The note a list with no conversation row shows.
#[derive(Clone, Copy, PartialEq)]
enum Note {
    NoMatch,
    NoConversations,
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
        Some(session.read(cx).settings_key(cx))
    }

    /// The conversation in the shown tab; a new one has none until the agent names it.
    fn shown(&self, cx: &App) -> Option<String> {
        let workspace = self.workspace.upgrade()?;
        let key = workspace.read(cx).active_item()?.dedupe_key(cx)?;
        key.strip_prefix("agent:").map(str::to_owned)
    }

    /// The agent the settings name, as this panel runs it to import or delete conversations.
    fn agent(cx: &App) -> Option<AgentLaunch> {
        let settings = &StudioSettings::get(cx).agent;
        (!settings.command.is_empty()).then(|| AgentLaunch {
            command: settings.command.clone(),
            env: settings.env.clone(),
        })
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
        self.sessions
            .update(cx, |sessions, cx| sessions.rename(&id, &title, cx));
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
            workspace.toggle_modal(window, cx, move |_, cx| {
                DeleteConversation::new(record, panel, cx)
            });
        });
    }

    /// Forgets the conversation here and asks the agent to delete it, when it can.
    fn delete(&mut self, record: &SessionRecord, cx: &mut Context<Self>) {
        let id = record.session.clone();
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
            let (kind, note) = match answer.await {
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
        self.search_open = !self.search_open;
        if self.search_open {
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
            let result = answer.await;
            drop(connection);
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

/// A conversation the agent knows and the list does not, as a record: the agent's title and
/// time, a branch named for when it was last active.
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
        title: info.title.filter(|title| !title.trim().is_empty()),
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
                    this.search_open = true;
                    this.toggle_search(window, cx);
                }))
                .into_any_element(),
            // Its words are made as it draws: the shortcut they name follows the keymap.
            ListRow::Note(Note::NoMatch) => {
                ds::sidebar_note("No conversation matches.").into_any_element()
            }
            ListRow::Note(Note::NoConversations) => ds::sidebar_note(
                match studio_ui::shortcut_text(&studio_actions::agent::OpenAgent, cx) {
                    Some(keys) => {
                        format!("No conversations yet. Ask the agent from the Agent tab ({keys}).")
                    }
                    None => "No conversations yet. Ask the agent from the Agent tab.".to_owned(),
                },
            )
            .into_any_element(),
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
                let shown = self.shown(cx).as_deref() == Some(record.session.as_str());
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
        .size_full()
        .into_any_element()
    }
}

/// The Conversations panel's header buttons: search, More (import), and a new conversation.
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
        let has_agent = SessionsPanel::agent(cx).is_some();
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
/// can. Nothing on the graph changes.
struct DeleteConversation {
    record: SessionRecord,
    panel: WeakEntity<SessionsPanel>,
    focus_handle: FocusHandle,
}

impl DeleteConversation {
    fn new(
        record: SessionRecord,
        panel: WeakEntity<SessionsPanel>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            record,
            panel,
            focus_handle: cx.focus_handle(),
        }
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
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
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DeleteConversation {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        ds::modal(cx)
            .key_context("DeleteConversation")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.delete(cx)))
            .child(
                ds::ModalHeader::new(format!("Delete “{}”", self.record.shown_title()))
                    .description(SharedString::from(
                        "Studio forgets this conversation and its open tab closes; the agent \
                         deletes it too when it can. This cannot be undone. The graph and the \
                         agent's branch are not touched.",
                    ))
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                ds::modal_footer(cx)
                    .child(
                        ds::cancel_button("cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        ds::action_button("delete", "Delete")
                            .variant(ds::Variant::Destructive)
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
