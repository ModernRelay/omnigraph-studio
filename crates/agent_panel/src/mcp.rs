//! The part of MCP the agent's tool server speaks: newline-delimited JSON-RPC 2.0 (MCP's stdio
//! transport, carried to Studio by the `mcp-proxy` command), with `initialize`, `ping`,
//! `tools/list` and `tools/call`. Everything else is answered "method not found".

use serde_json::{Value, json};

use crate::tools;

/// MCP revisions this server accepts, newest first. `initialize` echoes the client's revision
/// when it is one of these, and offers the newest otherwise.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Tools for one OmniGraph graph, served by OmniGraph Studio. Read the \
schema first. Reads run on the person's branch, or on your branch once you have one. Changes go \
through omnigraph_propose_change: the person approves each one in Studio, and it lands on a \
branch of your own, never on theirs.";

/// What to do with one incoming line.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Send this reply.
    Reply(Value),
    /// Run a tool and send [`tool_reply`] with the same id.
    CallTool {
        id: Value,
        name: String,
        arguments: Value,
    },
    /// A notification or a response: send nothing.
    Nothing,
}

pub fn handle(line: &str) -> Incoming {
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
                    "instructions": INSTRUCTIONS,
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
        let reply = |version: &str| match handle(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{version}"}}}}"#
        )) {
            Incoming::Reply(reply) => reply["result"]["protocolVersion"].clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(reply("2025-03-26"), "2025-03-26");
        assert_eq!(reply("1999-01-01"), PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn lists_tools_and_routes_calls() {
        let Incoming::Reply(list) = handle(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#)
        else {
            panic!()
        };
        let names: Vec<_> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            [
                "omnigraph_schema",
                "omnigraph_branches",
                "omnigraph_query",
                "omnigraph_propose_change"
            ]
        );
        assert_eq!(
            handle(
                r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"omnigraph_schema"}}"#
            ),
            Incoming::CallTool {
                id: json!(7),
                name: "omnigraph_schema".into(),
                arguments: json!({}),
            }
        );
    }

    #[test]
    fn notifications_and_responses_get_no_reply_and_unknown_methods_an_error() {
        assert_eq!(
            handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Incoming::Nothing
        );
        assert_eq!(
            handle(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#),
            Incoming::Nothing
        );
        let Incoming::Reply(reply) =
            handle(r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#)
        else {
            panic!()
        };
        assert_eq!(reply["error"]["code"], -32601);
        let Incoming::Reply(reply) = handle("not json") else {
            panic!()
        };
        assert_eq!(reply["error"]["code"], -32700);
    }
}
