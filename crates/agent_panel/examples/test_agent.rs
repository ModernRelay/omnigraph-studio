//! A scripted ACP agent for testing the agent panel without a model or network (spec 011).
//!
//! It speaks ACP v1 on stdio, and on every prompt reads the schema and counts `Person` nodes
//! with the conversation's omnigraph CLI, `./omnigraph` in the session's working directory, as an
//! agent's shell runs it (spec 013). When the prompt contains "change" or "cli", it also creates
//! its branch, named in the folder's `AGENTS.md`, retitles a person there, and tries to on the
//! base, which Studio refuses. When it contains "list", it reads three people's names and titles.
//!
//! It reports each command as a tool call titled with the command, as an agent's shell tool
//! does.
//!
//! One process serves every session Studio opens with it. It keeps each session's transcript in
//! the session's working directory and supports `session/load` (replaying the messages),
//! `session/list` (the transcripts in a directory, titled by their first message),
//! `session/delete` and `session/close`.
//!
//! It offers the session options Claude's ACP agent does (a mode, a model grouped by family, an
//! effort level, Fast mode as a switch when the client takes switches, and an agent profile),
//! takes `session/set_config_option`, and reports a growing context window and cost after each
//! turn.
//!
//!     cargo build -p agent_panel --example test_agent
//!     # then set the agent command to target/debug/examples/test_agent

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk, Cost,
    DeleteSessionRequest, DeleteSessionResponse, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionCapabilities, SessionCloseCapabilities, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectGroup,
    SessionConfigSelectOption, SessionDeleteCapabilities, SessionId, SessionInfo,
    SessionListCapabilities, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, TextContent, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, UsageUpdate,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Stdio};
use serde_json::{Map, Value, json};

const COUNT: &str = "query people() {\n  match { $p: Person }\n  return { count($p) as n }\n}";
const PEOPLE: &str =
    "query people() {\n  match { $p: Person }\n  return { $p.name, $p.title }\n  limit 3\n}";
const RETITLE: &str = "query retitle() {\n  update Person set { title: \"Retitled by the test agent\" } where @id = \"person-000000\"\n}";

/// What the agent keeps about one open session. One process serves every session Studio opens
/// with it, as Claude's ACP agent does.
#[derive(Default, Clone)]
struct Session {
    /// Its messages, one JSON line each.
    transcript: PathBuf,
    /// The options as the person last set them, by option id, kept beside the transcript so a
    /// loaded session comes back with its own. Unset options take their defaults.
    options: Map<String, Value>,
    /// Tokens in the context window and the cost so far.
    usage: (u64, f64),
    /// Its working directory, which holds the conversation's CLI and `AGENTS.md`.
    cwd: PathBuf,
}

static SESSIONS: LazyLock<Mutex<HashMap<String, Session>>> = LazyLock::new(Mutex::default);
/// Every transcript the agent knows of, by session id, open or not, kept in a file so a later
/// process can still delete one (`session/delete` names only the session), as a real agent
/// keeps its sessions.
static KNOWN: LazyLock<Mutex<HashMap<String, PathBuf>>> = LazyLock::new(|| {
    let known = std::fs::read_to_string(known_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    Mutex::new(known)
});

fn known_path() -> PathBuf {
    std::env::temp_dir().join("studio-test-agent-sessions.json")
}

fn remember(session: &str, transcript: PathBuf) {
    let mut known = KNOWN.lock().unwrap();
    known.insert(session.to_owned(), transcript);
    std::fs::write(
        known_path(),
        serde_json::to_string(&*known).unwrap_or_default(),
    )
    .ok();
}
/// Whether the client takes on/off options as switches; otherwise Fast mode is an On/Off menu.
static BOOLEANS: AtomicBool = AtomicBool::new(false);
const WINDOW: u64 = 200_000;

fn options_path(transcript: &std::path::Path) -> PathBuf {
    transcript.with_extension("options.json")
}

fn open(session: &str, transcript: PathBuf, cwd: PathBuf) {
    let options = std::fs::read_to_string(options_path(&transcript))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    remember(session, transcript.clone());
    SESSIONS.lock().unwrap().insert(
        session.to_owned(),
        Session {
            transcript,
            options,
            usage: (0, 0.),
            cwd,
        },
    );
}

fn with_session<T>(session: &str, f: impl FnOnce(&mut Session) -> T) -> Option<T> {
    SESSIONS.lock().unwrap().get_mut(session).map(f)
}

fn chosen(options: &Map<String, Value>, id: &str, default: &str) -> String {
    options
        .get(id)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_owned()
}

/// The options Claude's ACP agent offers, in its order: mode, model, effort, Fast mode and the
/// agent profile.
fn config_options(session: &str) -> Vec<SessionConfigOption> {
    let options = with_session(session, |s| s.options.clone()).unwrap_or_default();
    let fast = options
        .get("fast")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let fast_mode = if BOOLEANS.load(Ordering::SeqCst) {
        SessionConfigOption::boolean("fast", "Fast mode", fast)
    } else {
        SessionConfigOption::select(
            "fast",
            "Fast mode",
            if fast { "on" } else { "off" },
            vec![
                SessionConfigSelectOption::new("on", "On"),
                SessionConfigSelectOption::new("off", "Off"),
            ],
        )
    };
    vec![
        SessionConfigOption::select(
            "mode",
            "Mode",
            chosen(&options, "mode", "ask"),
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
            chosen(&options, "model", "sonnet"),
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
        SessionConfigOption::select(
            "effort",
            "Effort",
            chosen(&options, "effort", "high"),
            vec![
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("medium", "Medium"),
                SessionConfigSelectOption::new("high", "High"),
            ],
        )
        .description("How long the model thinks")
        .category(SessionConfigOptionCategory::ThoughtLevel),
        fast_mode
            .description("Faster output from the same model")
            .category(SessionConfigOptionCategory::ModelConfig),
        SessionConfigOption::select(
            "agent",
            "Agent",
            chosen(&options, "agent", "default"),
            vec![
                SessionConfigSelectOption::new("default", "Default"),
                SessionConfigSelectOption::new("reviewer", "Reviewer"),
            ],
        ),
    ]
}

fn record(session: &str, role: &str, text: &str) {
    let Some(path) = with_session(session, |s| s.transcript.clone()) else {
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

/// The sessions whose transcripts are in `cwd`, titled by their first message.
fn list(cwd: &std::path::Path) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(cwd) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_prefix("transcript-")
            .and_then(|rest| rest.strip_suffix(".jsonl"))
        else {
            continue;
        };
        let path = entry.path();
        remember(id, path.clone());
        let title = std::fs::read_to_string(&path).ok().and_then(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|message| message["role"] == "user")
                .and_then(|message| message["text"].as_str().map(str::to_owned))
        });
        let updated = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339());
        sessions.push(
            SessionInfo::new(SessionId::new(id), cwd)
                .title(title)
                .updated_at(updated),
        );
    }
    sessions
}

fn main() -> agent_client_protocol::Result<()> {
    smol::block_on(
        Agent
            .builder()
            .name("studio-test-agent")
            .on_receive_request(
                async move |request: InitializeRequest, responder, _connection| {
                    let booleans = request
                        .client_capabilities
                        .session
                        .as_ref()
                        .and_then(|session| session.config_options.as_ref())
                        .is_some_and(|options| options.boolean.is_some());
                    BOOLEANS.store(booleans, Ordering::SeqCst);
                    responder.respond(
                        InitializeResponse::new(request.protocol_version)
                            .agent_info(
                                Implementation::new("studio-test-agent/claude-agent-acp", "0.1.0")
                                    .title("Studio test agent"),
                            )
                            .agent_capabilities(
                                AgentCapabilities::new()
                                    .load_session(true)
                                    .session_capabilities(
                                        SessionCapabilities::new()
                                            .list(SessionListCapabilities::new())
                                            .delete(SessionDeleteCapabilities::new())
                                            .close(SessionCloseCapabilities::new()),
                                    ),
                            ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: NewSessionRequest, responder, _connection| {
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    let id = format!("test-{nanos}");
                    open(&id, transcript_path(&request.cwd, &id), request.cwd.clone());
                    let options = config_options(&id);
                    responder.respond(
                        NewSessionResponse::new(SessionId::new(id)).config_options(options),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: LoadSessionRequest,
                            responder,
                            connection: ConnectionTo<Client>| {
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
                    open(&request.session_id.0, path, request.cwd.clone());
                    responder.respond(
                        LoadSessionResponse::new()
                            .config_options(config_options(&request.session_id.0)),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: SetSessionConfigOptionRequest, responder, _connection| {
                    let id = request.config_id.0.to_string();
                    let value = match (&request.value, request.value.as_value_id()) {
                        (SessionConfigOptionValue::Boolean { value }, _) => Value::Bool(*value),
                        // An On/Off menu sets Fast mode as a value id.
                        (_, Some(value)) if id == "fast" => Value::Bool(value.0.as_ref() == "on"),
                        (_, Some(value)) => Value::String(value.0.to_string()),
                        _ => {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data("unknown value"),
                            );
                        }
                    };
                    let session = request.session_id.0.to_string();
                    if !config_options(&session)
                        .iter()
                        .any(|option| option.id.0.as_ref() == id)
                    {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params().data("no such option"),
                        );
                    }
                    with_session(&session, |s| {
                        s.options.insert(id, value);
                        std::fs::write(
                            options_path(&s.transcript),
                            Value::Object(s.options.clone()).to_string(),
                        )
                        .ok();
                    });
                    responder.respond(SetSessionConfigOptionResponse::new(config_options(
                        &session,
                    )))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: PromptRequest, responder, connection: ConnectionTo<Client>| {
                    let cwd = with_session(&request.session_id.0, |s| s.cwd.clone())
                        .unwrap_or_else(std::env::temp_dir);
                    let turn_connection = connection.clone();
                    connection.spawn(async move {
                        let stop = match turn(&turn_connection, &request, &cwd).await {
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
                        let (used, cost) = with_session(&request.session_id.0, |s| {
                            s.usage.0 = (s.usage.0 + 41_800).min(WINDOW);
                            s.usage.1 += 0.137;
                            s.usage
                        })
                        .unwrap_or_default();
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
            .on_receive_request(
                async move |request: ListSessionsRequest, responder, _connection| {
                    let sessions = request.cwd.as_deref().map(list).unwrap_or_default();
                    responder.respond(ListSessionsResponse::new(sessions))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: DeleteSessionRequest, responder, _connection| {
                    let id = request.session_id.0.to_string();
                    SESSIONS.lock().unwrap().remove(&id);
                    let path = {
                        let mut known = KNOWN.lock().unwrap();
                        let path = known.remove(&id);
                        std::fs::write(
                            known_path(),
                            serde_json::to_string(&*known).unwrap_or_default(),
                        )
                        .ok();
                        path
                    };
                    let Some(path) = path else {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params().data("no such session"),
                        );
                    };
                    std::fs::remove_file(options_path(&path)).ok();
                    std::fs::remove_file(path).ok();
                    responder.respond(DeleteSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: CloseSessionRequest, responder, _connection| {
                    SESSIONS
                        .lock()
                        .unwrap()
                        .remove(request.session_id.0.as_ref());
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(Stdio::new()),
    )
}

fn say(connection: &ConnectionTo<Client>, session: &SessionId, text: &str) {
    record(&session.0, "agent", text);
    connection
        .send_notification(SessionNotification::new(
            session.clone(),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))),
        ))
        .ok();
}

/// Runs the conversation's CLI in `cwd` as an agent's shell does, reporting the command as a
/// tool call titled with it. Returns its output, or what it wrote to stderr when it failed, and
/// whether it failed.
async fn run_cli(
    connection: &ConnectionTo<Client>,
    session: &SessionId,
    cwd: &Path,
    id: &str,
    args: &[&str],
) -> Result<(String, bool), String> {
    let wrapper = if cfg!(windows) {
        "omnigraph.cmd"
    } else {
        "omnigraph"
    };
    let title = format!("./omnigraph {}", args.join(" "));
    tool_status(connection, session, id, &title, ToolCallStatus::InProgress);
    let output = smol::process::Command::new(cwd.join(wrapper))
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|error| format!("run {wrapper}: {error}"))?;
    let failed = !output.status.success();
    tool_status(
        connection,
        session,
        id,
        &title,
        if failed {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        },
    );
    let text = if failed {
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .find(|line| line.contains("Studio") || line.contains("rror"))
            .unwrap_or("failed")
            .trim()
            .to_owned()
    } else {
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    Ok((text, failed))
}

/// Reports a tool call: its start, titled with what it runs, or its new status.
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
    cwd: &Path,
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
    record(&session.0, "user", &prompt);
    // "slow" holds the turn for two seconds, to see a conversation finish in the background.
    if prompt.contains("slow") {
        smol::Timer::after(std::time::Duration::from_secs(2)).await;
    }

    let (schema, _) = run_cli(connection, session, cwd, "schema", &["schema", "show"]).await?;
    let types = schema
        .lines()
        .filter(|line| line.starts_with("node "))
        .count();
    say(
        connection,
        session,
        &format!("The graph has {types} node types. "),
    );
    let (count, _) = run_cli(
        connection,
        session,
        cwd,
        "count",
        &["query", "-e", COUNT, "--json"],
    )
    .await?;
    let n = serde_json::from_str::<Value>(&count)
        .ok()
        .and_then(|value| value["rows"][0]["n"].as_i64());
    say(
        connection,
        session,
        &format!("It holds {} people.", n.unwrap_or(-1)),
    );
    if prompt.contains("list") {
        let (rows, _) = run_cli(
            connection,
            session,
            cwd,
            "list",
            &["query", "-e", PEOPLE, "--json"],
        )
        .await?;
        let names: Vec<String> = serde_json::from_str::<Value>(&rows)
            .ok()
            .and_then(|value| value["rows"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|row| row["p.name"].as_str().map(str::to_owned))
            .collect();
        say(
            connection,
            session,
            &format!("\n\nThe first three are {}.", names.join(", ")),
        );
    }
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

    // "change" (or "cli") creates the agent's branch as `AGENTS.md` names it, retitles a person
    // there, and tries the same on the base, which Studio refuses.
    if prompt.contains("change") || prompt.contains("cli") {
        let agents = std::fs::read_to_string(cwd.join("AGENTS.md"))
            .map_err(|error| format!("no AGENTS.md: {error}"))?;
        let after = |marker: &str, end: &[char]| -> Option<String> {
            let rest = agents.split(marker).nth(1)?;
            Some(rest.split(end).next()?.trim().to_owned())
        };
        let branch = after("on your own branch `", &['`']).ok_or("no branch")?;
        let base = after("--from ", &['`', ' ']).ok_or("no base")?;
        let (created, _) = run_cli(
            connection,
            session,
            cwd,
            "branch",
            &["branch", "create", &branch, "--from", &base],
        )
        .await?;
        let (changed, _) = run_cli(
            connection,
            session,
            cwd,
            "retitle",
            &["mutate", "--branch", &branch, "-e", RETITLE],
        )
        .await?;
        let (refused, _) = run_cli(
            connection,
            session,
            cwd,
            "elsewhere",
            &["mutate", "--branch", &base, "-e", RETITLE],
        )
        .await?;
        let first = |text: &str| text.lines().next().unwrap_or("").to_owned();
        say(
            connection,
            session,
            &format!(
                "\n\nChange: created {branch}: {}; retitled: {}; on {base}: {}",
                first(&created),
                first(&changed),
                first(&refused)
            ),
        );
    }
    Ok(())
}
