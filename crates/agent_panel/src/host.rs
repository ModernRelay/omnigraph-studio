//! The ACP host (spec 011): one process per agent, speaking ACP v1, serving every conversation
//! Studio has open with that agent, as Zed keeps one connection per agent server. Each
//! conversation opens a session on the connection and gets its traffic as [`HostEvent`]s, routed
//! by session id; the connection also lists and deletes the agent's sessions when it can.
//! Runs off the main thread; the panels talk to it through channels. The process stops when the
//! last handle to it is dropped.
//!
//! ACP request handlers hold the connection's dispatch loop until they return, so a permission
//! request that waits for the person is answered from a task spawned on the connection.

use std::collections::{BTreeMap, HashMap, VecDeque};
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
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, ConnectionTo, LineDirection};
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
    /// close, when the agent may let the session go.
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
}

impl Routes {
    fn update(&mut self, session: SessionId, update: SessionUpdate) {
        match self.sessions.get(&session) {
            Some(events) => {
                events.unbounded_send(HostEvent::Update(update)).ok();
            }
            None => {
                let early = self.early.entry(session).or_default();
                if early.len() < EARLY_UPDATES {
                    early.push_back(update);
                }
            }
        }
    }

    fn add(&mut self, session: SessionId, events: mpsc::UnboundedSender<HostEvent>) {
        for update in self.early.remove(&session).unwrap_or_default() {
            events.unbounded_send(HostEvent::Update(update)).ok();
        }
        self.sessions.insert(session, events);
    }
}

type SharedRoutes = Arc<Mutex<Routes>>;
/// Conversations asked to open but not yet routed: they hear an exit too.
type Opening = Arc<Mutex<Vec<mpsc::UnboundedSender<HostEvent>>>>;

/// Runs one agent until every handle to it is dropped or it exits; then every open conversation
/// hears why. Dropping the future kills the agent's process group.
pub async fn run(launch: AgentLaunch, commands: mpsc::UnboundedReceiver<ConnectionCommand>) {
    let stderr = Arc::new(Mutex::new(VecDeque::<String>::new()));
    let routes = SharedRoutes::default();
    let opening = Opening::default();
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
    let config = AcpAgentConfig::new(program)
        .args(args.iter().cloned())
        .envs(launch.env.clone());
    // Only stderr is kept, bounded, and only to show why the agent stopped. Stdin and stdout
    // carry prompts and rows and are never kept.
    let agent = AcpAgent::new(config).with_debug(move |line, direction| {
        if direction != LineDirection::Stderr {
            return;
        }
        if let Ok(mut lines) = stderr.lock() {
            lines.push_back(line.chars().take(STDERR_BYTES).collect());
            while lines.len() > STDERR_LINES
                || lines.iter().map(String::len).sum::<usize>() > STDERR_BYTES
            {
                lines.pop_front();
            }
        }
    });

    let updates = routes.clone();
    let permissions = routes.clone();
    agent_client_protocol::Client
        .builder()
        .name("omnigraph-studio")
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                if let Ok(mut routes) = updates.lock() {
                    routes.update(notification.session_id, notification.update);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, connection| {
                let title = request.tool_call.fields.title.clone().unwrap_or_default();
                if tools::is_studio_tool(&title) {
                    // Studio's reads are read-only and its writes wait for Studio's own
                    // approval, so the agent's prompt for them is answered at once.
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

            while let Some(command) = commands.next().await {
                match command {
                    ConnectionCommand::Open {
                        launch,
                        events,
                        commands,
                    } => {
                        if let Ok(mut opening) = opening.lock() {
                            opening.push(events.clone());
                        }
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

/// Opens one conversation and serves it until its panel lets go.
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
    // Come back to the earlier conversation when the agent can load it (with its messages) or
    // resume it (without); one it no longer knows starts anew.
    let mut opened = None;
    let mut config_options = Vec::new();
    if let Some(id) = launch.resume.clone() {
        let id = SessionId::new(id);
        if can.load {
            // The replay arrives as updates while the load runs.
            if let Ok(mut routes) = routes.lock() {
                routes.add(id.clone(), events.clone());
            }
            let loaded = connection
                .send_request(
                    LoadSessionRequest::new(id.clone(), launch.cwd.clone())
                        .mcp_servers(mcp_servers.clone()),
                )
                .block_task()
                .await;
            match loaded {
                Ok(loaded) => {
                    opened = Some((id.clone(), Opened::Loaded));
                    config_options = loaded.config_options.unwrap_or_default();
                }
                Err(_) => {
                    if let Ok(mut routes) = routes.lock() {
                        routes.sessions.remove(&id);
                    }
                }
            }
        } else if can.resume {
            let resumed = connection
                .send_request(
                    ResumeSessionRequest::new(id.clone(), launch.cwd.clone())
                        .mcp_servers(mcp_servers.clone()),
                )
                .block_task()
                .await;
            if let Ok(resumed) = resumed {
                opened = Some((id.clone(), Opened::Resumed));
                config_options = resumed.config_options.unwrap_or_default();
            }
        }
    }
    let (session_id, opened) = match opened {
        Some(opened) => opened,
        None => {
            let created = connection
                .send_request(NewSessionRequest::new(launch.cwd.clone()).mcp_servers(mcp_servers))
                .block_task()
                .await;
            match created {
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
    if let Ok(mut routes) = routes.lock() {
        routes.add(session_id.clone(), events.clone());
    }
    opened_or_failed(&opening);
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

    while let Some(command) = commands.next().await {
        match command {
            HostCommand::Prompt(text) => {
                let connection_for_turn = connection.clone();
                let session_id = session_id.clone();
                let events = events.clone();
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
    // The panel let go: the agent may free the session.
    if let Ok(mut routes) = routes.lock() {
        routes.sessions.remove(&session_id);
    }
    if can.close {
        connection
            .send_request(CloseSessionRequest::new(session_id))
            .block_task()
            .await
            .ok();
    }
    Ok(())
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

    fn chunk(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        ))))
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
    }
}
