//! What the editor shows after a run, as plain data: result tables, explain plans, server
//! diagnostics mapped back into the editor's text, and the small formats they use (spec 007).

use std::time::Duration;

use gq_language::ResultColumn;
use serde_json::{Map, Value};

/// Rows of a read, one value per column in the server's column order. A null cell's key is
/// omitted by the server; it reads as `Null` here.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultTable {
    pub columns: Vec<String>,
    /// The compiler's type for each column, when it knew the query (`String?`, `I32`, `Node`).
    pub types: Vec<Option<String>>,
    pub rows: Vec<Vec<Value>>,
}

impl ResultTable {
    /// Builds a table from a read's `columns` and `rows`. `expected` is the compiler's view of
    /// the same query; its types are matched to the server's columns by position, because the
    /// compiler names `$p.name` `name` where the server names it `p.name`.
    pub fn new(columns: Vec<String>, rows: &Value, expected: Option<&[ResultColumn]>) -> Self {
        let rows = match rows {
            Value::Array(items) => items
                .iter()
                .map(|row| {
                    columns
                        .iter()
                        .map(|column| row.get(column).cloned().unwrap_or(Value::Null))
                        .collect()
                })
                .collect(),
            _ => Vec::new(),
        };
        let types = match expected {
            Some(expected) if expected.len() == columns.len() => {
                expected.iter().map(|c| Some(type_label(c))).collect()
            }
            _ => vec![None; columns.len()],
        };
        Self {
            columns,
            types,
            rows,
        }
    }

    /// One row as a JSON object, for copying.
    pub fn row_json(&self, row: usize) -> Option<String> {
        let values = self.rows.get(row)?;
        let object: Map<String, Value> = self
            .columns
            .iter()
            .cloned()
            .zip(values.iter().cloned())
            .collect();
        serde_json::to_string_pretty(&Value::Object(object)).ok()
    }

    /// A column's width in pixels: wide enough for its title, type and the first rows' values,
    /// within bounds, so wide values do not push every other column off screen.
    pub fn column_width(&self, column: usize) -> f32 {
        let mut chars = self.columns[column].chars().count();
        if let Some(Some(ty)) = self.types.get(column) {
            chars = chars.max(ty.chars().count());
        }
        for row in self.rows.iter().take(200) {
            chars = chars.max(studio_ui::format_value(&row[column]).chars().count());
        }
        (chars as f32 * 7.5 + 24.).clamp(72., 360.)
    }
}

/// `String?`, `I64`, `[String]?`: the compiler's kind for a result column.
pub fn type_label(column: &ResultColumn) -> String {
    if column.nullable {
        format!("{}?", column.kind)
    } else {
        column.kind.clone()
    }
}

/// The text a copied cell gets: strings as they are, everything else as JSON.
pub fn copy_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The columns `explain` answers with.
pub fn is_plan(columns: &[String]) -> bool {
    columns == ["tree", "depth", "node", "detail"]
}

/// One row of a plan list: a section header for each tree, then its operators.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanItem {
    Header(String),
    Operator {
        depth: usize,
        node: String,
        /// `key=value` pairs from the operator's details, for one line.
        summary: String,
        /// The details as the server sent them.
        detail: String,
    },
    /// A planner fact without a place in a tree (`route engine`, `logical_hash …`).
    Note {
        name: String,
        value: String,
    },
}

/// Turns the rows of `explain` into a list: each tree (`logical`, `physical`, `datafusion`)
/// under a header, operators indented by depth.
pub fn plan_items(rows: &Value) -> Vec<PlanItem> {
    let mut items = Vec::new();
    let mut tree: Option<String> = None;
    for row in rows.as_array().map(Vec::as_slice).unwrap_or_default() {
        let name = row
            .get("tree")
            .and_then(Value::as_str)
            .unwrap_or("plan")
            .to_owned();
        if tree.as_deref() != Some(name.as_str()) {
            items.push(PlanItem::Header(tree_title(&name)));
            tree = Some(name);
        }
        let detail = match row.get("detail") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        let node = row
            .get("node")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned();
        items.push(match row.get("depth").and_then(Value::as_u64) {
            Some(depth) => PlanItem::Operator {
                depth: depth as usize,
                node,
                summary: summarize_detail(&detail),
                detail,
            },
            None => PlanItem::Note {
                name: node,
                value: summarize_detail(&detail),
            },
        });
    }
    items
}

fn tree_title(tree: &str) -> String {
    match tree {
        "logical" => "Logical plan".into(),
        "physical" => "Physical plan".into(),
        "datafusion" => "DataFusion plan".into(),
        "plan" => "Planner".into(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => "Plan".into(),
            }
        }
    }
}

/// `{"binding":"p","table":"node:Person","filter":null,"id":0,"properties":{…}}` →
/// `binding=p, table=node:Person`. Nulls, ids and the planner's property bag are left out;
/// text that is not a JSON object is kept as it is.
pub fn summarize_detail(detail: &str) -> String {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(detail) else {
        return detail.to_owned();
    };
    map.iter()
        .filter(|(key, value)| !value.is_null() && !matches!(key.as_str(), "id" | "properties"))
        .map(|(key, value)| {
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let text = if text.chars().count() > 60 {
                format!("{}…", text.chars().take(59).collect::<String>())
            } else {
                text
            };
            format!("{key}={text}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `34 ms`, `1.2 s`.
pub fn format_elapsed(elapsed: Duration) -> String {
    let ms = elapsed.as_secs_f64() * 1000.;
    if ms < 1000. {
        format!("{} ms", ms.round() as u64)
    } else {
        format!("{:.1} s", ms / 1000.)
    }
}

/// `1 row`, `12 rows`.
pub fn rows_label(count: i64) -> String {
    if count == 1 {
        "1 row".into()
    } else {
        format!("{count} rows")
    }
}

/// A commit id short enough for a status line; the full id goes in the tooltip. Graph commit
/// ids (`hb1.<lineage>.<sequence>.<ULID>`) share their head, so the tail is what tells two
/// apart.
pub fn short_commit(id: &str) -> String {
    let count = id.chars().count();
    if count > 12 {
        format!("…{}", id.chars().skip(count - 10).collect::<String>())
    } else {
        id.to_owned()
    }
}

/// Where the text sent to the server came from, so a position the server reports can be
/// mapped back into the editor: the settings prefix is copied as is, then `inserted` bytes the
/// editor added (`explain `), then the declaration that starts at `start` in the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SourceMap {
    pub prefix_len: usize,
    pub inserted: usize,
    pub start: usize,
}

impl SourceMap {
    pub fn editor_offset(self, sent: usize) -> usize {
        if sent < self.prefix_len {
            sent
        } else {
            self.start + sent.saturating_sub(self.prefix_len + self.inserted)
        }
    }
}

/// A problem the server reported with a position, already mapped into the editor's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDiagnostic {
    pub code: Option<String>,
    pub message: String,
    pub byte: Option<usize>,
}

/// Reads an `ErrorOutput.diagnostic` (`{"code":"Q001","position":{"line":1,"column":46,
/// "byte":45},"expected":"…"}`).
pub fn server_diagnostic(value: &Value, fallback: &str, map: SourceMap) -> ServerDiagnostic {
    let message = ["message", "expected", "detail"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .unwrap_or(fallback)
        .to_owned();
    ServerDiagnostic {
        code: value.get("code").and_then(Value::as_str).map(str::to_owned),
        message,
        byte: value
            .get("position")
            .and_then(|p| p.get("byte"))
            .and_then(Value::as_u64)
            .map(|byte| map.editor_offset(byte as usize)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn column(name: &str, kind: &str, nullable: bool) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            kind: kind.into(),
            nullable,
        }
    }

    #[test]
    fn tables_fill_missing_cells_and_take_types_by_position() {
        let rows = json!([{"p.name": "Ada", "p.born": "1984-04-14"}, {"p.name": "Alan"}]);
        let expected = [
            column("name", "String", false),
            column("born", "Date", true),
        ];
        let table = ResultTable::new(
            vec!["p.name".into(), "p.born".into()],
            &rows,
            Some(&expected),
        );
        assert_eq!(table.rows[1], vec![json!("Alan"), Value::Null]);
        assert_eq!(table.types, [Some("String".into()), Some("Date?".into())]);
        // A different column count means the compiler described another query.
        let table = ResultTable::new(vec!["p.name".into()], &rows, Some(&expected));
        assert_eq!(table.types, [None]);
        assert!(table.row_json(0).unwrap().contains("\"p.name\": \"Ada\""));
        assert!(table.row_json(5).is_none());
    }

    #[test]
    fn column_widths_follow_content_within_bounds() {
        let rows = json!([{"a": "x", "b": "a very long value that goes on and on and on and on and on and on"}]);
        let table = ResultTable::new(vec!["a".into(), "b".into()], &rows, None);
        assert_eq!(table.column_width(0), 72.);
        assert_eq!(table.column_width(1), 360.);
    }

    #[test]
    fn copied_cells_are_plain_text() {
        assert_eq!(copy_text(&json!("Ada")), "Ada");
        assert_eq!(copy_text(&json!(["a", "b"])), "[\"a\",\"b\"]");
        assert_eq!(copy_text(&Value::Null), "");
        assert_eq!(copy_text(&json!(4)), "4");
    }

    #[test]
    fn plans_become_an_indented_list_under_tree_headers() {
        let rows = json!([
            {"tree": "logical", "depth": 0, "node": "Limit", "detail": "{\"rows\":3,\"resume\":null}"},
            {"tree": "logical", "depth": 1, "node": "TableScan", "detail": "{\"binding\":\"p\",\"table\":\"node:Person\",\"id\":4,\"properties\":{\"rows\":1}}"},
            {"tree": "datafusion", "depth": 0, "node": "LimitExec", "detail": "rows=3"},
            {"tree": "plan", "depth": null, "node": "route", "detail": "engine"}
        ]);
        assert!(is_plan(&[
            "tree".into(),
            "depth".into(),
            "node".into(),
            "detail".into()
        ]));
        let items = plan_items(&rows);
        assert_eq!(items[0], PlanItem::Header("Logical plan".into()));
        assert!(matches!(
            &items[2],
            PlanItem::Operator { depth: 1, node, summary, .. }
                if node == "TableScan" && summary == "binding=p, table=node:Person"
        ));
        assert_eq!(items[3], PlanItem::Header("DataFusion plan".into()));
        assert!(matches!(&items[4], PlanItem::Operator { summary, .. } if summary == "rows=3"));
        assert_eq!(items[5], PlanItem::Header("Planner".into()));
        assert_eq!(
            items[6],
            PlanItem::Note {
                name: "route".into(),
                value: "engine".into()
            }
        );
    }

    #[test]
    fn small_formats() {
        assert_eq!(format_elapsed(Duration::from_millis(34)), "34 ms");
        assert_eq!(format_elapsed(Duration::from_millis(1250)), "1.2 s");
        assert_eq!(rows_label(1), "1 row");
        assert_eq!(rows_label(0), "0 rows");
        assert_eq!(short_commit("abc"), "abc");
        assert_eq!(
            short_commit("hb1.01M4RAJY7S28C66BR5E19AJPH0.2.01M4RCHTJZ78MSXG2BJ64ENC3T"),
            "…2BJ64ENC3T"
        );
    }

    #[test]
    fn server_positions_map_back_into_the_editor() {
        // The editor holds `set x = 1;\n` (11 bytes), a first query at 11..40, and the
        // explained query at 41; the server saw `set x = 1;\nexplain query b…`.
        let map = SourceMap {
            prefix_len: 11,
            inserted: 8,
            start: 41,
        };
        assert_eq!(map.editor_offset(3), 3);
        assert_eq!(map.editor_offset(19), 41);
        assert_eq!(map.editor_offset(25), 47);
        let diagnostic = server_diagnostic(
            &json!({"code": "Q001", "position": {"line": 1, "column": 7, "byte": 25}, "expected": "expected return_clause"}),
            "parse error",
            map,
        );
        assert_eq!(diagnostic.code.as_deref(), Some("Q001"));
        assert_eq!(diagnostic.message, "expected return_clause");
        assert_eq!(diagnostic.byte, Some(47));
    }
}
