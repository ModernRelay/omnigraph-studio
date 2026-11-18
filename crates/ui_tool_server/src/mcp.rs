//! The part of MCP the tool server speaks (spec 032): JSON-RPC 2.0, one message per POST, with
//! `initialize`, `notifications/initialized`, `ping`, `tools/list` and `tools/call`. Every
//! other method, `server/discover` among them, is answered "method not found", which is what
//! makes Claude Code fall back to `initialize`.
//!
//! Moved from the agent panel's removed tool server (`crates/agent_panel/src/mcp.rs` at
//! `0591de9c^`), from newline-delimited stdio to Streamable HTTP.

use serde_json::{Value, json};

/// MCP revisions this server accepts, newest first: Claude Code asks for `2025-11-25` and
/// Codex for `2025-06-18` (spec 032, Validation log). `initialize` echoes the client's revision
/// when it is one of these, and offers the newest otherwise.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18"];

/// What the server tells every agent about its tools, cut well inside the 2,048 characters
/// claude-agent-acp passes on.
pub const INSTRUCTIONS: &str = "Omnigraph Studio's window. These tools show the person things \
in Studio: a type's table, a node's page, a canvas or a tree walked from a node, the schema's \
map or source, a branch's review or history, a commit, and a query tab with text in it, which \
is not run. studio_windows, studio_tabs and studio_selection say what a window shows, as \
identifiers. No tool writes to a graph, runs a query or returns property values: read and \
write the graph with your own omnigraph CLI, and use these tools to put what you found in \
front of the person.";

/// What to do with one message.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Send this reply.
    Reply(Value),
    /// Answer `initialize` at this revision. `client` is the client's name as it gives it
    /// (`clientInfo.title`, else `clientInfo.name`).
    Initialize {
        id: Value,
        version: &'static str,
        client: Option<String>,
    },
    /// Answer `tools/list` with the same id.
    ListTools { id: Value },
    /// Run a tool and send [`tool_reply`] with the same id.
    CallTool {
        id: Value,
        name: String,
        arguments: Value,
    },
    /// A notification or a response: answered `202 Accepted`, with no body.
    Nothing,
}

/// Answers one message, the body of one POST.
pub fn handle(body: &[u8]) -> Incoming {
    let message: Value = match serde_json::from_slice(body) {
        Ok(message) => message,
        Err(_) => return Incoming::Reply(error(Value::Null, -32700, "Parse error")),
    };
    // 2025-03-26 allowed batches, which 2025-06-18 took out again and no client this serves
    // sends; one is answered rather than left to wait.
    if message.is_array() {
        return Incoming::Reply(error(
            Value::Null,
            -32600,
            "Studio's tool server takes one message per request, not a batch",
        ));
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Incoming::Nothing;
    };
    let Some(id) = message.get("id").cloned() else {
        return Incoming::Nothing;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let version = PROTOCOL_VERSIONS
                .iter()
                .find(|version| requested == Some(**version))
                .unwrap_or(&PROTOCOL_VERSIONS[0]);
            let info = params.get("clientInfo");
            let client = ["title", "name"].iter().find_map(|key| {
                info.and_then(|info| info.get(*key))
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .map(str::to_owned)
            });
            Incoming::Initialize {
                id,
                version,
                client,
            }
        }
        "ping" => Incoming::Reply(result(id, json!({}))),
        "tools/list" => Incoming::ListTools { id },
        "tools/call" => Incoming::CallTool {
            id,
            name: params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            arguments: params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
        },
        _ => Incoming::Reply(error(id, -32601, "Method not found")),
    }
}

/// The answer to `initialize` at `version`.
pub fn initialize(id: Value, version: &str) -> Value {
    result(
        id,
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": "omnigraph-studio",
                "title": "Omnigraph Studio",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": INSTRUCTIONS,
        }),
    )
}

/// The answer to `tools/list`.
pub fn tools_list(id: Value, tools: &Value) -> Value {
    result(id, json!({ "tools": tools }))
}

/// A tool's result: what it did in words first, then its JSON. An error result is still a
/// JSON-RPC success, as MCP specifies, so the agent reads the message and can correct itself.
pub fn tool_reply(id: Value, says: &str, json: &Value, is_error: bool) -> Value {
    result(
        id,
        json!({
            "content": [
                { "type": "text", "text": says },
                { "type": "text", "text": json.to_string() },
            ],
            "isError": is_error,
        }),
    )
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize_at(version: &str) -> Value {
        match handle(
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{version}","clientInfo":{{"name":"claude-code","version":"2.1"}}}}}}"#
            )
            .as_bytes(),
        ) {
            Incoming::Initialize {
                id,
                version,
                client,
            } => {
                assert_eq!(id, json!(1));
                assert_eq!(client.as_deref(), Some("claude-code"));
                super::initialize(id, version)["result"].clone()
            }
            other => panic!("{other:?}"),
        }
    }

    /// Both clients' revisions are echoed; another is offered the newest.
    #[test]
    fn initialize_answers_both_protocol_versions() {
        assert_eq!(initialize_at("2025-06-18")["protocolVersion"], "2025-06-18");
        assert_eq!(initialize_at("2025-11-25")["protocolVersion"], "2025-11-25");
        assert_eq!(initialize_at("2026-07-28")["protocolVersion"], "2025-11-25");
        let answer = initialize_at("2025-06-18");
        assert_eq!(answer["capabilities"]["tools"]["listChanged"], false);
        assert!(answer["instructions"].as_str().unwrap().len() < 2048);
    }

    /// `server/discover`, which Claude Code sends first, and every other method get -32601.
    #[test]
    fn every_other_method_is_not_found() {
        for method in [
            "server/discover",
            "resources/list",
            "prompts/list",
            "logging/setLevel",
        ] {
            let Incoming::Reply(reply) =
                handle(format!(r#"{{"jsonrpc":"2.0","id":4,"method":"{method}"}}"#).as_bytes())
            else {
                panic!("{method}")
            };
            assert_eq!(reply["error"]["code"], -32601, "{method}");
            assert_eq!(reply["id"], 4);
        }
    }

    #[test]
    fn calls_lists_and_notifications_are_routed() {
        assert_eq!(
            handle(br#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#),
            Incoming::ListTools { id: json!("a") }
        );
        assert_eq!(
            handle(
                br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"studio_windows"}}"#
            ),
            Incoming::CallTool {
                id: json!(7),
                name: "studio_windows".into(),
                arguments: json!({}),
            }
        );
        assert_eq!(
            handle(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Incoming::Nothing
        );
        assert_eq!(
            handle(br#"{"jsonrpc":"2.0","id":3,"result":{}}"#),
            Incoming::Nothing
        );
        let Incoming::Reply(reply) = handle(br#"[{"jsonrpc":"2.0","id":5,"method":"ping"}]"#)
        else {
            panic!()
        };
        assert_eq!(reply["error"]["code"], -32600);
        let Incoming::Reply(reply) = handle(b"not json") else {
            panic!()
        };
        assert_eq!(reply["error"]["code"], -32700);
    }
}
