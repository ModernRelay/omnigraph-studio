//! The agent panel's view and state: the thread, approval cards, the composer, and the tools
//! run on the person's session.

use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOption, PermissionOptionKind, PlanEntryStatus,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionValue, SessionConfigSelectOptions, SessionUpdate, StopReason,
    ToolCallStatus,
};
use editor::Editor;
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    ScrollHandle, SharedString, Task, Window,
};
use gq_language::QueryInfo;
use graph_client::WriteOutcome;
use graph_client::api::{ChangeOutput, ChangeRequest, QueryRequest};
use graph_store::GraphSession;
use serde_json::{Map, Value};
use studio_actions::agent::{Allow, AllowAlways, Reject, Restart, Send, Stop};
use studio_actions::graph::ReviewBranch;
use studio_settings::StudioSettings;
use studio_ui::ds;
use studio_workspace::{Item, ItemEvent, ItemFooter, SavedItem, Workspace};
use ui::prelude::*;
use ui::{
    Callout, CircularProgress, CommonAnimationExt as _, ContextMenu, Indicator, KeyBinding,
    PopoverMenu, Severity, Switch, SwitchLabelPosition, Tooltip,
};

use crate::host::{self, HostCommand, HostEvent, Launch};
use crate::socket::{ToolRequest, ToolSocket};
use crate::tools::{self, ToolCall};

/// Presets offered when no agent is set: the ACP crate's own preset commands.
pub const PRESETS: &[(&str, &[&str])] = &[
    (
        "Claude Agent",
        &["npx", "-y", "@agentclientprotocol/claude-agent-acp@latest"],
    ),
    (
        "Codex",
        &["npx", "-y", "@agentclientprotocol/codex-acp@latest"],
    ),
];

const UNKNOWN_OUTCOME: &str = "The server did not confirm this change. It was not retried; \
check the branch history before proposing it again.";

/// Opens the panel for the workspace's graph, or activates it.
pub fn open(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<AgentPanel>> {
    let session = workspace.session()?.clone();
    let panel = workspace.open_item("agent", window, cx, move |window, cx| {
        AgentPanel::new(session, window, cx)
    })?;
    panel.update(cx, |panel, cx| panel.resume_if_saved(cx));
    Some(panel)
}

enum State {
    /// No agent chosen yet.
    Setup,
    /// An agent is chosen but not running.
    Idle,
    Starting,
    Ready {
        agent: String,
    },
    Prompting {
        agent: String,
    },
    Exited {
        message: String,
        stderr: String,
    },
}

/// The agent's branch for this agent session.
struct AgentBranch {
    name: String,
    base: String,
    created: bool,
    changes: usize,
}

enum Decision {
    Allow,
    AllowAlways,
    Reject,
}

enum ProposalState {
    Waiting(oneshot::Sender<Decision>),
    Running,
    /// The commit id, or `None` when the mutation matched nothing, and what changed.
    Committed {
        commit: Option<String>,
        nodes: i64,
        edges: i64,
    },
    Refused(String),
    Unknown,
    Rejected,
}

struct Proposal {
    summary: String,
    query: String,
    params: Option<Map<String, Value>>,
    branch: String,
    creates_from: Option<String>,
    state: ProposalState,
}

struct PermissionCard {
    title: String,
    options: Vec<PermissionOption>,
    reply: Option<oneshot::Sender<RequestPermissionOutcome>>,
    answer: Option<String>,
}

enum Entry {
    User(String),
    Agent(String),
    Thought(String),
    Tool {
        id: String,
        title: String,
        status: ToolCallStatus,
    },
    Plan(Vec<(String, PlanEntryStatus)>),
    Permission(PermissionCard),
    Proposal(Proposal),
    Notice {
        text: String,
        error: bool,
    },
}

/// How full the agent's context window is, and what the session has cost so far, as the agent
/// last reported them.
struct Usage {
    used: u64,
    size: u64,
    cost: Option<(f64, String)>,
}

struct Host {
    commands: mpsc::UnboundedSender<HostCommand>,
    _socket: ToolSocket,
    _tasks: Vec<Task<()>>,
}

pub struct AgentPanel {
    session: Entity<GraphSession>,
    /// The agent's replies as rendered markdown, by entry index, with how much of the text each
    /// has taken in; a streaming reply appends its new tail.
    markdown: std::collections::HashMap<usize, (Entity<Markdown>, usize)>,
    focus_handle: FocusHandle,
    composer: Entity<Editor>,
    custom_command: Entity<Editor>,
    scroll: ScrollHandle,
    state: State,
    entries: Vec<Entry>,
    branch: Option<AgentBranch>,
    always_allow: bool,
    host: Option<Host>,
    /// A prompt sent before the agent was ready, sent once it is.
    queued: Option<String>,
    /// The open session, remembered to come back to.
    session_id: Option<String>,
    /// The next start begins a new conversation instead of resuming the last.
    start_fresh: bool,
    /// The options the agent offers for this session (its model, mode, reasoning level), in its
    /// order and as it last confirmed them.
    config_options: Vec<SessionConfigOption>,
    /// A value chosen and sent but not yet confirmed, shown in the meantime.
    setting_option: Option<(String, SessionConfigOptionValue)>,
    usage: Option<Usage>,
}

impl EventEmitter<ItemEvent> for AgentPanel {}

impl Focusable for AgentPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for AgentPanel {
    fn tab_title(&self, _cx: &App) -> SharedString {
        "Agent".into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::ZedAssistant)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some("agent".into())
    }

    /// The tab comes back after a restart; the agent starts again only when a message is sent.
    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "agent".into(),
            data: Value::Null,
        })
    }

    /// Delta's composer footer: the agent at work, how full its context is, its options (model,
    /// mode), the agent, then send or stop.
    fn footer(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<ItemFooter> {
        let running = matches!(self.state, State::Prompting { .. });
        let can_send = matches!(
            self.state,
            State::Ready { .. } | State::Idle | State::Exited { .. } | State::Starting
        );
        let send_or_stop = if running {
            ds::round_button("stop", IconName::Stop, false, cx)
                .tooltip(|_, cx| Tooltip::for_action("Stop", &Stop, cx))
                .on_click(cx.listener(|this, _, window, cx| this.stop(&Stop, window, cx)))
        } else {
            ds::round_button("send", IconName::ArrowUp, can_send, cx)
                .when(!can_send, |button| button.opacity(0.4))
                .tooltip(|_, cx| Tooltip::for_action("Send", &Send, cx))
                .on_click(cx.listener(|this, _, window, cx| this.send(&Send, window, cx)))
        };
        let (agent_name, dot, state) = self.status();
        let command = Self::command(cx).join(" ");
        let about = if command.is_empty() {
            format!("{state}. Choose the agent in Settings.")
        } else {
            format!("{state} · {command}. Choose the agent in Settings.")
        };
        let agent_pill = ds::pill("agent-pill", cx)
            .tooltip(Tooltip::text(about))
            .child(Indicator::dot().color(dot))
            .child(ds::text::body(agent_name))
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(studio_actions::workspace::OpenSettings), cx)
            });
        let mut end = Vec::new();
        if matches!(self.state, State::Ready { .. } | State::Exited { .. }) {
            end.push(
                studio_ui::ds::icon_button("restart", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(|_, cx| Tooltip::for_action("New agent session", &Restart, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.restart(&Restart, window, cx)))
                    .into_any_element(),
            );
        }
        if running {
            end.push(
                h_flex()
                    .gap(px(ds::space::XS))
                    .items_center()
                    .child(
                        Icon::new(IconName::ArrowCircle)
                            .size(IconSize::XSmall)
                            .color(Color::Muted)
                            .with_rotate_animation(2),
                    )
                    .child(ds::text::meta("Working"))
                    .into_any_element(),
            );
        }
        end.extend(self.render_usage(cx));
        end.extend(self.render_options(cx));
        end.push(
            h_flex()
                .gap(px(ds::space::SM))
                .items_center()
                .child(agent_pill)
                .child(send_or_stop)
                .into_any_element(),
        );
        Some(ItemFooter {
            start: Vec::new(),
            end,
        })
    }
}

impl AgentPanel {
    pub fn new(session: Entity<GraphSession>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 10, window, cx);
            editor.set_placeholder_text("Ask about the graph, or ask for a change…", window, cx);
            editor
        });
        let custom_command = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("program and arguments, e.g. my-agent --acp", window, cx);
            editor
        });
        cx.observe_global::<StudioSettings>(|this, cx| {
            this.sync_setup_state(cx);
            cx.notify();
        })
        .detach();
        let mut panel = Self {
            session,
            focus_handle: cx.focus_handle(),
            composer,
            custom_command,
            scroll: ScrollHandle::new(),
            state: State::Setup,
            entries: Vec::new(),
            markdown: std::collections::HashMap::new(),
            branch: None,
            always_allow: false,
            host: None,
            queued: None,
            session_id: None,
            start_fresh: false,
            config_options: Vec::new(),
            setting_option: None,
            usage: None,
        };
        panel.sync_setup_state(cx);
        panel
    }

    fn command(cx: &App) -> Vec<String> {
        StudioSettings::get(cx).agent.command.clone()
    }

    fn sync_setup_state(&mut self, cx: &App) {
        let has_command = !Self::command(cx).is_empty();
        match self.state {
            State::Setup if has_command => self.state = State::Idle,
            State::Idle if !has_command => self.state = State::Setup,
            _ => {}
        }
    }

    // ---- lifecycle ----

    /// Starts the agent at once when there is a conversation to come back to on this graph, so
    /// it is on screen without sending anything first.
    pub fn resume_if_saved(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.state, State::Idle) {
            return;
        }
        let command = Self::command(cx);
        let graph = self.session.read(cx).settings_key(cx);
        if crate::sessions::load(&graph, &command).is_some() {
            self.start(cx);
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let settings = StudioSettings::get(cx).agent.clone();
        if settings.command.is_empty() {
            self.state = State::Setup;
            return;
        }
        // The earlier conversation on this graph with this agent, unless a new one was asked
        // for (spec 011: resuming a session).
        let graph = self.session.read(cx).settings_key(cx);
        let saved = if self.start_fresh {
            self.start_fresh = false;
            crate::sessions::forget(&graph, &settings.command);
            None
        } else {
            crate::sessions::load(&graph, &settings.command)
        };
        let data = studio_settings::data_dir().join("agent");
        let cwd = data.join("workspace");
        if let Err(error) = std::fs::create_dir_all(&cwd) {
            self.fail_start(
                format!("Studio could not create {}: {error}", cwd.display()),
                cx,
            );
            return;
        }
        let mut socket = match ToolSocket::bind(&crate::socket::socket_dir()) {
            Ok(socket) => socket,
            Err(error) => {
                self.fail_start(
                    format!("Studio could not open the tool socket: {error}"),
                    cx,
                );
                return;
            }
        };
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                self.fail_start(
                    format!("Studio could not find its own executable: {error}"),
                    cx,
                );
                return;
            }
        };
        let base = self.session.read(cx).branch.clone();
        // A resumed conversation keeps writing to its branch.
        self.branch = Some(match &saved {
            Some(saved) => AgentBranch {
                name: saved.branch.clone(),
                base,
                created: saved.branch_created,
                changes: 0,
            },
            None => AgentBranch {
                name: format!("agent/{}", chrono::Local::now().format("%Y%m%d-%H%M%S")),
                base,
                created: false,
                changes: 0,
            },
        });
        self.always_allow = false;

        let launch = Launch {
            command: settings.command,
            env: settings.env,
            cwd,
            mcp_command: proxy_executable(executable),
            mcp_args: vec![
                crate::PROXY_COMMAND.to_owned(),
                socket.path.to_string_lossy().into_owned(),
            ],
            resume: saved.map(|saved| saved.session),
        };
        let (events_tx, mut events_rx) = mpsc::unbounded();
        let (commands_tx, commands_rx) = mpsc::unbounded();
        let (tools_tx, mut tools_rx) = mpsc::unbounded::<ToolRequest>();

        let serve = cx.background_spawn(socket.serve(tools_tx));
        let connection = cx.background_spawn(host::run(launch, events_tx, commands_rx));
        let event_pump = cx.spawn(async move |this, cx| {
            while let Some(event) = events_rx.next().await {
                if this
                    .update(cx, |this, cx| this.apply_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let tool_pump = cx.spawn(async move |this, cx| {
            while let Some(request) = tools_rx.next().await {
                if this
                    .update(cx, |this, cx| this.run_tool(request, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.host = Some(Host {
            commands: commands_tx,
            _socket: socket,
            _tasks: vec![serve, connection, event_pump, tool_pump],
        });
        self.state = State::Starting;
        cx.notify();
    }

    fn fail_start(&mut self, message: String, cx: &mut Context<Self>) {
        self.state = State::Exited {
            message,
            stderr: String::new(),
        };
        cx.notify();
    }

    fn stop_host(&mut self) {
        self.host = None;
        // Options can only be set on a running session.
        self.config_options.clear();
        self.setting_option = None;
        for entry in &mut self.entries {
            match entry {
                Entry::Permission(card) if card.reply.is_some() => {
                    card.reply = None;
                    card.answer = Some("Cancelled".into());
                }
                Entry::Proposal(proposal)
                    if matches!(proposal.state, ProposalState::Waiting(_)) =>
                {
                    proposal.state = ProposalState::Rejected;
                }
                _ => {}
            }
        }
    }

    /// A new conversation: the earlier one is not resumed again.
    fn restart(&mut self, _: &Restart, _window: &mut Window, cx: &mut Context<Self>) {
        self.stop_host();
        self.entries.clear();
        self.markdown.clear();
        self.usage = None;
        self.start_fresh = true;
        self.start(cx);
    }

    /// Records the open session, to come back to it at the next start.
    fn remember_session(&self, cx: &App) {
        let (Some(session), Some(branch)) = (&self.session_id, &self.branch) else {
            return;
        };
        let command = StudioSettings::get(cx).agent.command.clone();
        crate::sessions::save(
            &self.session.read(cx).settings_key(cx),
            &command,
            &crate::sessions::SavedSession {
                session: session.clone(),
                branch: branch.name.clone(),
                branch_created: branch.created,
            },
        );
    }

    fn apply_event(&mut self, event: HostEvent, cx: &mut Context<Self>) {
        match event {
            HostEvent::Ready {
                agent,
                session,
                resumed,
            } => {
                self.session_id = Some(session);
                self.remember_session(cx);
                if resumed {
                    self.entries.push(Entry::Notice {
                        text: "Resumed the last conversation; ↻ starts a new one.".into(),
                        error: false,
                    });
                } else {
                    // A resumed session keeps its own options; a new one starts from the ones
                    // chosen last for this agent.
                    self.apply_preferences(cx);
                }
                self.state = State::Ready { agent };
                if let Some(text) = self.queued.take() {
                    self.prompt(text, cx);
                }
            }
            HostEvent::Update(update) => self.apply_update(update),
            HostEvent::ConfigOptions(options) => {
                self.config_options = options;
                self.setting_option = None;
            }
            HostEvent::ConfigOptionFailed { message } => {
                self.setting_option = None;
                self.entries.push(Entry::Notice {
                    text: format!("The agent did not change the option: {message}"),
                    error: true,
                });
            }
            HostEvent::Permission { request, reply } => {
                self.entries.push(Entry::Permission(PermissionCard {
                    title: request
                        .tool_call
                        .fields
                        .title
                        .clone()
                        .unwrap_or_else(|| "The agent asks to use a tool".into()),
                    options: request.options,
                    reply: Some(reply),
                    answer: None,
                }));
            }
            HostEvent::TurnEnded { stop } => {
                if let State::Prompting { agent } = &self.state {
                    self.state = State::Ready {
                        agent: agent.clone(),
                    };
                }
                match stop {
                    Ok(StopReason::EndTurn) => {}
                    Ok(StopReason::Cancelled) => self.entries.push(Entry::Notice {
                        text: "Stopped.".into(),
                        error: false,
                    }),
                    Ok(other) => self.entries.push(Entry::Notice {
                        text: format!("The agent ended its turn: {other:?}."),
                        error: false,
                    }),
                    Err(error) => self.entries.push(Entry::Notice {
                        text: format!("The turn failed: {error}"),
                        error: true,
                    }),
                }
            }
            HostEvent::Exited { message, stderr } => {
                self.stop_host();
                self.state = State::Exited { message, stderr };
            }
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn apply_update(&mut self, update: SessionUpdate) {
        match update {
            // A resumed session replays the person's own messages too.
            SessionUpdate::UserMessageChunk(chunk) => {
                let text = content_text(&chunk.content);
                match self.entries.last_mut() {
                    Some(Entry::User(existing)) => existing.push_str(&text),
                    _ => self.entries.push(Entry::User(text)),
                }
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                let text = content_text(&chunk.content);
                match self.entries.last_mut() {
                    Some(Entry::Agent(existing)) => existing.push_str(&text),
                    _ => self.entries.push(Entry::Agent(text)),
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = content_text(&chunk.content);
                match self.entries.last_mut() {
                    Some(Entry::Thought(existing)) => existing.push_str(&text),
                    _ => self.entries.push(Entry::Thought(text)),
                }
            }
            SessionUpdate::ToolCall(call) => {
                self.entries.push(Entry::Tool {
                    id: call.tool_call_id.to_string(),
                    title: call.title,
                    status: call.status,
                });
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.to_string();
                for entry in self.entries.iter_mut().rev() {
                    if let Entry::Tool {
                        id: existing,
                        title,
                        status,
                    } = entry
                        && *existing == id
                    {
                        if let Some(new_title) = update.fields.title.clone() {
                            *title = new_title;
                        }
                        if let Some(new_status) = update.fields.status {
                            *status = new_status;
                        }
                        break;
                    }
                }
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                self.config_options = update.config_options;
            }
            SessionUpdate::UsageUpdate(update) => {
                self.usage = Some(Usage {
                    used: update.used,
                    size: update.size,
                    cost: update.cost.map(|cost| (cost.amount, cost.currency)),
                });
            }
            SessionUpdate::Plan(plan) => {
                let entries = plan
                    .entries
                    .into_iter()
                    .map(|entry| (entry.content, entry.status))
                    .collect();
                match self.entries.last_mut() {
                    Some(Entry::Plan(existing)) => *existing = entries,
                    _ => self.entries.push(Entry::Plan(entries)),
                }
            }
            _ => {}
        }
    }

    // ---- session options ----

    /// Sets one of the session's options and remembers the choice for this agent's next new
    /// session.
    fn set_option(&mut self, id: String, value: SessionConfigOptionValue, cx: &mut Context<Self>) {
        let Some(host) = &self.host else {
            return;
        };
        let remembered = match &value {
            SessionConfigOptionValue::Boolean { value } => Value::Bool(*value),
            other => match other.as_value_id() {
                Some(id) => Value::String(id.0.to_string()),
                None => return,
            },
        };
        crate::sessions::prefer(&Self::command(cx), &id, remembered);
        if host
            .commands
            .unbounded_send(HostCommand::SetConfigOption {
                id: id.clone(),
                value: value.clone(),
            })
            .is_ok()
        {
            self.setting_option = Some((id, value));
        }
        cx.notify();
    }

    /// Sets the options chosen last for this agent where the new session differs and still
    /// offers the value.
    fn apply_preferences(&mut self, cx: &mut Context<Self>) {
        let preferences = crate::sessions::preferences(&Self::command(cx));
        let Some(host) = &self.host else {
            return;
        };
        for option in &self.config_options {
            let Some(preferred) = preferences.get(option.id.0.as_ref()) else {
                continue;
            };
            let value = match (&option.kind, preferred) {
                (SessionConfigKind::Select(select), Value::String(preferred))
                    if select.current_value.0.as_ref() != preferred
                        && select_values(&select.options).any(|(value, _)| value == preferred) =>
                {
                    SessionConfigOptionValue::value_id(preferred.clone())
                }
                (SessionConfigKind::Boolean(boolean), Value::Bool(preferred))
                    if boolean.current_value != *preferred =>
                {
                    SessionConfigOptionValue::boolean(*preferred)
                }
                _ => continue,
            };
            host.commands
                .unbounded_send(HostCommand::SetConfigOption {
                    id: option.id.0.to_string(),
                    value,
                })
                .ok();
        }
    }

    // ---- prompting ----

    pub fn send_text(&mut self, text: String, _window: &mut Window, cx: &mut Context<Self>) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        match &self.state {
            State::Ready { .. } => self.prompt(text, cx),
            State::Prompting { .. } => {
                let notice = "The agent is still working. Stop it first, or wait.";
                // One note, however often Enter is pressed.
                if !matches!(self.entries.last(), Some(Entry::Notice { text, .. }) if text == notice)
                {
                    self.entries.push(Entry::Notice {
                        text: notice.into(),
                        error: false,
                    });
                }
            }
            State::Starting => self.queued = Some(text),
            State::Setup => {
                self.queued = Some(text);
                self.entries.push(Entry::Notice {
                    text: "Choose an agent to send this to.".into(),
                    error: false,
                });
            }
            State::Idle | State::Exited { .. } => {
                self.queued = Some(text);
                self.start(cx);
            }
        }
        cx.notify();
    }

    fn prompt(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(host) = &self.host else { return };
        let agent = match &self.state {
            State::Ready { agent } => agent.clone(),
            _ => return,
        };
        if host
            .commands
            .unbounded_send(HostCommand::Prompt(text.clone()))
            .is_ok()
        {
            self.entries.push(Entry::User(text));
            self.state = State::Prompting { agent };
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    fn send(&mut self, _: &Send, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        // While the agent works the message stays in the composer, to send once it is done.
        if matches!(self.state, State::Prompting { .. }) {
            self.send_text(text, window, cx);
            return;
        }
        self.composer
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.send_text(text, window, cx);
    }

    fn stop(&mut self, _: &Stop, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(host) = &self.host {
            host.commands.unbounded_send(HostCommand::Cancel).ok();
        }
    }

    // ---- approvals ----

    /// The oldest card still waiting for the person.
    fn pending_index(&self) -> Option<usize> {
        self.entries.iter().position(|entry| match entry {
            Entry::Permission(card) => card.reply.is_some(),
            Entry::Proposal(proposal) => matches!(proposal.state, ProposalState::Waiting(_)),
            _ => false,
        })
    }

    fn decide(&mut self, index: usize, decision: Decision, cx: &mut Context<Self>) {
        match self.entries.get_mut(index) {
            Some(Entry::Proposal(proposal)) => {
                if let ProposalState::Waiting(_) = proposal.state {
                    let ProposalState::Waiting(reply) =
                        std::mem::replace(&mut proposal.state, ProposalState::Running)
                    else {
                        return;
                    };
                    reply.send(decision).ok();
                }
            }
            Some(Entry::Permission(card)) => {
                let Some(reply) = card.reply.take() else {
                    return;
                };
                let wanted: &[PermissionOptionKind] = match decision {
                    Decision::Allow => &[PermissionOptionKind::AllowOnce],
                    Decision::AllowAlways => &[
                        PermissionOptionKind::AllowAlways,
                        PermissionOptionKind::AllowOnce,
                    ],
                    Decision::Reject => &[
                        PermissionOptionKind::RejectOnce,
                        PermissionOptionKind::RejectAlways,
                    ],
                };
                let option = wanted
                    .iter()
                    .find_map(|kind| card.options.iter().find(|o| o.kind == *kind));
                match option {
                    Some(option) => {
                        card.answer = Some(option.name.clone());
                        reply
                            .send(RequestPermissionOutcome::Selected(
                                SelectedPermissionOutcome::new(option.option_id.clone()),
                            ))
                            .ok();
                    }
                    None => {
                        card.answer = Some("Cancelled".into());
                        reply.send(RequestPermissionOutcome::Cancelled).ok();
                    }
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn choose_option(&mut self, index: usize, option_index: usize, cx: &mut Context<Self>) {
        if let Some(Entry::Permission(card)) = self.entries.get_mut(index)
            && let (Some(reply), Some(option)) = (card.reply.take(), card.options.get(option_index))
        {
            card.answer = Some(option.name.clone());
            reply
                .send(RequestPermissionOutcome::Selected(
                    SelectedPermissionOutcome::new(option.option_id.clone()),
                ))
                .ok();
        }
        cx.notify();
    }

    fn allow(&mut self, _: &Allow, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.pending_index() {
            self.decide(index, Decision::Allow, cx);
        }
    }

    fn allow_always(&mut self, _: &AllowAlways, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.pending_index() {
            self.decide(index, Decision::AllowAlways, cx);
        }
    }

    fn reject(&mut self, _: &Reject, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.pending_index() {
            self.decide(index, Decision::Reject, cx);
        }
    }

    // ---- tools ----

    fn run_tool(&mut self, request: ToolRequest, cx: &mut Context<Self>) {
        let ToolRequest {
            name,
            arguments,
            reply,
        } = request;
        let call = match ToolCall::parse(&name, &arguments) {
            Ok(call) => call,
            Err(error) => {
                reply.send((error, true)).ok();
                return;
            }
        };
        let session = self.session.read(cx);
        let Some(schema) = session.schema_model() else {
            reply
                .send((
                    "The schema is still loading; try again in a moment.".into(),
                    true,
                ))
                .ok();
            return;
        };
        let person_branch = session.branch.clone();
        let agent_branch = self
            .branch
            .as_ref()
            .filter(|branch| branch.created)
            .map(|branch| branch.name.clone());

        match call {
            ToolCall::Schema => {
                reply.send((tools::describe_schema(&schema), false)).ok();
            }
            ToolCall::Branches => {
                let Some(branches) = session.branches.loaded().cloned() else {
                    reply
                        .send(("The branch list is still loading.".into(), true))
                        .ok();
                    return;
                };
                let mut text = String::new();
                for branch in branches {
                    let mark = if branch == person_branch {
                        " (the person's branch)"
                    } else if agent_branch.as_deref() == Some(branch.as_str()) {
                        " (your branch)"
                    } else {
                        ""
                    };
                    text.push_str(&format!("{branch}{mark}\n"));
                }
                if agent_branch.is_none()
                    && let Some(branch) = &self.branch
                {
                    text.push_str(&format!(
                        "\nYour branch will be {} once you propose a change.\n",
                        branch.name
                    ));
                }
                reply.send((text, false)).ok();
            }
            ToolCall::Query {
                query,
                name,
                params,
                branch,
            } => {
                let check = gq_language::check(&schema, &query);
                let info: QueryInfo = match tools::select_query(&check, name.as_deref(), false) {
                    Ok(info) => info,
                    Err(error) => {
                        reply.send((error, true)).ok();
                        return;
                    }
                };
                let branch = branch.or(agent_branch).unwrap_or(person_branch);
                let request = QueryRequest {
                    query,
                    name: Some(info.name),
                    params: params.map(Value::Object),
                    branch: Some(branch.clone()),
                    snapshot: None,
                };
                let read = self
                    .session
                    .update(cx, |session, cx| session.read_request(request, cx));
                cx.spawn(async move |_, _| {
                    let result = match read.await {
                        Ok(output) => (
                            tools::describe_rows(
                                &branch,
                                &output.columns,
                                &output.rows,
                                output.graph_commit_id.as_deref(),
                            ),
                            false,
                        ),
                        Err(error) => (format!("The read failed: {error}"), true),
                    };
                    reply.send(result).ok();
                })
                .detach();
            }
            ToolCall::Propose {
                query,
                name,
                params,
                summary,
            } => {
                let check = gq_language::check(&schema, &query);
                let info = match tools::select_query(&check, name.as_deref(), true) {
                    Ok(info) => info,
                    Err(error) => {
                        reply.send((error, true)).ok();
                        return;
                    }
                };
                self.propose(query, info.name, params, summary, reply, cx);
            }
        }
    }

    fn propose(
        &mut self,
        query: String,
        query_name: String,
        params: Option<Map<String, Value>>,
        summary: String,
        reply: oneshot::Sender<(String, bool)>,
        cx: &mut Context<Self>,
    ) {
        let Some(branch) = &self.branch else {
            reply
                .send(("The agent session has no branch.".into(), true))
                .ok();
            return;
        };
        let (decide_tx, decide_rx) = oneshot::channel();
        let index = self.entries.len();
        self.entries.push(Entry::Proposal(Proposal {
            summary,
            query: query.clone(),
            params: params.clone(),
            branch: branch.name.clone(),
            creates_from: (!branch.created).then(|| branch.base.clone()),
            state: ProposalState::Waiting(decide_tx),
        }));
        if self.always_allow {
            self.decide(index, Decision::Allow, cx);
        }
        self.scroll.scroll_to_bottom();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let decision = decide_rx.await.unwrap_or(Decision::Reject);
            if let Decision::Reject = decision {
                this.update(cx, |this, cx| {
                    this.set_proposal_state(index, ProposalState::Rejected, cx)
                })
                .ok();
                reply
                    .send(("The person rejected this change.".into(), true))
                    .ok();
                return;
            }

            // Create the agent's branch on its first approved change.
            let create = this
                .update(cx, |this, cx| {
                    if let Decision::AllowAlways = decision {
                        this.always_allow = true;
                    }
                    let branch = this.branch.as_ref()?;
                    if branch.created {
                        return None;
                    }
                    let (name, base) = (branch.name.clone(), branch.base.clone());
                    Some(this.session.update(cx, |session, cx| {
                        session.create_branch(name, Some(base), cx)
                    }))
                })
                .ok()
                .flatten();
            if let Some(create) = create {
                match create.await {
                    WriteOutcome::Committed(_) => {
                        this.update(cx, |this, cx| {
                            if let Some(branch) = &mut this.branch {
                                branch.created = true;
                            }
                            this.remember_session(cx);
                        })
                        .ok();
                    }
                    WriteOutcome::Refused(error) => {
                        let message = format!(
                            "Studio could not create the agent's branch: {}",
                            error.body.error
                        );
                        this.update(cx, |this, cx| {
                            this.set_proposal_state(
                                index,
                                ProposalState::Refused(message.clone()),
                                cx,
                            )
                        })
                        .ok();
                        reply.send((message, true)).ok();
                        return;
                    }
                    WriteOutcome::Unknown { reason } => {
                        let message = format!(
                            "Studio could not confirm the agent's branch was created ({reason}). \
                             Nothing else was sent."
                        );
                        this.update(cx, |this, cx| {
                            this.set_proposal_state(index, ProposalState::Unknown, cx)
                        })
                        .ok();
                        reply.send((message, true)).ok();
                        return;
                    }
                }
            }

            let write = this
                .update(cx, |this, cx| {
                    let branch = this.branch.as_ref()?.name.clone();
                    let request = ChangeRequest {
                        query,
                        name: Some(query_name),
                        params: params.map(Value::Object),
                        branch: Some(branch.clone()),
                    };
                    Some(this.session.update(cx, |session, cx| {
                        session.write_request(request, None, "Agent change".to_owned(), cx)
                    }))
                })
                .ok()
                .flatten();
            let Some(write) = write else {
                reply.send(("The agent session ended.".into(), true)).ok();
                return;
            };
            let outcome = write.await;
            let (state, text, is_error) = describe_outcome(&outcome);
            let committed =
                matches!(&outcome, WriteOutcome::Committed(output) if output.commit.is_some());
            this.update(cx, |this, cx| {
                if committed && let Some(branch) = &mut this.branch {
                    branch.changes += 1;
                }
                this.set_proposal_state(index, state, cx);
            })
            .ok();
            reply.send((text, is_error)).ok();
        })
        .detach();
    }

    fn set_proposal_state(&mut self, index: usize, state: ProposalState, cx: &mut Context<Self>) {
        if let Some(Entry::Proposal(proposal)) = self.entries.get_mut(index) {
            proposal.state = state;
        }
        cx.notify();
    }

    fn review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.branch.as_ref().filter(|b| b.created) else {
            return;
        };
        let name = branch.name.clone();
        self.session
            .update(cx, |session, cx| session.set_branch(name, cx));
        window.dispatch_action(Box::new(ReviewBranch), cx);
    }

    fn use_command(&mut self, command: Vec<String>, cx: &mut Context<Self>) {
        studio_settings::update(cx, move |settings| settings.agent.command = command);
        self.sync_setup_state(cx);
        cx.notify();
    }
}

/// The proxy runs Studio's own executable. Test binaries and examples are not Studio, so they
/// name the built `omnigraph-studio` next to them when it exists.
fn proxy_executable(current: PathBuf) -> PathBuf {
    if let Ok(path) = std::env::var("OMNIGRAPH_STUDIO_PROXY")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    current
}

fn describe_outcome(outcome: &WriteOutcome<ChangeOutput>) -> (ProposalState, String, bool) {
    match outcome {
        WriteOutcome::Committed(output) => {
            let state = ProposalState::Committed {
                commit: output.commit.as_ref().map(|c| c.graph_commit_id.clone()),
                nodes: output.affected_nodes,
                edges: output.affected_edges,
            };
            let text = match &output.commit {
                Some(commit) => format!(
                    "Committed on {}: commit {}, {} nodes and {} edges changed.",
                    output.branch,
                    commit.graph_commit_id,
                    output.affected_nodes,
                    output.affected_edges
                ),
                None => "Nothing changed: the mutation matched no rows.".to_owned(),
            };
            (state, text, false)
        }
        WriteOutcome::Refused(error) => {
            let text = format!(
                "Refused by the server ({}): {}",
                error.status, error.body.error
            );
            (ProposalState::Refused(text.clone()), text, true)
        }
        WriteOutcome::Unknown { .. } => (ProposalState::Unknown, UNKNOWN_OUTCOME.to_owned(), true),
    }
}

/// The last eight characters of a graph commit id, as the activity log shows it.
/// A select option's values and names, across its groups.
fn select_values(
    options: &SessionConfigSelectOptions,
) -> Box<dyn Iterator<Item = (&str, &str)> + '_> {
    match options {
        SessionConfigSelectOptions::Ungrouped(options) => Box::new(
            options
                .iter()
                .map(|option| (option.value.0.as_ref(), option.name.as_str())),
        ),
        SessionConfigSelectOptions::Grouped(groups) => Box::new(groups.iter().flat_map(|group| {
            group
                .options
                .iter()
                .map(|option| (option.value.0.as_ref(), option.name.as_str()))
        })),
        _ => Box::new(std::iter::empty()),
    }
}

/// The longest value name a footer picker shows before it is cut.
const OPTION_NAME_CHARS: usize = 32;

fn cut(name: &str, chars: usize) -> String {
    if name.chars().count() <= chars {
        name.to_owned()
    } else {
        let kept: String = name.chars().take(chars.saturating_sub(1)).collect();
        format!("{}…", kept.trim_end())
    }
}

/// Token counts as people read them: 999, 1.2k, 84k, 1.2M.
fn humanize_tokens(count: u64) -> String {
    match count {
        0..=999 => count.to_string(),
        1_000..=9_999 => trim_point(format!("{:.1}", count as f64 / 1_000.)) + "k",
        10_000..=999_999 => format!("{}k", (count as f64 / 1_000.).round() as u64),
        _ => trim_point(format!("{:.1}", count as f64 / 1_000_000.)) + "M",
    }
}

fn trim_point(number: String) -> String {
    number
        .strip_suffix(".0")
        .map(str::to_owned)
        .unwrap_or(number)
}

/// A session's cost: four decimals under a cent, else two.
fn format_cost(amount: f64, currency: &str) -> String {
    let precision = if amount > 0. && amount < 0.01 { 4 } else { 2 };
    format!("{amount:.precision$} {currency}")
}

fn short_commit(id: &str) -> String {
    let tail = id.rsplit('.').next().unwrap_or(id);
    let skip = tail.chars().count().saturating_sub(8);
    tail.chars().skip(skip).collect()
}

fn content_text(content: &ContentBlock) -> String {
    match content {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::ResourceLink(link) => format!("[{}]({})", link.name, link.uri),
        _ => String::new(),
    }
}

// ---- rendering ----
//
// Layout, not sentences (spec 002): Delta's thread, a gutter icon beside each entry and one-line
// activity rows; Zed's tool cards, a tinted header and a footer of icon buttons with key caps.
// Explanations go in tooltips.

/// Width of the thread's gutter, where each entry's icon sits.
const GUTTER: f32 = 24.;

/// A thread row: an icon in the gutter, the content beside it.
fn gutter_row(icon: Option<Icon>, content: impl IntoElement) -> Div {
    h_flex()
        .w_full()
        .items_start()
        .gap_1()
        .child(
            div()
                .w(px(GUTTER))
                .flex_none()
                // Centers a small icon on the first line of `text_sm` content.
                .pt(px(4.))
                .flex()
                .justify_center()
                .children(icon),
        )
        .child(div().flex_1().min_w_0().child(content))
}

/// A short verb for one of Studio's tools, as Delta labels activity ("Edit", "Read").
fn tool_verb(title: &str) -> String {
    if title.contains("omnigraph_schema") {
        "Read the schema".into()
    } else if title.contains("omnigraph_branches") {
        "List branches".into()
    } else if title.contains("omnigraph_query") {
        "Query".into()
    } else if title.contains("omnigraph_propose_change") {
        "Propose a change".into()
    } else {
        title.to_owned()
    }
}

impl Render for AgentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_bg = cx.theme().colors().editor_background;
        let body = match &self.state {
            State::Setup => self.render_setup(cx).into_any_element(),
            _ => {
                self.sync_markdown(cx);
                self.render_thread(window, cx).into_any_element()
            }
        };
        let banner = self.render_branch_banner(cx);
        let (graph, branch) = {
            let session = self.session.read(cx);
            (session.graph_id.clone(), session.branch.clone())
        };
        // The prompt, inline under the thread as Delta writes it: the next message's gutter and
        // the editor. The agent and send are in the footer strip.
        let composer = div()
            .id("composer")
            .key_context("AgentComposer")
            .flex_none()
            .pl_2()
            .pr_4()
            .pt(px(ds::space::MD))
            .pb(px(ds::space::LG))
            .border_t_1()
            .border_color(ds::surface::rule(cx))
            .when(ds::is_glass(cx), |row| row.border_dashed())
            .tooltip(Tooltip::text(format!(
                "The agent reads {graph} at {branch}; its changes go to a branch of its own"
            )))
            .child(gutter_row(
                Some(
                    Icon::new(IconName::Person)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                ),
                div().text_sm().child(self.composer.clone()),
            ));

        v_flex()
            .key_context("AgentPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::stop))
            .on_action(cx.listener(Self::allow))
            .on_action(cx.listener(Self::allow_always))
            .on_action(cx.listener(Self::reject))
            .on_action(cx.listener(Self::restart))
            .size_full()
            .bg(editor_bg)
            .child(body)
            .children(banner)
            .child(composer)
    }
}

impl AgentPanel {
    /// The agent's name, a dot for its state and what the state means.
    fn status(&self) -> (String, Color, &'static str) {
        match &self.state {
            State::Setup => ("Agent".to_owned(), Color::Muted, "No agent chosen"),
            State::Idle => (
                "Agent".to_owned(),
                Color::Muted,
                "Starts when you send a message",
            ),
            State::Starting => ("Agent".to_owned(), Color::Warning, "Starting"),
            State::Ready { agent } => (agent.clone(), Color::Success, "Ready"),
            State::Prompting { agent } => (agent.clone(), Color::Accent, "Working"),
            State::Exited { .. } => ("Agent".to_owned(), Color::Error, "Stopped"),
        }
    }

    fn render_setup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border_variant;
        v_flex()
            .id("agent-setup")
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .p_6()
            .child(
                v_flex()
                    .w(px(320.))
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .mb_2()
                            .justify_center()
                            .child(Label::new("Choose an agent").color(Color::Muted))
                            .child(
                                div()
                                    .id("agent-info")
                                    .tooltip(Tooltip::text(
                                        "Studio runs the agent on this computer over the Agent \
                                         Client Protocol and gives it tools for this graph. It \
                                         never sees your token; each change it proposes waits \
                                         for you and lands on a branch of its own.",
                                    ))
                                    .child(
                                        Icon::new(IconName::Info)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            ),
                    )
                    // A question asked before an agent was chosen waits, and says so.
                    .children(self.queued.clone().map(|question| {
                        div().mb_2().child(
                            Label::new(format!("“{question}” is sent once you choose an agent."))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }))
                    .children(PRESETS.iter().enumerate().map(|(index, (name, command))| {
                        let command: Vec<String> =
                            command.iter().map(|s| (*s).to_owned()).collect();
                        let shown = command.join(" ");
                        studio_ui::ds::button(("preset", index), *name)
                            .full_width()
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .start_icon(
                                Icon::new(IconName::ZedAssistant)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .tooltip(Tooltip::text(format!(
                                "Runs {shown}; npx downloads it the first time you send a message"
                            )))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.use_command(command.clone(), cx)
                            }))
                    }))
                    .child(
                        h_flex()
                            .my_2()
                            .gap_2()
                            .child(ui::Divider::horizontal())
                            .child(Label::new("or").size(LabelSize::XSmall).color(Color::Muted))
                            .child(ui::Divider::horizontal()),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(border)
                                    .child(self.custom_command.clone()),
                            )
                            .child(
                                studio_ui::ds::icon_button("use-custom", IconName::ArrowRight)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Use this command"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let text = this.custom_command.read(cx).text(cx);
                                        let command: Vec<String> =
                                            text.split_whitespace().map(str::to_owned).collect();
                                        if !command.is_empty() {
                                            this.use_command(command, cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }

    /// Brings each agent reply's markdown up to its text: new replies get an entity, a growing
    /// one appends what streamed in since.
    fn sync_markdown(&mut self, cx: &mut Context<Self>) {
        self.markdown
            .retain(|index, _| matches!(self.entries.get(*index), Some(Entry::Agent(_))));
        for (index, entry) in self.entries.iter().enumerate() {
            let Entry::Agent(text) = entry else { continue };
            match self.markdown.get_mut(&index) {
                Some((markdown, taken)) if text.len() >= *taken => {
                    if text.len() > *taken {
                        let tail = text[*taken..].to_owned();
                        markdown.update(cx, |markdown, cx| markdown.append(&tail, cx));
                        *taken = text.len();
                    }
                }
                _ => {
                    let source: SharedString = text.clone().into();
                    let markdown = cx.new(|cx| Markdown::new(source, None, None, cx));
                    self.markdown.insert(index, (markdown, text.len()));
                }
            }
        }
    }

    fn render_thread(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut thread = v_flex()
            .id("agent-thread")
            .track_scroll(&self.scroll)
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .py_4()
            .pr_4()
            .pl_2()
            .gap_5();
        if self.entries.is_empty() && !matches!(self.state, State::Exited { .. }) {
            return thread
                .child(studio_ui::ds::empty_state(
                    IconName::ZedAssistant,
                    "Ask about the graph",
                ))
                .into_any_element();
        }
        for (index, entry) in self.entries.iter().enumerate() {
            thread = thread.child(self.render_entry(index, entry, window, cx));
        }
        if let State::Exited { message, stderr } = &self.state {
            let mut callout = Callout::new()
                .severity(Severity::Error)
                .icon(IconName::XCircle)
                .title(message.clone());
            if !stderr.is_empty() {
                callout = callout.description_slot(studio_ui::ds::code_block(stderr.clone(), cx));
            }
            thread = thread.child(gutter_row(None, callout));
        }
        thread.into_any_element()
    }

    fn render_entry(
        &self,
        index: usize,
        entry: &Entry,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let text_muted = cx.theme().colors().text_muted;
        match entry {
            Entry::User(text) => gutter_row(
                Some(
                    Icon::new(IconName::Person)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                ),
                div().text_sm().child(text.clone()),
            )
            .into_any_element(),
            Entry::Agent(text) => gutter_row(
                Some(
                    Icon::new(IconName::ZedAssistant)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                ),
                match self.markdown.get(&index) {
                    // Zed's markdown, as Zed's agent panel renders a reply.
                    Some((markdown, _)) => div()
                        .text_sm()
                        .child(MarkdownElement::new(
                            markdown.clone(),
                            MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                        ))
                        .into_any_element(),
                    None => div()
                        .text_sm()
                        .child(text.trim().to_owned())
                        .into_any_element(),
                },
            )
            .into_any_element(),
            Entry::Thought(text) => gutter_row(
                None,
                h_flex()
                    .id(("thought", index))
                    .gap_1()
                    .tooltip(Tooltip::text(text.clone()))
                    .child(
                        Icon::new(IconName::Sparkle)
                            .size(IconSize::XSmall)
                            .color(Color::Disabled),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(text_muted)
                            .truncate()
                            .child(text.lines().next().unwrap_or_default().to_owned()),
                    ),
            )
            .into_any_element(),
            Entry::Tool { title, status, .. } => {
                let icon = match status {
                    ToolCallStatus::Pending => Icon::new(IconName::Circle).color(Color::Disabled),
                    ToolCallStatus::InProgress => {
                        Icon::new(IconName::ArrowCircle).color(Color::Accent)
                    }
                    ToolCallStatus::Completed => Icon::new(IconName::Check).color(Color::Success),
                    ToolCallStatus::Failed => Icon::new(IconName::XCircle).color(Color::Error),
                    _ => Icon::new(IconName::Circle).color(Color::Disabled),
                };
                gutter_row(
                    Some(icon.size(IconSize::XSmall)),
                    h_flex()
                        .id(("tool", index))
                        .tooltip(Tooltip::text(title.clone()))
                        .child(
                            Label::new(tool_verb(title))
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .single_line()
                                .truncate(),
                        ),
                )
                .into_any_element()
            }
            Entry::Plan(items) => gutter_row(
                Some(
                    Icon::new(IconName::ListTodo)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                ),
                v_flex()
                    .gap_0p5()
                    .children(items.iter().map(|(content, status)| {
                        let icon = match status {
                            PlanEntryStatus::Completed => {
                                Icon::new(IconName::Check).color(Color::Success)
                            }
                            PlanEntryStatus::InProgress => {
                                Icon::new(IconName::ArrowRight).color(Color::Accent)
                            }
                            _ => Icon::new(IconName::Circle).color(Color::Disabled),
                        };
                        h_flex()
                            .gap_2()
                            .child(icon.size(IconSize::XSmall))
                            .child(Label::new(content.clone()).size(LabelSize::Small))
                    })),
            )
            .into_any_element(),
            Entry::Notice { text, error } => gutter_row(
                Some(
                    Icon::new(if *error {
                        IconName::Warning
                    } else {
                        IconName::Info
                    })
                    .size(IconSize::XSmall)
                    .color(if *error { Color::Error } else { Color::Muted }),
                ),
                Label::new(text.clone())
                    .size(LabelSize::Small)
                    .color(if *error { Color::Error } else { Color::Muted }),
            )
            .into_any_element(),
            Entry::Permission(card) => {
                gutter_row(None, self.render_permission(index, card, cx)).into_any_element()
            }
            Entry::Proposal(proposal) => {
                gutter_row(None, self.render_proposal(index, proposal, cx)).into_any_element()
            }
        }
    }

    fn render_permission(
        &self,
        index: usize,
        card: &PermissionCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = studio_ui::ds::card_header(cx)
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning),
            )
            .child(
                Label::new(card.title.clone())
                    .size(LabelSize::Small)
                    .single_line()
                    .truncate(),
            );
        let footer = match &card.answer {
            Some(answer) => studio_ui::ds::card_footer(cx).child(
                Label::new(answer.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
            None => studio_ui::ds::card_footer(cx).children(card.options.iter().enumerate().map(
                |(option_index, option)| {
                    let (icon, color) = match option.kind {
                        PermissionOptionKind::AllowOnce => (IconName::Check, Color::Success),
                        PermissionOptionKind::AllowAlways => {
                            (IconName::CheckDouble, Color::Success)
                        }
                        _ => (IconName::Close, Color::Error),
                    };
                    studio_ui::ds::button(
                        ("permission", index * 16 + option_index),
                        option.name.clone(),
                    )
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_option(index, option_index, cx)
                    }))
                },
            )),
        };
        studio_ui::ds::card(cx)
            .child(header)
            .child(footer)
            .into_any_element()
    }

    fn render_proposal(
        &self,
        index: usize,
        proposal: &Proposal,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focus = self.focus_handle.clone();
        let header = studio_ui::ds::card_header(cx)
            .child(
                Icon::new(IconName::Pencil)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(proposal.summary.clone())
                    .size(LabelSize::Small)
                    .single_line()
                    .truncate(),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id(("proposal-branch", index))
                    .tooltip(Tooltip::text(match &proposal.creates_from {
                        Some(base) => format!(
                            "Runs on {}, which Studio creates from {base} first",
                            proposal.branch
                        ),
                        None => format!("Runs on {}", proposal.branch),
                    }))
                    .child(studio_ui::ds::meta(
                        IconName::GitBranch,
                        proposal.branch.clone(),
                    )),
            )
            .when(proposal.creates_from.is_some(), |header| {
                header.child(ui::Chip::new("new").label_size(LabelSize::XSmall))
            });
        let params = proposal.params.as_ref().map(|params| {
            ds::fields().children(params.iter().map(|(key, value)| {
                ds::field(
                    format!("${key}"),
                    ds::text::code(
                        match value {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        },
                        cx,
                    )
                    .single_line()
                    .truncate(),
                )
            }))
        });
        let body = ds::card_body()
            .child(ds::code_block(proposal.query.clone(), cx))
            .children(params);
        let footer = match &proposal.state {
            ProposalState::Waiting(_) => studio_ui::ds::card_footer(cx)
                .child(
                    studio_ui::ds::button(("allow", index), "Allow")
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::Check)
                                .size(IconSize::XSmall)
                                .color(Color::Success),
                        )
                        .key_binding(KeyBinding::for_action_in(&Allow, &focus, cx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide(index, Decision::Allow, cx)
                        })),
                )
                .child(
                    studio_ui::ds::button(("always", index), "Always on this branch")
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::CheckDouble)
                                .size(IconSize::XSmall)
                                .color(Color::Success),
                        )
                        .key_binding(KeyBinding::for_action_in(&AllowAlways, &focus, cx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide(index, Decision::AllowAlways, cx)
                        })),
                )
                .child(div().flex_1())
                .child(
                    studio_ui::ds::button(("reject", index), "Reject")
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::Close)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                        .key_binding(KeyBinding::for_action_in(&Reject, &focus, cx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide(index, Decision::Reject, cx)
                        })),
                ),
            state => {
                let (icon, color, label, tooltip): (IconName, Color, String, Option<String>) =
                    match state {
                        ProposalState::Running => {
                            (IconName::ArrowCircle, Color::Accent, "Running".into(), None)
                        }
                        ProposalState::Committed {
                            commit: Some(commit),
                            nodes,
                            edges,
                        } => (
                            IconName::Check,
                            Color::Success,
                            "Committed".into(),
                            Some(format!(
                                "Commit {commit}: {nodes} nodes and {edges} edges changed"
                            )),
                        ),
                        ProposalState::Committed { commit: None, .. } => (
                            IconName::Check,
                            Color::Muted,
                            "No change".into(),
                            Some("The mutation matched no rows".into()),
                        ),
                        ProposalState::Refused(message) => (
                            IconName::XCircle,
                            Color::Error,
                            "Refused".into(),
                            Some(message.clone()),
                        ),
                        ProposalState::Unknown => (
                            IconName::Warning,
                            Color::Warning,
                            "Outcome unknown".into(),
                            Some(UNKNOWN_OUTCOME.into()),
                        ),
                        _ => (IconName::Close, Color::Muted, "Rejected".into(), None),
                    };
                let commit = match state {
                    ProposalState::Committed {
                        commit: Some(commit),
                        nodes,
                        edges,
                    } => Some((short_commit(commit), *nodes + *edges)),
                    _ => None,
                };
                studio_ui::ds::card_footer(cx)
                    .px_2()
                    .gap_3()
                    .child(
                        h_flex()
                            .id(("outcome", index))
                            .gap_1()
                            .when_some(tooltip, |d, tooltip| d.tooltip(Tooltip::text(tooltip)))
                            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                            .child(Label::new(label).size(LabelSize::Small).color(color)),
                    )
                    .when_some(commit, |d, (commit, changed)| {
                        d.child(studio_ui::ds::meta_code(IconName::GitCommit, commit, cx))
                            .child(studio_ui::ds::meta(IconName::Pencil, changed.to_string()))
                    })
            }
        };
        studio_ui::ds::card(cx)
            .child(header)
            .child(body)
            .child(footer)
            .into_any_element()
    }

    /// The agent's options as footer pickers, in the agent's order (spec 011): a menu for a
    /// choice of values, a switch for on and off. Only while the session runs.
    fn render_options(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.host.is_none() {
            return Vec::new();
        }
        let panel = cx.entity().downgrade();
        let mut pickers = Vec::new();
        for option in &self.config_options {
            let id = option.id.0.to_string();
            let pending = self
                .setting_option
                .as_ref()
                .filter(|(pending, _)| *pending == id)
                .map(|(_, value)| value);
            let about: SharedString = match &option.description {
                Some(description) => format!("{}: {description}", option.name).into(),
                None => option.name.clone().into(),
            };
            match &option.kind {
                SessionConfigKind::Select(select) => {
                    let current = pending
                        .and_then(SessionConfigOptionValue::as_value_id)
                        .map(|value| value.0.to_string())
                        .unwrap_or_else(|| select.current_value.0.to_string());
                    let name = select_values(&select.options)
                        .find(|(value, _)| *value == current)
                        .map(|(_, name)| name.to_owned())
                        .unwrap_or_else(|| current.clone());
                    // The menu's sections: the agent's groups, or the option's own name.
                    let sections: Vec<(String, Vec<(String, String)>)> = match &select.options {
                        SessionConfigSelectOptions::Grouped(groups) => groups
                            .iter()
                            .map(|group| {
                                (
                                    group.name.clone(),
                                    group
                                        .options
                                        .iter()
                                        .map(|o| (o.value.0.to_string(), o.name.clone()))
                                        .collect(),
                                )
                            })
                            .collect(),
                        options => vec![(
                            option.name.clone(),
                            select_values(options)
                                .map(|(value, name)| (value.to_owned(), name.to_owned()))
                                .collect(),
                        )],
                    };
                    let panel = panel.clone();
                    let menu_id = id.clone();
                    pickers.push(
                        PopoverMenu::new(SharedString::from(format!("option-{id}")))
                            .trigger(
                                ds::button(
                                    SharedString::from(format!("option-{id}-button")),
                                    cut(&name, OPTION_NAME_CHARS),
                                )
                                .style(ButtonStyle::Subtle)
                                .color(Color::Muted)
                                .disabled(pending.is_some())
                                .end_icon(
                                    Icon::new(IconName::ChevronDown)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .tooltip(Tooltip::text(about)),
                            )
                            .menu(move |window, cx| {
                                let sections = sections.clone();
                                let current = current.clone();
                                let id = menu_id.clone();
                                let panel = panel.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for (index, (header, values)) in
                                        sections.into_iter().enumerate()
                                    {
                                        if index > 0 {
                                            menu = menu.separator();
                                        }
                                        menu = menu.header(header);
                                        for (value, name) in values {
                                            let checked = value == current;
                                            let panel = panel.clone();
                                            let id = id.clone();
                                            menu = menu.toggleable_entry(
                                                name,
                                                checked,
                                                IconPosition::Start,
                                                None,
                                                move |_, cx| {
                                                    if checked {
                                                        return;
                                                    }
                                                    panel
                                                        .update(cx, |panel, cx| {
                                                            panel.set_option(
                                                                id.clone(),
                                                                SessionConfigOptionValue::value_id(
                                                                    value.clone(),
                                                                ),
                                                                cx,
                                                            )
                                                        })
                                                        .ok();
                                                },
                                            );
                                        }
                                    }
                                    menu
                                }))
                            })
                            .anchor(gpui::Anchor::BottomRight)
                            .into_any_element(),
                    );
                }
                SessionConfigKind::Boolean(boolean) => {
                    let on = match pending {
                        Some(SessionConfigOptionValue::Boolean { value }) => *value,
                        _ => boolean.current_value,
                    };
                    let panel = panel.clone();
                    pickers.push(
                        h_flex()
                            .id(SharedString::from(format!("option-{id}")))
                            .tooltip(Tooltip::text(about))
                            .child(
                                Switch::new(
                                    SharedString::from(format!("option-{id}-switch")),
                                    on.into(),
                                )
                                .label(option.name.clone())
                                .label_position(SwitchLabelPosition::Start)
                                .label_size(LabelSize::Small)
                                .label_color(Color::Muted)
                                .disabled(pending.is_some())
                                .on_click(move |state, _, cx| {
                                    let next = matches!(state, ToggleState::Selected);
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.set_option(
                                                id.clone(),
                                                SessionConfigOptionValue::boolean(next),
                                                cx,
                                            )
                                        })
                                        .ok();
                                }),
                            )
                            .into_any_element(),
                    );
                }
                _ => {}
            }
        }
        pickers
    }

    /// How full the context window is, as a ring and a percentage that turn to the warning
    /// colour near the end; the token counts and the cost so far on hover.
    fn render_usage(&self, cx: &App) -> Option<AnyElement> {
        let usage = self.usage.as_ref().filter(|usage| usage.size > 0)?;
        let ratio = (usage.used as f32 / usage.size as f32).clamp(0., 1.);
        let nearly_full = ratio >= 0.85;
        let color = if nearly_full {
            cx.theme().status().warning
        } else {
            cx.theme().colors().text_muted
        };
        let percent = format!("{}%", (ratio * 100.).round() as u32);
        let title = format!("{percent} of the context window");
        let mut meta = format!(
            "{} of {} tokens",
            humanize_tokens(usage.used),
            humanize_tokens(usage.size)
        );
        if let Some((amount, currency)) = &usage.cost {
            meta.push_str(&format!(" · {} so far", format_cost(*amount, currency)));
        }
        Some(
            h_flex()
                .id("context-usage")
                .flex_none()
                .gap(px(ds::space::XS))
                .items_center()
                .child(
                    CircularProgress::new(usage.used as f32, usage.size as f32, px(14.), cx)
                        .stroke_width(px(2.))
                        .progress_color(color),
                )
                // The number too: a ring alone reads as a spinner.
                .child(
                    ds::text::meta(percent).when(nearly_full, |label| label.color(Color::Warning)),
                )
                .tooltip(move |_, cx| Tooltip::with_meta(title.clone(), None, meta.clone(), cx))
                .into_any_element(),
        )
    }

    fn render_branch_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let branch = self
            .branch
            .as_ref()
            .filter(|b| b.created && b.changes > 0)?;
        Some(
            h_flex()
                .flex_none()
                .h(px(studio_ui::ds::size::STRIP))
                .px_3()
                .gap_3()
                .border_t_1()
                .border_color(studio_ui::ds::surface::rule(cx))
                .child(studio_ui::ds::meta(
                    IconName::GitBranch,
                    branch.name.clone(),
                ))
                .child(
                    div()
                        .id("agent-changes")
                        .tooltip(Tooltip::text("Changes committed on the agent's branch"))
                        .child(studio_ui::ds::meta(
                            IconName::Pencil,
                            branch.changes.to_string(),
                        )),
                )
                .child(div().flex_1())
                .child(
                    studio_ui::ds::button("review", "Review")
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Eye).size(IconSize::XSmall))
                        .on_click(cx.listener(|this, _, window, cx| this.review(window, cx))),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_and_costs_read_like_zeds() {
        assert_eq!(humanize_tokens(999), "999");
        assert_eq!(humanize_tokens(1_000), "1k");
        assert_eq!(humanize_tokens(1_234), "1.2k");
        assert_eq!(humanize_tokens(83_600), "84k");
        assert_eq!(humanize_tokens(200_000), "200k");
        assert_eq!(humanize_tokens(1_000_000), "1M");
        assert_eq!(humanize_tokens(1_250_000), "1.2M");
        assert_eq!(format_cost(0.0042, "USD"), "0.0042 USD");
        assert_eq!(format_cost(0.411, "USD"), "0.41 USD");
        assert_eq!(format_cost(0., "USD"), "0.00 USD");
    }

    #[test]
    fn long_option_names_are_cut_on_a_character() {
        assert_eq!(cut("Claude Sonnet", OPTION_NAME_CHARS), "Claude Sonnet");
        let long = "Claude Sonnet with an extended thinking budget";
        // No space is left before the ellipsis.
        assert_eq!(
            cut(long, OPTION_NAME_CHARS),
            "Claude Sonnet with an extended…"
        );
        assert_eq!(cut("模型模型模型", 4), "模型模…");
    }
}
