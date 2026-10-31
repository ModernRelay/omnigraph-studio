//! The table's pure state: columns from the schema, rows from query results, keyset paging
//! pinned to one commit, value parsing for edits and filters, and how a cell save ended. No
//! GPUI, so every rule here is unit-tested.

use std::cmp::Ordering;

use gq_language::builder::edge_property_column;
use gq_language::input::{compare_values as compare, list_items, parse_scalar, same_value, typed};
pub use gq_language::input::{edit_text, parse_input};
use gq_language::{
    EdgeType, Filter, FilterOp, NodeType, PageStart, Property, RelationshipGroup, ScalarKind,
};
use graph_client::WriteOutcome;
use graph_client::api::{ChangeOutput, CommitOutput};
use graph_store::{HeldAs, OutcomeKind};
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
    /// A node's neighbours through one relationship group: chips, then "+N".
    Relationship(RelationshipGroup),
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

    pub fn relationship(&self) -> Option<&RelationshipGroup> {
        match &self.role {
            ColumnRole::Relationship(group) => Some(group),
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
        // "Blob · 2.1 KB · Preview"
        ScalarKind::Blob => 190.,
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

/// A node table's columns: the pinned key (or `@id`), the properties in schema order, then one
/// column per relationship group in `groups`. Vector properties are hidden; Blob properties
/// show as "Blob".
pub fn node_columns(node: &NodeType, groups: &[RelationshipGroup]) -> Vec<ColumnSpec> {
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
    for group in groups {
        let key = gq_language::builder::relationship_key(group);
        columns.push(ColumnSpec {
            title: gq_language::builder::relationship_label(&key).unwrap_or_else(|| key.clone()),
            key,
            detail: group.other_type.clone(),
            role: ColumnRole::Relationship(group.clone()),
            width: 260.,
            align_right: false,
        });
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
    // As a node table's: a Blob shows as a cell, read on its own; a vector does not show.
    for property in edge
        .properties
        .iter()
        .filter(|p| p.kind != ScalarKind::Vector)
    {
        columns.push(property_column(property, property.name.clone()));
    }
    columns
}

/// Which columns a table shows and in what order (spec 004: a view holds its columns). The
/// first column, the key or an edge's source, is pinned: it always shows, first. The others
/// follow `order`, then any it does not name (a property added since) in schema order. Hiding
/// a column only stops showing it: a filter, an order or a grouping on it still applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnLayout {
    pub hidden: Vec<String>,
    /// The unpinned columns' keys as the person ordered them; empty is schema order.
    pub order: Vec<String>,
    /// Widths the person dragged a column to, in pixels, by column key; the others are fitted
    /// to their type.
    pub widths: std::collections::BTreeMap<String, u32>,
}

#[cfg(test)]
use studio_ui::{COLUMN_MAX_WIDTH, COLUMN_MIN_WIDTH};

impl ColumnLayout {
    pub fn new(hidden: Vec<String>, order: Vec<String>) -> Self {
        Self {
            hidden,
            order,
            widths: Default::default(),
        }
    }

    /// A column's width: the one it was dragged to, else its fitted one.
    pub fn width(&self, column: &ColumnSpec) -> f32 {
        self.widths
            .get(&column.key)
            .map_or(column.width, |width| *width as f32)
    }

    /// Sets the width a column was dragged to, within the bounds; `None` goes back to fitted.
    pub fn set_width(&mut self, key: &str, width: Option<f32>) {
        match width {
            Some(width) => {
                let width = studio_ui::clamp_width(width) as u32;
                self.widths.insert(key.to_owned(), width);
            }
            None => {
                self.widths.remove(key);
            }
        }
    }

    pub fn is_hidden(&self, key: &str) -> bool {
        self.hidden.iter().any(|hidden| hidden == key)
    }

    /// The unpinned columns' keys in this layout's order, hidden ones included.
    fn arranged(&self, all: &[ColumnSpec]) -> Vec<String> {
        let rest = all.get(1..).unwrap_or_default();
        let mut keys: Vec<String> = self
            .order
            .iter()
            .filter(|key| rest.iter().any(|c| c.key == **key))
            .cloned()
            .collect();
        for column in rest {
            if !keys.contains(&column.key) {
                keys.push(column.key.clone());
            }
        }
        keys
    }

    /// Every column in this layout's order, the pinned one first, with whether it shows.
    pub fn columns<'a>(&self, all: &'a [ColumnSpec]) -> Vec<(&'a ColumnSpec, bool)> {
        let Some(first) = all.first() else {
            return Vec::new();
        };
        let mut columns = vec![(first, true)];
        for key in self.arranged(all) {
            if let Some(column) = all.iter().find(|c| c.key == key) {
                columns.push((column, !self.is_hidden(&key)));
            }
        }
        columns
    }

    /// The columns the table shows, in order.
    pub fn shown(&self, all: &[ColumnSpec]) -> Vec<ColumnSpec> {
        self.columns(all)
            .into_iter()
            .filter(|(_, shown)| *shown)
            .map(|(column, _)| column.clone())
            .collect()
    }

    /// Shows a hidden column or hides a shown one; the pinned column always shows.
    pub fn toggle(&mut self, all: &[ColumnSpec], key: &str) {
        if all.first().is_some_and(|first| first.key == key) {
            return;
        }
        match self.hidden.iter().position(|hidden| hidden == key) {
            Some(index) => {
                self.hidden.remove(index);
            }
            None => self.hidden.push(key.to_owned()),
        }
    }

    /// Shows every column, in schema order.
    pub fn reset(&mut self) {
        self.hidden.clear();
        self.order.clear();
    }

    /// Moves a shown column `step` places among the shown ones; the pinned column stays.
    pub fn step(&mut self, all: &[ColumnSpec], key: &str, step: isize) {
        let shown: Vec<String> = self
            .arranged(all)
            .into_iter()
            .filter(|k| !self.is_hidden(k))
            .collect();
        let Some(from) = shown.iter().position(|k| k == key) else {
            return;
        };
        let to = from as isize + step;
        if to < 0 || to as usize >= shown.len() {
            return;
        }
        self.move_onto(all, key, &shown[to as usize]);
    }

    /// Puts `key` in `target`'s place: after it when moving right, before it when moving left.
    pub fn move_onto(&mut self, all: &[ColumnSpec], key: &str, target: &str) {
        let mut keys = self.arranged(all);
        let (Some(from), Some(to)) = (
            keys.iter().position(|k| k == key),
            keys.iter().position(|k| k == target),
        ) else {
            return;
        };
        if from == to {
            return;
        }
        let moved = keys.remove(from);
        keys.insert(to, moved);
        let schema: Vec<&str> = all.iter().skip(1).map(|c| c.key.as_str()).collect();
        self.order = if keys.iter().map(String::as_str).eq(schema) {
            Vec::new()
        } else {
            keys
        };
    }
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

/// What names a node of `node`'s type holding `values`: its label property's text, unless the
/// label is the key, which names it already.
pub fn node_label(node: &NodeType, values: &Map<String, Value>) -> Option<String> {
    let label = node.label_property()?;
    (Some(label) != node.key.as_deref())
        .then(|| values.get(label)?.as_str().map(str::to_owned))
        .flatten()
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

/// Rows of nodes read as fields (`builder::node_fields_by_id`, `builder::nodes_fields_by_id`,
/// `builder::node_page_fields`), shaped as a whole-node read gives them: `@id` and the
/// properties that hold a value.
pub fn field_rows(rows: &Value) -> Vec<Row> {
    let prefix = gq_language::builder::field_column("");
    let Some(rows) = rows.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let row = row.as_object()?;
            let id = row.get("id")?.as_str()?.to_owned();
            let mut values: Map<String, Value> = row
                .iter()
                .filter(|(_, value)| !value.is_null())
                .filter_map(|(column, value)| {
                    let property = column.strip_prefix(prefix.as_str())?;
                    Some((property.to_owned(), value.clone()))
                })
                .collect();
            values.insert(ID_COLUMN.to_owned(), Value::String(id.clone()));
            Some(Row {
                id,
                values,
                dimmed: false,
            })
        })
        .collect()
}

/// Rows of an edge page (`builder::edge_page_without`): `edge_id`, `from_id`, `to_id` and
/// `e_<property>` columns.
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

/// Where the Blob values a view shows hold as it shows them, which a Blob write is bound at: a
/// managed value's validator names its type's table version, so it is read at a commit holding
/// that table as the view shows it.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum BlobsAt {
    /// At the view's pin.
    #[default]
    Pin,
    /// At one of the view's own commits past its pin, made while it was behind its branch.
    Own(String),
    /// At no commit the view knows of: a write is bound to the validators the view shows.
    Unknown,
}

impl BlobsAt {
    /// The commit, for a view pinned at `pin`.
    pub fn commit<'a>(&'a self, pin: Option<&'a str>) -> Option<&'a str> {
        match self {
            BlobsAt::Pin => pin,
            BlobsAt::Own(commit) => Some(commit),
            BlobsAt::Unknown => None,
        }
    }

    /// One of the view's own writes landed at `commit`, made on `parent`; `pin` is the view's
    /// pin as it was sent, and `followed` says the pin moved to `commit`. A write made on the
    /// commit they held at holds them at its own, its changes being what the view shows; any
    /// other leaves a gap the view cannot see into.
    pub fn after_own_write(
        &mut self,
        pin: Option<&str>,
        parent: Option<&str>,
        commit: &str,
        followed: bool,
    ) {
        let on_them = parent.is_some() && self.commit(pin) == parent;
        *self = match (followed, on_them) {
            (true, true) => BlobsAt::Pin,
            (false, true) => BlobsAt::Own(commit.to_owned()),
            (_, false) => BlobsAt::Unknown,
        };
    }

    /// A Blob write bound to a validator read at `bound_at`, where they held as it was sent,
    /// landed at `commit` past the pin: the server applied it only while the type's table was
    /// as it was there, so the commit holds them, with the value written.
    pub fn after_bound_write(&mut self, bound_at: &str, before: Option<&str>, commit: &str) {
        if before == Some(bound_at) {
            *self = BlobsAt::Own(commit.to_owned());
        }
    }
}

/// Keyset paging pinned to the commit the first page read (constitution II).
#[derive(Debug, Clone, PartialEq)]
pub struct Paging {
    /// The `graph_commit_id` every read of this view is pinned to.
    pub pinned: Option<String>,
    /// Where the next page starts.
    pub start: PageStart,
    /// The last page came back short; there is nothing more to read.
    pub exhausted: bool,
    /// The pin moved to one of the view's own commits since its first page: later pages are
    /// read there, where a row already read may sort again further on.
    pub moved: bool,
    /// Where the Blob values the view shows hold.
    pub blobs: BlobsAt,
    /// How many rows the last page asked for: a page with fewer is the last.
    pub page_size: u32,
    /// The graph's page size: the next page is read while no more than this many rows are
    /// left past the rows in view ([`Self::wants_more`]).
    pub ahead: u32,
}

/// The most a page asks for, as a multiple of the graph's page size ([`Paging::next_page_size`]).
const MAX_PAGE_GROWTH: u32 = 8;

impl Default for Paging {
    fn default() -> Self {
        Self {
            pinned: None,
            start: PageStart::First,
            exhausted: false,
            moved: false,
            blobs: BlobsAt::Pin,
            page_size: 0,
            ahead: 0,
        }
    }
}

impl Paging {
    /// Forget the rows but keep the pin: the same view re-read with other filters or order.
    /// Every page is read at the pin from now on; the Blob values read stay as they were.
    pub fn restart(&mut self) {
        self.start = PageStart::First;
        self.exhausted = false;
        self.moved = false;
    }

    /// The commit the Blob values the view shows hold at ([`BlobsAt`]); `None` when it knows of
    /// none.
    pub fn blobs_at(&self) -> Option<&str> {
        self.blobs.commit(self.pinned.as_deref())
    }

    /// Forget everything, the pin included: Refresh.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// How many rows the next page asks for, `base` being the graph's page size. The first asks
    /// for twice `base`: it is read with no cursor, which costs the server little more than a
    /// smaller page, and holds the first screens with room to scroll. Each later page asks for
    /// twice the last, up to [`MAX_PAGE_GROWTH`] times `base`: a page after a cursor costs a
    /// scan to it whatever its size, so rows read cost fewer of them.
    pub fn next_page_size(&self, base: u32) -> u32 {
        let base = base.max(1);
        match self.start {
            PageStart::First => base.saturating_mul(2),
            _ => self
                .page_size
                .saturating_mul(2)
                .clamp(base, base.saturating_mul(MAX_PAGE_GROWTH)),
        }
    }

    /// Records a page that asked for `asked` rows, `base` being the graph's page size. The first
    /// page's commit becomes the pin; later pages were read at it. With a sort (`sort_column`,
    /// the rows' column for it), a short page of rows with values moves on to the rows without
    /// one rather than ending the list, when there can be any (`nulls_follow`).
    pub fn accept(
        &mut self,
        commit: Option<&str>,
        page: &[Row],
        asked: u32,
        base: u32,
        sort_column: Option<&str>,
        nulls_follow: bool,
    ) {
        if self.pinned.is_none() {
            self.pinned = commit.map(str::to_owned);
        }
        self.page_size = asked;
        self.ahead = base;
        let short = page.len() < asked as usize;
        let in_nulls = matches!(self.start, PageStart::Nulls { .. });
        match (sort_column, page.last()) {
            (Some(_), Some(last)) if in_nulls => {
                self.start = PageStart::Nulls {
                    after: last.id.clone(),
                };
            }
            (Some(column), Some(last)) => {
                self.start = PageStart::After {
                    id: last.id.clone(),
                    value: last.value(column).clone(),
                };
            }
            (None, Some(last)) => {
                self.start = PageStart::After {
                    id: last.id.clone(),
                    value: Value::Null,
                };
            }
            (_, None) => {}
        }
        if sort_column.is_some() && short && !in_nulls && nulls_follow {
            self.start = PageStart::Nulls {
                after: String::new(),
            };
            self.exhausted = false;
        } else {
            self.exhausted = short;
        }
    }

    /// Whether another page should be read, the view ending at row `visible_end` of the
    /// `loaded` rows: once no more than a page ([`Self::ahead`]) is left past it, so a scroll
    /// finds its rows read. Not before: a page landing with no row drawn yet reads nothing more,
    /// and a view nobody scrolls costs no read past the first page.
    pub fn wants_more(&self, loaded: usize, visible_end: usize, loading: bool) -> bool {
        !self.exhausted
            && !loading
            && loaded > 0
            && loaded <= visible_end.saturating_add(self.ahead as usize)
    }

    /// A write this view made committed. When it was made on top of the pinned commit, the
    /// view holds exactly the new commit's state and the pin moves with it; otherwise someone
    /// else committed in between and the view is now behind. Returns whether it is behind.
    pub fn advance_for_own_commit(&mut self, parent: Option<&str>, commit: &str) -> bool {
        let followed = self.pinned.is_some() && self.pinned.as_deref() == parent;
        let pin = self.pinned.clone();
        self.blobs
            .after_own_write(pin.as_deref(), parent, commit, followed);
        if followed {
            self.pinned = Some(commit.to_owned());
            self.moved = true;
        }
        !followed
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
    // A text value reads as markdown (spec 014): a cell shows its first line, markup removed.
    if let (Some(property), Some(text)) = (property, value.as_str())
        && reads_as_markdown(property)
    {
        return studio_ui::text::first_line(text);
    }
    studio_ui::format_value(value)
}

/// Whether a property's values read as markdown: a single text value that is neither a key nor
/// an enum (spec 014).
pub fn reads_as_markdown(property: &Property) -> bool {
    property.kind == ScalarKind::Text
        && !property.list
        && !property.is_key
        && property.enum_values.is_none()
}

/// What a date or time cell shows, and the exact value for its tooltip (spec 004: "dates
/// relative with exact value on hover"). A `DateTime` reads relative to `now` for two weeks
/// either way, then as its date; a `Date` reads Today, Yesterday or Tomorrow, else as itself.
/// `None` for other values.
pub fn relative_value(
    property: Option<&Property>,
    value: &Value,
    now: chrono::DateTime<chrono::Local>,
) -> Option<(String, String)> {
    let property = property.filter(|p| !p.list)?;
    let text = value.as_str()?;
    match property.kind {
        ScalarKind::DateTime => {
            let at = chrono::NaiveDateTime::parse_from_str(
                text.trim_end_matches('Z'),
                "%Y-%m-%dT%H:%M:%S%.f",
            )
            .ok()?
            .and_utc();
            let seconds = (now.with_timezone(&chrono::Utc) - at).num_seconds();
            let shown = studio_ui::relative_span(seconds)
                .unwrap_or_else(|| at.format("%Y-%m-%d").to_string());
            Some((shown, at.format("%Y-%m-%d %H:%M:%S%.3f UTC").to_string()))
        }
        ScalarKind::Date => {
            let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
            let days = (date - now.date_naive()).num_days();
            let shown = match days {
                0 => "Today",
                -1 => "Yesterday",
                1 => "Tomorrow",
                _ => return None,
            };
            Some((shown.to_owned(), text.to_owned()))
        }
        _ => None,
    }
}

/// What an edit's text asks for, its editor filled with `old`: `None` while it asks for `old`
/// (the text as it was filled, or the same value spelled otherwise), else the value typed, or
/// why the text is not one.
pub fn edited(property: &Property, old: &Value, text: &str) -> Option<Result<Value, String>> {
    match typed(property, old, text)? {
        Ok(new) if same_value(&new, old) => None,
        new => Some(new),
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

/// The values of an "is one of" filter, read as a list cell's items are: separated by commas,
/// one holding a comma quoted (`Founder, "Smith, John"`).
pub fn parse_filter_list(property: &Property, text: &str) -> Result<Value, String> {
    let element = Property {
        list: false,
        nullable: false,
        ..property.clone()
    };
    let values = list_items(text)?
        .iter()
        .map(|item| parse_scalar(&element, item))
        .collect::<Result<Vec<_>, _>>()?;
    if values.is_empty() {
        return Err("Enter values separated by commas".into());
    }
    Ok(Value::Array(values))
}

/// The days a date preset takes at `now`, as `[start, end)`: today; this week, from Monday;
/// the last 30 days, through today. A `Date` as `YYYY-MM-DD`, a `DateTime` as that local
/// midnight in UTC, spelled as the engine writes a time (no zone), so a row's value compared
/// with it here as text orders as the engine orders them.
pub fn preset_range(
    op: FilterOp,
    kind: Option<ScalarKind>,
    now: chrono::DateTime<chrono::Local>,
) -> Value {
    use chrono::{Datelike as _, Duration, TimeZone as _};
    let today = now.date_naive();
    let tomorrow = today + Duration::days(1);
    let (start, end) = match op {
        FilterOp::ThisWeek => {
            let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
            (monday, monday + Duration::days(7))
        }
        FilterOp::Last30Days => (today - Duration::days(30), tomorrow),
        _ => (today, tomorrow),
    };
    let day = |day: chrono::NaiveDate| match kind {
        Some(ScalarKind::DateTime) => {
            let midnight = day.and_hms_opt(0, 0, 0).expect("midnight exists");
            let local = chrono::Local
                .from_local_datetime(&midnight)
                .earliest()
                .unwrap_or(now);
            Value::String(
                local
                    .with_timezone(&chrono::Utc)
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string(),
            )
        }
        _ => Value::String(day.format("%Y-%m-%d").to_string()),
    };
    Value::Array(vec![day(start), day(end)])
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
            // A neighbour's property is not in the row either.
            _ if filter.property.contains('.') => true,
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
            FilterOp::StartsWith => match (value, &filter.value) {
                (Value::String(text), Value::String(start)) => text.starts_with(start.as_str()),
                _ => false,
            },
            // The engine's full-text search, near enough: every word of the query somewhere in
            // the text, whatever the case.
            FilterOp::Search => match (value, &filter.value) {
                (Value::String(text), Value::String(words)) => {
                    let text = text.to_lowercase();
                    words
                        .split_whitespace()
                        .all(|word| text.contains(&word.to_lowercase()))
                }
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
            FilterOp::Ge => matches!(
                compare(value, &filter.value),
                Some(Ordering::Greater | Ordering::Equal)
            ),
            // Within the preset's `[start, end)`.
            FilterOp::Today | FilterOp::ThisWeek | FilterOp::Last30Days => match &filter.value {
                Value::Array(bounds) if bounds.len() == 2 => {
                    matches!(
                        compare(value, &bounds[0]),
                        Some(Ordering::Greater | Ordering::Equal)
                    ) && compare(value, &bounds[1]) == Some(Ordering::Less)
                }
                _ => false,
            },
        }
    })
}

/// A filter as its pill's parts: the field, the test and the value it compares with, if any,
/// as it reads: `title is Engineer`, `email is empty`, `has worksAt →`.
pub fn filter_parts(filter: &Filter) -> (String, String, Option<String>) {
    // A neighbour's property is named by the traversal and the property: `attended name
    // contains Aaron`; the pill's icon shows the direction.
    if let Some((relationship, property)) = filter.property.split_once('.')
        && let Some((_, edge)) = gq_language::builder::parse_relationship_key(relationship)
    {
        let value = filter.op.takes_value().then(|| edit_text(&filter.value));
        return (
            format!("{} {property}", gq_language::traversal_name(edge)),
            filter.op.label().to_owned(),
            value,
        );
    }
    // A relationship is named by its traversal; the pill's icon shows its direction.
    if let Some((_, edge)) = gq_language::builder::parse_relationship_key(&filter.property) {
        let relationship = gq_language::traversal_name(edge);
        return match filter.op {
            FilterOp::HasAny => (relationship, "has any".into(), None),
            FilterOp::HasNone => (relationship, "has none".into(), None),
            _ => (relationship, "is".into(), Some(edit_text(&filter.value))),
        };
    }
    let value = filter.op.takes_value().then(|| edit_text(&filter.value));
    (filter.property.clone(), filter.op.label().to_owned(), value)
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

impl Tone {
    /// Red for what was refused or cannot be sent, amber for a conflict or an unknown outcome.
    pub fn color(self) -> ui::Color {
        match self {
            Tone::Refused | Tone::Invalid => ui::Color::Error,
            Tone::Conflict | Tone::Unknown => ui::Color::Warning,
        }
    }
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

/// What a write never sent says: why, and that nothing was written.
pub(crate) fn not_sent_notice(reason: &str) -> Notice {
    Notice::new(
        Tone::Refused,
        format!(
            "Not sent: {}. Nothing was written.",
            studio_ui::clause(reason)
        ),
    )
}

/// Classifies a write's outcome. `branch` names the write target in a denial.
pub fn save_result(outcome: &WriteOutcome<ChangeOutput>, branch: &str) -> SaveResult {
    match outcome {
        WriteOutcome::Committed(output) => match published(output.commit.as_ref(), branch) {
            Ok(Some(commit)) if output.affected_nodes + output.affected_edges > 0 => {
                SaveResult::Saved {
                    commit: commit.graph_commit_id.clone(),
                    parent: commit.parent_commit_id.clone(),
                }
            }
            Ok(_) => SaveResult::NoMatch,
            Err(unsettled) => unsettled,
        },
        WriteOutcome::Refused(error) => SaveResult::Refused(refusal(
            error.status,
            error.reason(),
            error.body.precondition_failure.is_some(),
            branch,
        )),
        WriteOutcome::NotSent { reason } => SaveResult::Refused(not_sent_notice(reason)),
        WriteOutcome::Unknown { .. } => {
            SaveResult::Unknown(Notice::new(Tone::Unknown, UNKNOWN_MESSAGE))
        }
    }
}

/// The commit a write that went through published on `branch`, by its kind as the session
/// logs it ([`OutcomeKind::written`]): `Ok(None)` for none. Only a published commit moved the
/// branch. A proposal or a snapshot is on no branch, so the write is not saved there, its draft
/// stays and nothing moves to it; of a kind this build does not know, whether the branch moved is
/// not known. Either is the save's outcome, `Err`.
pub fn published<'a>(
    commit: Option<&'a CommitOutput>,
    branch: &str,
) -> Result<Option<&'a CommitOutput>, SaveResult> {
    let Some(commit) = commit else {
        return Ok(None);
    };
    match OutcomeKind::written(Some((commit.graph_commit_id.clone(), commit.kind))) {
        OutcomeKind::Committed { commit: None } => Ok(None),
        OutcomeKind::Committed { commit: Some(_) } => Ok(Some(commit)),
        OutcomeKind::Held { commit, held } => {
            Err(SaveResult::Refused(held_notice(&commit, held, branch)))
        }
        OutcomeKind::Unknown { reason } => Err(SaveResult::Unknown(Notice::new(
            Tone::Unknown,
            format!("{reason}. {UNKNOWN_MESSAGE}."),
        ))),
        OutcomeKind::Refused { message } => Err(SaveResult::Refused(Notice::new(
            Tone::Refused,
            format!("Nothing was written. {message}"),
        ))),
    }
}

/// What a write held off every branch says: it is not on `branch`, which did not change.
pub fn held_notice(commit: &str, held: HeldAs, branch: &str) -> Notice {
    let commit = studio_ui::short_commit_id(commit);
    let message = match held {
        HeldAs::Proposal => format!(
            "{branch} did not change: the server held the write as commit {commit}, on no \
             branch until it is published to {branch}."
        ),
        HeldAs::Snapshot => format!(
            "{branch} did not change: the server saved the write as snapshot {commit}, on no \
             branch."
        ),
    };
    Notice::new(Tone::Conflict, message)
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

/// The most chips a relationship cell shows: two names say what a row links to; more crowd
/// the row.
pub const RELATION_CHIPS_SHOWN: usize = 2;

/// The most neighbour rows one page reads per relationship, and the most one row may bring:
/// a row with more shows its count alone, rather than starving the rows after it.
pub const RELATION_SAMPLE_BUDGET: i64 = 2000;
pub const RELATION_SAMPLE_HUB: i64 = 200;

/// The rows whose neighbours a page reads, in page order, and how many neighbour rows that is:
/// every row's whole list, while the budget lasts. Paging by count would hand the first rows
/// all of it and leave the rest bare.
pub fn relation_sample(
    ids: &[String],
    counts: &std::collections::HashMap<String, i64>,
) -> (Vec<String>, i64) {
    let mut picked = Vec::new();
    let mut total = 0;
    for id in ids {
        let degree = counts.get(id).copied().unwrap_or(0);
        if degree == 0 || degree > RELATION_SAMPLE_HUB || total + degree > RELATION_SAMPLE_BUDGET {
            continue;
        }
        picked.push(id.clone());
        total += degree;
    }
    (picked, total)
}

/// How many chips fit whole in `available` pixels, with room for a "+N" after them when some
/// are left out: by an estimate of each label's width at the tag's size, since a cell is laid
/// out before its text is measured. A chip cut mid-name reads as a glitch.
pub fn chips_that_fit(labels: &[&str], count: i64, available: f32) -> usize {
    let labels = &labels[..labels.len().min(RELATION_CHIPS_SHOWN)];
    const CHAR: f32 = 6.8;
    const CHROME: f32 = 2. * studio_ui::ds::space::SM;
    const GAP: f32 = studio_ui::ds::space::XS;
    let more_width = |left: i64| format!("+{left}").len() as f32 * CHAR + GAP;
    let mut used = 0.;
    for (shown, label) in labels.iter().enumerate() {
        let chip = (label.chars().count() as f32 * CHAR + CHROME).min(studio_ui::ds::TAG_MAX_WIDTH);
        let left = count - shown as i64 - 1;
        let reserve = if left > 0 { more_width(left) } else { 0. };
        if used + chip + reserve > available {
            return shown;
        }
        used += chip + GAP;
    }
    labels.len()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_page_samples_every_rows_neighbours_while_the_budget_lasts() {
        let ids: Vec<String> = ["a", "hub", "b", "none", "c"].map(String::from).to_vec();
        let counts = [
            ("a", 3),
            ("hub", RELATION_SAMPLE_HUB + 1),
            ("b", 5),
            ("c", 2),
        ]
        .map(|(id, n)| (id.to_owned(), n))
        .into_iter()
        .collect();
        let (picked, total) = relation_sample(&ids, &counts);
        assert_eq!(picked, ["a", "b", "c"]);
        assert_eq!(total, 10);

        let many: Vec<String> = (0..30).map(|i| format!("r{i}")).collect();
        let counts = many.iter().map(|id| (id.clone(), 100)).collect();
        let (picked, total) = relation_sample(&many, &counts);
        assert_eq!(picked.len() as i64, RELATION_SAMPLE_BUDGET / 100);
        assert!(total <= RELATION_SAMPLE_BUDGET);
    }

    fn columns(keys: &[&str]) -> Vec<ColumnSpec> {
        keys.iter()
            .map(|key| ColumnSpec {
                key: (*key).into(),
                title: (*key).into(),
                detail: String::new(),
                role: ColumnRole::Key,
                width: 100.,
                align_right: false,
            })
            .collect()
    }

    fn keys(columns: &[ColumnSpec]) -> Vec<&str> {
        columns.iter().map(|c| c.key.as_str()).collect()
    }

    #[test]
    fn a_layout_hides_and_orders_columns_and_keeps_the_pinned_one_first() {
        let all = columns(&["slug", "name", "title", "born"]);
        let mut layout = ColumnLayout::default();
        assert_eq!(keys(&layout.shown(&all)), ["slug", "name", "title", "born"]);

        layout.toggle(&all, "title");
        layout.toggle(&all, "slug");
        assert_eq!(
            keys(&layout.shown(&all)),
            ["slug", "name", "born"],
            "the key stays"
        );

        layout.step(&all, "born", -1);
        assert_eq!(
            keys(&layout.shown(&all)),
            ["slug", "born", "name"],
            "over the hidden one"
        );
        layout.toggle(&all, "title");
        assert_eq!(keys(&layout.shown(&all)), ["slug", "born", "name", "title"]);
        layout.step(&all, "born", -1);
        assert_eq!(
            keys(&layout.shown(&all)),
            ["slug", "born", "name", "title"],
            "not past the key"
        );

        layout.move_onto(&all, "title", "born");
        assert_eq!(
            keys(&layout.shown(&all)),
            ["slug", "title", "born", "name"],
            "before, moving left"
        );
        layout.move_onto(&all, "title", "name");
        assert_eq!(
            keys(&layout.shown(&all)),
            ["slug", "born", "name", "title"],
            "after, moving right"
        );
        layout.move_onto(&all, "born", "title");
        assert_eq!(
            layout.order,
            Vec::<String>::new(),
            "schema order again is no order"
        );
    }

    #[test]
    fn a_dragged_width_holds_within_bounds_until_reset() {
        let all = columns(&["slug", "name"]);
        let mut layout = ColumnLayout::default();
        let fitted = all[1].width;
        assert_eq!(layout.width(&all[1]), fitted);
        layout.set_width("name", Some(321.4));
        assert_eq!(layout.width(&all[1]), 321.);
        layout.set_width("name", Some(5.));
        assert_eq!(layout.width(&all[1]), COLUMN_MIN_WIDTH);
        layout.set_width("name", Some(99_999.));
        assert_eq!(layout.width(&all[1]), COLUMN_MAX_WIDTH);
        // Another column keeps its own.
        assert_eq!(layout.width(&all[0]), all[0].width);
        layout.set_width("name", None);
        assert_eq!(layout.width(&all[1]), fitted);
        assert_eq!(layout, ColumnLayout::default());
    }

    #[test]
    fn a_layout_survives_schema_changes() {
        let mut layout = ColumnLayout::new(
            vec!["gone".into()],
            vec!["born".into(), "gone".into(), "name".into()],
        );
        let all = columns(&["slug", "name", "born", "city"]);
        assert_eq!(
            keys(&layout.shown(&all)),
            ["slug", "born", "name", "city"],
            "new columns last"
        );
        layout.reset();
        assert_eq!(layout, ColumnLayout::default());
    }

    #[test]
    fn chips_fit_whole_with_room_for_the_rest() {
        let labels = ["Ada Lovelace", "Alan Turing", "Grace Hopper"];
        // A wide cell still shows two; the rest is a count.
        assert_eq!(chips_that_fit(&labels, 3, 1000.), RELATION_CHIPS_SHOWN);
        // A narrow one keeps whole chips and the "+N" after them.
        let one = chips_that_fit(&labels, 9, 160.);
        assert_eq!(one, 1);
        // Nothing fits: the cell shows the count alone.
        assert_eq!(chips_that_fit(&labels, 9, 20.), 0);
    }

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
        let groups = schema.relationships_of("Person");
        let columns = node_columns(schema.node("Person").unwrap(), groups);
        let keys: Vec<_> = columns.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(
            keys[..6],
            ["slug", "name", "title", "email", "born", "tags"]
        );
        let relationships: Vec<_> = columns[6..].iter().map(|c| c.title.as_str()).collect();
        assert_eq!(relationships.len(), groups.len());
        assert!(relationships.contains(&"worksAt →"), "{relationships:?}");
        assert!(columns[6..].iter().all(|c| !c.editable(TableKind::Node)));
        assert_eq!(columns[0].role, ColumnRole::Key);
        assert_eq!(columns[4].detail, "Date?");
        assert!(!columns[0].editable(TableKind::Node));
        assert!(columns[1].editable(TableKind::Node));
        assert!(!columns[1].editable(TableKind::Edge));
        let company = node_columns(schema.node("Company").unwrap(), &[]);
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

    fn after(id: &str, value: Value) -> PageStart {
        PageStart::After {
            id: id.into(),
            value,
        }
    }

    #[test]
    fn paging_pins_the_first_commit_and_pages_by_key() {
        let mut paging = Paging::default();
        paging.accept(Some("c1"), &rows(&["a", "b"]), 2, 2, None, true);
        assert_eq!(paging.pinned.as_deref(), Some("c1"));
        assert_eq!(paging.start, after("b", Value::Null));
        assert!(!paging.exhausted);
        // A later page read at the pin cannot move it.
        paging.accept(Some("c9"), &rows(&["c"]), 2, 2, None, true);
        assert_eq!(paging.pinned.as_deref(), Some("c1"));
        assert_eq!(paging.start, after("c", Value::Null));
        assert!(paging.exhausted);
        // New filters keep the pin; Refresh drops it.
        paging.restart();
        assert_eq!(paging.pinned.as_deref(), Some("c1"));
        assert!(paging.start == PageStart::First && !paging.exhausted);
        paging.reset();
        assert!(paging.pinned.is_none());
    }

    #[test]
    fn sorted_paging_reads_the_rows_with_values_then_those_without() {
        let row = |id: &str, title: Value| {
            let mut values = Map::new();
            values.insert("title".into(), title);
            Row {
                id: id.into(),
                values,
                dimmed: false,
            }
        };
        let mut paging = Paging::default();
        let first = [row("p1", "Analyst".into()), row("p7", "CTO".into())];
        paging.accept(Some("c1"), &first, 2, 2, Some("title"), true);
        assert_eq!(paging.start, after("p7", "CTO".into()));
        assert!(!paging.exhausted);
        // A short page of values goes on to the rows without one.
        let short = [row("p3", "Engineer".into())];
        let mut required = paging.clone();
        paging.accept(Some("c1"), &short, 2, 2, Some("title"), true);
        assert_eq!(
            paging.start,
            PageStart::Nulls {
                after: String::new()
            }
        );
        assert!(!paging.exhausted);
        paging.accept(
            Some("c1"),
            &[row("p2", Value::Null)],
            2,
            2,
            Some("title"),
            true,
        );
        assert_eq!(paging.start, PageStart::Nulls { after: "p2".into() });
        assert!(paging.exhausted, "a short page of nulls ends the list");
        // Sorted by a property every row must hold, the short page of values is the last.
        required.accept(Some("c1"), &short, 2, 2, Some("title"), false);
        assert!(required.exhausted, "no row without a value follows");
    }

    /// The first page asks for two pages, read with no cursor; the next is read once the view
    /// comes within a page of the rows read, not as the first lands; each later page asks for
    /// twice the last, up to eight pages.
    #[test]
    fn the_next_page_is_read_a_page_ahead_of_the_view() {
        let mut paging = Paging::default();
        assert!(!paging.wants_more(0, 0, false), "nothing read yet");
        assert_eq!(paging.next_page_size(100), 200, "the first, two pages");
        let ids: Vec<String> = (0..200).map(|i| format!("p{i:03}")).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        paging.accept(Some("c1"), &rows(&ids), 200, 100, None, true);
        assert!(
            !paging.wants_more(200, 0, false),
            "nothing more as the first page lands"
        );
        assert!(!paging.wants_more(200, 99, false), "a screen in");
        assert!(
            paging.wants_more(200, 100, false),
            "the view came within a page"
        );
        assert!(!paging.wants_more(200, 100, true), "one read at a time");
        assert_eq!(paging.next_page_size(100), 400);
        paging.page_size = 400;
        assert_eq!(paging.next_page_size(100), 800);
        paging.page_size = 800;
        assert_eq!(paging.next_page_size(100), 800, "eight pages at most");
        let done = Paging {
            exhausted: true,
            ..paging
        };
        assert!(!done.wants_more(200, 200, false));
    }

    #[test]
    fn own_commits_move_the_pin_only_from_the_pinned_parent() {
        let mut paging = Paging {
            pinned: Some("c1".into()),
            ..Paging::default()
        };
        assert!(!paging.advance_for_own_commit(Some("c1"), "c2"));
        assert_eq!(paging.pinned.as_deref(), Some("c2"));
        assert!(paging.moved, "later pages are read at another commit");
        assert_eq!(paging.blobs_at(), Some("c2"));
        // Someone else committed c3 first: the view is now behind, and cannot see what c3 did
        // to the Blob values it shows.
        assert!(paging.advance_for_own_commit(Some("c3"), "c4"));
        assert_eq!(paging.pinned.as_deref(), Some("c2"));
        assert_eq!(paging.blobs_at(), None);
        paging.restart();
        assert!(!paging.moved);

        // Behind, a Blob write bound to a validator read where the values held lands past the
        // pin: they hold at its commit, and at the own commits made on it after.
        let mut behind = Paging {
            pinned: Some("c1".into()),
            ..Paging::default()
        };
        let before = behind.blobs_at().map(str::to_owned);
        assert!(behind.advance_for_own_commit(Some("c5"), "c6"));
        behind
            .blobs
            .after_bound_write("c1", before.as_deref(), "c6");
        assert_eq!(behind.blobs_at(), Some("c6"));
        assert!(behind.advance_for_own_commit(Some("c6"), "c7"));
        assert_eq!(behind.blobs_at(), Some("c7"), "made on them");
        assert!(behind.advance_for_own_commit(Some("c9"), "c10"));
        assert_eq!(behind.blobs_at(), None, "made over another writer's");
        // A validator read elsewhere than where they held proves nothing about them.
        behind.blobs.after_bound_write("c1", Some("c7"), "c11");
        assert_eq!(behind.blobs_at(), None);
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
        // The engine says which spellings of a date it takes; a word is none.
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

    /// An edit asks for nothing while its text is the one its editor was filled with, even
    /// text that would not read back as the value: a blank kept in an optional text, an empty
    /// one in a required text.
    #[test]
    fn an_untouched_edit_asks_for_nothing() {
        let (title, name, founded) = (
            prop("Person", "title"),
            prop("Person", "name"),
            prop("Company", "founded"),
        );
        assert_eq!(edited(&title, &json!(" "), " "), None);
        assert_eq!(edited(&name, &json!(""), ""), None);
        assert_eq!(
            edited(&founded, &json!(1999), " 1999 "),
            None,
            "the same value"
        );
        assert_eq!(edited(&title, &json!(" "), ""), Some(Ok(Value::Null)));
        assert_eq!(
            edited(&founded, &json!(1999), "2000"),
            Some(Ok(json!(2000)))
        );
        assert!(matches!(edited(&founded, &json!(1999), "x"), Some(Err(_))));
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
            filter_parts(&filter("title", FilterOp::Eq, json!("CEO"))),
            ("title".into(), "is".into(), Some("CEO".into()))
        );
        assert_eq!(
            filter_parts(&filter("email", FilterOp::IsEmpty, Value::Null)),
            ("email".into(), "is empty".into(), None)
        );
    }

    fn committed(affected: i64, parent: Option<&str>) -> WriteOutcome<ChangeOutput> {
        WriteOutcome::Committed(ChangeOutput {
            branch: "main".into(),
            query_name: "studio_save_rows".into(),
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
                kind: graph_client::api::CommitKind::Published,
            }),
            actor_id: None,
            outcome: None,
        })
    }

    fn refused(status: u16, precondition: bool) -> WriteOutcome<ChangeOutput> {
        WriteOutcome::Refused(ApiError {
            status,
            unauthorized: None,
            token_replaced: false,
            body: ErrorOutput {
                error: "policy denied".into(),
                precondition_failure: precondition.then(|| PreconditionFailureOutput {
                    expected: "c1".into(),
                    actual: Some("c2".into()),
                }),
                ..Default::default()
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
        // A 401 says what became of the token, not the server's words.
        let unauthorized = WriteOutcome::Refused(ApiError {
            status: 401,
            unauthorized: Some(graph_client::Unauthorized::TokenNotReceived),
            token_replaced: false,
            body: ErrorOutput {
                error: "unauthorized".into(),
                ..Default::default()
            },
        });
        let SaveResult::Refused(notice) = save_result(&unauthorized, "main") else {
            panic!()
        };
        assert_eq!(
            notice.message,
            "Nothing was written. The server received no token, though Studio sent one: \
             something between them may drop it."
        );
        assert_eq!(
            save_result(
                &WriteOutcome::Unknown {
                    reason: "timeout".into()
                },
                "main"
            ),
            SaveResult::Unknown(Notice::new(Tone::Unknown, UNKNOWN_MESSAGE))
        );
        // Never sent (the target retired): nothing was written, said as a refusal, not unknown.
        assert_eq!(
            save_result(
                &WriteOutcome::NotSent {
                    reason: "The CLI no longer lists the target".into()
                },
                "main"
            ),
            SaveResult::Refused(Notice::new(
                Tone::Refused,
                "Not sent: The CLI no longer lists the target. Nothing was written."
            ))
        );
        assert_eq!(
            changed_elsewhere(None, Some(&json!("CEO"))).message,
            "Changed elsewhere to “CEO” while you were editing"
        );
    }

    /// A commit no branch moved to is not a save: a proposal or a snapshot leaves the branch as
    /// it was and the draft kept, and a kind this build does not know is an outcome not known.
    #[test]
    fn a_commit_on_no_branch_is_not_a_save() {
        use graph_client::api::CommitKind;
        let of_kind = |kind: CommitKind| {
            let mut outcome = committed(1, Some("c1"));
            if let WriteOutcome::Committed(output) = &mut outcome
                && let Some(commit) = &mut output.commit
            {
                commit.kind = kind;
            }
            save_result(&outcome, "main")
        };
        let SaveResult::Refused(notice) = of_kind(CommitKind::Unpublished) else {
            panic!("a proposal is not saved")
        };
        assert_eq!(notice.tone, Tone::Conflict);
        assert!(
            notice
                .message
                .starts_with("main did not change: the server held"),
            "{}",
            notice.message
        );
        let SaveResult::Refused(notice) = of_kind(CommitKind::Unattached) else {
            panic!("a snapshot is not saved")
        };
        assert!(notice.message.contains("snapshot c2"), "{}", notice.message);
        let SaveResult::Unknown(notice) = of_kind(CommitKind::Other) else {
            panic!("a kind not known is an outcome not known")
        };
        assert!(notice.message.ends_with(&format!("{UNKNOWN_MESSAGE}.")));
        assert!(matches!(
            of_kind(CommitKind::Published),
            SaveResult::Saved { .. }
        ));
    }

    #[test]
    fn an_empty_value_reads_as_a_dash() {
        assert_eq!(display_value(None, &Value::Null), "—");
    }

    /// A text value reads as markdown: a cell shows its first line without its markup. Keys,
    /// enums and lists are shown as they are (spec 014).
    #[test]
    fn a_text_cell_shows_its_markdowns_first_line() {
        let title = prop("Person", "title");
        assert_eq!(display_value(Some(&title), &json!("Founder")), "Founder");
        assert_eq!(
            display_value(Some(&title), &json!("**Founder** of\nAcme")),
            "Founder of…"
        );
        assert_eq!(
            display_value(Some(&title), &json!("# CTO\n\nSince 2020")),
            "CTO…"
        );
        assert_eq!(
            display_value(Some(&prop("Person", "slug")), &json!("**p1**")),
            "**p1**",
            "a key is not parsed"
        );
        assert_eq!(
            display_value(Some(&prop("Project", "status")), &json!("active")),
            "active"
        );
        assert!(
            !reads_as_markdown(&prop("Person", "tags")),
            "a list is not parsed"
        );
    }

    #[test]
    fn list_filters_parse_each_value_with_the_propertys_type() {
        assert_eq!(
            parse_filter_list(&prop("Person", "title"), "Founder, Engineer,").unwrap(),
            json!(["Founder", "Engineer"])
        );
        // A value holding a comma is quoted, as in a list cell, and the pill writes it so.
        let names = json!(["Smith, John", "Doe"]);
        assert_eq!(
            parse_filter_list(&prop("Person", "title"), &edit_text(&names)),
            Ok(names)
        );
        assert!(parse_filter_list(&prop("Person", "title"), "\"Smith, John").is_err());
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
            preset_range(FilterOp::Today, date, now),
            json!(["2026-10-14", "2026-10-15"])
        );
        assert_eq!(
            preset_range(FilterOp::ThisWeek, date, now),
            json!(["2026-10-12", "2026-10-19"])
        );
        assert_eq!(
            preset_range(FilterOp::Last30Days, date, now),
            json!(["2026-09-14", "2026-10-15"])
        );
        let instant = preset_range(FilterOp::Today, Some(ScalarKind::DateTime), now);
        let text = instant[0].as_str().unwrap();
        assert!(text.ends_with(":00") && text.len() == 19, "{text}");
        // A value at the bound, and one a quarter second past it, as the engine writes them,
        // are in the day; the one before it is not.
        let filter = |value: Value| Filter {
            property: "at".into(),
            op: FilterOp::Today,
            value,
        };
        let at = |value: &str| -> Map<String, Value> {
            serde_json::from_value(json!({ "at": value })).unwrap()
        };
        let quarter = format!("{text}.250");
        assert!(row_matches(&[filter(instant.clone())], &at(text)));
        assert!(row_matches(&[filter(instant.clone())], &at(&quarter)));
        let end = instant[1].as_str().unwrap();
        assert!(!row_matches(&[filter(instant.clone())], &at(end)));
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
            &[filter(
                "born",
                FilterOp::ThisWeek,
                json!(["2026-10-12", "2026-10-19"])
            )],
            &row
        ));
        assert!(!row_matches(
            &[filter(
                "born",
                FilterOp::Today,
                json!(["2026-10-14", "2026-10-15"])
            )],
            &row
        ));
        // A day after the range is outside it: a preset is not "on or after".
        assert!(!row_matches(
            &[filter(
                "born",
                FilterOp::Today,
                json!(["2026-10-12", "2026-10-13"])
            )],
            &row
        ));
        assert!(row_matches(
            &[filter("out:WorksAt", FilterOp::HasNone, Value::Null)],
            &row
        ));
        assert_eq!(
            filter_parts(&filter("in:Knows", FilterOp::HasAny, Value::Null)),
            ("knows".into(), "has any".into(), None)
        );
    }

    #[test]
    fn dates_read_relative_with_the_exact_value_kept() {
        use chrono::TimeZone as _;
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 10, 12, 12, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Local);
        let at = |kind: ScalarKind, text: &str| {
            let property = Property {
                name: "at".into(),
                kind,
                base_type: String::new(),
                nullable: true,
                list: false,
                enum_values: None,
                is_key: false,
                indexed: false,
            };
            relative_value(Some(&property), &Value::String(text.into()), now)
        };
        assert_eq!(
            at(ScalarKind::DateTime, "2026-10-12T09:00:00.000"),
            Some(("3 h ago".into(), "2026-10-12 09:00:00.000 UTC".into()))
        );
        assert_eq!(
            at(ScalarKind::DateTime, "2026-10-14T12:00:00Z").map(|(t, _)| t),
            Some("in 2 days".into())
        );
        assert_eq!(
            at(ScalarKind::DateTime, "2026-10-12T11:59:40").map(|(t, _)| t),
            Some("just now".into())
        );
        assert_eq!(
            at(ScalarKind::DateTime, "2024-01-01T12:34:56.789").map(|(t, _)| t),
            Some("2024-01-01".into()),
            "older than two weeks: the date"
        );
        let today = now.date_naive();
        assert_eq!(
            at(ScalarKind::Date, &today.format("%Y-%m-%d").to_string()).map(|(t, _)| t),
            Some("Today".into())
        );
        assert_eq!(
            at(ScalarKind::Date, "1978-03-20"),
            None,
            "a far date reads as itself"
        );
        assert_eq!(at(ScalarKind::Text, "2026-10-12"), None);
    }
}
