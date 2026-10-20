//! What the editor shows after a run, as plain data: result tables, explain plans, server
//! diagnostics mapped back into the editor's text, and the small formats they use (spec 007).

use std::time::Duration;

use std::collections::{HashMap, HashSet};

use gq_language::{ResultColumn, SchemaModel};
use serde_json::{Map, Value};
use studio_actions::graph::ResultNode;

/// A result's distinct nodes and the pairs of them that share a row.
pub type ResultGraph = (Vec<ResultNode>, Vec<(usize, usize)>);

/// Rows of a read, one value per column in the server's column order. A null cell's key is
/// omitted by the server; it reads as `Null` here.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultTable {
    pub columns: Vec<String>,
    /// The compiler's type for each column, when it knew the query (`String?`, `I32`, `Node`).
    pub types: Vec<Option<String>>,
    /// The node type of each column that returns whole nodes.
    pub node_types: Vec<Option<String>>,
    pub rows: Vec<Vec<Value>>,
}

impl ResultTable {
    /// Builds a table from a read's `columns` and `rows`, whose cells move into it: a large
    /// answer is held once, not twice. `expected` is the compiler's view of the same query; its
    /// types are matched to the server's columns by position, because the compiler names
    /// `$p.name` `name` where the server names it `p.name`.
    pub fn new(columns: Vec<String>, rows: Value, expected: Option<&[ResultColumn]>) -> Self {
        // A column named twice reads the same cell twice, so it is copied rather than moved.
        let unique = columns.iter().collect::<HashSet<_>>().len() == columns.len();
        let rows = match rows {
            Value::Array(items) => items
                .into_iter()
                .map(|row| match row {
                    Value::Object(mut cells) if unique => columns
                        .iter()
                        .map(|column| cells.remove(column).unwrap_or(Value::Null))
                        .collect(),
                    row => columns
                        .iter()
                        .map(|column| row.get(column).cloned().unwrap_or(Value::Null))
                        .collect(),
                })
                .collect(),
            _ => Vec::new(),
        };
        let (types, node_types) = match expected {
            Some(expected) if expected.len() == columns.len() => (
                expected.iter().map(|c| Some(type_label(c))).collect(),
                expected.iter().map(|c| c.node_type.clone()).collect(),
            ),
            _ => (vec![None; columns.len()], vec![None; columns.len()]),
        };
        Self {
            columns,
            types,
            node_types,
            rows,
        }
    }

    /// The result's nodes, each once, and which of them share a row, for drawing on a canvas
    /// (specs 006, 007). `None` when no column returns whole nodes.
    pub fn graph(&self, schema: &SchemaModel) -> Option<ResultGraph> {
        if self.node_types.iter().all(Option::is_none) {
            return None;
        }
        let mut nodes: Vec<ResultNode> = Vec::new();
        let mut index: HashMap<(String, String), usize> = HashMap::new();
        let mut links: Vec<(usize, usize)> = Vec::new();
        let mut linked: HashSet<(usize, usize)> = HashSet::new();
        for row in &self.rows {
            let mut previous: Option<usize> = None;
            for (value, node_type) in row.iter().zip(&self.node_types) {
                let (Some(type_name), Some(id)) =
                    (node_type, value.get("@id").and_then(Value::as_str))
                else {
                    continue;
                };
                let key = (type_name.clone(), id.to_owned());
                let at = *index.entry(key).or_insert_with(|| {
                    let label = schema
                        .node(type_name)
                        .and_then(|node| node.label_property())
                        .and_then(|label| value.get(label))
                        .and_then(Value::as_str)
                        .filter(|label| *label != id)
                        .map(str::to_owned);
                    nodes.push(ResultNode {
                        type_name: type_name.clone(),
                        id: id.to_owned(),
                        label,
                    });
                    nodes.len() - 1
                });
                if let Some(previous) = previous
                    && previous != at
                    && linked.insert((previous, at))
                {
                    links.push((previous, at));
                }
                previous = Some(at);
            }
        }
        Some((nodes, links))
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

    /// A column's width in pixels: wide enough for its header ([`header_width`]) and the
    /// first rows' values, within bounds, so wide values do not push every other column off
    /// screen.
    pub fn column_width(&self, column: usize) -> f32 {
        let ty = self.types.get(column).and_then(Option::as_deref);
        let header = header_text_width(&self.columns[column], ty);
        let values = self
            .rows
            .iter()
            .take(200)
            .map(|row| studio_ui::format_value(&row[column]).chars().count())
            .max()
            .unwrap_or(0) as f32
            * CHAR;
        (header.max(values) + PADDING).clamp(MIN_WIDTH, MAX_WIDTH)
    }
}

/// A character of a cell's text, and of a header's type, in pixels.
const CHAR: f32 = 7.5;
const TYPE_CHAR: f32 = 6.6;
/// The header's gap between name and type, and a cell's inset on both sides.
const GAP: f32 = studio_ui::ds::space::SM;
const PADDING: f32 = 2. * studio_ui::ds::space::LG;
const MIN_WIDTH: f32 = 72.;
const MAX_WIDTH: f32 = 360.;

/// A column's width for its header alone: the title, then its type beside it in smaller text,
/// within the bounds every column keeps. The columns a query will return are this wide before
/// it runs, so a run whose values fit leaves them as they were.
pub fn header_width(title: &str, ty: Option<&str>) -> f32 {
    (header_text_width(title, ty) + PADDING).clamp(MIN_WIDTH, MAX_WIDTH)
}

fn header_text_width(title: &str, ty: Option<&str>) -> f32 {
    let title = title.chars().count() as f32 * CHAR;
    match ty {
        Some(ty) => title + GAP + ty.chars().count() as f32 * TYPE_CHAR,
        None => title,
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
        /// The details as named facts, readable, in the server's order (see [`detail_facts`]).
        facts: Vec<(String, String)>,
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
                facts: detail_facts(&detail),
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

/// Details that say how the engine keeps its books, not what the operator does: identities,
/// table versions, statistics, tie-breaks.
const BOOKKEEPING: &[&str] = &[
    "id",
    "properties",
    "version",
    "versions",
    "tiebreak",
    "edge_binding",
    "resume",
];

/// An operator's details as facts to read: JSON details become `(key, value)` pairs, where a
/// filter is its text, an edge selection is `Knows out`, a list is its items, and bookkeeping
/// is left out; a text detail (DataFusion's) is one fact without a name.
pub fn detail_facts(detail: &str) -> Vec<(String, String)> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(detail) else {
        return if detail.is_empty() {
            Vec::new()
        } else {
            vec![(String::new(), detail.to_owned())]
        };
    };
    map.iter()
        .filter(|(key, _)| !BOOKKEEPING.contains(&key.as_str()))
        .filter(|(key, value)| !(key.as_str() == "bytes" && value.as_u64() == Some(0)))
        .filter_map(|(key, value)| Some((key.clone(), readable(value)?)))
        .collect()
}

/// A detail value as words: `None` for nothing to say.
fn readable(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(items) => {
            let filters = items.iter().all(|item| item.get("text").is_some());
            let parts: Vec<String> = items.iter().filter_map(readable).collect();
            (!parts.is_empty()).then(|| parts.join(if filters { " and " } else { ", " }))
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                return Some(text.to_owned());
            }
            if let Some(Value::Array(members)) = map.get("members") {
                let edges: Vec<String> = members
                    .iter()
                    .map(|member| {
                        let edge = member.get("edge_type").and_then(Value::as_str);
                        let direction = member.get("direction").and_then(Value::as_str);
                        [edge, direction]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect();
                return Some(edges.join(" | "));
            }
            Some(value.to_string())
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

/// Where the text sent to the server came from, so a position the server reports can be
/// mapped back into the editor: the settings prefix, which starts at `prefix_start` in the
/// editor, is copied as is, then `inserted` bytes the editor added (`explain `), then the
/// declaration that starts at `start` in the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SourceMap {
    pub prefix_start: usize,
    pub prefix_len: usize,
    pub inserted: usize,
    pub start: usize,
}

impl SourceMap {
    pub fn editor_offset(self, sent: usize) -> usize {
        if sent < self.prefix_len {
            self.prefix_start + sent
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
    use studio_ui::value_text;

    use super::*;

    fn column(name: &str, kind: &str, nullable: bool) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            kind: kind.into(),
            nullable,
            node_type: None,
        }
    }

    #[test]
    fn node_columns_become_a_graph_of_distinct_nodes_linked_by_row() {
        let schema = SchemaModel::parse(include_str!("../../../dev/cluster/knowledge.pg")).unwrap();
        let mut person = column("p", "Object", false);
        person.node_type = Some("Person".into());
        let mut company = column("c", "Object", false);
        company.node_type = Some("Company".into());
        let rows = serde_json::json!([
            {"p": {"@id": "p1", "name": "Ada"}, "c": {"@id": "c1", "name": "Acme"}},
            {"p": {"@id": "p2", "name": "Bob"}, "c": {"@id": "c1", "name": "Acme"}},
            {"p": {"@id": "p1", "name": "Ada"}, "c": {"@id": "c1", "name": "Acme"}}
        ]);
        let table = ResultTable::new(vec!["p".into(), "c".into()], rows, Some(&[person, company]));
        let (nodes, links) = table.graph(&schema).unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["p1", "c1", "p2"]);
        assert_eq!(nodes[0].label.as_deref(), Some("Ada"));
        assert_eq!(links, [(0, 1), (2, 1)]);
        let plain = ResultTable::new(vec!["n".into()], serde_json::json!([{"n": 1}]), None);
        assert!(plain.graph(&schema).is_none());
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
            rows.clone(),
            Some(&expected),
        );
        assert_eq!(table.rows[1], vec![json!("Alan"), Value::Null]);
        assert_eq!(table.types, [Some("String".into()), Some("Date?".into())]);
        // A different column count means the compiler described another query.
        let table = ResultTable::new(vec!["p.name".into()], rows, Some(&expected));
        assert_eq!(table.types, [None]);
        assert!(table.row_json(0).unwrap().contains("\"p.name\": \"Ada\""));
        assert!(table.row_json(5).is_none());
        // A column named twice shows its cell twice.
        let twice = ResultTable::new(vec!["a".into(), "a".into()], json!([{"a": 1}]), None);
        assert_eq!(twice.rows, [vec![json!(1), json!(1)]]);
    }

    #[test]
    fn column_widths_follow_content_within_bounds() {
        let rows = json!([{"a": "x", "b": "a very long value that goes on and on and on and on and on and on"}]);
        let table = ResultTable::new(vec!["a".into(), "b".into()], rows, None);
        assert_eq!(table.column_width(0), 72.);
        assert_eq!(table.column_width(1), 360.);
        // A narrow value under a title and its type: the header sets the width, so the title is
        // never cut.
        let mut counted = ResultTable::new(vec!["friends".into()], json!([{"friends": 1}]), None);
        counted.types = vec![Some("BigInt?".into())];
        assert!(counted.column_width(0) >= 7. * 7.5 + 6. + 7. * 6.6 + 24.);
    }

    #[test]
    fn copied_cells_are_plain_text() {
        assert_eq!(value_text(&json!("Ada")), "Ada");
        assert_eq!(value_text(&json!(["a", "b"])), "[\"a\",\"b\"]");
        assert_eq!(value_text(&Value::Null), "");
        assert_eq!(value_text(&json!(4)), "4");
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
        let facts = |ix: usize| match &items[ix] {
            PlanItem::Operator { facts, .. } => facts.clone(),
            other => panic!("not an operator: {other:?}"),
        };
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        // Bookkeeping (`id`, `properties`) is left out.
        assert_eq!(
            facts(2),
            [pair("binding", "p"), pair("table", "node:Person")]
        );
        assert_eq!(items[3], PlanItem::Header("DataFusion plan".into()));
        assert_eq!(facts(4), [pair("", "rows=3")]);
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
    fn details_read_as_facts() {
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        assert_eq!(
            detail_facts(
                r#"{"src":"p","dst":"f","edges":{"kind":"named","members":[{"edge_type":"Knows","direction":"out"},{"edge_type":"WorksAt","direction":"in"}]},"min_hops":1,"edge_binding":null,"versions":{"Knows":1}}"#
            ),
            [
                pair("src", "p"),
                pair("dst", "f"),
                pair("edges", "Knows out | WorksAt in"),
                pair("min_hops", "1"),
            ]
        );
        assert_eq!(
            detail_facts(
                r#"{"conjuncts":[{"kind":"gq","text":"$p.age > 3"},{"kind":"gq","text":"$p.name != $f.name"}]}"#
            ),
            [pair("conjuncts", "$p.age > 3 and $p.name != $f.name")]
        );
        assert_eq!(
            detail_facts(
                r#"{"binding":"p","table":"node:Person","version":139,"projection":["__id","name"],"filter":{"kind":"gq","reads":[],"text":"$p.x = 1"}}"#
            ),
            [
                pair("binding", "p"),
                pair("table", "node:Person"),
                pair("projection", "__id, name"),
                pair("filter", "$p.x = 1"),
            ]
        );
        assert_eq!(
            detail_facts(r#"{"rows":5,"bytes":0,"resume":null}"#),
            [pair("rows", "5")]
        );
        assert!(detail_facts("").is_empty());
    }

    #[test]
    fn small_formats() {
        assert_eq!(format_elapsed(Duration::from_millis(34)), "34 ms");
        assert_eq!(format_elapsed(Duration::from_millis(1250)), "1.2 s");
        assert_eq!(rows_label(1), "1 row");
        assert_eq!(rows_label(0), "0 rows");
    }

    #[test]
    fn server_positions_map_back_into_the_editor() {
        // The editor holds `set x = 1;\n` (11 bytes), a first query at 11..40, and the
        // explained query at 41; the server saw `set x = 1;\nexplain query b…`.
        let map = SourceMap {
            prefix_len: 11,
            inserted: 8,
            start: 41,
            ..Default::default()
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
        // Two lines typed above everything while it ran: both parts moved down by them.
        let moved = SourceMap {
            prefix_start: 2,
            start: 43,
            ..map
        };
        assert_eq!(moved.editor_offset(3), 5);
        assert_eq!(moved.editor_offset(25), 49);
    }
}
