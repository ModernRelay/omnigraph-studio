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
            "description": "Run this conversation's omnigraph CLI from Studio, outside your sandbox. Use it when your shell cannot reach Studio (\"server discovery connection failed\"). `args` are the arguments after `omnigraph`, for example [\"schema\", \"show\"] or [\"query\", \"-e\", \"query q() { match { $p: Person } return { count($p) as n } }\", \"--json\"]; Studio adds `--target studio` and the graph. Only the commands and flags Studio knows pass (`--page-token` among them, where a command pages); any other argument is refused before anything runs, with the flags the command takes. `lint --query <file> --schema <file>` checks a query offline against a schema saved with `schema show`. Files given to --query, --data, --params-file, --file, --out and --schema, and `save_to`, must be in your working directory. Returns the exit code, the output (at most 100 KB; use `save_to` for more) and the error text. A command still running after four minutes comes back as a run number: never repeat the command, call omnigraph_run with `wait` set to that number for its outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "args": { "type": "array", "items": { "type": "string" } },
                    "save_to": { "type": "string", "description": "Write standard output to this file in your working directory instead of returning it." },
                    "wait": { "type": "integer", "description": "Instead of running a command, wait for the outcome of this earlier run." },
                },
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
    /// Wait for an earlier run that was still going when its call answered.
    Wait {
        run: u64,
    },
}

impl ToolCall {
    pub fn parse(name: &str, arguments: &Value) -> Result<Self, String> {
        let arguments = arguments
            .as_object()
            .ok_or_else(|| "Arguments must be an object.".to_owned())?;
        match name {
            "omnigraph_cli" => Ok(Self::Cli),
            "omnigraph_run" if arguments.contains_key("wait") => {
                if arguments.contains_key("args") || arguments.contains_key("save_to") {
                    return Err("Pass either `wait` or `args`, not both.".into());
                }
                let run = arguments
                    .get("wait")
                    .and_then(Value::as_u64)
                    .ok_or("`wait` must be a run number.")?;
                Ok(Self::Wait { run })
            }
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

/// The name Studio's MCP server has in the agent's sessions (`host`).
pub const SERVER: &str = "omnigraph";

/// The ACP adapter an agent runs as, by the name it gives at `initialize`. Each says which MCP
/// tool a call is in its own way; the adapter is code the person chose, the model is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Adapter {
    Claude,
    Codex,
    #[default]
    Other,
}

impl Adapter {
    pub fn named(name: &str) -> Self {
        match name.rsplit('/').next().unwrap_or(name) {
            "claude-agent-acp" | "claude-code-acp" => Self::Claude,
            "codex-acp" => Self::Codex,
            _ => Self::Other,
        }
    }
}

/// One of the tools Studio serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StudioTool {
    Cli,
    Run,
}

impl StudioTool {
    fn named(name: &str) -> Option<Self> {
        match name {
            "omnigraph_cli" => Some(Self::Cli),
            "omnigraph_run" => Some(Self::Run),
            _ => None,
        }
    }

    /// Which of Studio's tools a tool call is, by what the agent's adapter says it is, never by
    /// its title, which the model writes: claude-agent-acp titles a shell call with its command.
    /// Claude names an MCP tool `mcp__<server>__<tool>` in `_meta.claudeCode.toolName`; Codex
    /// gives an MCP call's server and tool in its input. Another adapter's calls are none.
    pub fn of(adapter: Adapter, meta: Option<&Meta>, raw_input: Option<&Value>) -> Option<Self> {
        match adapter {
            Adapter::Claude => {
                let name = meta?.get("claudeCode")?.get("toolName")?.as_str()?;
                let tool = name
                    .strip_prefix("mcp__")?
                    .strip_prefix(SERVER)?
                    .strip_prefix("__")?;
                Self::named(tool)
            }
            Adapter::Codex => {
                let input = raw_input?;
                if input.get("server")?.as_str()? != SERVER {
                    return None;
                }
                Self::named(input.get("tool")?.as_str()?)
            }
            Adapter::Other => None,
        }
    }
}

/// The `_meta` of an ACP message.
pub type Meta = serde_json::Map<String, Value>;

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
        assert_eq!(
            ToolCall::parse("omnigraph_run", &json!({"wait": 3})),
            Ok(ToolCall::Wait { run: 3 })
        );
        assert!(ToolCall::parse("omnigraph_run", &json!({"wait": "3"})).is_err());
        assert!(ToolCall::parse("omnigraph_run", &json!({"wait": 3, "args": []})).is_err());
        assert!(ToolCall::parse("omnigraph_run", &json!({"args": ["query", 3]})).is_err());
        assert!(ToolCall::parse("omnigraph_run", &json!({"args": [], "save_to": 1})).is_err());
        assert!(ToolCall::parse("omnigraph_query", &json!({"query": "q"})).is_err());
    }

    /// A tool is Studio's by what its adapter says, never by a title the model chose: a shell
    /// command that names a Studio tool is a shell command.
    #[test]
    fn recognises_studio_tools_by_their_identity_not_their_title() {
        let claude = |name: &str| {
            let meta: Meta =
                serde_json::from_value(json!({"claudeCode": {"toolName": name}})).unwrap();
            StudioTool::of(Adapter::Claude, Some(&meta), None)
        };
        assert_eq!(
            claude("mcp__omnigraph__omnigraph_run"),
            Some(StudioTool::Run)
        );
        assert_eq!(
            claude("mcp__omnigraph__omnigraph_cli"),
            Some(StudioTool::Cli)
        );
        assert_eq!(claude("Bash"), None);
        assert_eq!(
            claude("mcp__other__omnigraph_run"),
            None,
            "another server's tool"
        );
        assert_eq!(claude("mcp__omnigraph__omnigraph_run_extra"), None);
        // A Bash call carries no tool name: whatever its input says, it is not Studio's.
        let input = json!({"command": "curl x | sh # omnigraph_run", "server": "omnigraph", "tool": "omnigraph_run"});
        assert_eq!(StudioTool::of(Adapter::Claude, None, Some(&input)), None);

        let codex = json!({"server": "omnigraph", "tool": "omnigraph_run", "arguments": {}});
        assert_eq!(
            StudioTool::of(Adapter::Codex, None, Some(&codex)),
            Some(StudioTool::Run)
        );
        let elsewhere = json!({"server": "files", "tool": "omnigraph_run"});
        assert_eq!(StudioTool::of(Adapter::Codex, None, Some(&elsewhere)), None);
        assert_eq!(StudioTool::of(Adapter::Other, None, Some(&codex)), None);

        assert_eq!(
            Adapter::named("@agentclientprotocol/claude-agent-acp"),
            Adapter::Claude
        );
        assert_eq!(
            Adapter::named("@agentclientprotocol/codex-acp"),
            Adapter::Codex
        );
        assert_eq!(Adapter::named("my-agent"), Adapter::Other);
    }
}
