//! A scripted ACP agent for testing the agent panel without a model or network (spec 011).
//!
//! It speaks ACP v1 on stdio, starts the MCP server Studio names in `session/new` (Studio's
//! `mcp-proxy`), and on every prompt reads the schema and counts `Person` nodes. When the prompt
//! contains "change", it also proposes retitling the first person and reports the outcome.
//!
//! It keeps each session's transcript in its working directory and supports `session/load`:
//! loading a session replays its messages, so a restarted Studio can come back to it.
//!
//! It offers two session options, a mode and a model (grouped by family), takes
//! `session/set_config_option`, and reports a growing context window and cost after each turn.
//!
//!     cargo build -p agent_panel --example test_agent
//!     # then set the agent command to target/debug/examples/test_agent

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, Cost, Implementation, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, McpServer, McpServerStdio,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectGroup, SessionConfigSelectOption, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, TextContent, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, UsageUpdate,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Stdio};
use serde_json::{Value, json};
use smol::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use smol::process::{Command, Stdio as ProcessStdio};
use smol::stream::StreamExt as _;

const COUNT: &str = "query people() { match { $p: Person } return { count($p) as n } }";
const FIRST: &str =
    "query first() { match { $p: Person } return { $p } order { $p.@id asc } limit 1 }";
const RETITLE: &str = "query retitle($id: String, $title: String) {\n  update Person set { title: $title } where @id = $id\n}";

/// The session's options as the person last set them: `(mode, model)`, kept beside the
/// transcript so a loaded session comes back with its own.
static OPTIONS: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

fn options_path() -> Option<PathBuf> {
    let transcript = TRANSCRIPT.lock().unwrap().clone()?;
    Some(transcript.with_extension("options.json"))
}

fn save_options() {
    let (mode, model) = OPTIONS.lock().unwrap().clone();
    if let Some(path) = options_path() {
        std::fs::write(path, json!({ "mode": mode, "model": model }).to_string()).ok();
    }
}

fn load_options() {
    let saved = options_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_default();
    let field = |name: &str| saved[name].as_str().unwrap_or_default().to_owned();
    *OPTIONS.lock().unwrap() = (field("mode"), field("model"));
}
/// Tokens in the context window and the cost so far.
static USAGE: Mutex<(u64, f64)> = Mutex::new((0, 0.));
const WINDOW: u64 = 200_000;

fn config_options() -> Vec<SessionConfigOption> {
    let (mode, model) = OPTIONS.lock().unwrap().clone();
    let mode = if mode.is_empty() { "ask".into() } else { mode };
    let model = if model.is_empty() {
        "sonnet".into()
    } else {
        model
    };
    vec![
        SessionConfigOption::select(
            "mode",
            "Mode",
            mode,
            vec![
                SessionConfigSelectOption::new("ask", "Ask before changes"),
                SessionConfigSelectOption::new("plan", "Plan only"),
            ],
        )
        .description("When the agent may propose changes")
        .category(SessionConfigOptionCategory::Mode),
        SessionConfigOption::select(
            "model",
            "Model",
            model,
            vec![
                SessionConfigSelectGroup::new(
                    "claude",
                    "Claude",
                    vec![
                        SessionConfigSelectOption::new("opus", "Claude Opus"),
                        SessionConfigSelectOption::new("sonnet", "Claude Sonnet"),
                        SessionConfigSelectOption::new("haiku", "Claude Haiku"),
                    ],
                ),
                SessionConfigSelectGroup::new(
                    "local",
                    "Local",
                    vec![SessionConfigSelectOption::new("scripted", "Scripted")],
                ),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
    ]
}

/// Where the current session's messages are written, one JSON line each.
static TRANSCRIPT: Mutex<Option<PathBuf>> = Mutex::new(None);

fn record(role: &str, text: &str) {
    let Some(path) = TRANSCRIPT.lock().unwrap().clone() else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        writeln!(file, "{}", json!({ "role": role, "text": text })).ok();
    }
}

fn transcript_path(cwd: &std::path::Path, session: &str) -> PathBuf {
    cwd.join(format!("transcript-{session}.jsonl"))
}

fn stdio_server(servers: Vec<McpServer>) -> Option<McpServerStdio> {
    servers.into_iter().find_map(|server| match server {
        McpServer::Stdio(stdio) => Some(stdio),
        _ => None,
    })
}

fn main() -> agent_client_protocol::Result<()> {
    let server: Arc<Mutex<Option<McpServerStdio>>> = Arc::default();
    let for_session = server.clone();
    let for_load = server.clone();
    smol::block_on(
        Agent
            .builder()
            .name("studio-test-agent")
            .on_receive_request(
                async move |request: InitializeRequest, responder, _connection| {
                    responder.respond(
                        InitializeResponse::new(request.protocol_version)
                            .agent_info(Implementation::new("Studio test agent", "0.1.0"))
                            .agent_capabilities(AgentCapabilities::new().load_session(true)),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: NewSessionRequest, responder, _connection| {
                    *for_session.lock().unwrap() = stdio_server(request.mcp_servers);
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    let id = format!("test-{nanos}");
                    *TRANSCRIPT.lock().unwrap() = Some(transcript_path(&request.cwd, &id));
                    *USAGE.lock().unwrap() = (0, 0.);
                    *OPTIONS.lock().unwrap() = Default::default();
                    responder.respond(
                        NewSessionResponse::new(SessionId::new(id))
                            .config_options(config_options()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: LoadSessionRequest,
                            responder,
                            connection: ConnectionTo<Client>| {
                    *for_load.lock().unwrap() = stdio_server(request.mcp_servers);
                    let path = transcript_path(&request.cwd, &request.session_id.0);
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params().data("no such session"),
                        );
                    };
                    // Replay the conversation, then carry on writing it.
                    for line in text.lines() {
                        let Ok(message) = serde_json::from_str::<Value>(line) else {
                            continue;
                        };
                        let content = ContentChunk::new(ContentBlock::Text(TextContent::new(
                            message["text"].as_str().unwrap_or_default(),
                        )));
                        let update = if message["role"] == "user" {
                            SessionUpdate::UserMessageChunk(content)
                        } else {
                            SessionUpdate::AgentMessageChunk(content)
                        };
                        connection
                            .send_notification(SessionNotification::new(
                                request.session_id.clone(),
                                update,
                            ))
                            .ok();
                    }
                    *TRANSCRIPT.lock().unwrap() = Some(path);
                    load_options();
                    responder.respond(LoadSessionResponse::new().config_options(config_options()))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: SetSessionConfigOptionRequest, responder, _connection| {
                    let Some(value) = request.value.as_value_id() else {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params().data("not a value id"),
                        );
                    };
                    let value = value.0.to_string();
                    {
                        let mut options = OPTIONS.lock().unwrap();
                        match request.config_id.0.as_ref() {
                            "mode" => options.0 = value,
                            "model" => options.1 = value,
                            _ => {
                                drop(options);
                                return responder.respond_with_error(
                                    agent_client_protocol::Error::invalid_params()
                                        .data("no such option"),
                                );
                            }
                        }
                    }
                    save_options();
                    responder.respond(SetSessionConfigOptionResponse::new(config_options()))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: PromptRequest, responder, connection: ConnectionTo<Client>| {
                    let server = server.lock().unwrap().clone();
                    let turn_connection = connection.clone();
                    connection.spawn(async move {
                        let stop = match turn(&turn_connection, &request, server).await {
                            Ok(()) => StopReason::EndTurn,
                            Err(error) => {
                                say(
                                    &turn_connection,
                                    &request.session_id,
                                    &format!("Failed: {error}"),
                                );
                                StopReason::EndTurn
                            }
                        };
                        // Each turn fills the window by its schema read and a little more.
                        let (used, cost) = {
                            let mut usage = USAGE.lock().unwrap();
                            usage.0 = (usage.0 + 41_800).min(WINDOW);
                            usage.1 += 0.137;
                            *usage
                        };
                        turn_connection
                            .send_notification(SessionNotification::new(
                                request.session_id.clone(),
                                SessionUpdate::UsageUpdate(
                                    UsageUpdate::new(used, WINDOW).cost(Cost::new(cost, "USD")),
                                ),
                            ))
                            .ok();
                        responder.respond(PromptResponse::new(stop))
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(Stdio::new()),
    )
}

fn say(connection: &ConnectionTo<Client>, session: &SessionId, text: &str) {
    record("agent", text);
    connection
        .send_notification(SessionNotification::new(
            session.clone(),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))),
        ))
        .ok();
}

fn tool_status(
    connection: &ConnectionTo<Client>,
    session: &SessionId,
    id: &str,
    title: &str,
    status: ToolCallStatus,
) {
    let update = if status == ToolCallStatus::InProgress {
        SessionUpdate::ToolCall(ToolCall::new(id.to_owned(), title).status(status))
    } else {
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id.to_owned(),
            ToolCallUpdateFields::new().status(status),
        ))
    };
    connection
        .send_notification(SessionNotification::new(session.clone(), update))
        .ok();
}

async fn turn(
    connection: &ConnectionTo<Client>,
    request: &PromptRequest,
    server: Option<McpServerStdio>,
) -> Result<(), String> {
    let session = &request.session_id;
    let prompt: String = request
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect();
    record("user", &prompt);
    let server = server.ok_or("Studio offered no MCP server")?;
    let mut mcp = Mcp::start(&server).await?;

    tool_status(
        connection,
        session,
        "schema",
        "omnigraph_schema",
        ToolCallStatus::InProgress,
    );
    let (schema, _) = mcp.call("omnigraph_schema", json!({})).await?;
    tool_status(
        connection,
        session,
        "schema",
        "omnigraph_schema",
        ToolCallStatus::Completed,
    );
    let types = schema
        .lines()
        .filter(|line| line.starts_with("node "))
        .count();
    say(
        connection,
        session,
        &format!("The graph has {types} node types. "),
    );

    tool_status(
        connection,
        session,
        "count",
        "omnigraph_query",
        ToolCallStatus::InProgress,
    );
    let (count, error) = mcp
        .call("omnigraph_query", json!({ "query": COUNT }))
        .await?;
    tool_status(
        connection,
        session,
        "count",
        "omnigraph_query",
        if error {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        },
    );
    let n = serde_json::from_str::<Value>(&count)
        .ok()
        .and_then(|value| value["rows"][0]["n"].as_i64());
    say(
        connection,
        session,
        &format!("It holds {} people.", n.unwrap_or(-1)),
    );
    // A reply with markdown, to see it rendered.
    if prompt.contains("markdown") {
        say(
            connection,
            session,
            "\n\n**People by title**, from a quick read:\n\n- `Engineer`: the largest group\n\
             - `Founder`: one per company\n\nTo list the founders:\n\n```\nquery founders() {\n  \
             match { $p: Person  $p.title = \"Founder\" }\n  return { $p.name }\n}\n```",
        );
    }

    if prompt.contains("change") {
        let (first, _) = mcp
            .call("omnigraph_query", json!({ "query": FIRST }))
            .await?;
        let first: Value = serde_json::from_str(&first).map_err(|e| e.to_string())?;
        let row = &first["rows"][0];
        let id = row["p"]["@id"]
            .as_str()
            .or_else(|| row["@id"].as_str())
            .or_else(|| row["p.@id"].as_str())
            .ok_or_else(|| format!("no id in {row}"))?
            .to_owned();
        tool_status(
            connection,
            session,
            "propose",
            "omnigraph_propose_change",
            ToolCallStatus::InProgress,
        );
        let (outcome, error) = mcp
            .call(
                "omnigraph_propose_change",
                json!({
                    "query": RETITLE,
                    "params": { "id": id, "title": "Retitled by the test agent" },
                    "summary": format!("Retitle {id}"),
                }),
            )
            .await?;
        tool_status(
            connection,
            session,
            "propose",
            "omnigraph_propose_change",
            if error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            },
        );
        say(connection, session, &format!("\n\nChange: {outcome}"));
    }
    Ok(())
}

struct Mcp {
    _child: smol::process::Child,
    stdin: smol::process::ChildStdin,
    lines: smol::io::Lines<BufReader<smol::process::ChildStdout>>,
    next_id: i64,
}

impl Mcp {
    async fn start(server: &McpServerStdio) -> Result<Self, String> {
        let mut child = Command::new(&server.command)
            .args(&server.args)
            .stdin(ProcessStdio::piped())
            .stdout(ProcessStdio::piped())
            .spawn()
            .map_err(|e| format!("start {}: {e}", server.command.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut mcp = Self {
            _child: child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 0,
        };
        mcp.request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test-agent", "version": "0" } }),
        )
        .await?;
        mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await?;
        Ok(mcp)
    }

    async fn send(&mut self, message: Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        self.stdin.flush().await.map_err(|e| e.to_string())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        while let Some(line) = self.lines.next().await {
            let line = line.map_err(|e| e.to_string())?;
            let message: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
            if message["id"] == json!(id) {
                if let Some(error) = message.get("error") {
                    return Err(error.to_string());
                }
                return Ok(message["result"].clone());
            }
        }
        Err("the MCP server closed".into())
    }

    /// Calls a tool: its text and whether it is an error.
    async fn call(&mut self, name: &str, arguments: Value) -> Result<(String, bool), String> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        Ok((text, result["isError"].as_bool().unwrap_or(false)))
    }
}
