//! The ACP host (spec 011): one process per agent, speaking ACP v1, serving every conversation
//! Studio has open with that agent, as Zed keeps one connection per agent server. Each
//! conversation opens a session on the connection and gets its traffic as [`HostEvent`]s, routed
//! by session id; the connection also lists and deletes the agent's sessions when it can.
//! Runs off the main thread; the panels talk to it through channels. The process stops when the
//! last handle to it is dropped.
//!
//! ACP request handlers hold the connection's dispatch loop until they return, so a permission
//! request that waits for the person is answered from a task spawned on the connection.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    BooleanConfigOptionCapabilities, CancelNotification, ClientCapabilities,
    ClientSessionCapabilities, CloseSessionRequest, ContentBlock, DeleteSessionRequest,
    Implementation, InitializeRequest, ListSessionsRequest, LoadSessionRequest, McpServer,
    McpServerStdio, NewSessionRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionConfigId, SessionConfigOption,
    SessionConfigOptionValue, SessionConfigOptionsCapabilities, SessionId, SessionInfo,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, StopReason, TextContent,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, ConnectionTo, LineDirection, SentRequest,
};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};

use crate::tools;

/// Lines of the agent's stderr kept for the "stopped" state.
const STDERR_LINES: usize = 20;
/// Bytes of the agent's stderr kept for the "stopped" state.
const STDERR_BYTES: usize = 4096;
/// Updates kept for a session the connection has not been told about yet: an agent may report
/// on a session before its `session/new` answer has been read.
const EARLY_UPDATES: usize = 256;
/// How many sessions not yet routed keep early updates.
const EARLY_SESSIONS: usize = 16;
/// Pages of `session/list` read at most.
const LIST_PAGES: usize = 20;

/// How to start an agent: one process for all of its conversations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentLaunch {
    /// The program and its arguments.
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// One conversation on the agent.
#[derive(Debug, Clone)]
pub struct SessionLaunch {
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The stdio MCP server the agent starts for it: Studio's own executable as `mcp-proxy`,
    /// with the conversation's own socket.
    pub mcp_command: PathBuf,
    pub mcp_args: Vec<String>,
    /// A session to come back to, when the agent can load or resume one.
    pub resume: Option<String>,
}

/// What the agent said about itself when it started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AgentInfo {
    name: String,
    /// `session/list` and `session/delete`.
    list: bool,
    delete: bool,
}

/// How a conversation came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    New,
    /// Loaded: its messages arrive as updates before `Ready`.
    Loaded,
    /// Resumed without its messages; the agent knows them, Studio does not show them.
    Resumed,
}

pub enum HostEvent {
    /// The adapter the agent runs as, before anything of the session: a loaded session replays
    /// its tool calls before `Ready`, and each is told apart by its adapter.
    Adapter(tools::Adapter),
    /// The session is open.
    Ready {
        agent: String,
        session: String,
        opened: Opened,
    },
    Update(SessionUpdate),
    /// The agent asks to use a tool that is not Studio's. Answer through `reply`; dropping it
    /// answers "cancelled".
    Permission {
        request: RequestPermissionRequest,
        reply: oneshot::Sender<RequestPermissionOutcome>,
    },
    /// The session's options as the agent now reports them: on opening, and after a change.
    ConfigOptions(Vec<SessionConfigOption>),
    /// Setting an option failed.
    ConfigOptionFailed {
        message: String,
    },
    /// A prompt turn ended.
    TurnEnded {
        stop: Result<StopReason, String>,
    },
    /// The agent stopped or could not start; `failed` unless the session simply ended.
    Exited {
        message: String,
        stderr: String,
        failed: bool,
    },
}

pub enum HostCommand {
    Prompt(String),
    Cancel,
    /// Sets one of the session's options (its model, mode, reasoning level).
    SetConfigOption {
        id: String,
        value: SessionConfigOptionValue,
    },
}

/// What the connection is asked to do.
pub enum ConnectionCommand {
    /// Opens a conversation: its traffic goes to `events`, and it takes `commands` until they
    /// close, when the agent may let the session go. The panel closes `events` as it lets go,
    /// so a conversation that asks for the same session next sees at once that this one let go
    /// and waits for it to free the session.
    Open {
        launch: SessionLaunch,
        events: mpsc::UnboundedSender<HostEvent>,
        commands: mpsc::UnboundedReceiver<HostCommand>,
    },
    /// The agent's sessions in `cwd`, when it can list them.
    List {
        cwd: PathBuf,
        reply: oneshot::Sender<Result<Vec<SessionInfo>, String>>,
    },
    /// Deletes a session at the agent, when it can.
    Delete {
        session: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/// Where each session's traffic goes, and what arrived before the session was known.
#[derive(Default)]
struct Routes {
    sessions: HashMap<SessionId, mpsc::UnboundedSender<HostEvent>>,
    early: HashMap<SessionId, VecDeque<SessionUpdate>>,
    /// The tool calls that are Studio's own, by id, as their first update said: Codex asks
    /// permission for an MCP call with its id alone.
    studio_calls: HashMap<String, tools::StudioTool>,
    /// Sessions their conversation closed: an update that comes after is not kept as early,
    /// where it would take the place of a session about to open.
    closed: HashSet<SessionId>,
    /// Conversations waiting for a session that one whose panel let go still holds, woken when
    /// it frees the session.
    waiting: HashMap<SessionId, Vec<oneshot::Sender<()>>>,
}

/// What [`Routes::claim`] found.
enum Claim {
    /// The session's traffic goes to the conversation.
    Routed,
    /// Another conversation still listens to the session.
    Held,
    /// A conversation whose panel let go holds the session until it has freed it; this resolves
    /// then, and the conversation claims the session again.
    Wait(oneshot::Receiver<()>),
}

/// The most closed sessions remembered, so their late updates are dropped.
const CLOSED_SESSIONS: usize = 256;
/// The most of Studio's tool calls remembered for their permission requests.
const STUDIO_CALLS: usize = 256;

impl Routes {
    /// Remembers a tool call that is one of Studio's tools, by what the adapter said of it.
    fn note_tool_call(&mut self, adapter: tools::Adapter, update: &SessionUpdate) {
        let (id, studio) = match update {
            SessionUpdate::ToolCall(call) => (
                &call.tool_call_id,
                tools::StudioTool::of(adapter, call.meta.as_ref(), call.raw_input.as_ref()),
            ),
            SessionUpdate::ToolCallUpdate(call) => (
                &call.tool_call_id,
                tools::StudioTool::of(adapter, call.meta.as_ref(), call.fields.raw_input.as_ref()),
            ),
            _ => return,
        };
        if let Some(studio) = studio {
            if self.studio_calls.len() >= STUDIO_CALLS {
                self.studio_calls.clear();
            }
            self.studio_calls.insert(id.to_string(), studio);
        }
    }

    fn update(&mut self, session: SessionId, update: SessionUpdate) {
        match self.sessions.get(&session) {
            Some(events) => {
                events.unbounded_send(HostEvent::Update(update)).ok();
            }
            None if self.closed.contains(&session) => {}
            // Kept for a session about to be routed (a load's replay can come first), for a
            // few sessions at a time.
            None if self.early.contains_key(&session) || self.early.len() < EARLY_SESSIONS => {
                let early = self.early.entry(session).or_default();
                if early.len() < EARLY_UPDATES {
                    early.push_back(update);
                }
            }
            None => {}
        }
    }

    /// Removes `session`'s route when it is `events`', and wakes the conversations waiting for
    /// it; a route that is another conversation's stays. Whether it was removed.
    fn remove(&mut self, session: &SessionId, events: &mpsc::UnboundedSender<HostEvent>) -> bool {
        self.early.remove(session);
        if self
            .sessions
            .get(session)
            .is_some_and(|routed| routed.same_receiver(events))
        {
            self.sessions.remove(session);
            if self.closed.len() >= CLOSED_SESSIONS {
                self.closed.clear();
            }
            self.closed.insert(session.clone());
            self.waiting.remove(session);
            return true;
        }
        false
    }

    fn add(&mut self, session: SessionId, events: mpsc::UnboundedSender<HostEvent>) {
        self.closed.remove(&session);
        for update in self.early.remove(&session).unwrap_or_default() {
            events.unbounded_send(HostEvent::Update(update)).ok();
        }
        self.sessions.insert(session, events);
    }

    /// Routes `session` to `events` unless another conversation holds it. One that still
    /// listens to it keeps it: a second would take its replies, tool calls and permission
    /// requests. One whose panel let go (its tab closed or restarted) keeps it until it has
    /// freed it, after the agent answered its open: opened again before that, the session would
    /// be opened twice at once, and the first load's replay would reach the second
    /// conversation. The claim then waits.
    fn claim(&mut self, session: SessionId, events: mpsc::UnboundedSender<HostEvent>) -> Claim {
        match self.sessions.get(&session) {
            Some(routed) if routed.same_receiver(&events) => {}
            Some(routed) if !routed.is_closed() => return Claim::Held,
            Some(_) => {
                let (freed, wait) = oneshot::channel();
                self.waiting.entry(session).or_default().push(freed);
                return Claim::Wait(wait);
            }
            None => {}
        }
        self.add(session, events);
        Claim::Routed
    }
}

type SharedRoutes = Arc<Mutex<Routes>>;
/// The conversations asked for and not yet routed, which hear an exit too:
/// `AgentConnection::open` adds each before the agent reads its request, so one asked for before
/// the agent fails to start hears why.
pub type Opening = Arc<Mutex<Vec<mpsc::UnboundedSender<HostEvent>>>>;

/// Runs one agent until every handle to it is dropped or it exits; then every open conversation
/// hears why. Dropping the future kills the agent's process group.
pub async fn run(
    launch: AgentLaunch,
    commands: mpsc::UnboundedReceiver<ConnectionCommand>,
    opening: Opening,
) {
    let stderr = Arc::new(Mutex::new(VecDeque::<String>::new()));
    let routes = SharedRoutes::default();
    let result = connect(
        launch,
        commands,
        stderr.clone(),
        routes.clone(),
        opening.clone(),
    )
    .await;
    let failed = result.is_err();
    let message = match result {
        Ok(()) => "The agent session ended.".to_owned(),
        Err(error) => format!("The agent stopped: {error}"),
    };
    let stderr = stderr
        .lock()
        .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    let mut listeners: Vec<_> = routes
        .lock()
        .map(|routes| routes.sessions.values().cloned().collect())
        .unwrap_or_default();
    listeners.extend(opening.lock().map(|o| o.clone()).unwrap_or_default());
    for events in listeners {
        events
            .unbounded_send(HostEvent::Exited {
                message: message.clone(),
                stderr: stderr.clone(),
                failed,
            })
            .ok();
    }
}

async fn connect(
    launch: AgentLaunch,
    mut commands: mpsc::UnboundedReceiver<ConnectionCommand>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    routes: SharedRoutes,
    opening: Opening,
) -> Result<(), agent_client_protocol::Error> {
    let Some((program, args)) = launch.command.split_first() else {
        return Err(agent_client_protocol::Error::invalid_params().data("no agent command is set"));
    };
    // The agent never inherits the person's graph credentials (spec 013): OmniGraph's own
    // variables in Studio's environment are removed for its process, unless the agent's
    // settings set them. The process API can add variables but not remove them, so the agent
    // starts through `env -u`, or on Windows through Studio's own launcher. Model providers'
    // variables (AWS, Azure, Google) stay: the agent may need them for its model.
    let removed = credential_variables(
        std::env::vars_os().map(|(name, _)| name),
        &launch.env,
        cfg!(windows),
    );
    let (program, args) = without_variables(&removed, program, args).map_err(|error| {
        agent_client_protocol::Error::internal_error()
            .data(format!("Studio could not find its own executable: {error}"))
    })?;
    let config = AcpAgentConfig::new(&program)
        .args(args.iter().cloned())
        .envs(launch.env.clone());
    // Only stderr is kept, bounded, and only to show why the agent stopped. Stdin and stdout
    // carry prompts and rows and are never kept.
    let agent = AcpAgent::new(config).with_debug(move |line, direction| {
        if direction != LineDirection::Stderr {
            return;
        }
        if let Ok(mut lines) = stderr.lock() {
            // Cut by bytes, the budget's unit, on a character: a long line cut by characters
            // could still be over the budget and push itself out.
            let mut end = line.len().min(STDERR_BYTES);
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            lines.push_back(line[..end].to_owned());
            while lines.len() > STDERR_LINES
                || lines.iter().map(String::len).sum::<usize>() > STDERR_BYTES
            {
                lines.pop_front();
            }
        }
    });

    let updates = routes.clone();
    let permissions = routes.clone();
    // Known once the agent answers `initialize`, before any session or permission request.
    let adapter: Arc<Mutex<tools::Adapter>> = Arc::default();
    let asked_by = adapter.clone();
    let noted_by = adapter.clone();
    agent_client_protocol::Client
        .builder()
        .name("omnigraph-studio")
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                let adapter = noted_by.lock().map(|adapter| *adapter).unwrap_or_default();
                if let Ok(mut routes) = updates.lock() {
                    routes.note_tool_call(adapter, &notification.update);
                    routes.update(notification.session_id, notification.update);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, connection| {
                let adapter = asked_by.lock().map(|adapter| *adapter).unwrap_or_default();
                let call = &request.tool_call;
                let studio = tools::StudioTool::of(
                    adapter,
                    call.meta.as_ref(),
                    call.fields.raw_input.as_ref(),
                )
                .or_else(|| {
                    let routes = permissions.lock().ok()?;
                    routes
                        .studio_calls
                        .get(&call.tool_call_id.to_string())
                        .copied()
                });
                if studio.is_some() {
                    // Studio's reads are read-only and its writes go through Studio's own
                    // approval setting, so the agent's prompt for them is answered at once.
                    let allow = request
                        .options
                        .iter()
                        .find(|option| option.kind == PermissionOptionKind::AllowOnce)
                        .or_else(|| request.options.first());
                    let outcome = match allow {
                        Some(option) => RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(option.option_id.clone()),
                        ),
                        None => RequestPermissionOutcome::Cancelled,
                    };
                    return responder.respond(RequestPermissionResponse::new(outcome));
                }
                let events = permissions
                    .lock()
                    .ok()
                    .and_then(|routes| routes.sessions.get(&request.session_id).cloned());
                let (reply, decision) = oneshot::channel();
                if let Some(events) = events {
                    events
                        .unbounded_send(HostEvent::Permission { request, reply })
                        .ok();
                }
                // No conversation to ask: the dropped reply answers "cancelled".
                connection.spawn(async move {
                    let outcome = decision
                        .await
                        .unwrap_or(RequestPermissionOutcome::Cancelled);
                    responder.respond(RequestPermissionResponse::new(outcome))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |connection: ConnectionTo<Agent>| {
            let initialized = connection
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_info(Implementation::new(
                            "omnigraph-studio",
                            env!("CARGO_PKG_VERSION"),
                        ))
                        // On/off options come as switches rather than On/Off menus.
                        .client_capabilities(
                            ClientCapabilities::new().session(
                                ClientSessionCapabilities::new().config_options(
                                    SessionConfigOptionsCapabilities::new()
                                        .boolean(BooleanConfigOptionCapabilities::new()),
                                ),
                            ),
                        ),
                )
                .block_task()
                .await?;
            let capabilities = &initialized.agent_capabilities;
            let sessions = &capabilities.session_capabilities;
            let adapter = {
                let named = initialized
                    .agent_info
                    .as_ref()
                    .map_or(tools::Adapter::Other, |info| {
                        tools::Adapter::named(&info.name)
                    });
                if let Ok(mut shared) = adapter.lock() {
                    *shared = named;
                }
                named
            };
            let agent = AgentInfo {
                name: initialized
                    .agent_info
                    .clone()
                    .map(|info| info.title.unwrap_or(info.name))
                    .unwrap_or_else(|| "Agent".to_owned()),
                list: sessions.list.is_some(),
                delete: sessions.delete.is_some(),
            };
            let can = Can {
                load: capabilities.load_session,
                resume: sessions.resume.is_some(),
                close: sessions.close.is_some(),
            };

            // An agent that exits cleanly ends the transport, not this loop: its end is watched
            // for too, so every conversation hears that the agent stopped.
            let mut closed = std::pin::pin!(connection.incoming_closed());
            loop {
                let next = std::pin::pin!(commands.next());
                let command = match futures::future::select(next, closed.as_mut()).await {
                    futures::future::Either::Left((Some(command), _)) => command,
                    futures::future::Either::Left((None, _)) => break,
                    futures::future::Either::Right(((), _)) => return Ok(()),
                };
                match command {
                    ConnectionCommand::Open {
                        launch,
                        events,
                        commands,
                    } => {
                        // Already in `opening`, from `AgentConnection::open`.
                        events.unbounded_send(HostEvent::Adapter(adapter)).ok();
                        let session = open_session(
                            connection.clone(),
                            launch,
                            events,
                            commands,
                            (routes.clone(), opening.clone()),
                            agent.name.clone(),
                            can,
                        );
                        connection.spawn(session)?;
                    }
                    ConnectionCommand::List { cwd, reply } => {
                        let connection_for_list = connection.clone();
                        let can_list = agent.list;
                        connection.spawn(async move {
                            let result = if can_list {
                                list_sessions(&connection_for_list, cwd).await
                            } else {
                                Err("The agent cannot list its conversations.".to_owned())
                            };
                            reply.send(result).ok();
                            Ok(())
                        })?;
                    }
                    ConnectionCommand::Delete { session, reply } => {
                        let connection_for_delete = connection.clone();
                        let can_delete = agent.delete;
                        connection.spawn(async move {
                            let result = if can_delete {
                                connection_for_delete
                                    .send_request(DeleteSessionRequest::new(SessionId::new(
                                        session,
                                    )))
                                    .block_task()
                                    .await
                                    .map(|_| ())
                                    .map_err(|error| error.to_string())
                            } else {
                                Err("The agent cannot delete its conversations.".to_owned())
                            };
                            reply.send(result).ok();
                            Ok(())
                        })?;
                    }
                }
            }
            Ok(())
        })
        .await
}

/// What the agent can do with an earlier session.
#[derive(Debug, Clone, Copy)]
struct Can {
    load: bool,
    resume: bool,
    close: bool,
}

/// Opens one conversation and serves it until its panel lets go. A panel can let go while the
/// agent opens the session (a restart while starting does), and its tab may already have asked
/// for the same session again: the agent is asked to cancel the open, its answer is waited for,
/// and the conversation frees what it opened; only then does the tab's new conversation open the
/// session. A conversation never takes a route another one holds.
async fn open_session(
    connection: ConnectionTo<Agent>,
    launch: SessionLaunch,
    events: mpsc::UnboundedSender<HostEvent>,
    mut commands: mpsc::UnboundedReceiver<HostCommand>,
    (routes, opening): (SharedRoutes, Opening),
    agent_name: String,
    can: Can,
) -> Result<(), agent_client_protocol::Error> {
    let opened_or_failed = |opening: &Opening| {
        if let Ok(mut opening) = opening.lock() {
            opening.retain(|waiting| !waiting.same_receiver(&events));
        }
    };
    let mcp =
        McpServerStdio::new("omnigraph", launch.mcp_command.clone()).args(launch.mcp_args.clone());
    let mcp_servers = vec![McpServer::Stdio(mcp)];
    // What the panel sends while the session opens, for the session once it is open.
    let mut held = Vec::new();
    // Come back to the earlier conversation when the agent can load it (with its messages) or
    // resume it (without); one it no longer knows starts anew.
    let mut opened = None;
    let mut config_options = Vec::new();
    if let Some(id) = launch.resume.clone()
        && (can.load || can.resume)
    {
        let id = SessionId::new(id);
        // Routed before it is asked for: a load's replay arrives as updates while it runs. A
        // session another open conversation still listens to is not opened a second time, nor
        // one for a panel that let go; one a conversation that let go still holds is opened
        // once that conversation has freed it.
        if !route(&routes, &id, &events, true).await {
            opened_or_failed(&opening);
            // A panel that let go hears nothing.
            events
                .unbounded_send(HostEvent::Exited {
                    message: "This conversation is open in another tab.".to_owned(),
                    stderr: String::new(),
                    failed: false,
                })
                .ok();
            return Ok(());
        }
        if can.load {
            let request = connection.send_request(
                LoadSessionRequest::new(id.clone(), launch.cwd.clone())
                    .mcp_servers(mcp_servers.clone()),
            );
            let loaded = open_request(&connection, request, &events, &mut commands, &mut held);
            if let Ok(loaded) = loaded.await {
                opened = Some((id.clone(), Opened::Loaded));
                config_options = loaded.config_options.unwrap_or_default();
            }
        } else {
            let request = connection.send_request(
                ResumeSessionRequest::new(id.clone(), launch.cwd.clone())
                    .mcp_servers(mcp_servers.clone()),
            );
            let resumed = open_request(&connection, request, &events, &mut commands, &mut held);
            if let Ok(resumed) = resumed.await {
                opened = Some((id.clone(), Opened::Resumed));
                config_options = resumed.config_options.unwrap_or_default();
            }
        }
        if opened.is_none()
            && let Ok(mut routes) = routes.lock()
        {
            routes.remove(&id, &events);
        }
    }
    // A new session is not opened for a panel that let go.
    if opened.is_none() && events.is_closed() {
        opened_or_failed(&opening);
        return Ok(());
    }
    let (session_id, opened) = match opened {
        Some(opened) => opened,
        None => {
            let request = connection
                .send_request(NewSessionRequest::new(launch.cwd.clone()).mcp_servers(mcp_servers));
            let created = open_request(&connection, request, &events, &mut commands, &mut held);
            match created.await {
                Ok(created) => {
                    config_options = created.config_options.unwrap_or_default();
                    (created.session_id, Opened::New)
                }
                Err(error) => {
                    opened_or_failed(&opening);
                    events
                        .unbounded_send(HostEvent::Exited {
                            message: format!("The agent could not open a conversation: {error}"),
                            stderr: String::new(),
                            failed: true,
                        })
                        .ok();
                    return Ok(());
                }
            }
        }
    };
    // A conversation whose panel let go meanwhile routes what the agent opened for it too, and
    // frees it below; one waiting for the session opens it after that.
    let routed = route(&routes, &session_id, &events, false).await;
    opened_or_failed(&opening);
    if !routed {
        events
            .unbounded_send(HostEvent::Exited {
                message: "This conversation is open in another tab.".to_owned(),
                stderr: String::new(),
                failed: false,
            })
            .ok();
        return Ok(());
    }
    if events.is_closed() {
        let_go(&connection, &routes, session_id, &events, false, can.close).await;
        return Ok(());
    }
    events
        .unbounded_send(HostEvent::ConfigOptions(config_options))
        .ok();
    events
        .unbounded_send(HostEvent::Ready {
            agent: agent_name,
            session: session_id.0.to_string(),
            opened,
        })
        .ok();

    // Turns sent and not ended: one still running when the panel lets go is stopped. The
    // agent's process serves other conversations, so it would go on spending and running tools
    // with nobody watching.
    let turns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut commands = futures::stream::iter(held).chain(commands);
    while let Some(command) = commands.next().await {
        match command {
            HostCommand::Prompt(text) => {
                let connection_for_turn = connection.clone();
                let session_id = session_id.clone();
                let events = events.clone();
                let turns = turns.clone();
                turns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                connection.spawn(async move {
                    let response = connection_for_turn
                        .send_request(PromptRequest::new(
                            session_id,
                            vec![ContentBlock::Text(TextContent::new(text))],
                        ))
                        .block_task()
                        .await;
                    let stop = response
                        .map(|response| response.stop_reason)
                        .map_err(|error| error.to_string());
                    turns.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    events.unbounded_send(HostEvent::TurnEnded { stop }).ok();
                    Ok(())
                })?;
            }
            HostCommand::Cancel => {
                connection.send_notification(CancelNotification::new(session_id.clone()))?;
            }
            HostCommand::SetConfigOption { id, value } => {
                let connection_for_set = connection.clone();
                let session_id = session_id.clone();
                let events = events.clone();
                connection.spawn(async move {
                    let response = connection_for_set
                        .send_request(SetSessionConfigOptionRequest::new(
                            session_id,
                            SessionConfigId::new(id),
                            value,
                        ))
                        .block_task()
                        .await;
                    let event = match response {
                        Ok(response) => HostEvent::ConfigOptions(response.config_options),
                        Err(error) => HostEvent::ConfigOptionFailed {
                            message: error.to_string(),
                        },
                    };
                    events.unbounded_send(event).ok();
                    Ok(())
                })?;
            }
        }
    }
    let running = turns.load(std::sync::atomic::Ordering::SeqCst) > 0;
    let_go(
        &connection,
        &routes,
        session_id,
        &events,
        running,
        can.close,
    )
    .await;
    Ok(())
}

/// Routes `session` to `events`, waiting while a conversation whose panel let go still holds
/// it. False when another conversation still listens to it, or, `unless_let_go`, once this
/// conversation's own panel let go.
async fn route(
    routes: &SharedRoutes,
    session: &SessionId,
    events: &mpsc::UnboundedSender<HostEvent>,
    unless_let_go: bool,
) -> bool {
    loop {
        let claim = {
            let Ok(mut routes) = routes.lock() else {
                return false;
            };
            if unless_let_go && events.is_closed() {
                return false;
            }
            routes.claim(session.clone(), events.clone())
        };
        match claim {
            Claim::Routed => return true,
            Claim::Held => return false,
            Claim::Wait(freed) => {
                freed.await.ok();
            }
        }
    }
}

/// The agent's answer to `request`, which opens a session (a new one, a load or a resume),
/// keeping in `held` what the panel sends meanwhile. When the panel lets go first, the agent is
/// asked to cancel the request and its answer is still waited for: a load replays the
/// conversation before it answers, so once the answer is in, nothing of that replay can reach a
/// conversation that opens the session next. `events` is closed then, so every later step sees
/// that the panel let go.
async fn open_request<T>(
    connection: &ConnectionTo<Agent>,
    request: SentRequest<T>,
    events: &mpsc::UnboundedSender<HostEvent>,
    commands: &mut mpsc::UnboundedReceiver<HostCommand>,
    held: &mut Vec<HostCommand>,
) -> Result<T, agent_client_protocol::Error> {
    let id = request.id().clone();
    let cancel = || {
        // Dropping the request would send the same `$/cancel_request`, but would drop its
        // answer too, the only sign that the agent is done with it.
        connection.send_cancel_request(id).ok();
        events.close_channel();
    };
    until_answered(request.block_task(), cancel, commands, held).await
}

/// Waits for `answer`, reading `commands` into `held` meanwhile. When they end first (the panel
/// let go), `cancel` runs and `answer` is still waited for.
async fn until_answered<T>(
    answer: impl Future<Output = T>,
    cancel: impl FnOnce(),
    commands: &mut mpsc::UnboundedReceiver<HostCommand>,
    held: &mut Vec<HostCommand>,
) -> T {
    let answer = std::pin::pin!(answer);
    let let_go = std::pin::pin!(async {
        while let Some(command) = commands.next().await {
            held.push(command);
        }
    });
    match futures::future::select(answer, let_go).await {
        futures::future::Either::Left((answer, _)) => answer,
        futures::future::Either::Right(((), answer)) => {
            cancel();
            answer.await
        }
    }
}

/// The panel of `events`' conversation let go of `session`: unless the session is no longer its
/// own, the agent stops its turn (when one is `running`) and may free it. Both are sent while
/// the routes are held, and a conversation waiting for the session is woken with them, so it
/// sends its request after them, never before.
async fn let_go(
    connection: &ConnectionTo<Agent>,
    routes: &SharedRoutes,
    session: SessionId,
    events: &mpsc::UnboundedSender<HostEvent>,
    running: bool,
    can_close: bool,
) {
    let close = routes.lock().ok().and_then(|mut routes| {
        if !routes.remove(&session, events) {
            return None;
        }
        if running {
            connection
                .send_notification(CancelNotification::new(session.clone()))
                .ok();
        }
        can_close.then(|| connection.send_request(CloseSessionRequest::new(session)))
    });
    if let Some(close) = close {
        close.block_task().await.ok();
    }
}

/// Every page of the agent's sessions in `cwd`, up to a bound.
async fn list_sessions(
    connection: &ConnectionTo<Agent>,
    cwd: PathBuf,
) -> Result<Vec<SessionInfo>, String> {
    let mut sessions = Vec::new();
    let mut cursor = None;
    for _ in 0..LIST_PAGES {
        let page = connection
            .send_request(ListSessionsRequest::new().cwd(cwd.clone()).cursor(cursor))
            .block_task()
            .await
            .map_err(|error| error.to_string())?;
        sessions.extend(page.sessions);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ContentChunk, TextContent};

    /// Codex asks permission for an MCP call by its id alone: the call is Studio's tool by what
    /// its first update said.
    #[test]
    fn a_tool_call_is_remembered_as_studios_by_its_first_update() {
        use agent_client_protocol::schema::v1::ToolCall;
        let mut routes = Routes::default();
        let call = |id: &str, server: &str| {
            SessionUpdate::ToolCall(ToolCall::new(id.to_owned(), "Call").raw_input(
                serde_json::json!({ "server": server, "tool": "omnigraph_run", "arguments": {} }),
            ))
        };
        routes.note_tool_call(tools::Adapter::Codex, &call("t1", "omnigraph"));
        routes.note_tool_call(tools::Adapter::Codex, &call("t2", "elsewhere"));
        routes.note_tool_call(tools::Adapter::Other, &call("t3", "omnigraph"));
        assert_eq!(routes.studio_calls.get("t1"), Some(&tools::StudioTool::Run));
        assert!(!routes.studio_calls.contains_key("t2"));
        assert!(
            !routes.studio_calls.contains_key("t3"),
            "only by its adapter's word"
        );
    }

    fn chunk(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        ))))
    }

    /// An agent that cannot start (no such program, as with no `npx` on an app's PATH) tells
    /// the conversation asked for before it read the request, which otherwise waits for ever.
    #[test]
    fn a_conversation_asked_for_before_the_agent_fails_hears_why() {
        let (events, mut heard) = mpsc::unbounded();
        let (commands, receiver) = mpsc::unbounded();
        let (_host, host_commands) = mpsc::unbounded();
        let opening = Opening::default();
        // As `AgentConnection::open` does.
        opening.lock().unwrap().push(events.clone());
        commands
            .unbounded_send(ConnectionCommand::Open {
                launch: SessionLaunch {
                    cwd: std::env::temp_dir(),
                    mcp_command: "studio".into(),
                    mcp_args: Vec::new(),
                    resume: None,
                },
                events,
                commands: host_commands,
            })
            .unwrap();
        let launch = AgentLaunch {
            command: vec!["/nonexistent/studio-test-agent".into()],
            env: BTreeMap::new(),
        };
        smol::block_on(run(launch, receiver, opening));
        let mut exited = false;
        while let Ok(event) = heard.try_recv() {
            exited |= matches!(event, HostEvent::Exited { failed: true, .. });
        }
        assert!(exited, "the conversation is told the agent stopped");
    }

    #[test]
    fn updates_go_to_their_session_and_early_ones_wait() {
        let mut routes = Routes::default();
        let (a_tx, mut a_rx) = mpsc::unbounded();
        let (b_tx, mut b_rx) = mpsc::unbounded();
        routes.add(SessionId::new("a"), a_tx);
        routes.update(SessionId::new("a"), chunk("for a"));
        // Before "b" is known its update waits, then arrives first.
        routes.update(SessionId::new("b"), chunk("early for b"));
        routes.add(SessionId::new("b"), b_tx);
        routes.update(SessionId::new("b"), chunk("for b"));
        let text = |event: Option<HostEvent>| match event {
            Some(HostEvent::Update(SessionUpdate::AgentMessageChunk(chunk))) => match chunk.content
            {
                ContentBlock::Text(text) => text.text,
                _ => String::new(),
            },
            _ => String::new(),
        };
        assert_eq!(text(a_rx.try_recv().ok()), "for a");
        assert!(a_rx.try_recv().is_err(), "a hears nothing of b");
        assert_eq!(text(b_rx.try_recv().ok()), "early for b");
        assert_eq!(text(b_rx.try_recv().ok()), "for b");
        // A session nobody opens keeps a bounded number of updates.
        for _ in 0..EARLY_UPDATES + 10 {
            routes.update(SessionId::new("c"), chunk("x"));
        }
        assert_eq!(routes.early[&SessionId::new("c")].len(), EARLY_UPDATES);
        // And only so many such sessions keep any.
        for n in 0..EARLY_SESSIONS * 2 {
            routes.update(SessionId::new(format!("stray-{n}")), chunk("x"));
        }
        assert!(routes.early.len() <= EARLY_SESSIONS);

        // The session reopened by another conversation keeps its route when the first one
        // lets go; its own conversation removes it.
        let (again_tx, mut again_rx) = mpsc::unbounded();
        let (first_tx, _first_rx) = mpsc::unbounded();
        routes.add(SessionId::new("b"), again_tx.clone());
        assert!(!routes.remove(&SessionId::new("b"), &first_tx));
        routes.update(SessionId::new("b"), chunk("still routed"));
        assert_eq!(text(again_rx.try_recv().ok()), "still routed");
        assert!(routes.remove(&SessionId::new("b"), &again_tx));
        assert!(!routes.sessions.contains_key(&SessionId::new("b")));

        // An update for a session its conversation closed is dropped, not kept as early, until
        // the session opens again.
        routes.early.clear();
        routes.update(SessionId::new("b"), chunk("late"));
        assert!(routes.early.is_empty());
        let (back_tx, mut back_rx) = mpsc::unbounded();
        routes.add(SessionId::new("b"), back_tx.clone());
        routes.update(SessionId::new("b"), chunk("again"));
        assert_eq!(text(back_rx.try_recv().ok()), "again");

        // A second conversation asking for a session another still listens to is refused, and
        // takes none of its traffic. Once that one lets go, the next one waits for it to free
        // the session, then takes it.
        let (second_tx, mut second_rx) = mpsc::unbounded();
        assert!(matches!(
            routes.claim(SessionId::new("b"), second_tx.clone()),
            Claim::Held
        ));
        routes.update(SessionId::new("b"), chunk("still the first's"));
        assert_eq!(text(back_rx.try_recv().ok()), "still the first's");
        assert!(second_rx.try_recv().is_err());
        drop(back_rx);
        let Claim::Wait(mut freed) = routes.claim(SessionId::new("b"), second_tx.clone()) else {
            panic!("a session its conversation let go of is not taken before it is freed");
        };
        assert!(matches!(freed.try_recv(), Ok(None)));
        assert!(routes.remove(&SessionId::new("b"), &back_tx));
        assert!(
            freed.try_recv().is_err(),
            "freeing it wakes the waiting one"
        );
        assert!(matches!(
            routes.claim(SessionId::new("b"), second_tx),
            Claim::Routed
        ));
        routes.update(SessionId::new("b"), chunk("the second's"));
        assert_eq!(text(second_rx.try_recv().ok()), "the second's");
    }

    /// A tab restarted while the agent loads its session asks for the session again. Its new
    /// conversation waits while the one its panel let go holds the session, so the two loads
    /// never run at once and the rest of the first load's replay reaches neither. Once the
    /// first load is answered and its conversation has freed the session, the restarted one
    /// opens it.
    #[test]
    fn a_conversation_let_go_while_loading_leaves_the_session_to_its_restarted_tab() {
        use std::task::{Context, Poll};
        let routes = SharedRoutes::default();
        let session = SessionId::new("s");
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        let (first_tx, first_rx) = mpsc::unbounded();
        assert!(smol::block_on(route(&routes, &session, &first_tx, true)));
        // The restart: the panel lets go of the first conversation and asks for the session
        // again.
        drop(first_rx);
        let (restarted_tx, mut restarted_rx) = mpsc::unbounded();
        let mut restarted = std::pin::pin!(route(&routes, &session, &restarted_tx, true));
        assert!(restarted.as_mut().poll(&mut cx).is_pending());
        routes
            .lock()
            .unwrap()
            .update(session.clone(), chunk("the first load's"));
        assert!(restarted.as_mut().poll(&mut cx).is_pending());
        // The first load is answered: its conversation routes the session, as `open_session`
        // does, and frees it.
        assert!(smol::block_on(route(&routes, &session, &first_tx, false)));
        assert!(routes.lock().unwrap().remove(&session, &first_tx));
        assert!(matches!(
            restarted.as_mut().poll(&mut cx),
            Poll::Ready(true)
        ));
        routes
            .lock()
            .unwrap()
            .update(session.clone(), chunk("the restarted tab's"));
        assert!(matches!(restarted_rx.try_recv(), Ok(HostEvent::Update(_))));
        assert!(
            restarted_rx.try_recv().is_err(),
            "nothing of the first load"
        );
        assert!(routes.lock().unwrap().remove(&session, &restarted_tx));

        // A conversation whose own panel lets go while it waits opens nothing.
        let (first_tx, first_rx) = mpsc::unbounded();
        assert!(smol::block_on(route(&routes, &session, &first_tx, true)));
        drop(first_rx);
        let (again_tx, again_rx) = mpsc::unbounded();
        let mut again = std::pin::pin!(route(&routes, &session, &again_tx, true));
        assert!(again.as_mut().poll(&mut cx).is_pending());
        drop(again_rx);
        assert!(routes.lock().unwrap().remove(&session, &first_tx));
        assert!(matches!(again.as_mut().poll(&mut cx), Poll::Ready(false)));
        assert!(!routes.lock().unwrap().sessions.contains_key(&session));

        // A session the agent created for a conversation whose panel let go meanwhile is
        // routed to it and freed at once.
        let (gone_tx, gone_rx) = mpsc::unbounded();
        drop(gone_rx);
        let created = SessionId::new("created");
        assert!(smol::block_on(route(&routes, &created, &gone_tx, false)));
        assert!(routes.lock().unwrap().remove(&created, &gone_tx));
    }

    /// The panel lets go while the agent opens a session: the agent is asked to cancel, and its
    /// answer is still waited for, since a load's replay ends only with it. What the panel sent
    /// before is kept for the session.
    #[test]
    fn a_panel_letting_go_cancels_the_open_and_waits_for_its_answer() {
        let (panel, mut commands) = mpsc::unbounded();
        panel.unbounded_send(HostCommand::Cancel).unwrap();
        drop(panel);
        let (agent, answer) = oneshot::channel();
        let mut held = Vec::new();
        // The agent answers only once it is asked to cancel.
        let answered = smol::block_on(until_answered(
            answer,
            move || agent.send("cancelled").unwrap(),
            &mut commands,
            &mut held,
        ));
        assert_eq!(answered, Ok("cancelled"));
        assert!(matches!(held.as_slice(), [HostCommand::Cancel]));

        // Answered before the panel lets go: nothing is cancelled, and what the panel sends
        // next stays in line for the session.
        let (panel, mut commands) = mpsc::unbounded();
        let mut held = Vec::new();
        let answered = smol::block_on(until_answered(
            std::future::ready("loaded"),
            || panic!("an answered open is not cancelled"),
            &mut commands,
            &mut held,
        ));
        assert_eq!(answered, "loaded");
        assert!(held.is_empty());
        panel.unbounded_send(HostCommand::Cancel).unwrap();
        assert!(matches!(commands.try_recv(), Ok(HostCommand::Cancel)));
    }
}

/// The command that runs `program` with `args` and none of `removed` in its environment: as it
/// is when nothing is removed, else through `env -u`. `env` reads a word holding `=` as an
/// assignment, so such a program runs through `sh`, which takes it as its own argument. Windows
/// has no `env`, so there it runs through Studio's own executable as its launcher (`launch.rs`).
fn without_variables(
    removed: &[String],
    program: &str,
    args: &[String],
) -> std::io::Result<(PathBuf, Vec<String>)> {
    if removed.is_empty() {
        return Ok((program.into(), args.to_vec()));
    }
    if cfg!(windows) {
        return Ok((
            std::env::current_exe()?,
            crate::launch::arguments(removed, program, args),
        ));
    }
    let mut wrapped: Vec<String> = removed
        .iter()
        .flat_map(|name| ["-u".to_owned(), name.clone()])
        .collect();
    if program.contains('=') {
        wrapped.extend(["/bin/sh", "-c", "exec \"$0\" \"$@\""].map(str::to_owned));
    }
    wrapped.push(program.to_owned());
    wrapped.extend(args.iter().cloned());
    Ok(("/usr/bin/env".into(), wrapped))
}

/// OmniGraph's variables among `names`, every `OMNIGRAPH_*` one, except those the agent's own
/// settings set: tokens for other servers (`OMNIGRAPH_MR_BEARER_TOKEN`), a local server's
/// token list and the CLI's addressing alike. Windows reads a variable by its name in any case,
/// so there `fold_case` matches names in any ASCII case: `omnigraph_profile` is the profile the
/// CLI reads.
fn credential_variables(
    names: impl Iterator<Item = std::ffi::OsString>,
    configured: &std::collections::BTreeMap<String, String>,
    fold_case: bool,
) -> Vec<String> {
    let same = |a: &str, b: &str| {
        if fold_case {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    let mut found: Vec<String> = names
        .filter_map(|name| name.into_string().ok())
        .filter(|name| is_omnigraph_variable(name, fold_case))
        .filter(|name| !configured.keys().any(|key| same(key, name)))
        .collect();
    found.sort();
    found
}

/// Whether a variable is one of OmniGraph's (`OMNIGRAPH_…`), its name in any ASCII case when
/// `fold_case`, as Windows reads names.
pub(crate) fn is_omnigraph_variable(name: &str, fold_case: bool) -> bool {
    if fold_case {
        name.to_ascii_uppercase().starts_with("OMNIGRAPH_")
    } else {
        name.starts_with("OMNIGRAPH_")
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    /// A program path holding `=` still runs, with the variables removed: `env` would read it
    /// as an assignment.
    #[cfg(unix)]
    #[test]
    fn a_program_whose_path_holds_an_equals_sign_runs_without_the_variables() {
        let dir = std::env::temp_dir().join(format!("studio-agent=dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("agent");
        std::fs::write(
            &program,
            "#!/bin/sh\necho \"ran $1 ${STUDIO_TEST_GONE:-unset}\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (command, args) = without_variables(
            &["STUDIO_TEST_GONE".to_owned()],
            &program.to_string_lossy(),
            &["one".to_owned()],
        )
        .unwrap();
        let output = std::process::Command::new(command)
            .args(args)
            .env("STUDIO_TEST_GONE", "secret")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "ran one unset\n");
        assert_eq!(
            without_variables(&[], "npx", &["x".into()]).unwrap(),
            ("npx".into(), vec!["x".to_owned()])
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Set for this test binary when it runs as the launcher or the agent.
    const STAND_IN: &str = "STUDIO_TEST_STAND_IN";
    /// The test that is the launcher or the agent when this test binary runs as one.
    const STAND_IN_TEST: &str = "host::credential_tests::stands_in_for_studio_or_the_agent";
    /// Where the stand-in agent's own arguments start.
    const AGENT: &str = "studio-test-agent";

    /// What `the_launcher_starts_the_agent_without_the_variables` starts this test binary as:
    /// Studio's launcher, as `main` dispatches it, or the agent, which prints its arguments and
    /// what it inherited and exits with 7. Run as a test, it does nothing.
    #[test]
    fn stands_in_for_studio_or_the_agent() {
        if std::env::var_os(STAND_IN).is_none() {
            return;
        }
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        if let Some(at) = args.iter().position(|arg| arg == crate::LAUNCH_COMMAND) {
            std::process::exit(crate::launch_agent(args[at + 1..].iter().cloned()));
        }
        if let Some(at) = args.iter().position(|arg| arg == AGENT) {
            let given: Vec<String> = args[at + 1..]
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            println!(
                "agent {given:?} gone={:?} kept={:?}",
                std::env::var_os("STUDIO_TEST_GONE"),
                std::env::var_os("STUDIO_TEST_KEPT"),
            );
            std::process::exit(7);
        }
    }

    /// The launcher Windows starts the agent through runs it without the variables, with its
    /// program and every argument as given, and exits as it does. This test binary stands in
    /// for Studio and for the agent, so the platform's command line carries the arguments both
    /// times, as it does for Studio's executable.
    #[test]
    fn the_launcher_starts_the_agent_without_the_variables() {
        let studio = std::env::current_exe().unwrap();
        // libtest's own arguments, which run only the stand-in test.
        let libtest = ["--exact", STAND_IN_TEST, "--nocapture", "--"].map(str::to_owned);
        let given = [
            "one",
            "two words",
            "a \"quoted\" word",
            "",
            "--",
            "trailing\\",
            "100%",
        ]
        .map(str::to_owned);
        let mut agent_args = libtest.to_vec();
        agent_args.push(AGENT.to_owned());
        agent_args.extend(given.iter().cloned());
        let removed = ["STUDIO_TEST_GONE".to_owned()];
        let program = studio.to_string_lossy();
        let launcher = crate::launch::arguments(&removed, &program, &agent_args);
        #[cfg(windows)]
        assert_eq!(
            without_variables(&removed, &program, &agent_args).unwrap(),
            (studio.clone(), launcher.clone()),
            "Windows has no `env`: the agent starts through Studio's launcher"
        );
        let output = std::process::Command::new(&studio)
            .args(&libtest)
            .args(&launcher)
            .env(STAND_IN, "1")
            .env("STUDIO_TEST_GONE", "secret")
            .env("STUDIO_TEST_KEPT", "kept")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let agent = stdout
            .lines()
            .find(|line| line.starts_with("agent "))
            .unwrap_or_else(|| panic!("the agent did not run: {stdout}"));
        assert_eq!(
            agent,
            format!("agent {given:?} gone=None kept=Some(\"kept\")")
        );
        assert_eq!(
            output.status.code(),
            Some(7),
            "the launcher exits as the agent does"
        );
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn the_agent_starts_without_omnigraph_credentials() {
        let names = [
            "PATH",
            "OMNIGRAPH_TOKEN_PERSONAL",
            "OMNIGRAPH_BEARER_TOKEN",
            "OMNIGRAPH_STUDIO_TOKEN",
            "OMNIGRAPH_PROFILE",
            "ANTHROPIC_API_KEY",
            "AWS_PROFILE",
            "OMNIGRAPH_HOME",
            "OMNIGRAPH_MR_BEARER_TOKEN",
            "OMNIGRAPH_SERVER_BEARER_TOKENS_JSON",
        ]
        .map(std::ffi::OsString::from);
        let mut configured = std::collections::BTreeMap::new();
        configured.insert("OMNIGRAPH_HOME".to_owned(), "/x".to_owned());
        assert_eq!(
            credential_variables(names.into_iter(), &configured, false),
            [
                "OMNIGRAPH_BEARER_TOKEN",
                "OMNIGRAPH_MR_BEARER_TOKEN",
                "OMNIGRAPH_PROFILE",
                "OMNIGRAPH_SERVER_BEARER_TOKENS_JSON",
                "OMNIGRAPH_STUDIO_TOKEN",
                "OMNIGRAPH_TOKEN_PERSONAL",
            ]
        );

        // Windows reads a variable by its name in any case, and settings set it in any case.
        let names = [
            "omnigraph_profile",
            "Omnigraph_Token_Work",
            "omnigraph_home",
            "PATH",
        ]
        .map(std::ffi::OsString::from);
        assert_eq!(
            credential_variables(names.clone().into_iter(), &configured, true),
            ["Omnigraph_Token_Work", "omnigraph_profile"]
        );
        assert!(credential_variables(names.into_iter(), &configured, false).is_empty());
    }
}
