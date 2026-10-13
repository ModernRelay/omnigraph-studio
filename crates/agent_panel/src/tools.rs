//! The tools Studio serves to the agent (spec 011): their definitions, argument parsing, and
//! how queries and results are checked and shaped. Running them against the graph is in
//! `panel.rs`, on the person's session.

use gq_language::{QueryCheck, QueryInfo, SchemaModel};
use serde_json::{Map, Value, json};

/// Most rows a query tool returns.
pub const MAX_ROWS: usize = 200;
/// Most bytes of row JSON a query tool returns.
pub const MAX_BYTES: usize = 100_000;

pub fn definitions() -> Vec<Value> {
    let text = json!({ "type": "string" });
    vec![
        json!({
            "name": "omnigraph_schema",
            "description": "The graph's schema in OmniGraph's .pg language, then one line per node and edge type.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "omnigraph_cli",
            "description": "This conversation's omnigraph CLI: the path to run, the flags every command takes, your branch, and the rules for writing. Prefer the CLI to the other tools when it is available.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "omnigraph_branches",
            "description": "The graph's branches, marking the person's branch and yours.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "omnigraph_query",
            "description": "Run a read written in OmniGraph's .gq query language, for example `query people() { match { $p: Person } return { $p.name } }`. Runs on `branch`, else on your branch once you have one, else on the person's branch. Returns at most 200 rows. Mutations are refused: use omnigraph_propose_change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": text,
                    "name": { "type": "string", "description": "Which query to run when the text declares several." },
                    "params": { "type": "object" },
                    "branch": text,
                },
                "required": ["query"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "omnigraph_propose_change",
            "description": "Propose a change as a .gq mutation (insert, update or delete). The person approves it in Studio first; it then runs on a branch of your own, created from the person's branch on your first change. Returns the outcome: committed, refused, or unknown. Unknown outcomes are not retried: check before proposing again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": text,
                    "name": { "type": "string", "description": "Which mutation to run when the text declares several." },
                    "params": { "type": "object" },
                    "summary": { "type": "string", "description": "One sentence for the person: what this change does and why." },
                },
                "required": ["query", "summary"],
                "additionalProperties": false,
            },
        }),
    ]
}

/// A parsed tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCall {
    Schema,
    Cli,
    Branches,
    Query {
        query: String,
        name: Option<String>,
        params: Option<Map<String, Value>>,
        branch: Option<String>,
    },
    Propose {
        query: String,
        name: Option<String>,
        params: Option<Map<String, Value>>,
        summary: String,
    },
}

impl ToolCall {
    pub fn parse(name: &str, arguments: &Value) -> Result<Self, String> {
        let arguments = arguments
            .as_object()
            .ok_or_else(|| "Arguments must be an object.".to_owned())?;
        let text = |key: &str| -> Result<Option<String>, String> {
            match arguments.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(value)) => Ok(Some(value.clone())),
                Some(_) => Err(format!("`{key}` must be a string.")),
            }
        };
        let params = || -> Result<Option<Map<String, Value>>, String> {
            match arguments.get("params") {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Object(map)) => Ok(Some(map.clone())),
                Some(_) => Err("`params` must be an object.".to_owned()),
            }
        };
        let required = |key: &str| -> Result<String, String> {
            text(key)?
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| format!("`{key}` is required."))
        };
        match name {
            "omnigraph_schema" => Ok(Self::Schema),
            "omnigraph_cli" => Ok(Self::Cli),
            "omnigraph_branches" => Ok(Self::Branches),
            "omnigraph_query" => Ok(Self::Query {
                query: required("query")?,
                name: text("name")?,
                params: params()?,
                branch: text("branch")?,
            }),
            "omnigraph_propose_change" => Ok(Self::Propose {
                query: required("query")?,
                name: text("name")?,
                params: params()?,
                summary: required("summary")?,
            }),
            other => Err(format!("Unknown tool `{other}`.")),
        }
    }
}

/// Whether a tool title or name is one of Studio's tools. Agents name MCP tools differently
/// (`mcp__omnigraph__omnigraph_query`, `omnigraph_query`), so this matches the tool name.
pub fn is_studio_tool(title: &str) -> bool {
    definitions().iter().any(|tool| {
        tool["name"]
            .as_str()
            .is_some_and(|name| title.contains(name))
    })
}

/// The schema tool's text.
pub fn describe_schema(schema: &SchemaModel) -> String {
    let mut text = String::from(schema.source.as_ref());
    text.push_str("\n\n# Types\n");
    for node in &schema.nodes {
        let properties: Vec<_> = node.properties.iter().map(|p| p.name.as_str()).collect();
        text.push_str(&format!(
            "node {} (key {}): {}\n",
            node.name,
            node.key.as_deref().unwrap_or("@id"),
            properties.join(", ")
        ));
    }
    for edge in &schema.edges {
        text.push_str(&format!(
            "edge {}: {} -> {}{}\n",
            edge.name,
            edge.from,
            edge.to,
            if edge.at_most_one {
                " (at most one)"
            } else {
                ""
            }
        ));
    }
    text
}

/// Picks the query to run from a checked document: the one named, or the only one. A read tool
/// refuses mutations and the change tool refuses reads.
pub fn select_query(
    check: &QueryCheck,
    name: Option<&str>,
    want_mutation: bool,
) -> Result<QueryInfo, String> {
    if !check.diagnostics.is_empty() {
        return Err(format_diagnostics(check));
    }
    let query = match name {
        Some(name) => check
            .queries
            .iter()
            .find(|query| query.name == name)
            .ok_or_else(|| format!("The text declares no query named `{name}`."))?,
        None => match check.queries.as_slice() {
            [only] => only,
            [] => return Err("The text declares no query.".to_owned()),
            several => {
                let names: Vec<_> = several.iter().map(|q| q.name.as_str()).collect();
                return Err(format!(
                    "The text declares several queries ({}); pass `name`.",
                    names.join(", ")
                ));
            }
        },
    };
    match (query.is_mutation, want_mutation) {
        (true, false) => Err(format!(
            "`{}` is a mutation. Use omnigraph_propose_change, which asks the person first.",
            query.name
        )),
        (false, true) => Err(format!(
            "`{}` is a read, not a change. Use omnigraph_query.",
            query.name
        )),
        _ => Ok(query.clone()),
    }
}

pub fn format_diagnostics(check: &QueryCheck) -> String {
    let mut text = String::from("The query does not check against the schema:\n");
    for diagnostic in &check.diagnostics {
        let at = match (diagnostic.line, diagnostic.column) {
            (Some(line), Some(column)) => format!("{line}:{column} "),
            _ => String::new(),
        };
        text.push_str(&format!("- {at}{}", diagnostic.message));
        if let Some(fix) = &diagnostic.fix {
            text.push_str(&format!(" ({fix})"));
        }
        text.push('\n');
    }
    text
}

/// The query tool's text: columns, rows (cut at [`MAX_ROWS`] or [`MAX_BYTES`]) and the commit
/// the rows were read at.
pub fn describe_rows(
    branch: &str,
    columns: &[String],
    rows: &Value,
    commit: Option<&str>,
) -> String {
    let all = rows.as_array().cloned().unwrap_or_default();
    let total = all.len();
    let mut kept = Vec::new();
    let mut bytes = 0;
    for row in all.into_iter().take(MAX_ROWS) {
        let size = serde_json::to_string(&row).map(|s| s.len()).unwrap_or(0);
        if bytes + size > MAX_BYTES {
            break;
        }
        bytes += size;
        kept.push(row);
    }
    let truncated = kept.len() < total;
    serde_json::to_string_pretty(&json!({
        "branch": branch,
        "commit": commit,
        "columns": columns,
        "row_count": total,
        "rows": kept,
        "truncated": truncated,
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = include_str!("../../../dev/cluster/knowledge.pg");

    fn schema() -> SchemaModel {
        SchemaModel::parse(SCHEMA).expect("the dev schema compiles")
    }

    #[test]
    fn parses_arguments_and_refuses_bad_ones() {
        assert_eq!(
            ToolCall::parse(
                "omnigraph_query",
                &json!({"query": "q", "params": {"a": 1}, "branch": "main"})
            ),
            Ok(ToolCall::Query {
                query: "q".into(),
                name: None,
                params: Some(json!({"a": 1}).as_object().unwrap().clone()),
                branch: Some("main".into()),
            })
        );
        assert_eq!(
            ToolCall::parse("omnigraph_propose_change", &json!({"query": "q"})),
            Err("`summary` is required.".into())
        );
        assert_eq!(
            ToolCall::parse("omnigraph_query", &json!({"query": 3})),
            Err("`query` must be a string.".into())
        );
        assert!(ToolCall::parse("rm_rf", &json!({})).is_err());
    }

    #[test]
    fn reads_and_changes_go_to_their_own_tools() {
        let schema = schema();
        let read = gq_language::check(
            &schema,
            "query people() { match { $p: Person } return { $p.name } }",
        );
        let change = gq_language::check(
            &schema,
            "query rename($id: String, $name: String) { update Person set { name: $name } where @id = $id }",
        );
        assert_eq!(select_query(&read, None, false).unwrap().name, "people");
        assert!(
            select_query(&read, None, true)
                .unwrap_err()
                .contains("omnigraph_query")
        );
        assert!(
            select_query(&change, None, false)
                .unwrap_err()
                .contains("omnigraph_propose_change")
        );
        assert_eq!(select_query(&change, None, true).unwrap().name, "rename");
    }

    #[test]
    fn diagnostics_come_back_with_positions() {
        let check = gq_language::check(
            &schema(),
            "query q() { match { $p: Persn } return { $p.name } }",
        );
        let error = select_query(&check, None, false).unwrap_err();
        assert!(error.starts_with("The query does not check"), "{error}");
        assert!(error.contains("Persn"), "{error}");
    }

    #[test]
    fn rows_are_cut_at_the_row_limit() {
        let rows = Value::Array((0..MAX_ROWS + 5).map(|i| json!({"i": i})).collect());
        let text = describe_rows("main", &["i".into()], &rows, Some("c1"));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["rows"].as_array().unwrap().len(), MAX_ROWS);
        assert_eq!(value["row_count"], MAX_ROWS + 5);
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn recognises_studio_tools_by_name() {
        assert!(is_studio_tool("mcp__omnigraph__omnigraph_query"));
        assert!(is_studio_tool("omnigraph_propose_change"));
        assert!(!is_studio_tool("Bash"));
    }
}
