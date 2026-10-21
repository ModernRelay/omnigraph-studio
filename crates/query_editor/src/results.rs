//! What the editor shows after a run, as plain data: result tables, explain plans, server
//! diagnostics mapped back into the editor's text, and the small formats they use (spec 007).

use std::time::Duration;

use std::collections::{HashMap, HashSet};

use gq_language::{ResultColumn, SchemaModel, TokenKind};
use graph_client::api::{BranchMergeOutcome, ChangeOutput, CommitKind};
use serde_json::{Map, Value};
use studio_actions::graph::ResultNode;
use studio_ui::short_commit_id;

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
    /// answer is held once, not twice. `expected` is the compiler's view of the same query,
    /// whose columns are named as the server names them; a column it does not name has no type.
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
        let described = |column: &String| {
            expected
                .unwrap_or_default()
                .iter()
                .find(|expected| expected.column == *column)
        };
        let types = columns
            .iter()
            .map(|column| described(column).map(type_label))
            .collect();
        let node_types = columns
            .iter()
            .map(|column| described(column).and_then(|c| c.node_type.clone()))
            .collect();
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

/// A tree's header, from the `tree` names the engine's explain writes (`engine/explain.rs`):
/// `logical_initial` (the plan of a load before the optimizer), `logical`, `physical`,
/// `datafusion`, then `plan` for the planner's notes. A name a later build adds reads as its
/// words.
fn tree_title(tree: &str) -> String {
    match tree {
        "logical_initial" => "Initial logical plan".into(),
        "logical" => "Logical plan".into(),
        "physical" => "Physical plan".into(),
        "datafusion" => "DataFusion plan".into(),
        "plan" => "Planner".into(),
        other => {
            let words = other.replace('_', " ");
            let mut chars = words.trim().chars();
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

/// `1 row`, `12,480 rows`: grouped, as Studio writes every count.
pub fn rows_label(count: i64) -> String {
    if count == 1 {
        "1 row".into()
    } else {
        format!("{} rows", studio_ui::group_count(count))
    }
}

/// What a write the server committed did, read from its answer (constitution III): a
/// mutation's by its commit's `kind`, a statement's by its `outcome` (`ChangeOutput.outcome`,
/// `BranchOutcomeOutput` in the contract). A commit moved its branch only when it is
/// `published`, the rule the session's write log follows too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteEffect {
    /// `branch`'s head moved to `commit`.
    Committed {
        branch: String,
        commit: String,
    },
    /// The write made no commit.
    Unchanged {
        branch: String,
    },
    /// A proposal (`as commit`): held as `commit`, on no branch until `commit publish` puts it
    /// on `target`.
    Proposal {
        target: String,
        commit: String,
    },
    /// An unattached snapshot (`as unattached branch`): read by its id, never published.
    Snapshot {
        commit: String,
    },
    /// A commit of a kind this build does not know: whether `branch` moved is not known.
    UnknownKind {
        branch: String,
        commit: String,
    },
    /// `branch create`: `name` starts at `from`'s commit `at`.
    BranchCreated {
        name: String,
        from: String,
        at: String,
    },
    BranchDeleted {
        name: String,
    },
    /// `branch merge`: `commit` is `target`'s new head, none when it already had `source`.
    Merged {
        source: String,
        target: String,
        outcome: BranchMergeOutcome,
        commit: Option<String>,
    },
    /// `commit publish`: the proposal is `commit` on `branch` now.
    Published {
        branch: String,
        commit: Option<String>,
    },
    /// `commit drop`: the proposal `commit` is gone.
    Dropped {
        commit: String,
    },
    /// A statement's outcome of a `kind` this build does not know.
    Statement {
        kind: String,
        branch: String,
    },
}

impl WriteEffect {
    pub fn of(output: &ChangeOutput) -> Self {
        let field = |key: &str| {
            output
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let branch = output.branch.clone();
        let kind = field("kind");
        // Neither publishes a commit.
        match kind.as_deref() {
            Some("created") => {
                return WriteEffect::BranchCreated {
                    name: field("name").unwrap_or(branch),
                    from: field("from").unwrap_or_default(),
                    at: field("from_commit_id").unwrap_or_default(),
                };
            }
            Some("deleted") => {
                return WriteEffect::BranchDeleted {
                    name: field("name").unwrap_or(branch),
                };
            }
            Some("dropped") => {
                return WriteEffect::Dropped {
                    commit: field("graph_commit_id").unwrap_or_default(),
                };
            }
            _ => {}
        }
        // A merge or a mutation `as commit` holds its result as a proposal, and a mutation
        // `as unattached branch` as a snapshot: no branch moved.
        if let Some(commit) = &output.commit {
            let id = commit.graph_commit_id.clone();
            match commit.kind {
                CommitKind::Published => {}
                CommitKind::Unpublished => {
                    return WriteEffect::Proposal {
                        target: branch,
                        commit: id,
                    };
                }
                CommitKind::Unattached => return WriteEffect::Snapshot { commit: id },
                CommitKind::Other => {
                    return WriteEffect::UnknownKind { branch, commit: id };
                }
            }
        }
        let published = output.commit.as_ref().map(|c| c.graph_commit_id.clone());
        match kind {
            Some(kind) if kind == "merged" => WriteEffect::Merged {
                source: field("source").unwrap_or_default(),
                target: field("target").unwrap_or(branch),
                outcome: output
                    .outcome
                    .as_ref()
                    .and_then(|outcome| outcome.get("merge"))
                    .and_then(|merge| serde_json::from_value(merge.clone()).ok())
                    .unwrap_or(BranchMergeOutcome::Other),
                commit: published,
            },
            Some(kind) if kind == "published" => WriteEffect::Published {
                branch,
                commit: published,
            },
            Some(kind) => WriteEffect::Statement { kind, branch },
            None => match published {
                Some(commit) => WriteEffect::Committed { branch, commit },
                None => WriteEffect::Unchanged { branch },
            },
        }
    }

    /// The outcome's title.
    pub fn title(&self) -> String {
        match self {
            WriteEffect::Committed { branch, .. } => format!("Committed to {branch}"),
            WriteEffect::Unchanged { branch } => format!("Nothing changed on {branch}"),
            WriteEffect::Proposal { commit, .. } => format!(
                "Held as commit {}, not on any branch yet",
                short_commit_id(commit)
            ),
            WriteEffect::Snapshot { commit } => format!(
                "Saved as snapshot {}, on no branch",
                short_commit_id(commit)
            ),
            WriteEffect::UnknownKind { commit, .. } => format!(
                "Wrote commit {}, of a kind Studio does not know",
                short_commit_id(commit)
            ),
            WriteEffect::BranchCreated { name, at, .. } => {
                format!("Created branch {name} at commit {}", short_commit_id(at))
            }
            WriteEffect::BranchDeleted { name } => format!("Deleted branch {name}"),
            WriteEffect::Merged {
                source,
                target,
                outcome: BranchMergeOutcome::AlreadyUpToDate,
                ..
            } => format!("{target} already has {source}; nothing changed"),
            WriteEffect::Merged {
                source,
                target,
                outcome: BranchMergeOutcome::FastForward,
                ..
            } => format!("Merged {source} into {target} by fast-forward"),
            WriteEffect::Merged { source, target, .. } => {
                format!("Merged {source} into {target}")
            }
            WriteEffect::Published {
                branch,
                commit: Some(commit),
            } => format!("Published commit {} to {branch}", short_commit_id(commit)),
            WriteEffect::Published { branch, .. } => format!("Published to {branch}"),
            WriteEffect::Dropped { commit } => {
                format!("Dropped commit {}", short_commit_id(commit))
            }
            WriteEffect::Statement { kind, branch } => {
                format!("Ran on {branch}, with an outcome Studio does not know ({kind})")
            }
        }
    }

    /// What to do next, when there is something to say.
    pub fn detail(&self) -> Option<String> {
        match self {
            WriteEffect::Proposal { target, commit } => Some(format!(
                "Nothing moved on {target}. `commit publish \"{commit}\"` puts it there; \
                 `commit drop \"{commit}\"` drops it."
            )),
            WriteEffect::Snapshot { .. } => {
                Some("Nothing moved on any branch. Read it by its id; it never publishes.".into())
            }
            WriteEffect::UnknownKind { branch, .. } | WriteEffect::Statement { branch, .. } => {
                Some(format!("Check {branch}'s history to see what changed."))
            }
            _ => None,
        }
    }

    /// A run line's few words: `nodes` and `edges` are a mutation's counts.
    pub fn note(&self, nodes: i64, edges: i64) -> String {
        match self {
            WriteEffect::Committed { .. } => format!("Committed · {nodes} nodes, {edges} edges"),
            WriteEffect::Unchanged { .. } => "Nothing changed".into(),
            WriteEffect::Proposal { commit, .. } => {
                format!("Held as commit {}", short_commit_id(commit))
            }
            WriteEffect::Snapshot { commit } => format!("Snapshot {}", short_commit_id(commit)),
            WriteEffect::UnknownKind { commit, .. } => {
                format!("Wrote commit {}", short_commit_id(commit))
            }
            other => other.title(),
        }
    }

    /// The commit the outcome names, and what it is, for its fact's tooltip.
    pub fn commit(&self) -> Option<(&str, &'static str)> {
        match self {
            WriteEffect::Committed { commit, .. }
            | WriteEffect::Merged {
                commit: Some(commit),
                ..
            }
            | WriteEffect::Published {
                commit: Some(commit),
                ..
            } => Some((commit, "Graph commit")),
            WriteEffect::Proposal { commit, .. } => Some((commit, "Unpublished commit")),
            WriteEffect::Snapshot { commit } => Some((commit, "Unattached snapshot")),
            WriteEffect::UnknownKind { commit, .. } => Some((commit, "Commit")),
            WriteEffect::BranchCreated { at, .. } if !at.is_empty() => {
                Some((at, "The branch starts at graph commit"))
            }
            WriteEffect::Dropped { commit } if !commit.is_empty() => {
                Some((commit, "Dropped commit"))
            }
            _ => None,
        }
    }
}

/// The `branch diff` statement of `source` (after its settings prefix) continued at the page
/// `token` names, as the server's `page.next_page_token` asks: its `after page` clause given
/// `token`, or one put before its `limit`, else at its end. `None` for any other statement.
pub fn diff_page_text(source: &str, token: &str) -> Option<String> {
    let tokens = diff_tokens(source)?;
    let word = |i: usize| tokens.get(i).map(|t| &source[t.range.clone()]);
    let quoted = format!("\"{}\"", token.replace('\\', "\\\\").replace('"', "\\\""));
    let mut text = source.to_owned();
    if let Some(after) =
        (0..tokens.len()).find(|&i| word(i) == Some("after") && word(i + 1) == Some("page"))
    {
        let string = tokens
            .get(after + 2)
            .filter(|t| t.kind == TokenKind::String)?;
        text.replace_range(string.range.clone(), &quoted);
    } else if let Some(limit) = tokens.iter().find(|t| &source[t.range.clone()] == "limit") {
        text.insert_str(limit.range.start, &format!("after page {quoted} "));
    } else {
        let end = tokens.last()?.range.end;
        text.insert_str(end, &format!(" after page {quoted}"));
    }
    Some(text)
}

/// Whether `source` is a `branch diff` statement, which the server answers a page at a time.
pub fn is_branch_diff(source: &str) -> bool {
    diff_tokens(source).is_some()
}

/// The words of the `branch diff` statement `source` holds after its settings prefix, or
/// `None` when it holds another.
fn diff_tokens(source: &str) -> Option<Vec<gq_language::Token>> {
    let prefix = gq_language::outline(source).prefix.end;
    let tokens: Vec<gq_language::Token> = gq_language::tokenize(source)
        .into_iter()
        .filter(|t| t.range.start >= prefix && t.kind != TokenKind::Comment)
        .collect();
    let word = |i: usize| tokens.get(i).map(|t| &source[t.range.clone()]);
    (word(0) == Some("branch") && word(1) == Some("diff")).then_some(tokens)
}

/// Where the text sent to the server came from, so a position the server reports can be
/// mapped back into the editor's text as it was sent: the settings prefix is copied as is,
/// then `inserted` bytes the editor added (`explain `), then the declaration that starts at
/// `start` in the editor.
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

    /// A byte range of the text sent, as a range of the editor's text: both ends in the
    /// settings prefix, or both in the declaration. One that reaches into what the editor
    /// added (`explain `), or across it, has no place in the editor's text.
    pub fn editor_range(self, sent: std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
        let declaration = self.prefix_len + self.inserted;
        if sent.end <= self.prefix_len || sent.start >= declaration {
            Some(self.editor_offset(sent.start)..self.editor_offset(sent.end))
        } else {
            None
        }
    }
}

/// The diagnostics contract of a refused query, `DiagnosticOutput` in the server's contract
/// 0.14: a stable code (`Q…` parse, `T…` typecheck); where the failure is, as a source position
/// or, after parsing, the stage and the expression; what was expected or violated; one concrete
/// fix, absent when `expected` names the decision; and the edits that make it. `graph_client`
/// keeps it untyped (`ErrorOutput.diagnostic`), so it is read here, field by field.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnosticOutput {
    code: String,
    expected: String,
    /// `position.byte`: where in the text sent.
    byte: Option<usize>,
    stage: Option<String>,
    expression: Option<String>,
    fix: Option<String>,
    suggestion: Option<SuggestionOutput>,
}

/// Non-overlapping edits against the text sent, and whether they apply as they are
/// (`applicability` `machine_applicable`) or want a look first (`needs_review`, or a kind a
/// later server adds).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SuggestionOutput {
    machine_applicable: bool,
    edits: Vec<TextEditOutput>,
}

/// A UTF-8 byte range of the text sent, its end excluded, and what replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TextEditOutput {
    start: usize,
    end: usize,
    replacement: String,
}

impl DiagnosticOutput {
    /// `None` when a required field (`code`, `expected`, a suggestion's `applicability` and
    /// `edits`, an edit's `start`, `end` and `replacement`) is missing or of another type.
    fn read(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let suggestion = match value.get("suggestion") {
            None | Some(Value::Null) => None,
            Some(suggestion) => Some(SuggestionOutput {
                machine_applicable: suggestion.get("applicability")?.as_str()?
                    == "machine_applicable",
                edits: suggestion
                    .get("edits")?
                    .as_array()?
                    .iter()
                    .map(|edit| {
                        Some(TextEditOutput {
                            start: usize::try_from(edit.get("start")?.as_u64()?).ok()?,
                            end: usize::try_from(edit.get("end")?.as_u64()?).ok()?,
                            replacement: edit.get("replacement")?.as_str()?.to_owned(),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            }),
        };
        Some(Self {
            code: text("code")?,
            expected: text("expected")?,
            byte: value
                .get("position")
                .and_then(|position| position.get("byte"))
                .and_then(Value::as_u64)
                .and_then(|byte| usize::try_from(byte).ok()),
            stage: text("stage"),
            expression: text("expression"),
            fix: text("fix"),
            suggestion,
        })
    }
}

/// A problem the server reported, its position and edits already mapped into the editor's
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDiagnostic {
    pub code: String,
    pub message: String,
    pub byte: Option<usize>,
    /// The one concrete fix the server names, absent when the message names the decision.
    pub fix: Option<String>,
    /// The stage that found a problem after parsing (`typecheck`), when the server names one.
    pub stage: Option<String>,
    /// The server's suggested edits, in the editor's text as it was sent; empty when it
    /// suggests none, or when one has no place there.
    pub edits: Vec<(std::ops::Range<usize>, String)>,
    /// What the edits do, in words: ``insert `()` ``.
    pub suggestion: Option<String>,
    /// The server says to review the edits before running again.
    pub review: bool,
}

/// Reads an `ErrorOutput.diagnostic` in the contract's shape ([`DiagnosticOutput`]), `sent`
/// being the text the request carried, which its position and edits count in: a failure with
/// no position names the expression it was found in. `None` for a diagnostic in another shape;
/// the refusal's own message still says what went wrong.
pub fn server_diagnostic(value: &Value, sent: &str, map: SourceMap) -> Option<ServerDiagnostic> {
    let diagnostic = DiagnosticOutput::read(value)?;
    let byte = diagnostic.byte.map(|byte| map.editor_offset(byte));
    let message = match diagnostic.expression.filter(|_| byte.is_none()) {
        Some(expression) => format!("{}, in `{expression}`", diagnostic.expected),
        None => diagnostic.expected,
    };
    let (edits, suggestion, review) = match diagnostic.suggestion {
        Some(suggestion) => {
            let in_sent = suggestion.edits.iter().all(|edit| {
                edit.start <= edit.end
                    && sent.get(edit.start..edit.end).is_some()
                    && map.editor_range(edit.start..edit.end).is_some()
            });
            let edits: Vec<(std::ops::Range<usize>, String)> = if in_sent {
                suggestion
                    .edits
                    .iter()
                    .filter_map(|edit| {
                        Some((
                            map.editor_range(edit.start..edit.end)?,
                            edit.replacement.clone(),
                        ))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let said = in_sent.then(|| edits_in_words(sent, &suggestion.edits));
            (
                edits,
                said.filter(|said| !said.is_empty()),
                !suggestion.machine_applicable,
            )
        }
        None => (Vec::new(), None, false),
    };
    Some(ServerDiagnostic {
        code: diagnostic.code,
        message,
        byte,
        fix: diagnostic.fix,
        stage: diagnostic.stage,
        edits,
        suggestion,
        review,
    })
}

/// Suggested edits as a reader takes them: ``insert `()` ``, ``replace `nmae` with `name` ``,
/// ``delete `,` ``, the first two said and a count of the rest.
fn edits_in_words(sent: &str, edits: &[TextEditOutput]) -> String {
    /// The most characters of one side shown.
    const SHOWN: usize = 40;
    let shown = |text: &str| -> String {
        if text.chars().count() > SHOWN {
            format!("{}…", text.chars().take(SHOWN - 1).collect::<String>())
        } else {
            text.to_owned()
        }
    };
    let mut words: Vec<String> = edits
        .iter()
        .take(2)
        .filter_map(|edit| {
            let old = sent.get(edit.start..edit.end)?;
            Some(match (old.is_empty(), edit.replacement.is_empty()) {
                (true, true) => return None,
                (true, false) => format!("insert `{}`", shown(&edit.replacement)),
                (false, true) => format!("delete `{}`", shown(old)),
                (false, false) => {
                    format!(
                        "replace `{}` with `{}`",
                        shown(old),
                        shown(&edit.replacement)
                    )
                }
            })
        })
        .collect();
    if edits.len() > 2 {
        words.push(format!("{} more", edits.len() - 2));
    }
    words.join("; ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use studio_ui::value_text;

    use super::*;

    /// A column the compiler described, named `column` in the server's answer.
    fn column(column: &str, kind: &str, nullable: bool) -> ResultColumn {
        ResultColumn {
            name: column.rsplit('.').next().unwrap_or(column).into(),
            column: column.into(),
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

    /// The compiler names a column as the server does (`gq_language::check`), so types are
    /// matched by name: a column it did not describe has none, wherever it stands.
    #[test]
    fn tables_fill_missing_cells_and_take_types_by_name() {
        let rows = json!([{"p.name": "Ada", "p.born": "1984-04-14"}, {"p.name": "Alan"}]);
        let expected = [
            column("p.born", "Date", true),
            column("p.name", "String", false),
        ];
        let table = ResultTable::new(
            vec!["p.name".into(), "p.born".into()],
            rows.clone(),
            Some(&expected),
        );
        assert_eq!(table.rows[1], vec![json!("Alan"), Value::Null]);
        assert_eq!(table.types, [Some("String".into()), Some("Date?".into())]);
        let table = ResultTable::new(
            vec!["p.name".into(), "p.title".into()],
            rows,
            Some(&expected),
        );
        assert_eq!(table.types, [Some("String".into()), None]);
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
        // A load's plan begins with its logical plan before the optimizer; a tree a later
        // build adds reads as its words.
        let load = json!([
            {"tree": "logical_initial", "depth": 0, "node": "FragmentWrite", "detail": "{}"},
            {"tree": "later_tree", "depth": 0, "node": "Validate", "detail": "{}"}
        ]);
        let headers: Vec<PlanItem> = plan_items(&load)
            .into_iter()
            .filter(|item| matches!(item, PlanItem::Header(_)))
            .collect();
        assert_eq!(
            headers,
            [
                PlanItem::Header("Initial logical plan".into()),
                PlanItem::Header("Later tree".into())
            ]
        );
    }

    fn change(json: Value) -> ChangeOutput {
        serde_json::from_value(json).unwrap()
    }

    fn commit(kind: &str) -> Value {
        json!({"graph_commit_id": "01JCOMMITAAAA", "graph_manifest_version": 3, "created_at": 0, "kind": kind})
    }

    /// Each outcome says what moved, from the commit's kind and the statement's outcome, as
    /// the server answers them (`run_branch_statement`, `run_commit_statement`): only a
    /// published commit moved a branch.
    #[test]
    fn write_effects_say_what_moved() {
        let mutation = |commit: Value| {
            change(
                json!({"branch": "main", "query_name": "add", "affected_nodes": 1, "affected_edges": 0, "commit": commit}),
            )
        };
        let committed = WriteEffect::of(&mutation(commit("published")));
        assert_eq!(committed.title(), "Committed to main");
        assert_eq!(committed.note(1, 0), "Committed · 1 nodes, 0 edges");
        assert_eq!(committed.commit(), Some(("01JCOMMITAAAA", "Graph commit")));
        let unchanged = WriteEffect::of(&mutation(Value::Null));
        assert_eq!(unchanged.title(), "Nothing changed on main");
        assert_eq!(unchanged.commit(), None);

        let proposal = WriteEffect::of(&mutation(commit("unpublished")));
        assert_eq!(
            proposal,
            WriteEffect::Proposal {
                target: "main".into(),
                commit: "01JCOMMITAAAA".into()
            }
        );
        let title = proposal.title();
        assert!(title.starts_with("Held as commit ") && title.ends_with(", not on any branch yet"));
        assert!(!title.contains("Committed"), "{title}");
        assert!(
            proposal
                .detail()
                .unwrap()
                .contains("`commit publish \"01JCOMMITAAAA\"`")
        );
        let snapshot = WriteEffect::of(&mutation(commit("unattached")));
        assert!(snapshot.title().starts_with("Saved as snapshot "));
        assert_eq!(
            snapshot.commit(),
            Some(("01JCOMMITAAAA", "Unattached snapshot"))
        );

        let statement = |outcome: Value, commit: Value| {
            change(
                json!({"branch": "feature", "query_name": "branch", "affected_nodes": 0, "affected_edges": 0, "commit": commit, "outcome": outcome}),
            )
        };
        let created = WriteEffect::of(&statement(
            json!({"kind": "created", "from": "main", "name": "feature", "from_commit_id": "01JSTARTAAAA"}),
            Value::Null,
        ));
        assert_eq!(
            created,
            WriteEffect::BranchCreated {
                name: "feature".into(),
                from: "main".into(),
                at: "01JSTARTAAAA".into()
            }
        );
        assert!(
            created
                .title()
                .starts_with("Created branch feature at commit ")
        );
        let deleted = WriteEffect::of(&statement(
            json!({"kind": "deleted", "name": "feature"}),
            Value::Null,
        ));
        assert_eq!(deleted.title(), "Deleted branch feature");
        let dropped = WriteEffect::of(&statement(
            json!({"kind": "dropped", "graph_commit_id": "01JCOMMITAAAA"}),
            Value::Null,
        ));
        assert!(dropped.title().starts_with("Dropped commit "));
        let published = WriteEffect::of(&statement(
            json!({"kind": "published"}),
            commit("published"),
        ));
        assert!(published.title().starts_with("Published commit "));
        assert!(published.title().ends_with(" to feature"));
        let up_to_date = WriteEffect::of(&statement(
            json!({"kind": "merged", "source": "feature", "target": "main", "merge": "already_up_to_date"}),
            Value::Null,
        ));
        assert_eq!(
            up_to_date.title(),
            "main already has feature; nothing changed"
        );
        let merged = WriteEffect::of(&statement(
            json!({"kind": "merged", "source": "feature", "target": "main", "merge": "merged"}),
            commit("published"),
        ));
        assert_eq!(merged.title(), "Merged feature into main");
        // A merge `as commit` is a proposal: main did not move.
        let proposed = WriteEffect::of(&statement(
            json!({"kind": "merged", "source": "feature", "target": "main", "merge": "merged"}),
            commit("unpublished"),
        ));
        assert!(matches!(proposed, WriteEffect::Proposal { .. }));
        let later = WriteEffect::of(&statement(json!({"kind": "renamed"}), Value::Null));
        assert_eq!(
            later,
            WriteEffect::Statement {
                kind: "renamed".into(),
                branch: "feature".into()
            }
        );
    }

    /// A diff's next page is the same statement with the token: its clause's token replaced,
    /// or a clause put before its `limit`, else at its end.
    #[test]
    fn a_diff_continues_at_its_next_page() {
        assert_eq!(
            diff_page_text("branch diff feature into main", "t2").as_deref(),
            Some("branch diff feature into main after page \"t2\"")
        );
        assert_eq!(
            diff_page_text(
                "set x = 1;\n// changes\nbranch diff a to b limit 10\n",
                "t\"2"
            )
            .as_deref(),
            Some("set x = 1;\n// changes\nbranch diff a to b after page \"t\\\"2\" limit 10\n")
        );
        assert_eq!(
            diff_page_text("branch diff a after page \"t1\" limit 10", "t2").as_deref(),
            Some("branch diff a after page \"t2\" limit 10")
        );
        assert_eq!(diff_page_text("branch list", "t2"), None);
        // Each continued text parses as a diff with that token.
        let schema = SchemaModel::parse(include_str!("../../../dev/cluster/knowledge.pg")).unwrap();
        let check = |text: &str| gq_language::check(&schema, text).diagnostics;
        for text in [
            diff_page_text("branch diff feature into main", "t2").unwrap(),
            diff_page_text("branch diff a to b limit 10", "t\"2").unwrap(),
        ] {
            assert_eq!(check(&text), [], "{text}");
        }
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
        assert_eq!(rows_label(12_480), "12,480 rows");
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
        let sent = "set x = 1;\nexplain query b { match { $p: Person } }";
        let diagnostic = server_diagnostic(
            &json!({"code": "Q001", "position": {"line": 2, "column": 15, "byte": 25}, "expected": "expected return_clause"}),
            sent,
            map,
        )
        .expect("the contract's shape");
        assert_eq!(diagnostic.code, "Q001");
        assert_eq!(diagnostic.message, "expected return_clause");
        assert_eq!(diagnostic.byte, Some(47));
        assert_eq!(diagnostic.fix, None);
        assert!(diagnostic.edits.is_empty() && diagnostic.suggestion.is_none());

        // A failure found after parsing has no position: it names its expression, its stage
        // and the fix.
        let diagnostic = server_diagnostic(
            &json!({"code": "T27", "stage": "typecheck", "expression": "search($p.name, $q)", "expected": "a full-text index on Person.name", "fix": "declare @index(name) on Person"}),
            sent,
            map,
        )
        .expect("the contract's shape");
        assert_eq!(diagnostic.byte, None);
        assert_eq!(
            diagnostic.message,
            "a full-text index on Person.name, in `search($p.name, $q)`"
        );
        assert_eq!(
            diagnostic.fix.as_deref(),
            Some("declare @index(name) on Person")
        );
        assert_eq!(diagnostic.stage.as_deref(), Some("typecheck"));
    }

    /// The contract 0.14 shape is read, its suggested edits moved into the editor's text; the
    /// shape before it (`message`, `detail`, no `expected`) is not a diagnostic of this
    /// contract, and the refusal's own message stands for it.
    #[test]
    fn a_suggested_edit_lands_in_the_editors_text() {
        // The editor holds `// people\n` (10 bytes) and, at 30, `query people { … }`, sent
        // after that prefix alone.
        let map = SourceMap {
            prefix_len: 10,
            inserted: 0,
            start: 30,
        };
        let sent = "// people\nquery people { match { $p: Person } return { $p.name } }";
        let at = sent.find(" {").unwrap();
        let diagnostic = server_diagnostic(
            &json!({
                "code": "Q002",
                "position": {"line": 2, "column": 13, "byte": at},
                "expected": "a parameter list",
                "fix": "add `()` after the query's name",
                "suggestion": {"applicability": "machine_applicable",
                               "edits": [{"start": at, "end": at, "replacement": "()"}]},
            }),
            sent,
            map,
        )
        .unwrap();
        let editor_at = 30 + at - 10;
        assert_eq!(diagnostic.edits, [(editor_at..editor_at, "()".to_owned())]);
        assert_eq!(diagnostic.suggestion.as_deref(), Some("insert `()`"));
        assert!(!diagnostic.review);

        // A replacement to review, said as one; an edit of the `explain ` Studio added has no
        // place in the editor, so none of the suggestion is offered.
        let review = |edits: Value, map: SourceMap| {
            server_diagnostic(
                &json!({"code": "T6", "expected": "a property",
                        "suggestion": {"applicability": "needs_review", "edits": edits}}),
                "set x = 1;\nexplain query q() { return { $p.nmae } }",
                map,
            )
            .unwrap()
        };
        let explained = SourceMap {
            prefix_len: 11,
            inserted: 8,
            start: 40,
        };
        let name = "set x = 1;\nexplain query q() { return { $p.nmae } }"
            .find("nmae")
            .unwrap();
        let replaced = review(
            json!([{"start": name, "end": name + 4, "replacement": "name"}]),
            explained,
        );
        assert!(replaced.review);
        assert_eq!(
            replaced.suggestion.as_deref(),
            Some("replace `nmae` with `name`")
        );
        assert_eq!(replaced.edits[0].0.start, 40 + name - 19);
        let into_explain = review(
            json!([{"start": 11, "end": 19, "replacement": ""}]),
            explained,
        );
        assert!(into_explain.edits.is_empty() && into_explain.suggestion.is_none());
        let past_the_end = review(
            json!([{"start": 500, "end": 501, "replacement": "x"}]),
            explained,
        );
        assert!(past_the_end.edits.is_empty());

        for legacy in [
            json!({"code": "Q001", "message": "expected return_clause"}),
            json!({"message": "x", "detail": "y"}),
            json!({"code": "Q001", "expected": "x", "suggestion": {"edits": []}}),
        ] {
            assert_eq!(server_diagnostic(&legacy, sent, map), None, "{legacy}");
        }
    }
}
