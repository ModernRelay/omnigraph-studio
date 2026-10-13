//! The part of MCP the agent's tool server speaks: newline-delimited JSON-RPC 2.0 (MCP's stdio
//! transport, carried to Studio by the `mcp-proxy` command), with `initialize`, `ping`,
//! `tools/list` and `tools/call`. Everything else is answered "method not found".

use serde_json::{Value, json};

use crate::tools;

/// MCP revisions this server accepts, newest first. `initialize` echoes the client's revision
/// when it is one of these, and offers the newest otherwise.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub(crate) const INSTRUCTIONS: &str = "OmniGraph Studio gives you the omnigraph CLI for one OmniGraph graph. \
Use it for everything: `schema show` first, then `query`, and `mutate` or `load` on your own \
branch, which the person reviews before it reaches theirs. Run it in your shell by the path \
omnigraph_cli gives you, or with omnigraph_run when your shell has no network.";

/// What to do with one incoming line.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Send this reply.
    Reply(Value),
    /// Run a tool and send [`tool_reply`] with the same id. `progress` is the client's
    /// progress token, for [`progress`] notifications while the call runs.
    CallTool {
        id: Value,
        name: String,
        arguments: Value,
        progress: Option<Value>,
    },
    /// A notification or a response: send nothing.
    Nothing,
}

/// Answers one line. `conversation` is appended to the server's instructions: what this
/// conversation's `omnigraph` CLI is and which branch is the agent's (spec 013).
pub fn handle(line: &str, conversation: &str) -> Incoming {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(_) => return Incoming::Reply(error(Value::Null, -32700, "Parse error")),
    };
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
            let version = requested
                .filter(|version| PROTOCOL_VERSIONS.contains(version))
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            Incoming::Reply(result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "omnigraph-studio",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": if conversation.is_empty() {
                        INSTRUCTIONS.to_owned()
                    } else {
                        format!("{INSTRUCTIONS}\n\n{conversation}")
                    },
                }),
            ))
        }
        "ping" => Incoming::Reply(result(id, json!({}))),
        "tools/list" => Incoming::Reply(result(id, json!({ "tools": tools::definitions() }))),
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
            progress: params
                .pointer("/_meta/progressToken")
                .filter(|token| token.is_string() || token.is_number())
                .cloned(),
        },
        _ => Incoming::Reply(error(id, -32601, "Method not found")),
    }
}

/// A tool's result. An error result is still a JSON-RPC success, as MCP specifies, so the
/// agent reads the message and can correct itself.
pub fn tool_reply(id: Value, text: String, is_error: bool) -> Value {
    result(
        id,
        json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }),
    )
}

/// A progress notification for a call that is still running. Agents abort a tool call that
/// sends nothing for a while (Claude Code: 30 minutes on stdio), and a write can wait that long
/// for the person's approval. `progress` must grow with every notification.
pub fn progress(token: &Value, progress: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": progress, "message": "Still running" },
    })
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_a_known_revision_and_offers_the_newest_otherwise() {
        let initialize = |version: &str, conversation: &str| match handle(
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{version}"}}}}"#
            ),
            conversation,
        ) {
            Incoming::Reply(reply) => reply["result"].clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            initialize("2025-03-26", "")["protocolVersion"],
            "2025-03-26"
        );
        assert_eq!(
            initialize("1999-01-01", "")["protocolVersion"],
            PROTOCOL_VERSIONS[0]
        );
        // A conversation's own instructions follow the server's.
        let instructions = initialize("2025-06-18", "Your branch is agent/x.")["instructions"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(instructions.starts_with(INSTRUCTIONS));
        assert!(instructions.ends_with("Your branch is agent/x."));
    }

    #[test]
    fn lists_tools_and_routes_calls() {
        let Incoming::Reply(list) =
            handle(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#, "")
        else {
            panic!()
        };
        let names: Vec<_> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["omnigraph_cli", "omnigraph_run",]);
        assert_eq!(
            handle(
                r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"omnigraph_run"}}"#,
                ""
            ),
            Incoming::CallTool {
                id: json!(7),
                name: "omnigraph_run".into(),
                arguments: json!({}),
                progress: None,
            }
        );
        // The client's progress token comes with the call.
        assert_eq!(
            handle(
                r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"omnigraph_run","arguments":{"args":[]},"_meta":{"progressToken":"p1"}}}"#,
                ""
            ),
            Incoming::CallTool {
                id: json!(8),
                name: "omnigraph_run".into(),
                arguments: json!({"args": []}),
                progress: Some(json!("p1")),
            }
        );
    }

    #[test]
    fn notifications_and_responses_get_no_reply_and_unknown_methods_an_error() {
        assert_eq!(
            handle(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                ""
            ),
            Incoming::Nothing
        );
        assert_eq!(
            handle(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#, ""),
            Incoming::Nothing
        );
        let Incoming::Reply(reply) =
            handle(r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#, "")
        else {
            panic!()
        };
        assert_eq!(reply["error"]["code"], -32601);
        let Incoming::Reply(reply) = handle("not json", "") else {
            panic!()
        };
        assert_eq!(reply["error"]["code"], -32700);
    }
}
