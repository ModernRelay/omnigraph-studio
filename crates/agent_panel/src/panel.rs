//! The agent panel's view and state: the thread, approval cards, the composer, and the tools
//! run on the person's session.

use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOption, PermissionOptionKind, PlanEntryStatus,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate, StopReason,
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
use studio_workspace::{Item, ItemEvent, SavedItem, Workspace};
use ui::KeyBinding;
use ui::prelude::*;

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
    workspace.open_item("agent", window, cx, move |window, cx| {
        AgentPanel::new(session, window, cx)
    })
}

enum State {
    /// No agent chosen yet.
    Setup,
    /// An agent is chosen but not running.
    Idle,
    Starting,
    Ready { agent: String },
    Prompting { agent: String },
    Exited { message: String, stderr: String },
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
    Committed(String),
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
    Notice { text: String, error: bool },
}

struct Host {
    commands: mpsc::UnboundedSender<HostCommand>,
    _socket: ToolSocket,
    _tasks: Vec<Task<()>>,
}

pub struct AgentPanel {
    session: Entity<GraphSession>,
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
}

impl AgentPanel {
    pub fn new(
        session: Entity<GraphSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
            branch: None,
            always_allow: false,
            host: None,
            queued: None,
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

    fn start(&mut self, cx: &mut Context<Self>) {
        let settings = StudioSettings::get(cx).agent.clone();
        if settings.command.is_empty() {
            self.state = State::Setup;
            return;
        }
        let data = studio_settings::data_dir().join("agent");
        let cwd = data.join("workspace");
        if let Err(error) = std::fs::create_dir_all(&cwd) {
            self.fail_start(format!("Studio could not create {}: {error}", cwd.display()), cx);
            return;
        }
        let mut socket = match ToolSocket::bind(&crate::socket::socket_dir()) {
            Ok(socket) => socket,
            Err(error) => {
                self.fail_start(format!("Studio could not open the tool socket: {error}"), cx);
                return;
            }
        };
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                self.fail_start(format!("Studio could not find its own executable: {error}"), cx);
                return;
            }
        };
        let base = self.session.read(cx).branch.clone();
        self.branch = Some(AgentBranch {
            name: format!("agent/{}", chrono::Local::now().format("%Y%m%d-%H%M%S")),
            base,
            created: false,
            changes: 0,
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

    fn restart(&mut self, _: &Restart, _window: &mut Window, cx: &mut Context<Self>) {
        self.stop_host();
        self.entries.push(Entry::Notice {
            text: "Started a new agent session.".into(),
            error: false,
        });
        self.start(cx);
    }

    fn apply_event(&mut self, event: HostEvent, cx: &mut Context<Self>) {
        match event {
            HostEvent::Ready { agent } => {
                self.state = State::Ready { agent };
                if let Some(text) = self.queued.take() {
                    self.prompt(text, cx);
                }
            }
            HostEvent::Update(update) => self.apply_update(update),
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
                    {
                        if *existing == id {
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

    // ---- prompting ----

    pub fn send_text(&mut self, text: String, _window: &mut Window, cx: &mut Context<Self>) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        match &self.state {
            State::Ready { .. } => self.prompt(text, cx),
            State::Prompting { .. } => self.entries.push(Entry::Notice {
                text: "The agent is still working. Stop it first, or wait.".into(),
                error: false,
            }),
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
        if let Some(Entry::Permission(card)) = self.entries.get_mut(index) {
            if let (Some(reply), Some(option)) =
                (card.reply.take(), card.options.get(option_index))
            {
                card.answer = Some(option.name.clone());
                reply
                    .send(RequestPermissionOutcome::Selected(
                        SelectedPermissionOutcome::new(option.option_id.clone()),
                    ))
                    .ok();
            }
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
                .send(("The schema is still loading; try again in a moment.".into(), true))
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
                if agent_branch.is_none() {
                    if let Some(branch) = &self.branch {
                        text.push_str(&format!(
                            "\nYour branch will be {} once you propose a change.\n",
                            branch.name
                        ));
                    }
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
                        this.update(cx, |this, _| {
                            if let Some(branch) = &mut this.branch {
                                branch.created = true;
                            }
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
                reply
                    .send(("The agent session ended.".into(), true))
                    .ok();
                return;
            };
            let outcome = write.await;
            let (state, text, is_error) = describe_outcome(&outcome);
            let committed = matches!(&outcome, WriteOutcome::Committed(output) if output.commit.is_some());
            this.update(cx, |this, cx| {
                if committed {
                    if let Some(branch) = &mut this.branch {
                        branch.changes += 1;
                    }
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
    if let Ok(path) = std::env::var("OMNIGRAPH_STUDIO_PROXY") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    current
}

fn describe_outcome(outcome: &WriteOutcome<ChangeOutput>) -> (ProposalState, String, bool) {
    match outcome {
        WriteOutcome::Committed(output) => match &output.commit {
            Some(commit) => {
                let counts = format!(
                    "{} nodes and {} edges changed",
                    output.affected_nodes, output.affected_edges
                );
                // The agent gets the full id to refer to; the card shows its short form.
                let card = format!(
                    "Committed on {} as {}: {counts}.",
                    output.branch,
                    short_commit(&commit.graph_commit_id)
                );
                let text = format!(
                    "Committed on {}: commit {}, {counts}.",
                    output.branch, commit.graph_commit_id
                );
                (ProposalState::Committed(card), text, false)
            }
            None => {
                let text = "Nothing changed: the mutation matched no rows.".to_owned();
                (ProposalState::Committed(text.clone()), text, false)
            }
        },
        WriteOutcome::Refused(error) => {
            let text = format!(
                "Refused by the server ({}): {}",
                error.status, error.body.error
            );
            (ProposalState::Refused(text.clone()), text, true)
        }
        WriteOutcome::Unknown { .. } => {
            (ProposalState::Unknown, UNKNOWN_OUTCOME.to_owned(), true)
        }
    }
}

/// The last eight characters of a graph commit id, as the activity log shows it.
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

impl Render for AgentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (border, panel_bg, editor_bg) = (
            colors.border,
            colors.panel_background,
            colors.editor_background,
        );
        let header = self.render_header(cx).into_any_element();
        let body = match &self.state {
            State::Setup => self.render_setup(cx).into_any_element(),
            _ => self.render_thread(window, cx).into_any_element(),
        };
        let banner = self.render_branch_banner(cx);
        let composer = self.composer.clone();
        let focus = self.focus_handle.clone();
        let can_send = matches!(
            self.state,
            State::Ready { .. } | State::Idle | State::Exited { .. } | State::Starting
        );

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
            .child(header)
            .child(body)
            .children(banner)
            .child(
                v_flex()
                    .flex_none()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(border)
                    .bg(panel_bg)
                    .child(
                        div()
                            .key_context("AgentComposer")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(border)
                            .bg(editor_bg)
                            .child(composer),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap_3()
                                    .when(!matches!(self.state, State::Setup), |hints| hints.children(
                                        [
                                            (&Send as &dyn gpui::Action, "send"),
                                            (&Allow, "allow"),
                                            (&Reject, "reject"),
                                            (&Stop, "stop"),
                                        ]
                                        .into_iter()
                                        .map(|(action, label)| {
                                            h_flex()
                                                .gap_1()
                                                .child(KeyBinding::for_action_in(action, &focus, cx))
                                                .child(
                                                    Label::new(label)
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                        }),
                                    )),
                            )
                            .child(
                                Button::new("send", "Send")
                                    .label_size(LabelSize::Small)
                                    .disabled(!can_send)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send(&Send, window, cx)
                                    })),
                            ),
                    ),
            )
    }
}

impl AgentPanel {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border_variant;
        let (status, color) = match &self.state {
            State::Setup => ("No agent chosen".to_owned(), Color::Muted),
            State::Idle => ("Not started".to_owned(), Color::Muted),
            State::Starting => ("Starting…".to_owned(), Color::Muted),
            State::Ready { agent } => (format!("{agent} · ready"), Color::Success),
            State::Prompting { agent } => (format!("{agent} · working"), Color::Accent),
            State::Exited { .. } => ("Stopped".to_owned(), Color::Error),
        };
        let command = Self::command(cx).join(" ");
        let running = matches!(self.state, State::Prompting { .. });
        let restartable = matches!(
            self.state,
            State::Ready { .. } | State::Exited { .. }
        );
        h_flex()
            .flex_none()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .child(Icon::new(IconName::ZedAssistant).size(IconSize::Small))
            .child(Label::new(status).size(LabelSize::Small).color(color))
            .child(
                div().flex_1().overflow_hidden().child(
                    Label::new(command)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .single_line()
                        .truncate(),
                ),
            )
            .when(running, |row| {
                row.child(
                    Button::new("stop", "Stop")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| this.stop(&Stop, window, cx))),
                )
            })
            .when(restartable, |row| {
                row.child(
                    Button::new("restart", "New session")
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Subtle)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.restart(&Restart, window, cx)),
                        ),
                )
            })
    }

    fn render_setup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border;
        v_flex()
            .id("agent-setup")
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .p_6()
            .gap_3()
            .child(Headline::new("Choose an agent").size(HeadlineSize::Small))
            .child(
                Label::new(
                    "Studio runs the agent on this computer over the Agent Client Protocol and \
                     gives it tools for this graph. The agent never sees your token; every \
                     change it proposes waits for you and lands on a branch of its own.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .children(PRESETS.iter().enumerate().map(|(index, (name, command))| {
                let command: Vec<String> = command.iter().map(|s| (*s).to_owned()).collect();
                let shown = command.join(" ");
                h_flex()
                    .gap_3()
                    .child(
                        Button::new(("preset", index), *name)
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.use_command(command.clone(), cx)
                            })),
                    )
                    .child(Label::new(shown).size(LabelSize::XSmall).color(Color::Muted))
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(420.))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(border)
                            .child(self.custom_command.clone()),
                    )
                    .child(
                        Button::new("custom", "Use this command")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let text = this.custom_command.read(cx).text(cx);
                                let command: Vec<String> =
                                    text.split_whitespace().map(str::to_owned).collect();
                                if !command.is_empty() {
                                    this.use_command(command, cx);
                                }
                            })),
                    ),
            )
            .child(
                Label::new(
                    "Presets run npx, which downloads the agent the first time you send a message.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
    }

    fn render_thread(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut thread = v_flex()
            .id("agent-thread")
            .track_scroll(&self.scroll)
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .p_3()
            .gap_2();
        if self.entries.is_empty() {
            let text = match &self.state {
                State::Exited { .. } => "",
                _ => "Ask a question about the graph, or ask for a change.",
            };
            thread = thread.child(Label::new(text).color(Color::Muted));
        }
        for (index, entry) in self.entries.iter().enumerate() {
            thread = thread.child(self.render_entry(index, entry, cx));
        }
        if let State::Exited { message, stderr } = &self.state {
            thread = thread.child(
                v_flex()
                    .gap_1()
                    .child(Label::new(message.clone()).color(Color::Error))
                    .when(!stderr.is_empty(), |col| {
                        col.child(
                            div()
                                .p_2()
                                .rounded_md()
                                .bg(cx.theme().colors().surface_background)
                                .font_buffer(cx)
                                .text_xs()
                                .child(stderr.clone()),
                        )
                    }),
            );
        }
        thread
    }

    fn render_entry(&self, index: usize, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let (surface, border) = (colors.surface_background, colors.border_variant);
        match entry {
            Entry::User(text) => div()
                .p_2()
                .rounded_md()
                .bg(surface)
                .child(text.clone())
                .into_any_element(),
            Entry::Agent(text) => div().text_sm().child(text.clone()).into_any_element(),
            Entry::Thought(text) => v_flex()
                .child(
                    Label::new("Thinking")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .max_h(px(80.))
                        .overflow_hidden()
                        .child(text.clone()),
                )
                .into_any_element(),
            Entry::Tool { title, status, .. } => {
                let (icon, color, label) = match status {
                    ToolCallStatus::Pending => (IconName::Circle, Color::Muted, "pending"),
                    ToolCallStatus::InProgress => (IconName::ArrowCircle, Color::Accent, "running"),
                    ToolCallStatus::Completed => (IconName::Check, Color::Success, "done"),
                    ToolCallStatus::Failed => (IconName::XCircle, Color::Error, "failed"),
                    _ => (IconName::Circle, Color::Muted, ""),
                };
                h_flex()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .child(
                        Label::new(title.clone())
                            .size(LabelSize::Small)
                            .single_line()
                            .truncate(),
                    )
                    .child(Label::new(label).size(LabelSize::XSmall).color(Color::Muted))
                    .into_any_element()
            }
            Entry::Plan(items) => v_flex()
                .gap_0p5()
                .child(Label::new("Plan").size(LabelSize::XSmall).color(Color::Muted))
                .children(items.iter().map(|(content, status)| {
                    let (icon, color) = match status {
                        PlanEntryStatus::Completed => (IconName::Check, Color::Success),
                        PlanEntryStatus::InProgress => (IconName::ArrowRight, Color::Accent),
                        _ => (IconName::Circle, Color::Muted),
                    };
                    h_flex()
                        .gap_2()
                        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                        .child(Label::new(content.clone()).size(LabelSize::Small))
                }))
                .into_any_element(),
            Entry::Notice { text, error } => Label::new(text.clone())
                .size(LabelSize::Small)
                .color(if *error { Color::Error } else { Color::Muted })
                .into_any_element(),
            Entry::Permission(card) => self.render_permission(index, card, cx),
            Entry::Proposal(proposal) => self.render_proposal(index, proposal, cx),
        }
    }

    fn render_permission(
        &self,
        index: usize,
        card: &PermissionCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let border = cx.theme().colors().border;
        v_flex()
            .p_2()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(border)
            .child(Label::new(card.title.clone()).size(LabelSize::Small))
            .map(|card_el| match &card.answer {
                Some(answer) => card_el.child(
                    Label::new(answer.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
                None => card_el.child(h_flex().gap_1().children(
                    card.options.iter().enumerate().map(|(option_index, option)| {
                        Button::new(("permission", index * 16 + option_index), option.name.clone())
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.choose_option(index, option_index, cx)
                            }))
                    }),
                )),
            })
            .into_any_element()
    }

    fn render_proposal(
        &self,
        index: usize,
        proposal: &Proposal,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let (border, surface) = (colors.border, colors.surface_background);
        let target = match &proposal.creates_from {
            Some(base) => format!("Creates {} from {base}, then runs there", proposal.branch),
            None => format!("Runs on {}", proposal.branch),
        };
        let params = proposal
            .params
            .as_ref()
            .map(|params| serde_json::to_string_pretty(params).unwrap_or_default());
        let (status, color): (Option<String>, Color) = match &proposal.state {
            ProposalState::Waiting(_) => (None, Color::Default),
            ProposalState::Running => (Some("Running…".into()), Color::Muted),
            ProposalState::Committed(text) => (Some(text.clone()), Color::Success),
            ProposalState::Refused(text) => (Some(text.clone()), Color::Error),
            ProposalState::Unknown => (Some(UNKNOWN_OUTCOME.into()), Color::Warning),
            ProposalState::Rejected => (Some("Rejected.".into()), Color::Muted),
        };
        let waiting = matches!(proposal.state, ProposalState::Waiting(_));
        v_flex()
            .p_2()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(border)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Pencil).size(IconSize::Small))
                    .child(Label::new(proposal.summary.clone())),
            )
            .child(
                Label::new(target)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .p_2()
                    .rounded_sm()
                    .bg(surface)
                    .font_buffer(cx)
                    .text_xs()
                    .child(proposal.query.clone()),
            )
            .children(params.map(|params| {
                div()
                    .p_2()
                    .rounded_sm()
                    .bg(surface)
                    .font_buffer(cx)
                    .text_xs()
                    .child(params)
            }))
            .children(
                status.map(|status| Label::new(status).size(LabelSize::Small).color(color)),
            )
            .when(waiting, |card| {
                card.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(("allow", index), "Allow")
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide(index, Decision::Allow, cx)
                                })),
                        )
                        .child(
                            Button::new(("always", index), "Always for this branch")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide(index, Decision::AllowAlways, cx)
                                })),
                        )
                        .child(
                            Button::new(("reject", index), "Reject")
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide(index, Decision::Reject, cx)
                                })),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_branch_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let branch = self.branch.as_ref().filter(|b| b.created && b.changes > 0)?;
        let border = cx.theme().colors().border_variant;
        let text = format!(
            "{} · {} change{}",
            branch.name,
            branch.changes,
            if branch.changes == 1 { "" } else { "s" }
        );
        Some(
            h_flex()
                .flex_none()
                .px_3()
                .py_1()
                .gap_2()
                .border_t_1()
                .border_color(border)
                .child(Icon::new(IconName::GitBranch).size(IconSize::Small))
                .child(Label::new(text).size(LabelSize::Small))
                .child(div().flex_1())
                .child(
                    Button::new("review", "Review")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| this.review(window, cx))),
                )
                .into_any_element(),
        )
    }
}
