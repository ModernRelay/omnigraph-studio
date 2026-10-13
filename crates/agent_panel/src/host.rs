//! The ACP host (spec 011): spawns the agent, speaks ACP v1 to it, and turns its traffic into
//! [`HostEvent`]s for the panel. Runs off the main thread; the panel talks to it through
//! channels.
//!
//! ACP request handlers hold the connection's dispatch loop until they return, so a permission
//! request that waits for the person is answered from a task spawned on the connection.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    BooleanConfigOptionCapabilities, CancelNotification, ClientCapabilities,
    ClientSessionCapabilities, ContentBlock, Implementation, InitializeRequest, LoadSessionRequest,
    McpServer, McpServerStdio, NewSessionRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigId, SessionConfigOption, SessionConfigOptionValue,
    SessionConfigOptionsCapabilities, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, StopReason, TextContent,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, ConnectionTo, LineDirection};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};

use crate::tools;

/// Lines of the agent's stderr kept for the "stopped" state.
const STDERR_LINES: usize = 20;
/// Bytes of the agent's stderr kept for the "stopped" state.
const STDERR_BYTES: usize = 4096;

/// How to start the agent.
#[derive(Debug, Clone)]
pub struct Launch {
    /// The program and its arguments.
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The agent's working directory.
    pub cwd: PathBuf,
    /// The stdio MCP server the agent starts: Studio's own executable as `mcp-proxy`.
    pub mcp_command: PathBuf,
    pub mcp_args: Vec<String>,
    /// A session to come back to, when the agent can load one.
    pub resume: Option<String>,
}

pub enum HostEvent {
    /// The session is open: `resumed` when an earlier one was loaded (its messages arrive as
    /// updates before this), else a new one.
    Ready {
        agent: String,
        session: String,
        resumed: bool,
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
    /// The agent stopped or could not start.
    Exited {
        message: String,
        stderr: String,
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

/// Runs one agent session until the command channel closes or the agent exits. Dropping the
/// future kills the agent's process group.
pub async fn run(
    launch: Launch,
    events: mpsc::UnboundedSender<HostEvent>,
    commands: mpsc::UnboundedReceiver<HostCommand>,
) {
    let stderr = Arc::new(Mutex::new(VecDeque::<String>::new()));
    let result = connect(launch, events.clone(), commands, stderr.clone()).await;
    let message = match result {
        Ok(()) => "The agent session ended.".to_owned(),
        Err(error) => format!("The agent stopped: {error}"),
    };
    let stderr = stderr
        .lock()
        .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    events
        .unbounded_send(HostEvent::Exited { message, stderr })
        .ok();
}

async fn connect(
    launch: Launch,
    events: mpsc::UnboundedSender<HostEvent>,
    mut commands: mpsc::UnboundedReceiver<HostCommand>,
    stderr: Arc<Mutex<VecDeque<String>>>,
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

    let updates = events.clone();
    let permissions = events.clone();
    agent_client_protocol::Client
        .builder()
        .name("omnigraph-studio")
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                updates
                    .unbounded_send(HostEvent::Update(notification.update))
                    .ok();
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
                let (reply, decision) = oneshot::channel();
                permissions
                    .unbounded_send(HostEvent::Permission { request, reply })
                    .ok();
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
            let agent_name = initialized
                .agent_info
                .map(|info| info.title.unwrap_or(info.name))
                .unwrap_or_else(|| "Agent".to_owned());

            let mcp = McpServerStdio::new("omnigraph", launch.mcp_command.clone())
                .args(launch.mcp_args.clone());
            // Come back to the earlier conversation when the agent can load it; a session it no
            // longer knows starts a new one.
            let can_load = initialized.agent_capabilities.load_session;
            let mut resumed = None;
            let mut config_options = Vec::new();
            if let Some(id) = launch.resume.clone().filter(|_| can_load) {
                let loaded = connection
                    .send_request(
                        LoadSessionRequest::new(SessionId::new(id.clone()), launch.cwd.clone())
                            .mcp_servers(vec![McpServer::Stdio(mcp.clone())]),
                    )
                    .block_task()
                    .await;
                if let Ok(loaded) = loaded {
                    resumed = Some(SessionId::new(id));
                    config_options = loaded.config_options.unwrap_or_default();
                }
            }
            let session_id = match resumed.clone() {
                Some(id) => id,
                None => {
                    let created = connection
                        .send_request(
                            NewSessionRequest::new(launch.cwd.clone())
                                .mcp_servers(vec![McpServer::Stdio(mcp)]),
                        )
                        .block_task()
                        .await?;
                    config_options = created.config_options.unwrap_or_default();
                    created.session_id
                }
            };
            events
                .unbounded_send(HostEvent::ConfigOptions(config_options))
                .ok();
            events
                .unbounded_send(HostEvent::Ready {
                    agent: agent_name,
                    session: session_id.0.to_string(),
                    resumed: resumed.is_some(),
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
                        connection
                            .send_notification(CancelNotification::new(session_id.clone()))?;
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
            Ok(())
        })
        .await
}
