//! The tools Studio serves to the agent (spec 013): the conversation's `omnigraph` CLI is the
//! agent's one way to the graph. `omnigraph_cli` says where the CLI is and how to use it, for an
//! agent whose shell reaches Studio; `omnigraph_run` runs it from Studio, for an agent whose
//! sandbox has no network. Running them is in `panel.rs`.

use serde_json::{Value, json};

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "omnigraph_cli",
            "description": "This conversation's omnigraph CLI: the path to run in your shell, the flags every command takes, your branch, and the rules for writing. Call it first.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "omnigraph_run",
            "description": "Run this conversation's omnigraph CLI from Studio, outside your sandbox. Use it when your shell cannot reach Studio (\"server discovery connection failed\"). `args` are the arguments after `omnigraph`, for example [\"schema\", \"show\"] or [\"query\", \"-e\", \"query q() { match { $p: Person } return { count($p) as n } }\", \"--json\"]; Studio adds `--server studio` and the graph. Files given to --query, --data, --params-file, --file and --out, and `save_to`, must be in your working directory. Returns the exit code, the output (at most 100 KB; use `save_to` for more) and the error text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "args": { "type": "array", "items": { "type": "string" } },
                    "save_to": { "type": "string", "description": "Write standard output to this file in your working directory instead of returning it." },
                },
                "required": ["args"],
                "additionalProperties": false,
            },
        }),
    ]
}

/// A parsed tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCall {
    Cli,
    Run {
        args: Vec<String>,
        save_to: Option<String>,
    },
}

impl ToolCall {
    pub fn parse(name: &str, arguments: &Value) -> Result<Self, String> {
        let arguments = arguments
            .as_object()
            .ok_or_else(|| "Arguments must be an object.".to_owned())?;
        match name {
            "omnigraph_cli" => Ok(Self::Cli),
            "omnigraph_run" => {
                let args = arguments
                    .get("args")
                    .and_then(Value::as_array)
                    .ok_or("`args` is required: a list of strings.")?
                    .iter()
                    .map(|arg| arg.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or("Every entry of `args` must be a string.")?;
                let save_to = match arguments.get("save_to") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(path)) => Some(path.clone()),
                    Some(_) => return Err("`save_to` must be a string.".into()),
                };
                Ok(Self::Run { args, save_to })
            }
            other => Err(format!("Unknown tool `{other}`.")),
        }
    }
}

/// Whether a tool title or name is one of Studio's tools. Agents name MCP tools differently
/// (`mcp__omnigraph__omnigraph_run`, `omnigraph_run`), so this matches the tool name.
pub fn is_studio_tool(title: &str) -> bool {
    definitions().iter().any(|tool| {
        tool["name"]
            .as_str()
            .is_some_and(|name| title.contains(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_arguments_and_refuses_bad_ones() {
        assert_eq!(
            ToolCall::parse(
                "omnigraph_run",
                &json!({"args": ["query", "-e", "q"], "save_to": "out.json"})
            ),
            Ok(ToolCall::Run {
                args: vec!["query".into(), "-e".into(), "q".into()],
                save_to: Some("out.json".into()),
            })
        );
        assert_eq!(
            ToolCall::parse("omnigraph_cli", &json!({})),
            Ok(ToolCall::Cli)
        );
        assert!(ToolCall::parse("omnigraph_run", &json!({})).is_err());
        assert!(ToolCall::parse("omnigraph_run", &json!({"args": ["query", 3]})).is_err());
        assert!(ToolCall::parse("omnigraph_run", &json!({"args": [], "save_to": 1})).is_err());
        assert!(ToolCall::parse("omnigraph_query", &json!({"query": "q"})).is_err());
    }

    #[test]
    fn recognises_studio_tools_by_name() {
        assert!(is_studio_tool("mcp__omnigraph__omnigraph_run"));
        assert!(is_studio_tool("omnigraph_cli"));
        assert!(!is_studio_tool("Bash"));
    }
}
