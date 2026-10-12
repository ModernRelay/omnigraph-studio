//! The table's pure state: columns from the schema, rows from query results, keyset paging
//! pinned to one commit, value parsing for edits and filters, and how a cell save ended. No
//! GPUI, so every rule here is unit-tested.

use std::cmp::Ordering;

use gq_language::builder::edge_property_column;
use gq_language::input::parse_scalar;
pub use gq_language::input::{edit_text, parse_input};
use gq_language::{EdgeType, Filter, FilterOp, NodeType, Property, ScalarKind};
use graph_client::WriteOutcome;
use graph_client::api::ChangeOutput;
use serde_json::{Map, Value};

/// The system column holding a node's or edge's `@id`.
pub const ID_COLUMN: &str = "@id";
/// An edge row's source and target ids.
pub const FROM_COLUMN: &str = "@from";
pub const TO_COLUMN: &str = "@to";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Node,
    Edge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnRole {
    /// The pinned column: the node's key, or `@id` when it has none.
    Key,
    /// An edge's source or target node, with the node type.
    Endpoint { node_type: String },
    /// A declared property.
    Property(Property),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSpec {
    /// The key into [`Row::values`].
    pub key: String,
    pub title: String,
    /// The declared type, shown under the title.
    pub detail: String,
    pub role: ColumnRole,
    pub width: f32,
    pub align_right: bool,
}

impl ColumnSpec {
    pub fn property(&self) -> Option<&Property> {
        match &self.role {
            ColumnRole::Property(property) => Some(property),
            _ => None,
        }
    }

    /// Whether a click on this column edits it (rather than peeking the row).
    pub fn editable(&self, kind: TableKind) -> bool {
        kind == TableKind::Node && self.property().is_some_and(Property::editable_in_cell)
    }
}

fn width_for(property: &Property) -> f32 {
    if property.list {
        return 220.;
    }
    match property.kind {
        ScalarKind::Bool => 90.,
        ScalarKind::Integer | ScalarKind::Float => 110.,
        ScalarKind::Date => 120.,
        ScalarKind::DateTime => 190.,
        ScalarKind::Blob => 100.,
        ScalarKind::Text | ScalarKind::Vector => {
            if property.enum_values.is_some() {
                120.
            } else {
                200.
            }
        }
    }
}

fn property_column(property: &Property, key: String) -> ColumnSpec {
    let numeric =
        !property.list && matches!(property.kind, ScalarKind::Integer | ScalarKind::Float);
    ColumnSpec {
        key,
        title: property.name.clone(),
        detail: property.display_type(),
        role: ColumnRole::Property(property.clone()),
        width: width_for(property),
        align_right: numeric,
    }
}

/// A node table's columns: the pinned key (or `@id`), then properties in schema order. Vector
/// properties are hidden; Blob properties show as "Blob".
pub fn node_columns(node: &NodeType) -> Vec<ColumnSpec> {
    let mut columns = Vec::new();
    match node.key.as_deref().and_then(|key| node.property(key)) {
        Some(key) => columns.push(ColumnSpec {
            key: key.name.clone(),
            title: key.name.clone(),
            detail: format!("{} · key", key.display_type()),
            role: ColumnRole::Key,
            width: 190.,
            align_right: false,
        }),
        None => columns.push(ColumnSpec {
            key: ID_COLUMN.into(),
            title: ID_COLUMN.into(),
            detail: "id".into(),
            role: ColumnRole::Key,
            width: 240.,
            align_right: false,
        }),
    }
    for property in &node.properties {
        if property.is_key && node.key.as_deref() == Some(property.name.as_str()) {
            continue;
        }
        if property.kind == ScalarKind::Vector {
            continue;
        }
        columns.push(property_column(property, property.name.clone()));
    }
    columns
}

/// An edge table's columns: the source (pinned), the target, then the edge's properties.
pub fn edge_columns(edge: &EdgeType) -> Vec<ColumnSpec> {
    let mut columns = vec![
        ColumnSpec {
            key: FROM_COLUMN.into(),
            title: format!("from {}", edge.from),
            detail: "@id".into(),
            role: ColumnRole::Endpoint {
                node_type: edge.from.clone(),
            },
            width: 200.,
            align_right: false,
        },
        ColumnSpec {
            key: TO_COLUMN.into(),
            title: format!("to {}", edge.to),
            detail: "@id".into(),
            role: ColumnRole::Endpoint {
                node_type: edge.to.clone(),
            },
            width: 200.,
            align_right: false,
        },
    ];
    for property in edge.properties.iter().filter(|p| p.projectable()) {
        columns.push(property_column(property, property.name.clone()));
    }
    columns
}

/// One row as read: its `@id` and its values by column key. A null cell is absent.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub values: Map<String, Value>,
    /// The row no longer matches the filters after an edit. It stays in place until the list
    /// is read again (constitution V).
    pub dimmed: bool,
}

impl Row {
    pub fn value(&self, column: &str) -> &Value {
        self.values.get(column).unwrap_or(&Value::Null)
    }
}

/// Rows of `node_page` / `node_by_id`: each result row is `{"n": {"@id": …, props…}}`.
pub fn node_rows(rows: &Value) -> Vec<Row> {
    let Some(rows) = rows.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let node = row.get("n")?.as_object()?;
            let id = node.get(ID_COLUMN)?.as_str()?.to_owned();
            let values = node
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            Some(Row {
                id,
                values,
                dimmed: false,
            })
        })
        .collect()
}

/// Rows of `edge_page`: `edge_id`, `from_id`, `to_id` and `e_<property>` columns.
pub fn edge_rows(edge: &EdgeType, rows: &Value) -> Vec<Row> {
    let Some(rows) = rows.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let row = row.as_object()?;
            let id = row.get("edge_id")?.as_str()?.to_owned();
            let mut values = Map::new();
            values.insert(ID_COLUMN.into(), Value::String(id.clone()));
            for (column, key) in [("from_id", FROM_COLUMN), ("to_id", TO_COLUMN)] {
                if let Some(value) = row.get(column).filter(|v| !v.is_null()) {
                    values.insert(key.into(), value.clone());
                }
            }
            for property in &edge.properties {
                if let Some(value) = row
                    .get(&edge_property_column(&property.name))
                    .filter(|v| !v.is_null())
                {
                    values.insert(property.name.clone(), value.clone());
                }
            }
            Some(Row {
                id,
                values,
                dimmed: false,
            })
        })
        .collect()
}

/// The single count a `count(…) as n` query returns.
pub fn count_of(rows: &Value) -> Option<i64> {
    rows.as_array()?.first()?.get("n")?.as_i64()
}

/// Keyset paging pinned to the commit the first page read (constitution II).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Paging {
    /// The `graph_commit_id` every read of this view is pinned to.
    pub pinned: Option<String>,
    /// The last loaded row's `@id`: the next page starts after it.
    pub after: Option<String>,
    /// The last page came back short; there is nothing more to read.
    pub exhausted: bool,
}

impl Paging {
    /// Forget the rows but keep the pin: the same view re-read with other filters.
    pub fn restart(&mut self) {
        self.after = None;
        self.exhausted = false;
    }

    /// Forget everything, the pin included: Refresh.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Records a page. The first page's commit becomes the pin; later pages were read at it.
    pub fn accept(&mut self, commit: Option<&str>, page: &[Row], page_size: u32) {
        if self.pinned.is_none() {
            self.pinned = commit.map(str::to_owned);
        }
        if let Some(last) = page.last() {
            self.after = Some(last.id.clone());
        }
        self.exhausted = page.len() < page_size as usize;
    }

    /// Whether the viewport, which ends at row `visible_end`, has passed 80% of the `loaded`
    /// rows and another page should be read.
    pub fn wants_more(&self, loaded: usize, visible_end: usize, loading: bool) -> bool {
        !self.exhausted && !loading && loaded > 0 && visible_end * 5 >= loaded * 4
    }

    /// A write this view made committed. When it was made on top of the pinned commit, the
    /// view holds exactly the new commit's state and the pin moves with it; otherwise someone
    /// else committed in between and the view is now behind. Returns whether it is behind.
    pub fn advance_for_own_commit(&mut self, parent: Option<&str>, commit: &str) -> bool {
        if self.pinned.is_some() && self.pinned.as_deref() == parent {
            self.pinned = Some(commit.to_owned());
            false
        } else {
            true
        }
    }
}

/// Where a new row belongs among rows ordered by `@id`, or `None` when it falls after the last
/// loaded row of an unfinished list (a later page will bring it).
pub fn insert_position(rows: &[Row], id: &str, exhausted: bool) -> Option<usize> {
    let position = rows.partition_point(|row| row.id.as_str() < id);
    if position == rows.len() && !exhausted {
        None
    } else {
        Some(position)
    }
}

// ---- values ----

/// How a value reads in a cell. Blob values are not read inline.
pub fn display_value(property: Option<&Property>, value: &Value) -> String {
    if value.is_null() {
        return "—".into();
    }
    if property.is_some_and(|p| p.kind == ScalarKind::Blob) {
        return "Blob".into();
    }
    studio_ui::format_value(value)
}

/// The text copied for a cell: the raw string, or JSON for anything else.
pub fn copy_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Parses a filter's value: one element for a list's membership test.
pub fn parse_filter_value(property: &Property, text: &str) -> Result<Value, String> {
    let element = Property {
        list: false,
        nullable: false,
        ..property.clone()
    };
    if text.trim().is_empty() {
        return Err("Enter a value".into());
    }
    parse_scalar(&element, text)
}

/// The values of an "is one of" filter, separated by commas: `Founder, Engineer`.
pub fn parse_filter_list(property: &Property, text: &str) -> Result<Value, String> {
    let element = Property {
        list: false,
        nullable: false,
        ..property.clone()
    };
    let values = text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| parse_scalar(&element, part))
        .collect::<Result<Vec<_>, _>>()?;
    if values.is_empty() {
        return Err("Enter values separated by commas".into());
    }
    Ok(Value::Array(values))
}

/// Where a date preset starts at `now`: today, the Monday of this week, or 30 days ago; a
/// `Date` as `YYYY-MM-DD`, a `DateTime` as that local midnight in UTC.
pub fn preset_start(
    op: FilterOp,
    kind: Option<ScalarKind>,
    now: chrono::DateTime<chrono::Local>,
) -> Value {
    use chrono::{Datelike as _, Duration, TimeZone as _};
    let today = now.date_naive();
    let start = match op {
        FilterOp::ThisWeek => today - Duration::days(today.weekday().num_days_from_monday() as i64),
        FilterOp::Last30Days => today - Duration::days(30),
        _ => today,
    };
    match kind {
        Some(ScalarKind::DateTime) => {
            let midnight = start.and_hms_opt(0, 0, 0).expect("midnight exists");
            let local = chrono::Local
                .from_local_datetime(&midnight)
                .earliest()
                .unwrap_or(now);
            Value::String(
                local
                    .with_timezone(&chrono::Utc)
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string(),
            )
        }
        _ => Value::String(start.format("%Y-%m-%d").to_string()),
    }
}

fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Whether two cell values are the same value (`1` and `1.0` are).
pub fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_value(a, b))
        }
        _ => a == b || compare(a, b) == Some(Ordering::Equal),
    }
}

/// Whether a row's values satisfy every filter, with the engine's semantics: a comparison with
/// a null is false; `contains` is a substring test on text and membership on lists.
/// Relationship filters are not checked here: a row's relationships are not in the row.
pub fn row_matches(filters: &[Filter], values: &Map<String, Value>) -> bool {
    filters.iter().all(|filter| {
        let value = values.get(&filter.property).unwrap_or(&Value::Null);
        match filter.op {
            _ if filter.property == gq_language::builder::ID_FILTER => true,
            FilterOp::HasAny | FilterOp::HasNone | FilterOp::RelatedTo => true,
            FilterOp::IsEmpty => value.is_null(),
            FilterOp::IsNotEmpty => !value.is_null(),
            _ if value.is_null() => false,
            FilterOp::In => filter
                .value
                .as_array()
                .is_some_and(|items| items.iter().any(|item| same_value(value, item))),
            FilterOp::Contains => match (value, &filter.value) {
                (Value::String(text), Value::String(part)) => text.contains(part.as_str()),
                (Value::Array(items), wanted) => items.iter().any(|v| same_value(v, wanted)),
                _ => false,
            },
            FilterOp::Eq => same_value(value, &filter.value),
            FilterOp::Ne => compare(value, &filter.value).is_some_and(|o| o != Ordering::Equal),
            FilterOp::Lt => compare(value, &filter.value) == Some(Ordering::Less),
            FilterOp::Le => matches!(
                compare(value, &filter.value),
                Some(Ordering::Less | Ordering::Equal)
            ),
            FilterOp::Gt => compare(value, &filter.value) == Some(Ordering::Greater),
            FilterOp::Ge | FilterOp::Today | FilterOp::ThisWeek | FilterOp::Last30Days => {
                matches!(
                    compare(value, &filter.value),
                    Some(Ordering::Greater | Ordering::Equal)
                )
            }
        }
    })
}

/// A filter as a chip reads: `title is Engineer`, `email is empty`, `has worksAt →`.
pub fn filter_label(filter: &Filter) -> String {
    if let Some(relationship) = gq_language::builder::relationship_label(&filter.property) {
        return match filter.op {
            FilterOp::HasAny => format!("has {relationship}"),
            FilterOp::HasNone => format!("no {relationship}"),
            _ => format!("{relationship} {}", edit_text(&filter.value)),
        };
    }
    if filter.op.takes_value() {
        format!(
            "{} {} {}",
            filter.property,
            filter.op.label(),
            edit_text(&filter.value)
        )
    } else {
        format!("{} {}", filter.property, filter.op.label())
    }
}

// ---- write outcomes ----

/// How a cell edit, insert or delete reads back to the person (constitution III).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The value changed elsewhere; nothing was written.
    Conflict,
    /// The server refused; nothing was written.
    Refused,
    /// The response was lost.
    Unknown,
    /// Input that cannot be sent.
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub tone: Tone,
    pub message: String,
}

impl Notice {
    pub fn new(tone: Tone, message: impl Into<String>) -> Self {
        Self {
            tone,
            message: message.into(),
        }
    }
}

/// How one value-bound write ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveResult {
    /// Published as `commit`, on top of `parent`.
    Saved {
        commit: String,
        parent: Option<String>,
    },
    /// The predicate matched nothing: the bound value (or the row) changed elsewhere.
    NoMatch,
    Refused(Notice),
    Unknown(Notice),
}

pub const UNKNOWN_MESSAGE: &str = "Check History before trying again; your changes are still here";

/// Classifies a write's outcome. `branch` names the write target in a denial.
pub fn save_result(outcome: &WriteOutcome<ChangeOutput>, branch: &str) -> SaveResult {
    match outcome {
        WriteOutcome::Committed(output) => match &output.commit {
            Some(commit) if output.affected_nodes + output.affected_edges > 0 => {
                SaveResult::Saved {
                    commit: commit.graph_commit_id.clone(),
                    parent: commit.parent_commit_id.clone(),
                }
            }
            _ => SaveResult::NoMatch,
        },
        WriteOutcome::Refused(error) => SaveResult::Refused(refusal(
            error.status,
            &error.body.error,
            error.body.precondition_failure.is_some(),
            branch,
        )),
        WriteOutcome::Unknown { .. } => {
            SaveResult::Unknown(Notice::new(Tone::Unknown, UNKNOWN_MESSAGE))
        }
    }
}

fn refusal(status: u16, reason: &str, precondition: bool, branch: &str) -> Notice {
    let message = if status == 403 {
        format!("Your token cannot change {branch}. Nothing was written.")
    } else if status == 412 || precondition {
        format!(
            "Nothing was written. {branch} moved since this table was read; refresh it, then try again."
        )
    } else {
        format!("Nothing was written. {reason}")
    };
    Notice::new(Tone::Refused, message)
}

/// The message a cell reopens with when its bound value changed elsewhere.
pub fn changed_elsewhere(property: Option<&Property>, now: Option<&Value>) -> Notice {
    match now {
        Some(value) => Notice::new(
            Tone::Conflict,
            format!(
                "Changed elsewhere to “{}” while you were editing",
                display_value(property, value)
            ),
        ),
        None => Notice::new(
            Tone::Conflict,
            "This row was deleted elsewhere while you were editing. Nothing was written.",
        ),
    }
}

/// `12480` → `12 480`, with a no-break space.
pub fn format_count(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{a0}');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// The last segment of a graph commit id, for display.
pub fn short_commit(id: &str) -> String {
    let tail = id.rsplit('.').next().unwrap_or(id);
    let start = tail.len().saturating_sub(8);
    tail[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use gq_language::SchemaModel;
    use graph_client::api::{CommitOutput, ErrorOutput, PreconditionFailureOutput};
    use graph_client::{ApiError, WriteOutcome};
    use serde_json::json;

    use super::*;

    fn schema() -> SchemaModel {
        SchemaModel::parse(include_str!("../../../dev/cluster/knowledge.pg")).unwrap()
    }

    fn prop(type_name: &str, name: &str) -> Property {
        schema()
            .node(type_name)
            .unwrap()
            .property(name)
            .unwrap()
            .clone()
    }

    #[test]
    fn node_columns_pin_the_key_and_follow_schema_order() {
        let schema = schema();
        let columns = node_columns(schema.node("Person").unwrap());
        let keys: Vec<_> = columns.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["slug", "name", "title", "email", "born", "tags"]);
        assert_eq!(columns[0].role, ColumnRole::Key);
        assert_eq!(columns[4].detail, "Date?");
        assert!(!columns[0].editable(TableKind::Node));
        assert!(columns[1].editable(TableKind::Node));
        assert!(!columns[1].editable(TableKind::Edge));
        let company = node_columns(schema.node("Company").unwrap());
        assert!(
            company
                .iter()
                .find(|c| c.key == "founded")
                .unwrap()
                .align_right
        );
    }

    #[test]
    fn edge_columns_lead_with_endpoints() {
        let schema = schema();
        let columns = edge_columns(schema.edge("WorksAt").unwrap());
        let keys: Vec<_> = columns.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, [FROM_COLUMN, TO_COLUMN, "role", "since"]);
        assert_eq!(columns[0].title, "from Person");
    }

    #[test]
    fn reads_node_and_edge_rows() {
        let rows = node_rows(&json!([
            {"n": {"@id": "p1", "slug": "p1", "name": "Ada", "title": null}},
            {"n": {"slug": "no-id"}},
        ]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "p1");
        assert_eq!(rows[0].value("name"), &json!("Ada"));
        assert_eq!(rows[0].value("title"), &Value::Null);
        assert!(!rows[0].values.contains_key("title"));

        let schema = schema();
        let edges = edge_rows(
            schema.edge("WorksAt").unwrap(),
            &json!([{"edge_id": "e1", "from_id": "p1", "to_id": "c1", "e_role": "lead"}]),
        );
        assert_eq!(edges[0].id, "e1");
        assert_eq!(edges[0].value(FROM_COLUMN), &json!("p1"));
        assert_eq!(edges[0].value("role"), &json!("lead"));
        assert_eq!(count_of(&json!([{"n": 37}])), Some(37));
    }

    fn rows(ids: &[&str]) -> Vec<Row> {
        ids.iter()
            .map(|id| Row {
                id: (*id).into(),
                values: Map::new(),
                dimmed: false,
            })
            .collect()
    }

    #[test]
    fn paging_pins_the_first_commit_and_pages_by_key() {
        let mut paging = Paging::default();
        paging.accept(Some("c1"), &rows(&["a", "b"]), 2);
        assert_eq!(paging.pinned.as_deref(), Some("c1"));
        assert_eq!(paging.after.as_deref(), Some("b"));
        assert!(!paging.exhausted);
        // A later page read at the pin cannot move it.
        paging.accept(Some("c9"), &rows(&["c"]), 2);
        assert_eq!(paging.pinned.as_deref(), Some("c1"));
        assert_eq!(paging.after.as_deref(), Some("c"));
        assert!(paging.exhausted);
        // New filters keep the pin; Refresh drops it.
        paging.restart();
        assert_eq!(paging.pinned.as_deref(), Some("c1"));
        assert!(paging.after.is_none() && !paging.exhausted);
        paging.reset();
        assert!(paging.pinned.is_none());
    }

    #[test]
    fn next_page_is_read_past_eighty_percent() {
        let paging = Paging::default();
        assert!(!paging.wants_more(100, 79, false));
        assert!(paging.wants_more(100, 80, false));
        assert!(!paging.wants_more(100, 100, true));
        assert!(!paging.wants_more(0, 0, false));
        let done = Paging {
            exhausted: true,
            ..Paging::default()
        };
        assert!(!done.wants_more(100, 100, false));
    }

    #[test]
    fn own_commits_move_the_pin_only_from_the_pinned_parent() {
        let mut paging = Paging {
            pinned: Some("c1".into()),
            ..Paging::default()
        };
        assert!(!paging.advance_for_own_commit(Some("c1"), "c2"));
        assert_eq!(paging.pinned.as_deref(), Some("c2"));
        // Someone else committed c3 first: the view is now behind.
        assert!(paging.advance_for_own_commit(Some("c3"), "c4"));
        assert_eq!(paging.pinned.as_deref(), Some("c2"));
    }

    #[test]
    fn inserted_rows_find_their_place() {
        let loaded = rows(&["b", "d"]);
        assert_eq!(insert_position(&loaded, "a", false), Some(0));
        assert_eq!(insert_position(&loaded, "c", false), Some(1));
        assert_eq!(insert_position(&loaded, "e", false), None);
        assert_eq!(insert_position(&loaded, "e", true), Some(2));
    }

    #[test]
    fn parses_input_by_property_type() {
        assert_eq!(
            parse_input(&prop("Person", "title"), "CEO"),
            Ok(json!("CEO"))
        );
        assert_eq!(parse_input(&prop("Person", "title"), "  "), Ok(Value::Null));
        assert!(parse_input(&prop("Person", "name"), "").is_err());
        assert_eq!(
            parse_input(&prop("Company", "founded"), " 1999 "),
            Ok(json!(1999))
        );
        assert_eq!(
            parse_input(&prop("Company", "founded"), "199x"),
            Err("Enter a whole number".into())
        );
        assert_eq!(
            parse_input(&prop("Person", "born"), "1980-02-30"),
            Ok(json!("1980-02-30"))
        );
        assert!(parse_input(&prop("Person", "born"), "1980-13-01").is_err());
        assert!(parse_input(&prop("Person", "born"), "yesterday").is_err());
        assert_eq!(
            parse_input(&prop("Person", "tags"), "a, b,,c "),
            Ok(json!(["a", "b", "c"]))
        );
        assert_eq!(
            parse_input(&prop("Project", "status"), "done"),
            Ok(json!("done"))
        );
        assert_eq!(
            parse_input(&prop("Project", "status"), "finished"),
            Err("Use one of: active, done, paused".into())
        );
        assert_eq!(
            parse_filter_value(&prop("Person", "tags"), "graphs"),
            Ok(json!("graphs"))
        );
        assert_eq!(edit_text(&json!(["a", "b"])), "a, b");
        assert_eq!(edit_text(&Value::Null), "");
        assert_eq!(edit_text(&json!(42)), "42");
    }

    fn filter(property: &str, op: FilterOp, value: Value) -> Filter {
        Filter {
            property: property.into(),
            op,
            value,
        }
    }

    #[test]
    fn rows_are_matched_like_the_engine() {
        let values = json!({"title": "Engineer", "born": "1980-05-01", "tags": ["graphs"], "n": 3})
            .as_object()
            .unwrap()
            .clone();
        assert!(row_matches(&[], &values));
        assert!(row_matches(
            &[filter("title", FilterOp::Eq, json!("Engineer"))],
            &values
        ));
        assert!(!row_matches(
            &[filter("title", FilterOp::Ne, json!("Engineer"))],
            &values
        ));
        assert!(row_matches(
            &[filter("born", FilterOp::Gt, json!("1979-12-31"))],
            &values
        ));
        assert!(row_matches(
            &[filter("n", FilterOp::Le, json!(3.0))],
            &values
        ));
        assert!(row_matches(
            &[filter("title", FilterOp::Contains, json!("gine"))],
            &values
        ));
        assert!(row_matches(
            &[filter("tags", FilterOp::Contains, json!("graphs"))],
            &values
        ));
        assert!(row_matches(
            &[filter("email", FilterOp::IsEmpty, Value::Null)],
            &values
        ));
        // A comparison with a missing value is false, even "is not".
        assert!(!row_matches(
            &[filter("email", FilterOp::Ne, json!("x"))],
            &values
        ));
        assert_eq!(
            filter_label(&filter("title", FilterOp::Eq, json!("CEO"))),
            "title is CEO"
        );
        assert_eq!(
            filter_label(&filter("email", FilterOp::IsEmpty, Value::Null)),
            "email is empty"
        );
    }

    fn committed(affected: i64, parent: Option<&str>) -> WriteOutcome<ChangeOutput> {
        WriteOutcome::Committed(ChangeOutput {
            branch: "main".into(),
            query_name: "studio_set".into(),
            affected_nodes: affected,
            affected_edges: 0,
            commit: (affected > 0).then(|| CommitOutput {
                graph_commit_id: "c2".into(),
                graph_manifest_version: 2,
                created_at: 0,
                graph_branch: None,
                parent_commit_id: parent.map(str::to_owned),
                merged_parent_commit_id: None,
                actor_id: None,
            }),
            actor_id: None,
            outcome: None,
        })
    }

    fn refused(status: u16, precondition: bool) -> WriteOutcome<ChangeOutput> {
        WriteOutcome::Refused(ApiError {
            status,
            body: ErrorOutput {
                error: "policy denied".into(),
                code: None,
                merge_conflicts: None,
                precondition_failure: precondition.then(|| PreconditionFailureOutput {
                    expected: "c1".into(),
                    actual: Some("c2".into()),
                }),
                diagnostic: None,
                other: Map::new(),
            },
        })
    }

    #[test]
    fn classifies_write_outcomes() {
        assert_eq!(
            save_result(&committed(1, Some("c1")), "main"),
            SaveResult::Saved {
                commit: "c2".into(),
                parent: Some("c1".into())
            }
        );
        assert_eq!(
            save_result(&committed(0, None), "main"),
            SaveResult::NoMatch
        );
        let SaveResult::Refused(notice) = save_result(&refused(403, false), "main") else {
            panic!()
        };
        assert_eq!(
            notice.message,
            "Your token cannot change main. Nothing was written."
        );
        let SaveResult::Refused(notice) = save_result(&refused(412, true), "main") else {
            panic!()
        };
        assert!(
            notice
                .message
                .starts_with("Nothing was written. main moved")
        );
        let SaveResult::Refused(notice) = save_result(&refused(400, false), "main") else {
            panic!()
        };
        assert_eq!(notice.message, "Nothing was written. policy denied");
        assert_eq!(
            save_result(
                &WriteOutcome::Unknown {
                    reason: "timeout".into()
                },
                "main"
            ),
            SaveResult::Unknown(Notice::new(Tone::Unknown, UNKNOWN_MESSAGE))
        );
        assert_eq!(
            changed_elsewhere(None, Some(&json!("CEO"))).message,
            "Changed elsewhere to “CEO” while you were editing"
        );
    }

    #[test]
    fn formats_counts_and_commits() {
        assert_eq!(format_count(12480), "12\u{a0}480");
        assert_eq!(format_count(200), "200");
        assert_eq!(format_count(1_000_000), "1\u{a0}000\u{a0}000");
        assert_eq!(
            short_commit("hb1.01M4.3.01M4RAGY10QYVXPVTS7S94HEZ3"),
            "7S94HEZ3"
        );
        assert!(same_value(&json!(1), &json!(1.0)));
        assert!(!same_value(&json!(["a"]), &json!(["b"])));
        assert_eq!(display_value(None, &Value::Null), "—");
    }

    #[test]
    fn list_filters_parse_each_value_with_the_propertys_type() {
        assert_eq!(
            parse_filter_list(&prop("Person", "title"), "Founder, Engineer,").unwrap(),
            json!(["Founder", "Engineer"])
        );
        assert!(parse_filter_list(&prop("Person", "born"), "2020-01-01, soon").is_err());
        assert!(parse_filter_list(&prop("Person", "title"), " , ").is_err());
    }

    #[test]
    fn date_presets_start_today_on_monday_or_thirty_days_back() {
        use chrono::TimeZone as _;
        // Wednesday 2026-10-14, mid-afternoon local time.
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 14, 15, 30, 0)
            .unwrap();
        let date = Some(ScalarKind::Date);
        assert_eq!(
            preset_start(FilterOp::Today, date, now),
            json!("2026-10-14")
        );
        assert_eq!(
            preset_start(FilterOp::ThisWeek, date, now),
            json!("2026-10-12")
        );
        assert_eq!(
            preset_start(FilterOp::Last30Days, date, now),
            json!("2026-09-14")
        );
        let instant = preset_start(FilterOp::Today, Some(ScalarKind::DateTime), now);
        let text = instant.as_str().unwrap();
        assert!(text.ends_with('Z') && text.len() == 20, "{text}");
    }

    #[test]
    fn membership_and_presets_match_rows_locally_and_relationships_are_left_to_the_engine() {
        let row: Map<String, Value> = serde_json::from_value(json!({
            "title": "Founder",
            "born": "2026-10-13"
        }))
        .unwrap();
        let filter = |property: &str, op: FilterOp, value: Value| Filter {
            property: property.into(),
            op,
            value,
        };
        assert!(row_matches(
            &[filter(
                "title",
                FilterOp::In,
                json!(["Engineer", "Founder"])
            )],
            &row
        ));
        assert!(!row_matches(
            &[filter("title", FilterOp::In, json!([]))],
            &row
        ));
        assert!(row_matches(
            &[filter("born", FilterOp::ThisWeek, json!("2026-10-12"))],
            &row
        ));
        assert!(!row_matches(
            &[filter("born", FilterOp::Today, json!("2026-10-14"))],
            &row
        ));
        assert!(row_matches(
            &[filter("out:WorksAt", FilterOp::HasNone, Value::Null)],
            &row
        ));
        assert_eq!(
            filter_label(&filter("in:Knows", FilterOp::HasAny, Value::Null)),
            "has ← knows"
        );
    }
}
